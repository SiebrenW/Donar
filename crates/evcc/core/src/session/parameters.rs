//! Charge parameter discovery and the schedule exchange.

use iso15118_20::common::{self, *};
use iso15118_20::common_types::Processing;
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

    pub(super) fn on_schedule_exchange_res(&mut self, res: ScheduleExchangeRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ScheduleExchangeRes, res.response_code) {
            return end;
        }
        self.log.record(&Record::EvseProcessing { processing: res.evse_processing });
        if !matches!(res.se_res_control_mode, common::SeResControlModeChoice::Dynamic(_)) {
            // We asked for Dynamic; a Scheduled answer is a protocol violation.
            return self.end(EndReason::UnexpectedMessage);
        }
        match res.evse_processing {
            Processing::Finished => {
                self.ongoing_armed = false;
                self.poll_pending = false;
                let mut step = self.send_cable_check_req(tx);
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

    pub(super) fn send_schedule_exchange_req(&mut self, tx: &mut [u8]) -> Step {
        let framed = self.frame_schedule_exchange_req(PayloadType::MAINSTREAM, tx);
        if self.state == State::AwaitScheduleExchangeRes {
            return self.sent(MsgKind::ScheduleExchangeReq, framed);
        }
        self.send_then(State::AwaitScheduleExchangeRes, MsgKind::ScheduleExchangeReq, framed)
    }

    pub(super) fn frame_schedule_exchange_req(&mut self, payload_type: PayloadType, tx: &mut [u8]) -> Result<usize, ()> {
        let r = self.ev.schedule_request();
        let msg = common::ScheduleExchangeReq {
            header: self.header(),
            maximum_supporting_points: self.config.maximum_supporting_points,
            se_req_control_mode: common::SeReqControlModeChoice::Dynamic(common::DynamicSeReqControlMode {
                departure_time: r.departure_time,
                minimum_soc: r.minimum_soc.map(|v| v as i8),
                target_soc: r.target_soc.map(|v| v as i8),
                ev_target_energy_request: r.target_energy,
                ev_maximum_energy_request: r.maximum_energy,
                ev_minimum_energy_request: r.minimum_energy,
                ev_maximum_v2x_energy_request: None,
                ev_minimum_v2x_energy_request: None,
            }),
        };
        frame(tx, payload_type, |out| msg.encode(out))
    }}
