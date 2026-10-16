//! The simulated EVSE: a sans-IO state machine from received bytes to frames to send.

use std::time::{SystemTime, UNIX_EPOCH};

use app_protocol::{self, ResponseCode as SapCode, SupportedAppProtocolRes};
use exi::{Arena, EncodeError};
use iso15118_20::common::{
    self, AsResAuthorizationModeChoice, Authorization, AuthorizationRes,
    AuthorizationSetupRes, EimAsResAuthorizationMode, Parameter, ParameterChoice, ParameterSet, SelectedService,
    Service, ServiceDetailRes, ServiceDiscoveryRes, ServiceList, ServiceParameterList, ServiceSelectionRes,
    SessionSetupRes,
};
use iso15118_20::common_types::{self, MessageHeader, Processing, RationalNumber, ResponseCode};
use iso15118_20::dc;
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use crate::config::Config;

const DC_NAMESPACE: &str = "urn:iso:std:iso:15118:-20:DC";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    AwaitSupportedAppProtocolReq,
    AwaitSessionSetupReq,
    AwaitAuthorizationSetupReq,
    AwaitAuthorizationReq,
    AwaitServiceDiscoveryReq,
    AwaitServiceDetailReq,
    AwaitServiceSelectionReq,
    AwaitChargeParameterDiscoveryReq,
    AwaitScheduleExchangeReq,
    AwaitCableCheckReq,
    AwaitPreChargeReq,
    AwaitPowerDeliveryReq,
    /// Energy flows; charge loop requests and the stop are accepted.
    ChargeLoop,
    /// Welding detection after the stop (V2G20-1448); ends with a `Finished` request.
    AwaitWeldingReq,
    AwaitSessionStopReq,
    /// The session was stopped properly.
    Stopped,
    Closed,
}

/// What the host should do after a step.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    /// A framed message to write to the stream.
    pub send: Option<Vec<u8>>,
    /// Close the connection afterwards.
    pub close: bool,
}

pub struct Evse {
    config: Config,
    state: State,
    rx: Vec<u8>,
    schema_id: u8,
    authorization_reqs_seen: u32,
    schedule_reqs_seen: u32,
    cable_reqs_seen: u32,
    precharge_reqs_seen: u32,
    loops_seen: u32,
}

impl Evse {
    pub fn new(config: Config) -> Self {
        Self { config, state: State::AwaitSupportedAppProtocolReq, rx: Vec::new(), schema_id: 0, authorization_reqs_seen: 0, schedule_reqs_seen: 0, cable_reqs_seen: 0, precharge_reqs_seen: 0, loops_seen: 0 }
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Feeds bytes read from the stream; returns the messages to send, in order.
    pub fn handle(&mut self, bytes: &[u8]) -> Vec<Output> {
        self.rx.extend_from_slice(bytes);
        let mut outputs = Vec::new();
        while self.state != State::Closed {
            let (payload_type, payload, consumed) = match parse(&self.rx, 1 << 20) {
                Ok(Parse::Frame(f)) => (f.payload_type, f.payload.to_vec(), f.consumed),
                Ok(Parse::Incomplete { .. }) => break,
                Err(_) => return self.close(outputs),
            };
            self.rx.drain(..consumed);
            let out = self.on_frame(payload_type, &payload);
            let close = out.close;
            outputs.push(out);
            // A properly stopped session keeps that state (it is what the tests look at).
            if close && self.state != State::Stopped {
                self.state = State::Closed;
            }
        }
        outputs
    }

    fn close(&mut self, mut outputs: Vec<Output>) -> Vec<Output> {
        self.state = State::Closed;
        outputs.push(Output { send: None, close: true });
        outputs
    }

    fn on_frame(&mut self, payload_type: PayloadType, payload: &[u8]) -> Output {
        let mut mem = vec![0u8; 8192];
        let arena = Arena::new(&mut mem);
        let closed = Output { send: None, close: true };
        match self.state {
            State::AwaitSupportedAppProtocolReq if payload_type == PayloadType::SAP => {
                let Ok(app_protocol::Message::SupportedAppProtocolReq(req)) = app_protocol::decode(payload, &arena)
                else {
                    return closed;
                };
                // Highest priority = lowest number among the protocols we speak.
                let pick = req
                    .app_protocol
                    .iter()
                    .filter(|p| p.protocol_namespace == DC_NAMESPACE && p.version_number_major == 1)
                    .min_by_key(|p| p.priority);
                match pick.filter(|_| !self.config.reject_protocol) {
                    Some(p) => {
                        self.schema_id = p.schema_id;
                        self.state = State::AwaitSessionSetupReq;
                        let res = SupportedAppProtocolRes { response_code: SapCode::OkSuccessfulNegotiation, schema_id: Some(p.schema_id) };
                        Output { send: frame(PayloadType::SAP, |out| res.encode(out)), close: false }
                    }
                    None => {
                        let res = SupportedAppProtocolRes { response_code: SapCode::FailedNoNegotiation, schema_id: None };
                        Output { send: frame(PayloadType::SAP, |out| res.encode(out)), close: true }
                    }
                }
            }
            State::AwaitSessionSetupReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::SessionSetupReq(_)) = common::decode(payload, &arena) else { return closed };
                if self.config.silent_on_setup {
                    return Output::default();
                }
                let ok = matches!(
                    self.config.setup_code,
                    ResponseCode::Ok | ResponseCode::OkNewSessionEstablished | ResponseCode::OkOldSessionJoined
                );
                let res = SessionSetupRes {
                    header: MessageHeader { session_id: self.config.session_id, time_stamp: now() },
                    response_code: self.config.setup_code,
                    evseid: &self.config.evse_id,
                };
                if ok {
                    self.state = State::AwaitAuthorizationSetupReq;
                }
                Output {
                    send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)),
                    close: !ok,
                }
            }
            State::AwaitAuthorizationSetupReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::AuthorizationSetupReq(req)) = common::decode(payload, &arena) else { return closed };
                if req.header.session_id != self.config.session_id {
                    return self.unknown_session(common::Message::AuthorizationSetupReq(req));
                }
                if self.config.offered_authorization.is_empty() || self.config.offered_authorization.len() > 2 {
                    return closed;
                }
                self.state = State::AwaitAuthorizationReq;
                let res = AuthorizationSetupRes {
                    header: self.header(),
                    response_code: ResponseCode::Ok,
                    authorization_services: &self.config.offered_authorization,
                    certificate_installation_service: false,
                    as_res_authorization_mode: AsResAuthorizationModeChoice::Eim(EimAsResAuthorizationMode {}),
                };
                Output { send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)), close: false }
            }
            State::AwaitAuthorizationReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::AuthorizationReq(req)) = common::decode(payload, &arena) else { return closed };
                if req.header.session_id != self.config.session_id {
                    return self.unknown_session(common::Message::AuthorizationReq(req));
                }
                self.authorization_reqs_seen += 1;
                let ongoing = self.config.authorization_never_finishes
                    || self.authorization_reqs_seen <= self.config.authorization_ongoing_responses;
                let (evse_processing, response_code) = if ongoing {
                    (Processing::Ongoing, ResponseCode::Ok)
                } else {
                    (Processing::Finished, self.config.authorization_code)
                };
                let res = AuthorizationRes { header: self.header(), response_code, evse_processing };
                let finished_ok = !ongoing && matches!(response_code, ResponseCode::Ok);
                if finished_ok {
                    self.state = State::AwaitServiceDiscoveryReq;
                }
                Output {
                    send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)),
                    close: !ongoing && !finished_ok,
                }
            }
            State::AwaitServiceDiscoveryReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::ServiceDiscoveryReq(req)) = common::decode(payload, &arena) else { return closed };
                if req.header.session_id != self.config.session_id {
                    return self.discovery_res(ResponseCode::FailedUnknownSession);
                }
                self.state = State::AwaitServiceDetailReq;
                self.discovery_res(ResponseCode::Ok)
            }
            State::AwaitServiceDetailReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::ServiceDetailReq(req)) = common::decode(payload, &arena) else { return closed };
                if req.header.session_id != self.config.session_id {
                    return self.detail_res(ResponseCode::FailedUnknownSession, req.service_id);
                }
                if !self.config.energy_services.contains(&req.service_id) {
                    return self.detail_res(ResponseCode::FailedServiceIdInvalid, req.service_id);
                }
                self.state = State::AwaitServiceSelectionReq;
                self.detail_res(ResponseCode::Ok, req.service_id)
            }
            State::AwaitServiceSelectionReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::ServiceSelectionReq(req)) = common::decode(payload, &arena) else { return closed };
                if req.header.session_id != self.config.session_id {
                    return self.selection_res(ResponseCode::FailedUnknownSession);
                }
                let SelectedService { service_id, parameter_set_id } = req.selected_energy_transfer_service;
                let valid = self.config.energy_services.contains(&service_id)
                    && self.config.parameter_sets.iter().any(|(id, _)| *id == parameter_set_id);
                if !valid {
                    return self.selection_res(ResponseCode::FailedServiceSelectionInvalid);
                }
                let code = self.config.selection_code;
                if matches!(code, ResponseCode::Ok) {
                    self.state = State::AwaitChargeParameterDiscoveryReq;
                }
                self.selection_res(code)
            }
            State::AwaitChargeParameterDiscoveryReq if payload_type == PayloadType::DC_MAINSTREAM => {
                let Ok(dc::Message::DcChargeParameterDiscoveryReq(req)) = dc::decode(payload, &arena) else { return closed };
                let code = if req.header.session_id != self.config.session_id {
                    ResponseCode::FailedUnknownSession
                } else {
                    self.config.cpd_code
                };
                let rn = |value| RationalNumber { exponent: 0, value };
                let res = dc::DcChargeParameterDiscoveryRes {
                    header: self.header(),
                    response_code: code,
                    dc_cpd_res_energy_transfer_mode: dc::DcCpdResEnergyTransferModeChoice::Base(
                        dc::DcCpdResEnergyTransferMode {
                            evse_maximum_charge_power: RationalNumber { exponent: 3, value: 150 },
                            evse_minimum_charge_power: rn(1000),
                            evse_maximum_charge_current: rn(300),
                            evse_minimum_charge_current: rn(2),
                            evse_maximum_voltage: rn(920),
                            evse_minimum_voltage: rn(150),
                            evse_power_ramp_limitation: None,
                        },
                    ),
                };
                if is_ok(code) {
                    self.state = State::AwaitScheduleExchangeReq;
                }
                Output {
                    send: frame(PayloadType::DC_MAINSTREAM, |out| res.encode(out)),
                    close: !is_ok(code),
                }
            }
            State::AwaitScheduleExchangeReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::ScheduleExchangeReq(req)) = common::decode(payload, &arena) else { return closed };
                self.schedule_reqs_seen += 1;
                let (processing, code) = if req.header.session_id != self.config.session_id {
                    (Processing::Finished, ResponseCode::FailedUnknownSession)
                } else if !matches!(req.se_req_control_mode, common::SeReqControlModeChoice::Dynamic(_)) {
                    (Processing::Finished, ResponseCode::FailedWrongChargeParameter)
                } else if self.schedule_reqs_seen <= self.config.schedule_ongoing_responses {
                    (Processing::Ongoing, ResponseCode::Ok)
                } else {
                    (Processing::Finished, self.config.schedule_code)
                };
                let res = common::ScheduleExchangeRes {
                    header: self.header(),
                    response_code: code,
                    evse_processing: processing,
                    go_to_pause: None,
                    se_res_control_mode: common::SeResControlModeChoice::Dynamic(common::DynamicSeResControlMode {
                        departure_time: None,
                        minimum_soc: None,
                        target_soc: None,
                        schedule: None,
                    }),
                };
                let ongoing = processing == Processing::Ongoing;
                if !ongoing && is_ok(code) {
                    self.state = State::AwaitCableCheckReq;
                }
                Output { send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)), close: !ongoing && !is_ok(code) }
            }
            State::AwaitCableCheckReq if payload_type == PayloadType::DC_MAINSTREAM => {
                let Ok(dc::Message::DcCableCheckReq(req)) = dc::decode(payload, &arena) else { return closed };
                self.cable_reqs_seen += 1;
                let (processing, code) = if req.header.session_id != self.config.session_id {
                    (Processing::Finished, ResponseCode::FailedUnknownSession)
                } else if self.cable_reqs_seen <= self.config.cable_check_ongoing_responses {
                    (Processing::Ongoing, ResponseCode::Ok)
                } else {
                    (Processing::Finished, self.config.cable_check_code)
                };
                let res = dc::DcCableCheckRes { header: self.header(), response_code: code, evse_processing: processing };
                let ongoing = processing == Processing::Ongoing;
                if !ongoing && is_ok(code) {
                    self.state = State::AwaitPreChargeReq;
                }
                Output { send: frame(PayloadType::DC_MAINSTREAM, |out| res.encode(out)), close: !ongoing && !is_ok(code) }
            }
            State::AwaitPreChargeReq if payload_type == PayloadType::DC_MAINSTREAM => {
                let Ok(dc::Message::DcPreChargeReq(req)) = dc::decode(payload, &arena) else { return closed };
                let code = if req.header.session_id != self.config.session_id {
                    ResponseCode::FailedUnknownSession
                } else {
                    ResponseCode::Ok
                };
                self.precharge_reqs_seen += 1;
                // Until "settled" the output is 100 V away from the EV's target.
                let settled = self.precharge_reqs_seen >= self.config.precharge_requests_to_settle;
                let target = req.ev_target_voltage;
                let voltage = if settled {
                    target
                } else {
                    RationalNumber { exponent: target.exponent, value: target.value.saturating_sub(100) }
                };
                let res = dc::DcPreChargeRes { header: self.header(), response_code: code, evse_present_voltage: voltage };
                if is_ok(code) && req.ev_processing == Processing::Finished {
                    self.state = State::AwaitPowerDeliveryReq;
                }
                Output { send: frame(PayloadType::DC_MAINSTREAM, |out| res.encode(out)), close: !is_ok(code) }
            }
            State::AwaitPowerDeliveryReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::PowerDeliveryReq(req)) = common::decode(payload, &arena) else { return closed };
                let code = if req.header.session_id != self.config.session_id {
                    ResponseCode::FailedUnknownSession
                } else if req.charge_progress != common::ChargeProgress::Start || req.ev_power_profile.is_none() {
                    // V2G20-1546: the profile must accompany Start.
                    ResponseCode::FailedPowerDeliveryNotApplied
                } else {
                    self.config.power_delivery_code
                };
                let res = common::PowerDeliveryRes { header: self.header(), response_code: code, evse_status: None };
                if is_ok(code) {
                    self.state = State::ChargeLoop;
                }
                Output { send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)), close: !is_ok(code) }
            }
            State::ChargeLoop if payload_type == PayloadType::DC_MAINSTREAM => {
                let Ok(dc::Message::DcChargeLoopReq(req)) = dc::decode(payload, &arena) else { return closed };
                self.loops_seen += 1;
                let code = if req.header.session_id != self.config.session_id {
                    ResponseCode::FailedUnknownSession
                } else if !matches!(req.cl_req_control_mode, dc::ClReqControlModeChoice::DynamicDc(_)) {
                    ResponseCode::FailedWrongChargeParameter
                } else {
                    ResponseCode::Ok
                };
                let terminate = self.config.terminate_after_loops.is_some_and(|n| self.loops_seen >= n);
                let rn = |value| RationalNumber { exponent: 0, value };
                let res = dc::DcChargeLoopRes {
                    header: self.header(),
                    response_code: code,
                    evse_status: terminate.then_some(common_types::EvseStatus {
                        notification_max_delay: 5,
                        evse_notification: common_types::EvseNotification::Terminate,
                    }),
                    meter_info: None,
                    receipt: None,
                    evse_present_current: rn(100),
                    evse_present_voltage: req.ev_present_voltage,
                    evse_power_limit_achieved: false,
                    evse_current_limit_achieved: false,
                    evse_voltage_limit_achieved: false,
                    cl_res_control_mode: dc::ClResControlModeChoice::DynamicDc(dc::DynamicDcClResControlMode {
                        departure_time: None,
                        minimum_soc: None,
                        target_soc: None,
                        ack_max_delay: None,
                        evse_maximum_charge_power: RationalNumber { exponent: 3, value: 150 },
                        evse_minimum_charge_power: rn(100),
                        evse_maximum_charge_current: rn(300),
                        evse_maximum_voltage: rn(920),
                    }),
                };
                Output { send: frame(PayloadType::DC_MAINSTREAM, |out| res.encode(out)), close: !is_ok(code) }
            }
            State::ChargeLoop if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::PowerDeliveryReq(req)) = common::decode(payload, &arena) else { return closed };
                let code = if req.header.session_id != self.config.session_id {
                    ResponseCode::FailedUnknownSession
                } else if req.charge_progress != common::ChargeProgress::Stop {
                    ResponseCode::FailedPowerDeliveryNotApplied
                } else {
                    ResponseCode::Ok
                };
                let res = common::PowerDeliveryRes { header: self.header(), response_code: code, evse_status: None };
                if is_ok(code) {
                    self.state = State::AwaitWeldingReq;
                }
                Output { send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)), close: !is_ok(code) }
            }
            State::AwaitWeldingReq if payload_type == PayloadType::DC_MAINSTREAM => {
                let Ok(dc::Message::DcWeldingDetectionReq(req)) = dc::decode(payload, &arena) else { return closed };
                let code = if req.header.session_id != self.config.session_id {
                    ResponseCode::FailedUnknownSession
                } else {
                    self.config.welding_code
                };
                let res = dc::DcWeldingDetectionRes {
                    header: self.header(),
                    response_code: code,
                    evse_present_voltage: RationalNumber { exponent: 0, value: 5 },
                };
                if is_ok(code) && req.ev_processing == Processing::Finished {
                    self.state = State::AwaitSessionStopReq;
                }
                Output { send: frame(PayloadType::DC_MAINSTREAM, |out| res.encode(out)), close: !is_ok(code) }
            }
            State::AwaitSessionStopReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::SessionStopReq(req)) = common::decode(payload, &arena) else { return closed };
                let code = if req.header.session_id != self.config.session_id {
                    ResponseCode::FailedUnknownSession
                } else {
                    ResponseCode::Ok
                };
                let res = common::SessionStopRes { header: self.header(), response_code: code };
                if is_ok(code) {
                    self.state = State::Stopped;
                }
                Output { send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)), close: true }
            }
            // The rest of the flow is added with the later slices.
            _ => closed,
        }
    }
}

impl Evse {
    /// `ServiceDiscoveryRes`; a failure code closes the connection.
    fn discovery_res(&self, code: ResponseCode) -> Output {
        let list: Vec<Service> =
            self.config.energy_services.iter().map(|&service_id| Service { service_id, free_service: false }).collect();
        let res = ServiceDiscoveryRes {
            header: self.header(),
            response_code: code,
            service_renegotiation_supported: false,
            energy_transfer_service_list: ServiceList { service: &list },
            vas_list: None,
        };
        Output {
            send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)),
            close: !is_ok(code),
        }
    }

    fn detail_res(&self, code: ResponseCode, service_id: u16) -> Output {
        let names: Vec<Vec<Parameter<'_>>> = self
            .config
            .parameter_sets
            .iter()
            .map(|(_, params)| {
                params.iter().map(|(name, v)| Parameter { name, choice: ParameterChoice::IntValue(*v) }).collect()
            })
            .collect();
        let sets: Vec<ParameterSet<'_>> = self
            .config
            .parameter_sets
            .iter()
            .zip(&names)
            .map(|((id, _), parameters)| ParameterSet { parameter_set_id: *id, parameter: parameters })
            .collect();
        let res = ServiceDetailRes {
            header: self.header(),
            response_code: code,
            service_id,
            service_parameter_list: ServiceParameterList { parameter_set: &sets },
        };
        Output {
            send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)),
            close: !is_ok(code),
        }
    }

    fn selection_res(&self, code: ResponseCode) -> Output {
        let res = ServiceSelectionRes { header: self.header(), response_code: code };
        Output {
            send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)),
            close: !is_ok(code),
        }
    }

    fn header(&self) -> MessageHeader {
        MessageHeader { session_id: self.config.session_id, time_stamp: now() }
    }

    /// Answers a message that carries the wrong session id with `FAILED_UnknownSession`.
    fn unknown_session(&self, msg: common::Message<'_>) -> Output {
        let send = match msg {
            common::Message::AuthorizationSetupReq(_) => {
                let res = AuthorizationSetupRes {
                    header: self.header(),
                    response_code: ResponseCode::FailedUnknownSession,
                    authorization_services: &[Authorization::Eim],
                    certificate_installation_service: false,
                    as_res_authorization_mode: AsResAuthorizationModeChoice::Eim(EimAsResAuthorizationMode {}),
                };
                frame(PayloadType::MAINSTREAM, |out| res.encode(out))
            }
            _ => {
                let res = AuthorizationRes {
                    header: self.header(),
                    response_code: ResponseCode::FailedUnknownSession,
                    evse_processing: Processing::Finished,
                };
                frame(PayloadType::MAINSTREAM, |out| res.encode(out))
            }
        };
        Output { send, close: true }
    }
}

fn is_ok(code: ResponseCode) -> bool {
    matches!(code, ResponseCode::Ok | ResponseCode::OkNewSessionEstablished | ResponseCode::OkOldSessionJoined)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn frame(payload_type: PayloadType, encode: impl FnOnce(&mut [u8]) -> Result<usize, EncodeError>) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    let n = encode(&mut buf[HEADER_LEN..]).ok()?;
    write_header(&mut buf, payload_type, n).ok()?;
    buf.truncate(HEADER_LEN + n);
    Some(buf)
}

