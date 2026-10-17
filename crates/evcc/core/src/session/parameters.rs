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
                let Some(first) = m.schedule_tuple.first() else { return false };
                self.selected_tuple = first.schedule_tuple_id;
                self.tolerance_offered = first.charging_schedule.power_schedule.power_tolerance.is_some();
                self.scheduled_power = first.charging_schedule.power_schedule.power_schedule_entries.power_schedule_entry.first().map(|e| crate::ev::Phases { l1: e.power, l2: e.power_l2, l3: e.power_l3 });
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
