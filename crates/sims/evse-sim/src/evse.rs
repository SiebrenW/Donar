//! The simulated EVSE: a sans-IO state machine from received bytes to frames to send.

use std::time::{SystemTime, UNIX_EPOCH};

use app_protocol::{self, ResponseCode as SapCode, SupportedAppProtocolRes};
use exi::{Arena, EncodeError};
use iso15118_20::common::{
    self, AReqAuthorizationModeChoice, PnCAsResAuthorizationMode, AsResAuthorizationModeChoice, Authorization, AuthorizationRes,
    AuthorizationSetupRes, EimAsResAuthorizationMode, Parameter, ParameterChoice, ParameterSet, SelectedService,
    Service, ServiceDetailRes, ServiceDiscoveryRes, ServiceList, ServiceParameterList, ServiceSelectionRes,
    SessionSetupRes,
};
use iso15118_20::common_types::{self, MessageHeader, Processing, RationalNumber, ResponseCode};
use iso15118_20::dc;
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use std::sync::Arc;

use crate::config::Config;
use crate::live::EvseLive;

fn random_challenge() -> [u8; 16] {
    let mut challenge = [0u8; 16];
    openssl::rand::rand_bytes(&mut challenge).expect("random bytes");
    challenge
}

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
    /// The EV paused the session (`SessionStopReq` Pause); it can be resumed on a new connection.
    Paused,
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
    /// A standby period is on, and how many charge loop answers it has had.
    standby: Option<u32>,
    /// The EV answered the pause request (with standby or a refusal was sent).
    pause_answered: bool,
    /// The id of this session (the configured one, or a fresh one when a resume is refused).
    session_id: [u8; 8],
    /// A service renegotiation was requested (and done) in this session.
    renegotiation_requested: bool,
    renegotiated: bool,
    side_schedule_reqs_seen: u32,
    /// A schedule renegotiation finished on the side stream.
    side_renegotiated: bool,
    live: Option<Arc<EvseLive>>,
    log: Vec<String>,
    /// The challenge of this session's `AuthorizationSetupRes` (V2G20-697: 128 random bits).
    challenge: [u8; 16],
}

impl Evse {
    pub fn new(config: Config) -> Self {
        Self { session_id: config.session_id, config, state: State::AwaitSupportedAppProtocolReq, rx: Vec::new(), schema_id: 0, authorization_reqs_seen: 0, schedule_reqs_seen: 0, cable_reqs_seen: 0, precharge_reqs_seen: 0, loops_seen: 0, standby: None, pause_answered: false, renegotiation_requested: false, renegotiated: false, side_schedule_reqs_seen: 0, side_renegotiated: false, live: None, log: Vec::new(), challenge: random_challenge() }
    }

    /// Reports the charge loop measurements to a test platform and takes its edits.
    pub fn with_live(mut self, live: Arc<EvseLive>) -> Self {
        self.live = Some(live);
        self
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// What the charger did since the last call, one line each: what it received, what it
    /// offered and answered, why it refused or closed.
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    /// The limits now: a test platform's live ones, which it may change during the session.
    fn limits(&self) -> crate::config::Limits {
        self.live.as_ref().map_or(self.config.limits, |l| l.limits())
    }

    fn note(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
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
            if close && !matches!(self.state, State::Stopped | State::Paused) {
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

    /// The request the charger waits for in `state`.
    fn expects(state: State) -> &'static str {
        match state {
            State::AwaitSupportedAppProtocolReq => "SupportedAppProtocolReq",
            State::AwaitSessionSetupReq => "SessionSetupReq",
            State::AwaitAuthorizationSetupReq => "AuthorizationSetupReq",
            State::AwaitAuthorizationReq => "AuthorizationReq",
            State::AwaitServiceDiscoveryReq => "ServiceDiscoveryReq",
            State::AwaitServiceDetailReq => "ServiceDetailReq",
            State::AwaitServiceSelectionReq => "ServiceSelectionReq",
            State::AwaitChargeParameterDiscoveryReq => "DC_ChargeParameterDiscoveryReq",
            State::AwaitScheduleExchangeReq => "ScheduleExchangeReq",
            State::AwaitCableCheckReq => "DC_CableCheckReq",
            State::AwaitPreChargeReq => "DC_PreChargeReq",
            State::AwaitPowerDeliveryReq => "PowerDeliveryReq",
            State::ChargeLoop => "DC_ChargeLoopReq or PowerDeliveryReq",
            State::AwaitWeldingReq => "DC_WeldingDetectionReq",
            State::AwaitSessionStopReq => "SessionStopReq",
            State::Stopped | State::Paused | State::Closed => "nothing",
        }
    }

    fn on_frame(&mut self, payload_type: PayloadType, payload: &[u8]) -> Output {
        let before = self.state;
        self.note(format!("rx {} bytes, payload type {:#06x}, waiting for {}", payload.len(), payload_type.0, Self::expects(before)));
        let out = self.on_frame_inner(payload_type, payload);
        match (&out.send, out.close) {
            (None, true) => self.note(format!("closing without an answer: the frame is not a decodable {} (state {before:?})", Self::expects(before))),
            (None, false) => self.note("no answer"),
            (Some(bytes), close) => self.note(format!("tx {} bytes{}", bytes.len(), if close { ", then closing" } else { "" })),
        }
        if self.state != before {
            self.note(format!("state {before:?} -> {:?}", self.state));
        }
        out
    }

    fn on_frame_inner(&mut self, payload_type: PayloadType, payload: &[u8]) -> Output {
        let mut mem = vec![0u8; 8192];
        let arena = Arena::new(&mut mem);
        let closed = Output { send: None, close: true };
        if payload_type == PayloadType::SCHEDULE_RENEGOTIATION {
            return self.on_side(payload);
        }
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
                let Ok(common::Message::SessionSetupReq(req)) = common::decode(payload, &arena) else { return closed };
                if self.config.silent_on_setup {
                    return Output::default();
                }
                let asked = req.header.session_id;
                let mut code = self.config.setup_code;
                let mut resumed = false;
                if asked != [0; 8] && is_ok(code) {
                    if asked == self.config.session_id && !self.config.forget_paused_sessions {
                        // V2G20-2623: a resumed session; authorization carries over.
                        code = ResponseCode::OkOldSessionJoined;
                        resumed = true;
                    } else {
                        // V2G20-2547: an id we do not know is a new session, with an id of its own.
                        code = ResponseCode::OkNewSessionEstablished;
                        self.session_id = self.config.session_id.map(|b| !b);
                    }
                }
                let ok = matches!(
                    code,
                    ResponseCode::Ok | ResponseCode::OkNewSessionEstablished | ResponseCode::OkOldSessionJoined
                );
                let res = SessionSetupRes {
                    header: MessageHeader::new(self.session_id, now()),
                    response_code: code,
                    evseid: &self.config.evse_id,
                };
                if ok {
                    // V2G20-1032: after a resumed session the next request is the charge
                    // parameter discovery.
                    self.state = if resumed { State::AwaitChargeParameterDiscoveryReq } else { State::AwaitAuthorizationSetupReq };
                }
                Output {
                    send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)),
                    close: !ok,
                }
            }
            State::AwaitAuthorizationSetupReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::AuthorizationSetupReq(req)) = common::decode(payload, &arena) else { return closed };
                if req.header.session_id != self.session_id {
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
                    // V2G20-1219, 1220: the PnC element when PnC is offered, EIM's otherwise.
                    as_res_authorization_mode: if self.config.offered_authorization.contains(&Authorization::PnC) {
                        AsResAuthorizationModeChoice::PnC(PnCAsResAuthorizationMode { gen_challenge: self.challenge, supported_providers: None })
                    } else {
                        AsResAuthorizationModeChoice::Eim(EimAsResAuthorizationMode {})
                    },
                };
                Output { send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)), close: false }
            }
            State::AwaitAuthorizationReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::AuthorizationReq(req)) = common::decode(payload, &arena) else { return closed };
                if req.header.session_id != self.session_id {
                    return self.unknown_session(common::Message::AuthorizationReq(req));
                }
                self.authorization_reqs_seen += 1;
                let ongoing = self.config.authorization_never_finishes
                    || self.authorization_reqs_seen <= self.config.authorization_ongoing_responses;
                // What the selection and, for Plug & Charge, the contract come to (V2G20-2564, 2565).
                let verdict = match (req.selected_authorization_service, &req.a_req_authorization_mode) {
                    (Authorization::Eim, AReqAuthorizationModeChoice::Eim(_)) if self.config.offered_authorization.contains(&Authorization::Eim) => ResponseCode::Ok,
                    (Authorization::PnC, AReqAuthorizationModeChoice::PnC(mode)) if self.config.offered_authorization.contains(&Authorization::PnC) => {
                        let verdict = crate::pnc::verify(&self.config.pnc, self.challenge, &req.header, mode);
                        self.note(format!("PnC authorization: {verdict:?}, challenge {}, contract chain of {} certificate(s)", if mode.gen_challenge == self.challenge { "matches" } else { "does not match" }, 1 + mode.contract_certificate_chain.sub_certificates.certificate.len()));
                        verdict
                    }
                    _ => ResponseCode::WarningAuthorizationSelectionInvalid,
                };
                let (evse_processing, response_code) = if ongoing {
                    (Processing::Ongoing, ResponseCode::Ok)
                } else if is_ok(verdict) {
                    // The knob can still refuse an authorization that was fine.
                    (Processing::Finished, if self.config.authorization_code == ResponseCode::Ok { verdict } else { self.config.authorization_code })
                } else {
                    (Processing::Finished, verdict)
                };
                let res = AuthorizationRes { header: self.header(), response_code, evse_processing };
                let finished_ok = !ongoing && is_ok(response_code);
                if finished_ok {
                    self.state = State::AwaitServiceDiscoveryReq;
                }
                // A warning leaves the vehicle another try (V2G20-1583); a failure ends the session.
                Output {
                    send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)),
                    close: !ongoing && response_code.xml_name().starts_with("FAILED"),
                }
            }
            State::AwaitServiceDiscoveryReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::ServiceDiscoveryReq(req)) = common::decode(payload, &arena) else { return closed };
                if req.header.session_id != self.session_id {
                    return self.discovery_res(ResponseCode::FailedUnknownSession);
                }
                self.state = State::AwaitServiceDetailReq;
                self.discovery_res(ResponseCode::Ok)
            }
            State::AwaitServiceDetailReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::ServiceDetailReq(req)) = common::decode(payload, &arena) else { return closed };
                if req.header.session_id != self.session_id {
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
                if req.header.session_id != self.session_id {
                    return self.selection_res(ResponseCode::FailedUnknownSession);
                }
                let SelectedService { service_id, parameter_set_id } = req.selected_energy_transfer_service;
                let valid = self.config.energy_services.contains(&service_id)
                    && self.config.parameter_sets.iter().any(|set| set.id == parameter_set_id);
                if !valid {
                    let sets: Vec<u16> = self.config.parameter_sets.iter().map(|s| s.id).collect();
                    self.note(format!("selected service {service_id} with parameter set {parameter_set_id}; offered services {:?}, parameter set ids {sets:?}", self.config.energy_services));
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
                let code = if req.header.session_id != self.session_id {
                    ResponseCode::FailedUnknownSession
                } else {
                    self.config.cpd_code
                };
                let res = self.cpd_res(code);
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
                let (processing, code) = if req.header.session_id != self.session_id {
                    (Processing::Finished, ResponseCode::FailedUnknownSession)
                } else if !matches!(req.se_req_control_mode, common::SeReqControlModeChoice::Dynamic(_)) {
                    (Processing::Finished, ResponseCode::FailedWrongChargeParameter)
                } else if self.schedule_reqs_seen <= self.config.schedule_ongoing_responses {
                    (Processing::Ongoing, ResponseCode::Ok)
                } else {
                    (Processing::Finished, self.config.schedule_code)
                };
                let res = self.schedule_res(processing, code);
                let ongoing = processing == Processing::Ongoing;
                if !ongoing && is_ok(code) {
                    self.state = State::AwaitCableCheckReq;
                }
                Output { send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)), close: !ongoing && !is_ok(code) }
            }
            State::AwaitCableCheckReq if payload_type == PayloadType::DC_MAINSTREAM => {
                let Ok(dc::Message::DcCableCheckReq(req)) = dc::decode(payload, &arena) else { return closed };
                self.cable_reqs_seen += 1;
                let (processing, code) = if req.header.session_id != self.session_id {
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
                let code = if req.header.session_id != self.session_id {
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
                let code = if req.header.session_id != self.session_id {
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
                let code = if req.header.session_id != self.session_id {
                    ResponseCode::FailedUnknownSession
                } else if !matches!(req.cl_req_control_mode, dc::ClReqControlModeChoice::DynamicDc(_)) {
                    ResponseCode::FailedWrongChargeParameter
                } else {
                    ResponseCode::Ok
                };
                let terminate = self.config.terminate_after_loops.is_some_and(|n| self.loops_seen >= n);
                let notification = if terminate {
                    Some((common_types::EvseNotification::Terminate, 5))
                } else if !self.side_renegotiated
                    && self.config.schedule_renegotiation_after_loops.is_some_and(|n| self.loops_seen >= n)
                {
                    Some((common_types::EvseNotification::ScheduleRenegotiation, 10))
                } else if !self.renegotiated
                    && self.config.service_renegotiation_after_loops.is_some_and(|n| self.loops_seen >= n)
                {
                    self.renegotiation_requested = true;
                    Some((common_types::EvseNotification::ServiceRenegotiation, 10))
                } else if let Some(rounds) = &mut self.standby {
                    *rounds += 1;
                    (*rounds > self.config.standby_loops).then_some((common_types::EvseNotification::ExitStandby, 60))
                } else if !self.pause_answered && self.config.pause_after_loops.is_some_and(|n| self.loops_seen >= n) {
                    Some((common_types::EvseNotification::Pause, 60))
                } else {
                    None
                };
                let rn = |value: u32| RationalNumber::from_f64(f64::from(value));
                let l = &self.limits();
                let (ev_voltage, ev_max_current, ev_max_power) = match &req.cl_req_control_mode {
                    dc::ClReqControlModeChoice::DynamicDc(m) => (req.ev_present_voltage.to_f64(), m.ev_maximum_charge_current.to_f64(), m.ev_maximum_charge_power.to_f64()),
                    _ => (req.ev_present_voltage.to_f64(), 0.0, 0.0),
                };
                let (voltage, current, max_power) = match &self.live {
                    Some(live) => live.charge_loop(ev_voltage, ev_max_current, ev_max_power),
                    None => (ev_voltage, crate::live::modelled_current(ev_voltage, ev_max_current, ev_max_power, l), f64::from(l.max_charge_power_w)),
                };
                let res = dc::DcChargeLoopRes {
                    header: self.header(),
                    response_code: code,
                    evse_status: notification.map(|(evse_notification, notification_max_delay)| common_types::EvseStatus {
                        notification_max_delay,
                        evse_notification,
                    }),
                    meter_info: None,
                    receipt: None,
                    evse_present_current: RationalNumber::from_f64(current),
                    evse_present_voltage: RationalNumber::from_f64(voltage),
                    evse_power_limit_achieved: false,
                    evse_current_limit_achieved: false,
                    evse_voltage_limit_achieved: false,
                    cl_res_control_mode: dc::ClResControlModeChoice::DynamicDc(dc::DynamicDcClResControlMode {
                        departure_time: None,
                        minimum_soc: None,
                        target_soc: None,
                        ack_max_delay: None,
                        evse_maximum_charge_power: RationalNumber::from_f64(max_power),
                        evse_minimum_charge_power: rn(l.min_charge_power_w),
                        evse_maximum_charge_current: rn(l.max_charge_current_a),
                        evse_maximum_voltage: rn(l.max_voltage_v),
                    }),
                };
                Output { send: frame(PayloadType::DC_MAINSTREAM, |out| res.encode(out)), close: !is_ok(code) }
            }
            State::ChargeLoop if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::PowerDeliveryReq(req)) = common::decode(payload, &arena) else { return closed };
                let code = if req.header.session_id != self.session_id {
                    ResponseCode::FailedUnknownSession
                } else {
                    match (req.charge_progress, self.standby.is_some()) {
                        (common::ChargeProgress::Stop, _) => ResponseCode::Ok,
                        (common::ChargeProgress::Standby, false) => {
                            // V2G20-1385, V2G20-1387
                            self.pause_answered = true;
                            if self.config.standby_allowed {
                                self.standby = Some(0);
                                ResponseCode::Ok
                            } else {
                                ResponseCode::WarningStandbyNotAllowed
                            }
                        }
                        // V2G20-1386: Start ends the standby period.
                        (common::ChargeProgress::Start, true) => {
                            self.standby = None;
                            ResponseCode::Ok
                        }
                        _ => ResponseCode::FailedPowerDeliveryNotApplied,
                    }
                };
                let res = common::PowerDeliveryRes { header: self.header(), response_code: code, evse_status: None };
                if is_ok(code) && req.charge_progress == common::ChargeProgress::Stop {
                    self.state = State::AwaitWeldingReq;
                }
                let refused = code == ResponseCode::WarningStandbyNotAllowed;
                Output { send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)), close: !is_ok(code) && !refused }
            }
            State::AwaitWeldingReq if payload_type == PayloadType::DC_MAINSTREAM => {
                let Ok(dc::Message::DcWeldingDetectionReq(req)) = dc::decode(payload, &arena) else { return closed };
                let code = if req.header.session_id != self.session_id {
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
                let code = if req.header.session_id != self.session_id {
                    ResponseCode::FailedUnknownSession
                } else {
                    ResponseCode::Ok
                };
                let renegotiation = req.charging_session == common::ChargingSession::ServiceRenegotiation;
                let pause = req.charging_session == common::ChargingSession::Pause;
                // V2G20-1643, V2G20-1955.
                let code = if renegotiation && (!self.config.service_renegotiation_supported || self.config.refuse_service_renegotiation) && is_ok(code) {
                    ResponseCode::FailedNoServiceRenegotiationSupported
                } else {
                    code
                };
                let res = common::SessionStopRes { header: self.header(), response_code: code };
                if is_ok(code) && renegotiation {
                    // The connection and the authorization stay; the service flow starts over.
                    self.state = State::AwaitServiceDiscoveryReq;
                    self.renegotiated = true;
                    self.renegotiation_requested = false;
                    (self.cable_reqs_seen, self.precharge_reqs_seen, self.loops_seen) = (0, 0, 0);
                    self.standby = None;
                    self.pause_answered = false;
                    return Output { send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)), close: false };
                }
                if is_ok(code) {
                    self.state = if pause { State::Paused } else { State::Stopped };
                }
                Output { send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)), close: true }
            }
            // The rest of the flow is added with the later slices.
            _ => closed,
        }
    }
}

impl Evse {
    fn cpd_res(&self, code: ResponseCode) -> dc::DcChargeParameterDiscoveryRes<'static> {
        let rn = |value: u32| RationalNumber::from_f64(f64::from(value));
        let l = &self.limits();
        dc::DcChargeParameterDiscoveryRes {
            header: self.header(),
            response_code: code,
            dc_cpd_res_energy_transfer_mode: dc::DcCpdResEnergyTransferModeChoice::Base(dc::DcCpdResEnergyTransferMode {
                evse_maximum_charge_power: rn(l.max_charge_power_w),
                evse_minimum_charge_power: rn(l.min_charge_power_w),
                evse_maximum_charge_current: rn(l.max_charge_current_a),
                evse_minimum_charge_current: rn(l.min_charge_current_a),
                evse_maximum_voltage: rn(l.max_voltage_v),
                evse_minimum_voltage: rn(l.min_voltage_v),
                evse_power_ramp_limitation: l.power_ramp_limitation_w.map(rn),
            }),
        }
    }

    fn schedule_res(&self, processing: Processing, code: ResponseCode) -> common::ScheduleExchangeRes<'static> {
        common::ScheduleExchangeRes {
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
        }
    }

    /// The schedule renegotiation side stream (V2G20-1046 ff.), answered next to the charge loop.
    fn on_side(&mut self, payload: &[u8]) -> Output {
        let mut mem = vec![0u8; 8192];
        let arena = Arena::new(&mut mem);
        let nothing = Output { send: None, close: false };
        if self.state != State::ChargeLoop || self.config.side_stream_silent {
            return nothing;
        }
        let side = PayloadType::SCHEDULE_RENEGOTIATION;
        if let Ok(dc::Message::DcChargeParameterDiscoveryReq(req)) = dc::decode(payload, &arena) {
            let code = if req.header.session_id != self.session_id { ResponseCode::FailedUnknownSession } else { self.config.cpd_code };
            self.side_schedule_reqs_seen = 0;
            let res = self.cpd_res(code);
            return Output { send: frame(side, |out| res.encode(out)), close: !is_ok(code) };
        }
        match common::decode(payload, &arena) {
            Ok(common::Message::ScheduleExchangeReq(req)) => {
                self.side_schedule_reqs_seen += 1;
                let (processing, code) = if req.header.session_id != self.session_id {
                    (Processing::Finished, ResponseCode::FailedUnknownSession)
                } else if self.side_schedule_reqs_seen <= self.config.schedule_ongoing_responses {
                    (Processing::Ongoing, ResponseCode::Ok)
                } else {
                    (Processing::Finished, self.config.schedule_code)
                };
                let res = self.schedule_res(processing, code);
                Output { send: frame(side, |out| res.encode(out)), close: processing != Processing::Ongoing && !is_ok(code) }
            }
            Ok(common::Message::PowerDeliveryReq(req)) => {
                let code = if req.header.session_id != self.session_id {
                    ResponseCode::FailedUnknownSession
                } else if req.charge_progress != common::ChargeProgress::ScheduleRenegotiation || req.ev_power_profile.is_none() {
                    ResponseCode::FailedPowerDeliveryNotApplied
                } else {
                    self.side_renegotiated = true;
                    ResponseCode::Ok
                };
                let res = common::PowerDeliveryRes { header: self.header(), response_code: code, evse_status: None };
                Output { send: frame(side, |out| res.encode(out)), close: !is_ok(code) }
            }
            _ => nothing,
        }
    }

    /// `ServiceDiscoveryRes`; a failure code closes the connection.
    fn discovery_res(&mut self, code: ResponseCode) -> Output {
        self.note(format!("ServiceDiscoveryReq: answering {code:?}, energy services {:?}", self.config.energy_services));
        let list: Vec<Service> =
            self.config.energy_services.iter().map(|&service_id| Service { service_id, free_service: false }).collect();
        let res = ServiceDiscoveryRes {
            header: self.header(),
            response_code: code,
            service_renegotiation_supported: self.config.service_renegotiation_supported,
            energy_transfer_service_list: ServiceList { service: &list },
            vas_list: None,
        };
        Output {
            send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)),
            close: !is_ok(code),
        }
    }

    fn detail_res(&mut self, code: ResponseCode, service_id: u16) -> Output {
        let offered: Vec<String> = self.config.parameter_sets.iter().map(|s| s.describe()).collect();
        self.note(format!("ServiceDetailReq for service {service_id}: answering {code:?}, offering {}", if offered.is_empty() { "no parameter sets".to_string() } else { offered.join("; ") }));
        let names: Vec<Vec<Parameter<'_>>> = self
            .config
            .parameter_sets
            .iter()
            .map(|set| {
                set.values.iter().map(|(name, v)| Parameter { name, choice: ParameterChoice::IntValue(*v) }).collect()
            })
            .collect();
        let sets: Vec<ParameterSet<'_>> = self
            .config
            .parameter_sets
            .iter()
            .zip(&names)
            .map(|(set, parameters)| ParameterSet { parameter_set_id: set.id, parameter: parameters })
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

    fn selection_res(&mut self, code: ResponseCode) -> Output {
        self.note(format!("ServiceSelectionReq: answering {code:?}"));
        let res = ServiceSelectionRes { header: self.header(), response_code: code };
        Output {
            send: frame(PayloadType::MAINSTREAM, |out| res.encode(out)),
            close: !is_ok(code),
        }
    }

    fn header(&self) -> MessageHeader<'static> {
        MessageHeader::new(self.session_id, now())
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
                    // V2G20-1219, 1220: the PnC element when PnC is offered, EIM's otherwise.
                    as_res_authorization_mode: if self.config.offered_authorization.contains(&Authorization::PnC) {
                        AsResAuthorizationModeChoice::PnC(PnCAsResAuthorizationMode { gen_challenge: self.challenge, supported_providers: None })
                    } else {
                        AsResAuthorizationModeChoice::Eim(EimAsResAuthorizationMode {})
                    },
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

