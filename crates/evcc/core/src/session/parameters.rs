//! Charge parameter discovery and the schedule exchange.

use iso15118_20::common::{self, *};
use iso15118_20::common_types::Processing;
use iso15118_20::ac;
use iso15118_20::dc;
use v2gtp::PayloadType;

use super::{Machine, frame};
use super::types::*;
use crate::ev::Ev;
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
            (common::SeResControlModeChoice::Dynamic(_), false) => true,
            (common::SeResControlModeChoice::Scheduled(m), true) => {
                if m.schedule_tuple.is_empty() {
                    return false;
                }
                // V2G20-297, 298: the first tuple is the default; a price schedule that can be compared
                // lets the vehicle pick the cheapest that delivers what it needs before it leaves.
                let r = self.ev.schedule_request();
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

    pub(super) fn send_schedule_exchange_req(&mut self, tx: &mut [u8]) -> Step {
        let framed = self.frame_schedule_exchange_req(PayloadType::MAINSTREAM, tx);
        if self.state == State::AwaitScheduleExchangeRes {
            return self.sent(MsgKind::ScheduleExchangeReq, framed);
        }
        self.send_then(State::AwaitScheduleExchangeRes, MsgKind::ScheduleExchangeReq, framed)
    }

    pub(super) fn frame_schedule_exchange_req(&mut self, payload_type: PayloadType, tx: &mut [u8]) -> Result<usize, ()> {
        let r = self.ev.schedule_request();
        let mode = if self.config.scheduled {
            // The vehicle offers no energy schedule of its own (EVEnergyOffer).
            common::SeReqControlModeChoice::Scheduled(common::ScheduledSeReqControlMode {
                departure_time: Some(r.departure_time),
                ev_target_energy_request: Some(r.target_energy),
                ev_maximum_energy_request: Some(r.maximum_energy),
                ev_minimum_energy_request: Some(r.minimum_energy),
                ev_energy_offer: None,
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
        let msg = common::ScheduleExchangeReq { header: self.header(), maximum_supporting_points: self.config.maximum_supporting_points, se_req_control_mode: mode };
        frame(tx, payload_type, |out| msg.encode(out))
    }}
