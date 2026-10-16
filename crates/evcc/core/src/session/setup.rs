//! Session setup, authorization and service selection.

use app_protocol::{self, SupportedAppProtocolReq};
use iso15118_20::common::*;
use iso15118_20::common_types::{MessageHeader, Processing, ResponseCode};
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
        let resuming = self.config.resume;
        self.session_id = res.header.session_id;
        self.clock_offset = i128::from(res.header.time_stamp) - i128::from(self.ev.now_micros());
        // Switching to EVSE time may move the stamps backwards: the first two
        // messages are exempt from "always increasing" (V2G20-1538).
        self.last_timestamp = 0;
        self.log.record(&Record::SessionSetUp { session_id: self.session_id, evse_id: res.evseid });
        if let Some(old) = resuming {
            // V2G20-2539: joined with the id we sent. (That it is still the same charger is
            // for the host to check: the certificate of the new TLS connection, 8.3.4.1.4.3.)
            let joined = res.response_code == ResponseCode::OkOldSessionJoined && res.header.session_id == old.session_id;
            // V2G20-2615: any other id is a new session, with new authorization (V2G20-2617).
            let new = res.response_code == ResponseCode::OkNewSessionEstablished && res.header.session_id != old.session_id;
            if joined {
                // V2G20-1844, V2G20-2098: authorization and service selection are valid; the
                // next request is the charge parameter discovery.
                self.log.record(&Record::SessionResumed { session_id: old.session_id });
                self.selected = old.selected;
                self.renegotiation_supported = old.renegotiation_supported;
                let framed = self.frame_charge_parameter_discovery_req(PayloadType::DC_MAINSTREAM, tx);
                let mut step = self.send_then(State::AwaitChargeParameterDiscoveryRes, MsgKind::DcChargeParameterDiscoveryReq, framed);
                if step.end.is_none() {
                    step.timers = step.timers.with(TimerCmd::Cancel(Timer::Setup));
                    step.notice = Some(Notice::SessionResumed { session_id: old.session_id });
                }
                return step;
            }
            if !new {
                // V2G20-2613, V2G20-2614: neither joined nor a clean new session.
                return self.end(EndReason::UnexpectedMessage);
            }
        }
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
            self.log.record(&Record::NoAuthorization { wanted: Authorization::Eim, offered: res.authorization_services });
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
                let mut step = self.send_service_discovery_req(tx);
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

    pub(super) fn send_service_discovery_req(&mut self, tx: &mut [u8]) -> Step {
        let msg = ServiceDiscoveryReq { header: self.header(), supported_service_ids: None };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        self.send_then(State::AwaitServiceDiscoveryRes, MsgKind::ServiceDiscoveryReq, framed)
    }

    pub(super) fn on_service_discovery_res(&mut self, res: ServiceDiscoveryRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServiceDiscoveryRes, res.response_code) {
            return end;
        }
        // V2G20-1480: only the first ServiceDiscoveryRes of the service session counts.
        if self.renegotiation_supported.is_none() {
            self.renegotiation_supported = Some(res.service_renegotiation_supported);
        }
        let wanted = self.config.energy_service;
        if !res.energy_transfer_service_list.service.iter().any(|s| s.service_id == wanted) {
            self.log.record(&Record::NoService { wanted, offered: res.energy_transfer_service_list.service });
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
            self.log.record(&Record::NoParameterSet {
                connector: self.ev.dc_connector() as i32,
                filter: self.config.parameter_filter,
                offered: res.service_parameter_list.parameter_set,
            });
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
        let framed = self.frame_charge_parameter_discovery_req(PayloadType::DC_MAINSTREAM, tx);
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

    /// The request for the EVSE's DC limits; also the first message of a schedule renegotiation.
    pub(super) fn frame_charge_parameter_discovery_req(&mut self, payload_type: PayloadType, tx: &mut [u8]) -> Result<usize, ()> {
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
        frame(tx, payload_type, |out| msg.encode(out))
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
            // V2G20-1243: zeroes for a new session, V2G20-1541: the old id to resume one.
            header: MessageHeader::new(self.config.resume.map_or([0; 8], |r| r.session_id), self.timestamp()),
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
