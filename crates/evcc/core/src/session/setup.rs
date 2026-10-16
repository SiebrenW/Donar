//! Session setup, authorization and service selection.

use app_protocol::{self, SupportedAppProtocolReq};
use iso15118_20::common::*;
use iso15118_20::common_types::{MessageHeader, Processing};
use iso15118_20::dc;
use v2gtp::PayloadType;

use super::{Machine, frame, is_ok};
use super::types::*;
use crate::ev::Ev;
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd};

impl<E: Ev, L: LogSink> Machine<E, L> {
    pub(super) fn on_session_setup_res(&mut self, res: SessionSetupRes<'_>, tx: &mut [u8]) -> Step {
        self.log.record(&Record::MsgRx { msg: MsgKind::SessionSetupRes });
        self.log.record(&Record::Response { msg: MsgKind::SessionSetupRes, code: res.response_code });
        if !is_ok(res.response_code) {
            return self.end(EndReason::Rejected(res.response_code));
        }
        self.session_id = res.header.session_id;
        self.clock_offset = i128::from(res.header.time_stamp) - i128::from(self.ev.now_micros());
        // Switching to EVSE time may move the stamps backwards: the first two
        // messages are exempt from "always increasing" (V2G20-1538).
        self.last_timestamp = 0;
        self.log.record(&Record::SessionSetUp { session_id: self.session_id, evse_id: res.evseid });
        let msg = AuthorizationSetupReq { header: self.header() };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        let mut step = self.send_then(State::AwaitAuthorizationSetupRes, MsgKind::AuthorizationSetupReq, framed);
        step.notice = Some(Notice::SessionSetUp { session_id: self.session_id });
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::Setup));
        }
        step
    }

    pub(super) fn on_authorization_setup_res(&mut self, res: AuthorizationSetupRes<'_>, tx: &mut [u8]) -> Step {
        self.log.record(&Record::MsgRx { msg: MsgKind::AuthorizationSetupRes });
        self.log.record(&Record::Response { msg: MsgKind::AuthorizationSetupRes, code: res.response_code });
        if !is_ok(res.response_code) {
            return self.end(EndReason::Rejected(res.response_code));
        }
        if !res.authorization_services.contains(&Authorization::Eim) {
            return self.end(EndReason::NoCompatibleAuthorization);
        }
        self.send_authorization_req(State::AwaitAuthorizationRes, tx)
    }

    pub(super) fn on_authorization_res(&mut self, res: AuthorizationRes, tx: &mut [u8]) -> Step {
        self.log.record(&Record::MsgRx { msg: MsgKind::AuthorizationRes });
        self.log.record(&Record::Response { msg: MsgKind::AuthorizationRes, code: res.response_code });
        self.log.record(&Record::EvseProcessing { processing: res.evse_processing });
        if !is_ok(res.response_code) {
            return self.end(EndReason::Rejected(res.response_code));
        }
        match res.evse_processing {
            Processing::Finished => {
                let msg = ServiceDiscoveryReq { header: self.header(), supported_service_ids: None };
                let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
                let mut step =
                    self.send_then(State::AwaitServiceDiscoveryRes, MsgKind::ServiceDiscoveryReq, framed);
                if step.end.is_none() {
                    self.ongoing_armed = false;
                    step.timers = step.timers.with(TimerCmd::Cancel(Timer::Ongoing));
                    step.notice = Some(Notice::Authorized);
                }
                step
            }
            // Ref: ISO 15118-20:2022 V2G20-1582: after an AuthorizationRes with
            // ResponseCode OK and EVSEProcessing Ongoing the EVCC sends another
            // unaltered AuthorizationReq (except TimeStamp) within
            // V2G_EVCC_Sequence_Performance_Time. Ongoing_WaitingForCustomerInteraction
            // is not in the 2022 text (only in the schema); it is treated the same.
            Processing::Ongoing | Processing::OngoingWaitingForCustomerInteraction => {
                self.ongoing_repeat(tx)
            }
        }
    }

    pub(super) fn on_service_discovery_res(&mut self, res: ServiceDiscoveryRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServiceDiscoveryRes, res.response_code) {
            return end;
        }
        let wanted = self.config.energy_service;
        if !res.energy_transfer_service_list.service.iter().any(|s| s.service_id == wanted) {
            return self.end(EndReason::NoCompatibleService);
        }
        let msg = ServiceDetailReq { header: self.header(), service_id: wanted };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        self.send_then(State::AwaitServiceDetailRes, MsgKind::ServiceDetailReq, framed)
    }

    pub(super) fn on_service_detail_res(&mut self, res: ServiceDetailRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServiceDetailRes, res.response_code) {
            return end;
        }
        if res.service_id != self.config.energy_service {
            return self.end(EndReason::UnexpectedMessage);
        }
        let Some(parameter_set_id) = self.pick_parameter_set(res.service_parameter_list.parameter_set) else {
            return self.end(EndReason::NoCompatibleParameterSet);
        };
        let selected = SelectedService { service_id: res.service_id, parameter_set_id };
        self.selected = selected;
        let msg = ServiceSelectionReq {
            header: self.header(),
            selected_energy_transfer_service: selected,
            selected_vas_list: None,
        };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        self.send_then(State::AwaitServiceSelectionRes, MsgKind::ServiceSelectionReq, framed)
    }

    pub(super) fn on_service_selection_res(&mut self, res: ServiceSelectionRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServiceSelectionRes, res.response_code) {
            return end;
        }
        let SelectedService { service_id, parameter_set_id } = self.selected;
        self.log.record(&Record::ServiceSelected { service_id, parameter_set_id });
        let limits = self.ev.dc_limits();
        let msg = dc::DcChargeParameterDiscoveryReq {
            header: self.header(),
            dc_cpd_req_energy_transfer_mode: dc::DcCpdReqEnergyTransferModeChoice::Base(
                dc::DcCpdReqEnergyTransferMode {
                    ev_maximum_charge_power: limits.maximum_charge_power,
                    ev_minimum_charge_power: limits.minimum_charge_power,
                    ev_maximum_charge_current: limits.maximum_charge_current,
                    ev_minimum_charge_current: limits.minimum_charge_current,
                    ev_maximum_voltage: limits.maximum_voltage,
                    ev_minimum_voltage: limits.minimum_voltage,
                    target_soc: limits.target_soc.map(|s| s as i8),
                },
            ),
        };
        let framed = frame(tx, PayloadType::DC_MAINSTREAM, |out| msg.encode(out));
        let mut step = self.send_then(
            State::AwaitChargeParameterDiscoveryRes,
            MsgKind::DcChargeParameterDiscoveryReq,
            framed,
        );
        if step.end.is_none() {
            step.notice = Some(Notice::ServiceSelected { service_id, parameter_set_id });
        }
        step
    }

    pub(super) fn send_supported_app_protocol_req(&mut self, tx: &mut [u8]) -> Step {
        let offer = [self.config.offer];
        let msg = SupportedAppProtocolReq { app_protocol: &offer };
        let framed = frame(tx, PayloadType::SAP, |out| msg.encode(out));
        self.send_then(State::AwaitSupportedAppProtocolRes, MsgKind::SupportedAppProtocolReq, framed)
    }
    pub(super) fn on_supported_app_protocol_res(&mut self, res: app_protocol::SupportedAppProtocolRes, tx: &mut [u8]) -> Step {
        if res.response_code == app_protocol::ResponseCode::FailedNoNegotiation {
            return self.end(EndReason::NoCommonProtocol);
        }
        // The EVSE must pick the schema id we offered.
        if res.schema_id != Some(self.config.offer.schema_id) {
            return self.end(EndReason::NoCommonProtocol);
        }
        let msg = SessionSetupReq {
            header: MessageHeader { session_id: [0; 8], time_stamp: self.timestamp() },
            evccid: self.ev.evcc_id(),
        };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        self.send_then(State::AwaitSessionSetupRes, MsgKind::SessionSetupReq, framed)
    }
    pub(super) fn send_authorization_req(&mut self, next: State, tx: &mut [u8]) -> Step {
        // EIM is the only authorization mode implemented, so the request carries no data
        // beyond the header; a repeat is therefore identical except for its timestamp.
        let msg = AuthorizationReq {
            header: self.header(),
            selected_authorization_service: Authorization::Eim,
            a_req_authorization_mode: AReqAuthorizationModeChoice::Eim(EimAReqAuthorizationMode {}),
        };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        if self.state == next {
            // A repeat: stay in the same state without logging a transition.
            return self.sent(MsgKind::AuthorizationReq, framed);
        }
        self.send_then(next, MsgKind::AuthorizationReq, framed)
    }
    pub(super) fn pick_parameter_set(&self, sets: &[ParameterSet<'_>]) -> Option<u16> {
        let as_int = |v: ParameterChoice<'_>| match v {
            ParameterChoice::ByteValue(v) => Some(i32::from(v)),
            ParameterChoice::ShortValue(v) => Some(i32::from(v)),
            ParameterChoice::IntValue(v) => Some(v),
            _ => None,
        };
        let connector = self.ev.dc_connector() as i32;
        let has = |set: &ParameterSet<'_>, name: &str, want: i32| {
            set.parameter.iter().any(|p| p.name == name && as_int(p.choice) == Some(want))
        };
        sets.iter()
            .find(|set| {
                has(set, "Connector", connector)
                    && self.config.parameter_filter.iter().all(|&(name, want)| has(set, name, want))
            })
            .map(|set| set.parameter_set_id)
    }}
