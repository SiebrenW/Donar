//! Charge parameter discovery and the schedule exchange.

use iso15118_20::common::{self, *};
use iso15118_20::common_types::{Processing, RationalNumber};
use iso15118_20::ac;
use iso15118_20::dc;
use v2gtp::PayloadType;

use super::{Machine, frame};
use super::types::*;
use crate::ev::{Ev, EnergyOffer};
use crate::log::{LogSink, MsgKind, Record};

impl<E: Ev, L: LogSink> Machine<E, L> {
    pub(super) fn on_charge_parameter_discovery_res(&mut self, res: dc::DcChargeParameterDiscoveryRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::DcChargeParameterDiscoveryRes, res.response_code) {
            return end;
        }
        self.note_evse_limits(&res.dc_cpd_res_energy_transfer_mode);
        let mut step = self.send_schedule_exchange_req(tx);
        if step.end.is_none() {
            step.notice = Some(Notice::ChargeParametersDiscovered(res.dc_cpd_res_energy_transfer_mode));
        }
        step
    }

    pub(super) fn on_ac_charge_parameter_discovery_res(&mut self, res: ac::AcChargeParameterDiscoveryRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::AcChargeParameterDiscoveryRes, res.response_code) {
            return end;
        }
        self.note_evse_ac_limits(&res.ac_cpd_res_energy_transfer_mode);
        let mut step = self.send_schedule_exchange_req(tx);
        if step.end.is_none() {
            step.notice = Some(Notice::AcChargeParametersDiscovered(res.ac_cpd_res_energy_transfer_mode));
        }
        step
    }

    pub(super) fn on_schedule_exchange_res(&mut self, res: ScheduleExchangeRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ScheduleExchangeRes, res.response_code) {
            return end;
        }
        self.log.record(&Record::EvseProcessing { processing: res.evse_processing });
        if !self.take_schedule(&res.se_res_control_mode) {
            // We asked for one mode; the other is a protocol violation.
            return self.end(EndReason::UnexpectedMessage);
        }
        match res.evse_processing {
            Processing::Finished => {
                self.ongoing_armed = false;
                self.poll_pending = false;
                // AC has no cable check or pre-charge: the power delivery starts at once.
                let mut step = if self.config.ac() { self.send_power_delivery(common::ChargeProgress::Start, tx) } else { self.send_cable_check_req(tx) };
                if step.end.is_none() {
                    step.notice = Some(Notice::ScheduleExchanged);
                }
                step
            }
            // While the EVSE is still working the request is repeated, like
            // the AuthorizationReq (the 2022 text has no ScheduleExchange-specific requirement for it;
            // V2G_EVCC_Ongoing_Timeout covers any response with EVSEProcessing Ongoing).
            Processing::Ongoing | Processing::OngoingWaitingForCustomerInteraction => {
                self.ongoing_repeat(tx)
            }
        }
    }

    /// Whether the answer is of the control mode asked for; in Scheduled mode the vehicle selects
    /// the first schedule tuple (and notes its power, which it follows when the charger names no
    /// target).
    pub(super) fn take_schedule(&mut self, mode: &common::SeResControlModeChoice<'_>) -> bool {
        match (mode, self.config.scheduled) {
            (common::SeResControlModeChoice::Dynamic(m), false) => {
                self.take_mobility_needs(m.departure_time, m.minimum_soc, m.target_soc);
                true
            }
            (common::SeResControlModeChoice::Scheduled(m), true) => {
                if m.schedule_tuple.is_empty() {
                    return false;
                }
                // V2G20-297, 298: the first tuple is the default; a price schedule that can be compared
                // lets the vehicle pick the cheapest that delivers what it needs before it leaves.
                let r = self.request();
                let max_watts = if self.config.ac() {
                    let l = self.ev_announced_ac.unwrap_or_else(|| self.ev.ac20_limits()).maximum_charge_power;
                    super::config::rational_to_volts(l.l1) + l.l2.map_or(0.0, super::config::rational_to_volts) + l.l3.map_or(0.0, super::config::rational_to_volts)
                } else {
                    super::config::rational_to_volts(self.ev_announced.unwrap_or_else(|| self.ev.dc_limits()).maximum_charge_power)
                };
                let need_wh = super::config::rational_to_volts(r.target_energy).max(0.0);
                let chosen = &m.schedule_tuple[super::tariff::choose(m.schedule_tuple, need_wh, max_watts, r.departure_time as f32)];
                self.selected_tuple = chosen.schedule_tuple_id;
                self.log.record(&crate::log::Record::ScheduleSelected { tuple_id: chosen.schedule_tuple_id, offered: m.schedule_tuple.len(), need_wh: need_wh as u32, departure_s: r.departure_time });
                self.tolerance_offered = chosen.charging_schedule.power_schedule.power_tolerance.is_some();
                self.schedule = super::schedule::Schedule::of(chosen, self.secc_now());
                true
            }
            _ => false,
        }
    }

    /// The vehicle's energy request, with the departure time and the state of charge limits the charger
    /// named laid over it when the charger provides the mobility needs (`MobilityNeedsMode` 2). The
    /// departure time counts down from the answer that named it (it is always above zero, V2G20-2104).
    pub(super) fn request(&self) -> crate::ev::ScheduleRequest {
        let mut r = self.ev.schedule_request();
        if self.config.mobility_needs_by_secc {
            if let Some(departure) = self.mobility.departure_time {
                let elapsed_s = self.secc_now().saturating_sub(self.mobility_at) / 1_000_000;
                r.departure_time = departure.saturating_sub(u32::try_from(elapsed_s).unwrap_or(u32::MAX)).max(1);
            }
            r.minimum_soc = self.mobility.minimum_soc.or(r.minimum_soc);
            r.target_soc = self.mobility.target_soc.or(r.target_soc);
        }
        r
    }

    /// The mobility needs an answer named. With `MobilityNeedsMode` 2 they replace the vehicle's; the
    /// return is what changed. Otherwise the charger has no business naming them (V2G20-2663 offers
    /// the mode, and the vehicle did not choose it): they are ignored and logged.
    pub(super) fn take_mobility_needs(&mut self, departure_time: Option<u32>, minimum_soc: Option<i8>, target_soc: Option<i8>) -> Option<crate::ev::MobilityNeeds> {
        if departure_time.is_none() && minimum_soc.is_none() && target_soc.is_none() {
            return None;
        }
        let named = crate::ev::MobilityNeeds { departure_time, minimum_soc: minimum_soc.map(|v| v.clamp(0, 100) as u8), target_soc: target_soc.map(|v| v.clamp(0, 100) as u8) };
        if !self.config.mobility_needs_by_secc {
            self.log.record(&Record::MobilityNeedsIgnored { needs: named });
            return None;
        }
        let before = self.mobility;
        if let Some(d) = named.departure_time {
            self.mobility.departure_time = Some(d);
            self.mobility_at = self.secc_now();
        }
        self.mobility.minimum_soc = named.minimum_soc.or(self.mobility.minimum_soc);
        self.mobility.target_soc = named.target_soc.or(self.mobility.target_soc);
        // A departure time restates the countdown even when it is the same number.
        if self.mobility == before && named.departure_time.is_none() {
            return None;
        }
        self.log.record(&Record::MobilityNeedsFromSecc { needs: named });
        Some(named)
    }

    pub(super) fn send_schedule_exchange_req(&mut self, tx: &mut [u8]) -> Step {
        let framed = self.frame_schedule_exchange_req(PayloadType::MAINSTREAM, tx);
        if self.state == State::AwaitScheduleExchangeRes {
            return self.sent(MsgKind::ScheduleExchangeReq, framed);
        }
        self.send_then(State::AwaitScheduleExchangeRes, MsgKind::ScheduleExchangeReq, framed)
    }

    pub(super) fn frame_schedule_exchange_req(&mut self, payload_type: PayloadType, tx: &mut [u8]) -> Result<usize, ()> {
        let r = self.request();
        let header = self.header();
        let anchor = self.secc_now();
        // The energy offer (Scheduled mode): one rule stack per price, built here so that they can borrow
        // their rules.
        let offer = self.ev.energy_offer().filter(|o| !o.power_entries().is_empty() && !o.prices().is_empty());
        let zero = RationalNumber { exponent: 0, value: 0 };
        let price = |i: usize| offer.as_ref().and_then(|o| o.prices().get(i));
        let rules: [EvPriceRule; EnergyOffer::CAPACITY] = core::array::from_fn(|i| EvPriceRule { energy_fee: price(i).map_or(zero, |p| p.energy_fee), power_range_start: zero });
        let stacks: [EvPriceRuleStack<'_>; EnergyOffer::CAPACITY] = core::array::from_fn(|i| EvPriceRuleStack { duration: price(i).map_or(0, |p| p.duration), ev_price_rule: &rules[i..=i] });
        let mode = if self.config.scheduled {
            common::SeReqControlModeChoice::Scheduled(common::ScheduledSeReqControlMode {
                departure_time: Some(r.departure_time),
                ev_target_energy_request: Some(r.target_energy),
                ev_maximum_energy_request: Some(r.maximum_energy),
                ev_minimum_energy_request: Some(r.minimum_energy),
                ev_energy_offer: offer.as_ref().map(|o| EvEnergyOffer {
                    ev_power_schedule: EvPowerSchedule { time_anchor: anchor, ev_power_schedule_entries: EvPowerScheduleEntryList { ev_power_schedule_entry: o.power_entries() } },
                    ev_absolute_price_schedule: EvAbsolutePriceSchedule {
                        time_anchor: anchor,
                        currency: o.currency,
                        price_algorithm: o.price_algorithm,
                        ev_price_rule_stacks: EvPriceRuleStackList { ev_price_rule_stack: &stacks[..o.prices().len()] },
                    },
                }),
            })
        } else {
            common::SeReqControlModeChoice::Dynamic(common::DynamicSeReqControlMode {
                departure_time: r.departure_time,
                minimum_soc: r.minimum_soc.map(|v| v as i8),
                target_soc: r.target_soc.map(|v| v as i8),
                ev_target_energy_request: r.target_energy,
                ev_maximum_energy_request: r.maximum_energy,
                ev_minimum_energy_request: r.minimum_energy,
                ev_maximum_v2x_energy_request: None,
                ev_minimum_v2x_energy_request: None,
            })
        };
        let msg = common::ScheduleExchangeReq { header, maximum_supporting_points: self.config.maximum_supporting_points, se_req_control_mode: mode };
        frame(tx, payload_type, |out| msg.encode(out))
    }}
