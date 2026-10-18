//! The charger's session machine: ISO 15118-20 from `SupportedAppProtocolReq` to `SessionStopReq`, DC
//! and AC, Dynamic and Scheduled control mode, bidirectional services, the multiplexed schedule
//! renegotiation and metering confirmation streams.

use app_protocol::{self, ResponseCode as SapCode, SupportedAppProtocolRes};
use exi::{Arena, EncodeError};
use iso15118_20::ResponseMessage as M;
use iso15118_20::ac;
use iso15118_20::common::{
    self, AReqAuthorizationModeChoice, AsResAuthorizationModeChoice, Authorization, AuthorizationRes, AuthorizationSetupRes, ChargeProgress, ChargingSession,
    EimAsResAuthorizationMode, EvpptControlModeChoice, PnCAsResAuthorizationMode, SelectedService, ServiceDetailRes, ServiceDiscoveryRes, ServiceList,
    ServiceParameterList, ServiceSelectionRes, SessionSetupRes,
};
use iso15118_20::common_types::{MessageHeader, Processing, RationalNumber, ResponseCode};
use iso15118_20::dc;
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use crate::secc::{Action, Authorize, Ended, Progress, Request, Secc, Signed};
use crate::{Event, State, Step, Timer, TimerCmd};

/// Memory the host provides; nothing is allocated here.
pub struct Buffers<'a> {
    /// Receives V2GTP frames; must hold the largest expected frame (header + payload).
    pub rx: &'a mut [u8],
    /// Holds the frames to send in one step.
    pub tx: &'a mut [u8],
    /// Scratch for decoded messages, reset for every received frame.
    pub arena: &'a mut [u8],
    /// Scratch for the stream a signature covers (a Plug & Charge request: a few kilobytes).
    pub scratch: &'a mut [u8],
}

/// The timeout of `V2G_SECC_Sequence_Timeout` (Table 215).
pub const SEQUENCE_TIMEOUT_MS: u32 = 60_000;

/// Service ids of the AC services (Table 204).
const SERVICE_AC: u16 = 1;
const SERVICE_AC_BPT: u16 = 5;
const SERVICE_DC_BPT: u16 = 6;

fn is_ac_service(service_id: u16) -> bool {
    matches!(service_id, SERVICE_AC | SERVICE_AC_BPT)
}

fn is_bpt_service(service_id: u16) -> bool {
    matches!(service_id, SERVICE_AC_BPT | SERVICE_DC_BPT)
}

pub fn is_ok(code: ResponseCode) -> bool {
    matches!(
        code,
        ResponseCode::Ok
            | ResponseCode::OkCertificateExpiresSoon
            | ResponseCode::OkNewSessionEstablished
            | ResponseCode::OkOldSessionJoined
            | ResponseCode::OkPowerToleranceConfirmed
    )
}

fn is_failed(code: ResponseCode) -> bool {
    code.xml_name().starts_with("FAILED")
}

pub struct Session<S: Secc> {
    /// Bytes of an incomplete frame the host has put into `Buffers::rx`.
    rx_len: usize,
    m: Machine<S>,
}

struct Machine<S> {
    secc: S,
    state: State,
    secured: bool,
    /// The vehicle offered the AC schema.
    ac_namespace: bool,
    session_id: [u8; 8],
    challenge: [u8; 16],
    service: u16,
    scheduled: bool,
    authorization: Option<Authorization>,
    selected_tuple: u32,
    /// A standby period is on.
    standby: bool,
    /// The schedule exchange said `GoToPause`: the power delivery that follows is a stop, the power never on.
    no_power: bool,
    last_stamp: u64,
}

/// What one frame led to: bytes appended to the output and whether the connection ends.
#[derive(Default)]
struct Reply {
    len: usize,
    close: bool,
}

impl<S: Secc> Session<S> {
    pub fn new(secc: S) -> Self {
        Self {
            rx_len: 0,
            m: Machine {
                secc,
                state: State::AwaitSupportedAppProtocolReq,
                secured: false,
                ac_namespace: false,
                session_id: [0; 8],
                challenge: [0; 16],
                service: 0,
                scheduled: false,
                authorization: None,
                selected_tuple: 0,
                standby: false,
                no_power: false,
                last_stamp: 0,
            },
        }
    }

    pub fn state(&self) -> State {
        self.m.state
    }

    /// The charger the machine asks.
    pub fn secc(&self) -> &S {
        &self.m.secc
    }

    pub fn secc_mut(&mut self) -> &mut S {
        &mut self.m.secc
    }

    pub fn session_id(&self) -> [u8; 8] {
        self.m.session_id
    }

    /// Free space of `rx` to read the next bytes from the stream into (the buffer is the host's and is
    /// the one every call gets).
    pub fn rx_space<'b>(&self, rx: &'b mut [u8]) -> &'b mut [u8] {
        &mut rx[self.rx_len..]
    }

    /// Handles an event. The buffers are the host's, and the same ones for every call: what the machine
    /// has not consumed of `rx` stays there; the frames of the answer are written to `tx` (up to
    /// [`Step::send`]).
    pub fn handle(&mut self, event: Event, buffers: &mut Buffers<'_>) -> Step {
        let Self { rx_len, m } = self;
        let Buffers { rx, tx, arena, scratch } = buffers;
        let mut arena = Arena::new(arena);
        let ended = |s: State| matches!(s, State::Stopped | State::Paused | State::Closed);
        match event {
            Event::Connected { secured } => {
                m.secured = secured;
                Step { timer: Some(TimerCmd::Arm { timer: Timer::Sequence, ms: SEQUENCE_TIMEOUT_MS }), ..Step::default() }
            }
            Event::TimerExpired(Timer::Sequence) => {
                if ended(m.state) {
                    return Step::default();
                }
                m.secc.note(format_args!("the vehicle sent nothing for {SEQUENCE_TIMEOUT_MS} ms (V2G_SECC_Sequence_Timeout)"));
                m.secc.session_ended(Ended::Lost);
                m.state = State::Closed;
                Step { close: true, ..Step::default() }
            }
            Event::Received(n) => {
                *rx_len = (*rx_len + n).min(rx.len());
                let mut sent = 0;
                let mut close = false;
                loop {
                    match parse(&rx[..*rx_len], rx.len() - HEADER_LEN) {
                        Ok(Parse::Incomplete { .. }) => break,
                        Ok(Parse::Frame(frame)) => {
                            arena.reset();
                            let (payload_type, payload) = (frame.payload_type, frame.payload);
                            let reply = m.on_frame(payload_type, payload, &arena, scratch, &mut tx[sent..]);
                            sent += reply.len;
                            close |= reply.close;
                            let consumed = frame.consumed;
                            rx.copy_within(consumed..*rx_len, 0);
                            *rx_len -= consumed;
                            if close || ended(m.state) {
                                break;
                            }
                        }
                        Err(_) => {
                            m.state = State::Closed;
                            close = true;
                            break;
                        }
                    }
                }
                if *rx_len == rx.len() {
                    m.state = State::Closed;
                    close = true;
                }
                let timer = if close || ended(m.state) {
                    Some(TimerCmd::Cancel(Timer::Sequence))
                } else if sent > 0 {
                    Some(TimerCmd::Arm { timer: Timer::Sequence, ms: SEQUENCE_TIMEOUT_MS })
                } else {
                    None
                };
                Step { send: sent, timer, close: close || ended(m.state) }
            }
        }
    }
}

/// Encodes a payload behind a V2GTP header at the start of `out`; returns the framed length.
fn frame(out: &mut [u8], payload_type: PayloadType, encode: impl FnOnce(&mut [u8]) -> Result<usize, EncodeError>) -> Result<usize, ()> {
    let n = encode(out.get_mut(HEADER_LEN..).ok_or(())?).map_err(|_| ())?;
    write_header(out, payload_type, n).map_err(|_| ())?;
    Ok(HEADER_LEN + n)
}

/// A reply that ends the connection when `close` is set (or the frame did not fit).
fn reply(framed: Result<usize, ()>, close: bool) -> Reply {
    match framed {
        Ok(len) => Reply { len, close },
        Err(()) => Reply { len: 0, close: true },
    }
}

impl<S: Secc> Machine<S> {
    fn stamp(&mut self) -> u64 {
        let t = self.secc.now_micros().max(self.last_stamp + 1);
        self.last_stamp = t;
        t
    }

    fn header(&mut self) -> MessageHeader<'static> {
        MessageHeader::new(self.session_id, self.stamp())
    }

    /// The code the machine acts on: its own, as the host may change it.
    fn code(&mut self, message: M, code: ResponseCode) -> ResponseCode {
        let code = self.secc.response_code(message, code);
        if !message.allows(code) {
            self.secc.note(format_args!("violation: {} with {} is not defined (Tables 224, 226)", message.name(), code.xml_name()));
        }
        code
    }

    fn goto(&mut self, to: State) {
        let from = core::mem::replace(&mut self.state, to);
        if from != to {
            self.secc.note(format_args!("state {from:?} -> {to:?}"));
        }
    }

    fn unknown_session(&self, session: [u8; 8]) -> bool {
        session != self.session_id
    }

    fn on_frame(&mut self, payload_type: PayloadType, payload: &[u8], arena: &Arena<'_>, scratch: &mut [u8], out: &mut [u8]) -> Reply {
        self.secc.note(format_args!("rx {} bytes, payload type {:#06x}, in {:?}", payload.len(), payload_type.0, self.state));
        if self.state == State::AwaitSupportedAppProtocolReq {
            return if payload_type == PayloadType::SAP { self.on_sap(payload, arena, out) } else { Reply { len: 0, close: true } };
        }
        let decoded = match payload_type {
            PayloadType::MAINSTREAM | PayloadType::METERING_CONFIRMATION => common::decode(payload, arena).ok().map(Request::Common),
            PayloadType::DC_MAINSTREAM => dc::decode(payload, arena).ok().map(Request::Dc),
            PayloadType::AC_MAINSTREAM => ac::decode(payload, arena).ok().map(Request::Ac),
            PayloadType::SCHEDULE_RENEGOTIATION => ac::decode(payload, arena)
                .ok()
                .map(Request::Ac)
                .or_else(|| dc::decode(payload, arena).ok().map(Request::Dc))
                .or_else(|| common::decode(payload, arena).ok().map(Request::Common)),
            _ => None,
        };
        let Some(request) = decoded else {
            self.secc.note(format_args!("closing without an answer: not a decodable request in {:?}", self.state));
            self.goto(State::Closed);
            return Reply { len: 0, close: true };
        };
        match self.secc.intercept(payload_type, &request) {
            Action::Pass => {}
            Action::Silent => {
                self.secc.note(format_args!("silent: not answering"));
                return Reply::default();
            }
            Action::Close => {
                self.goto(State::Closed);
                return Reply { len: 0, close: true };
            }
        }
        if payload_type == PayloadType::SCHEDULE_RENEGOTIATION {
            self.on_side(request, out)
        } else if payload_type == PayloadType::METERING_CONFIRMATION {
            self.on_metering(request, scratch, out)
        } else {
            self.on_main(request, scratch, out)
        }
    }

    fn on_sap(&mut self, payload: &[u8], arena: &Arena<'_>, out: &mut [u8]) -> Reply {
        let Ok(app_protocol::Message::SupportedAppProtocolReq(req)) = app_protocol::decode(payload, arena) else { return Reply { len: 0, close: true } };
        // The highest priority (lowest number) among the protocols the charger speaks.
        let pick = req.app_protocol.iter().filter(|p| self.secc.supports_protocol(p.protocol_namespace, p.version_number_major)).min_by_key(|p| p.priority);
        match pick {
            Some(p) => {
                self.ac_namespace = p.protocol_namespace.ends_with(":AC");
                self.secc.note(format_args!("protocol {}, schema id {}", p.protocol_namespace, p.schema_id));
                self.goto(State::AwaitSessionSetupReq);
                let res = SupportedAppProtocolRes { response_code: SapCode::OkSuccessfulNegotiation, schema_id: Some(p.schema_id) };
                reply(frame(out, PayloadType::SAP, |o| res.encode(o)), false)
            }
            None => {
                let res = SupportedAppProtocolRes { response_code: SapCode::FailedNoNegotiation, schema_id: None };
                self.goto(State::Closed);
                reply(frame(out, PayloadType::SAP, |o| res.encode(o)), true)
            }
        }
    }
}

/// An integer parameter of a parameter set by name.
fn parameter(set: &common::ParameterSet<'_>, name: &str) -> Option<i32> {
    set.parameter.iter().find(|p| p.name == name).and_then(|p| match p.choice {
        common::ParameterChoice::ByteValue(v) => Some(i32::from(v)),
        common::ParameterChoice::ShortValue(v) => Some(i32::from(v)),
        common::ParameterChoice::IntValue(v) => Some(v),
        _ => None,
    })
}

const ZERO: RationalNumber = RationalNumber { exponent: 0, value: 0 };

impl<S: Secc> Machine<S> {
    /// `ServiceDetailRes` etc. need the charger's offer; a request out of turn gets
    /// `FAILED_SequenceError` of the type it asks for (V2G20-459), and the connection ends.
    fn out_of_turn(&mut self, request: &Request<'_>, out: &mut [u8]) -> Reply {
        self.secc.note(format_args!("a request out of sequence (state {:?}): FAILED_SequenceError (V2G20-459)", self.state));
        let framed = self.failing(request, ResponseCode::FailedSequenceError, out);
        self.goto(State::Closed);
        match framed {
            Some(len) => Reply { len, close: true },
            None => Reply { len: 0, close: true },
        }
    }

    /// The failing response of the type that answers `request`, or `None` for a message that is no
    /// request (a response).
    fn failing(&mut self, request: &Request<'_>, code: ResponseCode, out: &mut [u8]) -> Option<usize> {
        let header = self.header();
        let main = PayloadType::MAINSTREAM;
        let framed = match request {
            Request::Common(m) => match m {
                common::Message::SessionSetupReq(_) => frame(out, main, |o| SessionSetupRes { header, response_code: code, evseid: self.secc.evse_id() }.encode(o)),
                common::Message::AuthorizationSetupReq(_) => frame(out, main, |o| {
                    AuthorizationSetupRes {
                        header,
                        response_code: code,
                        authorization_services: &[Authorization::Eim],
                        certificate_installation_service: false,
                        as_res_authorization_mode: AsResAuthorizationModeChoice::Eim(EimAsResAuthorizationMode {}),
                    }
                    .encode(o)
                }),
                common::Message::AuthorizationReq(_) => frame(out, main, |o| AuthorizationRes { header, response_code: code, evse_processing: Processing::Finished }.encode(o)),
                common::Message::ServiceDiscoveryReq(_) => frame(out, main, |o| {
                    ServiceDiscoveryRes {
                        header,
                        response_code: code,
                        service_renegotiation_supported: false,
                        energy_transfer_service_list: ServiceList { service: &[common::Service { service_id: 2, free_service: false }] },
                        vas_list: None,
                    }
                    .encode(o)
                }),
                common::Message::ServiceDetailReq(r) => frame(out, main, |o| {
                    // At least one parameter set with one parameter (the lists have a minimum).
                    let parameters = [common::Parameter { name: "Connector", choice: common::ParameterChoice::IntValue(1) }];
                    let sets = [common::ParameterSet { parameter_set_id: 1, parameter: &parameters }];
                    ServiceDetailRes { header, response_code: code, service_id: r.service_id, service_parameter_list: ServiceParameterList { parameter_set: &sets } }.encode(o)
                }),
                common::Message::ServiceSelectionReq(_) => frame(out, main, |o| ServiceSelectionRes { header, response_code: code }.encode(o)),
                common::Message::ScheduleExchangeReq(_) => frame(out, main, |o| {
                    common::ScheduleExchangeRes {
                        header,
                        response_code: code,
                        evse_processing: Processing::Finished,
                        go_to_pause: None,
                        se_res_control_mode: common::SeResControlModeChoice::Dynamic(common::DynamicSeResControlMode { departure_time: None, minimum_soc: None, target_soc: None, schedule: None }),
                    }
                    .encode(o)
                }),
                common::Message::PowerDeliveryReq(_) => frame(out, main, |o| common::PowerDeliveryRes { header, response_code: code, evse_status: None }.encode(o)),
                common::Message::SessionStopReq(_) => frame(out, main, |o| common::SessionStopRes { header, response_code: code }.encode(o)),
                common::Message::MeteringConfirmationReq(_) => frame(out, PayloadType::METERING_CONFIRMATION, |o| common::MeteringConfirmationRes { header, response_code: code }.encode(o)),
                _ => return None,
            },
            Request::Dc(m) => {
                let dcm = PayloadType::DC_MAINSTREAM;
                match m {
                    dc::Message::DcChargeParameterDiscoveryReq(_) => frame(out, dcm, |o| {
                        dc::DcChargeParameterDiscoveryRes {
                            header,
                            response_code: code,
                            dc_cpd_res_energy_transfer_mode: dc::DcCpdResEnergyTransferModeChoice::Base(dc::DcCpdResEnergyTransferMode {
                                evse_maximum_charge_power: ZERO,
                                evse_minimum_charge_power: ZERO,
                                evse_maximum_charge_current: ZERO,
                                evse_minimum_charge_current: ZERO,
                                evse_maximum_voltage: ZERO,
                                evse_minimum_voltage: ZERO,
                                evse_power_ramp_limitation: None,
                            }),
                        }
                        .encode(o)
                    }),
                    dc::Message::DcCableCheckReq(_) => frame(out, dcm, |o| dc::DcCableCheckRes { header, response_code: code, evse_processing: Processing::Finished }.encode(o)),
                    dc::Message::DcPreChargeReq(_) => frame(out, dcm, |o| dc::DcPreChargeRes { header, response_code: code, evse_present_voltage: ZERO }.encode(o)),
                    dc::Message::DcChargeLoopReq(_) => frame(out, dcm, |o| {
                        dc::DcChargeLoopRes {
                            header,
                            response_code: code,
                            evse_status: None,
                            meter_info: None,
                            receipt: None,
                            evse_present_current: ZERO,
                            evse_present_voltage: ZERO,
                            evse_power_limit_achieved: false,
                            evse_current_limit_achieved: false,
                            evse_voltage_limit_achieved: false,
                            cl_res_control_mode: dc::ClResControlModeChoice::DynamicDc(dc::DynamicDcClResControlMode {
                                departure_time: None,
                                minimum_soc: None,
                                target_soc: None,
                                ack_max_delay: None,
                                evse_maximum_charge_power: ZERO,
                                evse_minimum_charge_power: ZERO,
                                evse_maximum_charge_current: ZERO,
                                evse_maximum_voltage: ZERO,
                            }),
                        }
                        .encode(o)
                    }),
                    dc::Message::DcWeldingDetectionReq(_) => frame(out, dcm, |o| dc::DcWeldingDetectionRes { header, response_code: code, evse_present_voltage: ZERO }.encode(o)),
                    _ => return None,
                }
            }
            Request::Ac(m) => match m {
                ac::Message::AcChargeParameterDiscoveryReq(_) => frame(out, PayloadType::AC_MAINSTREAM, |o| {
                    ac::AcChargeParameterDiscoveryRes {
                        header,
                        response_code: code,
                        ac_cpd_res_energy_transfer_mode: ac::AcCpdResEnergyTransferModeChoice::Base(ac::AcCpdResEnergyTransferMode {
                            evse_maximum_charge_power: ZERO,
                            evse_maximum_charge_power_l2: None,
                            evse_maximum_charge_power_l3: None,
                            evse_minimum_charge_power: ZERO,
                            evse_minimum_charge_power_l2: None,
                            evse_minimum_charge_power_l3: None,
                            evse_nominal_frequency: ZERO,
                            maximum_power_asymmetry: None,
                            evse_power_ramp_limitation: None,
                            evse_present_active_power: None,
                            evse_present_active_power_l2: None,
                            evse_present_active_power_l3: None,
                        }),
                    }
                    .encode(o)
                }),
                ac::Message::AcChargeLoopReq(_) => frame(out, PayloadType::AC_MAINSTREAM, |o| {
                    ac::AcChargeLoopRes {
                        header,
                        response_code: code,
                        evse_status: None,
                        meter_info: None,
                        receipt: None,
                        evse_target_frequency: None,
                        cl_res_control_mode: ac::ClResControlModeChoice::DynamicAc(ac::DynamicAcClResControlMode {
                            departure_time: None,
                            minimum_soc: None,
                            target_soc: None,
                            ack_max_delay: None,
                            evse_target_active_power: ZERO,
                            evse_target_active_power_l2: None,
                            evse_target_active_power_l3: None,
                            evse_target_reactive_power: None,
                            evse_target_reactive_power_l2: None,
                            evse_target_reactive_power_l3: None,
                            evse_present_active_power: None,
                            evse_present_active_power_l2: None,
                            evse_present_active_power_l3: None,
                        }),
                    }
                    .encode(o)
                }),
                _ => return None,
            },
        };
        framed.ok()
    }
}

impl<S: Secc> Machine<S> {
    fn ac(&self) -> bool {
        is_ac_service(self.service)
    }

    fn bpt(&self) -> bool {
        is_bpt_service(self.service)
    }

    /// A response to a message of the main stream.
    fn main_reply(&mut self, out: &mut [u8], payload_type: PayloadType, encode: impl FnOnce(&mut [u8]) -> Result<usize, EncodeError>, close: bool) -> Reply {
        let r = reply(frame(out, payload_type, encode), close);
        if close {
            self.goto(State::Closed);
        }
        r
    }

    fn on_main(&mut self, request: Request<'_>, scratch: &mut [u8], out: &mut [u8]) -> Reply {
        use common::Message as C;
        let main = PayloadType::MAINSTREAM;
        match (self.state, request) {
            (State::AwaitSessionSetupReq, Request::Common(C::SessionSetupReq(req))) => {
                let asked = req.header.session_id;
                let mut code = ResponseCode::OkNewSessionEstablished;
                let mut resumed = false;
                if asked != [0; 8] {
                    match self.secc.resume(asked) {
                        // V2G20-2623: a resumed session; authorization and the selection carry over.
                        Some(selected) => {
                            code = ResponseCode::OkOldSessionJoined;
                            resumed = true;
                            self.session_id = asked;
                            self.service = selected.service_id;
                            self.scheduled = self.secc.parameter_sets(selected.service_id).iter().find(|s| s.parameter_set_id == selected.parameter_set_id).and_then(|s| parameter(s, "ControlMode")) == Some(1);
                        }
                        // V2G20-2547: an id we do not know is a new session, with an id of its own.
                        None => self.session_id = self.secc.new_session_id(),
                    }
                } else {
                    self.session_id = self.secc.new_session_id();
                }
                let code = self.code(M::SessionSetup, code);
                let header = self.header();
                let ok = is_ok(code);
                if ok {
                    // V2G20-1032: after a resumed session the next request is the charge parameter discovery.
                    self.goto(if resumed { State::AwaitChargeParameterDiscoveryReq } else { State::AwaitAuthorizationSetupReq });
                }
                let res = SessionSetupRes { header, response_code: code, evseid: self.secc.evse_id() };
                let r = reply(frame(out, main, |o| res.encode(o)), !ok);
                if !ok {
                    self.goto(State::Closed);
                }
                r
            }
            (State::AwaitAuthorizationSetupReq, Request::Common(C::AuthorizationSetupReq(req))) => {
                if self.unknown_session(req.header.session_id) {
                    return self.out_of_turn_code(&Request::Common(C::AuthorizationSetupReq(req)), ResponseCode::FailedUnknownSession, out);
                }
                let services_len = self.secc.authorization_services().len();
                if services_len == 0 || services_len > 2 {
                    self.goto(State::Closed);
                    return Reply { len: 0, close: true };
                }
                self.challenge = self.secc.gen_challenge();
                let code = self.code(M::AuthorizationSetup, ResponseCode::Ok);
                let header = self.header();
                self.goto(State::AwaitAuthorizationReq);
                let services = self.secc.authorization_services();
                let res = AuthorizationSetupRes {
                    header,
                    response_code: code,
                    authorization_services: services,
                    certificate_installation_service: self.secc.certificate_installation_service(),
                    // V2G20-1219, 1220: the PnC element when PnC is offered, EIM's otherwise.
                    as_res_authorization_mode: if services.contains(&Authorization::PnC) {
                        AsResAuthorizationModeChoice::PnC(PnCAsResAuthorizationMode { gen_challenge: self.challenge, supported_providers: None })
                    } else {
                        AsResAuthorizationModeChoice::Eim(EimAsResAuthorizationMode {})
                    },
                };
                reply(frame(out, main, |o| res.encode(o)), false)
            }
            (State::AwaitAuthorizationReq, Request::Common(C::AuthorizationReq(req))) => {
                if self.unknown_session(req.header.session_id) {
                    return self.out_of_turn_code(&Request::Common(C::AuthorizationReq(req)), ResponseCode::FailedUnknownSession, out);
                }
                let offers = |a: Authorization, secc: &S| secc.authorization_services().contains(&a);
                let progress = match (req.selected_authorization_service, &req.a_req_authorization_mode) {
                    (Authorization::Eim, AReqAuthorizationModeChoice::Eim(_)) if offers(Authorization::Eim, &self.secc) => self.secc.authorize(Authorize::Eim),
                    (Authorization::PnC, AReqAuthorizationModeChoice::PnC(mode)) if offers(Authorization::PnC, &self.secc) => {
                        match iso15118_20::signature::encode_fragment(scratch, iso15118_20::signature::fragment::PNC_A_REQ_AUTHORIZATION_MODE, |e| mode.encode_content(e)) {
                            Ok(len) => {
                                let challenge = self.challenge;
                                self.secc.authorize(Authorize::PnC { header: &req.header, mode, fragment: &scratch[..len], challenge })
                            }
                            Err(_) => Progress::Done(ResponseCode::FailedSignatureError),
                        }
                    }
                    _ => Progress::Done(ResponseCode::WarningAuthorizationSelectionInvalid),
                };
                let (processing, code) = match progress {
                    Progress::Ongoing => (Processing::Finished, ResponseCode::Ok),
                    Progress::Done(code) => (Processing::Finished, code),
                };
                let (processing, code) = if progress == Progress::Ongoing { (Processing::Ongoing, ResponseCode::Ok) } else { (processing, code) };
                let code = self.code(M::Authorization, code);
                let header = self.header();
                let finished_ok = processing == Processing::Finished && is_ok(code);
                if finished_ok {
                    self.authorization = Some(req.selected_authorization_service);
                    self.goto(State::AwaitServiceDiscoveryReq);
                }
                let res = AuthorizationRes { header, response_code: code, evse_processing: processing };
                // A warning leaves the vehicle another try (V2G20-1583); a failure ends the session.
                self.main_reply(out, main, |o| res.encode(o), processing == Processing::Finished && is_failed(code))
            }
            (State::AwaitServiceDiscoveryReq, Request::Common(C::ServiceDiscoveryReq(req))) => {
                let (code, header) = (self.code(M::ServiceDiscovery, if self.unknown_session(req.header.session_id) { ResponseCode::FailedUnknownSession } else { ResponseCode::Ok }), self.header());
                if is_ok(code) {
                    self.goto(State::AwaitServiceDetailReq);
                }
                let res = ServiceDiscoveryRes {
                    header,
                    response_code: code,
                    service_renegotiation_supported: self.secc.service_renegotiation_supported(),
                    energy_transfer_service_list: ServiceList { service: self.secc.energy_services() },
                    vas_list: None,
                };
                let framed = frame(out, main, |o| res.encode(o));
                let close = !is_ok(code);
                if close {
                    self.goto(State::Closed);
                }
                reply(framed, close)
            }
            (State::AwaitServiceDetailReq | State::AwaitServiceSelectionReq, Request::Common(C::ServiceDetailReq(req))) => {
                let offered = self.secc.energy_services().iter().any(|s| s.service_id == req.service_id) && !self.secc.parameter_sets(req.service_id).is_empty();
                let code = if self.unknown_session(req.header.session_id) {
                    ResponseCode::FailedUnknownSession
                } else if !offered {
                    ResponseCode::FailedServiceIdInvalid
                } else {
                    ResponseCode::Ok
                };
                let code = self.code(M::ServiceDetail, code);
                let header = self.header();
                if is_ok(code) {
                    self.goto(State::AwaitServiceSelectionReq);
                }
                let res = ServiceDetailRes {
                    header,
                    response_code: code,
                    service_id: req.service_id,
                    service_parameter_list: ServiceParameterList { parameter_set: self.secc.parameter_sets(req.service_id) },
                };
                let framed = frame(out, main, |o| res.encode(o));
                let close = !is_ok(code);
                if close {
                    self.goto(State::Closed);
                }
                reply(framed, close)
            }
            (State::AwaitServiceSelectionReq, Request::Common(C::ServiceSelectionReq(req))) => {
                let SelectedService { service_id, parameter_set_id } = req.selected_energy_transfer_service;
                let set_scheduled = self.secc.parameter_sets(service_id).iter().find(|s| s.parameter_set_id == parameter_set_id).map(|s| parameter(s, "ControlMode") == Some(1));
                let valid = self.secc.energy_services().iter().any(|s| s.service_id == service_id)
                    && set_scheduled.is_some()
                    // The schema the vehicle offered is that of the service it selects.
                    && is_ac_service(service_id) == self.ac_namespace;
                let code = if self.unknown_session(req.header.session_id) {
                    ResponseCode::FailedUnknownSession
                } else if !valid {
                    self.secc.note(format_args!("selected service {service_id} with parameter set {parameter_set_id}: not offered"));
                    ResponseCode::FailedServiceSelectionInvalid
                } else {
                    ResponseCode::Ok
                };
                let code = self.code(M::ServiceSelection, code);
                let header = self.header();
                if valid && is_ok(code) {
                    self.service = service_id;
                    self.scheduled = set_scheduled == Some(true);
                    self.secc.service_selected(&req);
                    self.secc.note(format_args!("service {service_id} selected, parameter set {parameter_set_id}"));
                    self.goto(State::AwaitChargeParameterDiscoveryReq);
                }
                let res = ServiceSelectionRes { header, response_code: code };
                self.main_reply(out, main, |o| res.encode(o), !is_ok(code))
            }
            (State::AwaitChargeParameterDiscoveryReq, Request::Dc(dc::Message::DcChargeParameterDiscoveryReq(req))) if !self.ac() => self.dc_charge_parameters(&req, PayloadType::DC_MAINSTREAM, false, out),
            (State::AwaitChargeParameterDiscoveryReq, Request::Ac(ac::Message::AcChargeParameterDiscoveryReq(req))) if self.ac() => self.ac_charge_parameters(&req, PayloadType::AC_MAINSTREAM, false, out),
            (State::AwaitScheduleExchangeReq, Request::Common(C::ScheduleExchangeReq(req))) => self.schedule_exchange(&req, main, false, out),
            (State::AwaitCableCheckReq, Request::Dc(dc::Message::DcCableCheckReq(req))) => {
                let (processing, code) = if self.unknown_session(req.header.session_id) {
                    (Processing::Finished, ResponseCode::FailedUnknownSession)
                } else {
                    match self.secc.cable_check() {
                        Progress::Ongoing => (Processing::Ongoing, ResponseCode::Ok),
                        Progress::Done(code) => (Processing::Finished, code),
                    }
                };
                let code = self.code(M::DcCableCheck, code);
                let header = self.header();
                let ongoing = processing == Processing::Ongoing;
                if !ongoing && is_ok(code) {
                    self.goto(State::AwaitPreChargeReq);
                }
                let res = dc::DcCableCheckRes { header, response_code: code, evse_processing: processing };
                self.main_reply(out, PayloadType::DC_MAINSTREAM, |o| res.encode(o), !ongoing && !is_ok(code))
            }
            (State::AwaitPreChargeReq, Request::Dc(dc::Message::DcPreChargeReq(req))) => {
                let code = if self.unknown_session(req.header.session_id) { ResponseCode::FailedUnknownSession } else { ResponseCode::Ok };
                let code = self.code(M::DcPreCharge, code);
                let voltage = self.secc.pre_charge(req.ev_target_voltage, req.ev_present_voltage);
                let header = self.header();
                if is_ok(code) && req.ev_processing == Processing::Finished {
                    self.goto(State::AwaitPowerDeliveryReq);
                }
                let res = dc::DcPreChargeRes { header, response_code: code, evse_present_voltage: voltage };
                self.main_reply(out, PayloadType::DC_MAINSTREAM, |o| res.encode(o), !is_ok(code))
            }
            // After GoToPause the vehicle stops before it starts (V2G20-2123): nothing was on, the session stops or pauses.
            (State::AwaitPowerDeliveryReq, Request::Common(C::PowerDeliveryReq(req))) if self.no_power && req.charge_progress == ChargeProgress::Stop => {
                let code = if self.unknown_session(req.header.session_id) { ResponseCode::FailedUnknownSession } else { ResponseCode::Ok };
                let code = self.code(M::PowerDelivery, code);
                let header = self.header();
                if is_ok(code) {
                    self.goto(State::AwaitSessionStopReq);
                }
                let res = common::PowerDeliveryRes { header, response_code: code, evse_status: None };
                self.main_reply(out, main, |o| res.encode(o), !is_ok(code))
            }
            (State::AwaitPowerDeliveryReq, Request::Common(C::PowerDeliveryReq(req))) => {
                let code = if self.unknown_session(req.header.session_id) {
                    ResponseCode::FailedUnknownSession
                } else if req.charge_progress != ChargeProgress::Start || req.ev_power_profile.is_none() {
                    // V2G20-1546: the profile must accompany Start.
                    ResponseCode::FailedPowerDeliveryNotApplied
                } else if !self.selects_offered_tuple(&req) {
                    // V2G20-479: a schedule tuple that was not offered.
                    ResponseCode::FailedScheduleSelectionInvalid
                } else {
                    self.secc.power_delivery(&req)
                };
                let code = self.code(M::PowerDelivery, code);
                let header = self.header();
                if is_ok(code) {
                    self.goto(State::ChargeLoop);
                }
                let res = common::PowerDeliveryRes { header, response_code: code, evse_status: None };
                self.main_reply(out, main, |o| res.encode(o), !is_ok(code))
            }
            (State::ChargeLoop, Request::Dc(dc::Message::DcChargeLoopReq(req))) if !self.ac() => {
                let bpt = self.bpt();
                let matching = matches!((&req.cl_req_control_mode, bpt, self.scheduled), (dc::ClReqControlModeChoice::DynamicDc(_), false, false) | (dc::ClReqControlModeChoice::BptDynamicDc(_), true, false));
                let (code, answer) = if self.unknown_session(req.header.session_id) {
                    (ResponseCode::FailedUnknownSession, None)
                } else if !matching {
                    // Table 226: FAILED_WrongChargeParameter is for the parameter discovery only.
                    (ResponseCode::Failed, None)
                } else {
                    match self.secc.dc_loop(&req, bpt) {
                        Some(a) => (a.response_code, Some(a)),
                        None => (ResponseCode::Failed, None),
                    }
                };
                let code = self.code(M::DcChargeLoop, code);
                let header = self.header();
                let a = answer.unwrap_or(DcLoopAnswer::empty());
                let wanted = req.meter_info_requested || matches!(a.status, Some(s) if s.evse_notification == iso15118_20::common_types::EvseNotification::MeteringConfirmation);
                let res = dc::DcChargeLoopRes {
                    header,
                    response_code: code,
                    evse_status: a.status,
                    meter_info: self.secc.meter_info(wanted),
                    receipt: self.secc.receipt(),
                    evse_present_current: a.present_current,
                    evse_present_voltage: a.present_voltage,
                    evse_power_limit_achieved: a.power_limit_achieved,
                    evse_current_limit_achieved: a.current_limit_achieved,
                    evse_voltage_limit_achieved: a.voltage_limit_achieved,
                    cl_res_control_mode: a.control_mode,
                };
                let framed = frame(out, PayloadType::DC_MAINSTREAM, |o| res.encode(o));
                let close = !is_ok(code);
                if close {
                    self.goto(State::Closed);
                }
                reply(framed, close)
            }
            (State::ChargeLoop, Request::Ac(ac::Message::AcChargeLoopReq(req))) if self.ac() => {
                let bpt = self.bpt();
                let matching = matches!(
                    (&req.cl_req_control_mode, bpt, self.scheduled),
                    (ac::ClReqControlModeChoice::DynamicAc(_), false, false)
                        | (ac::ClReqControlModeChoice::BptDynamicAc(_), true, false)
                        | (ac::ClReqControlModeChoice::ScheduledAc(_), false, true)
                        | (ac::ClReqControlModeChoice::BptScheduledAc(_), true, true)
                );
                let (code, answer) = if self.unknown_session(req.header.session_id) {
                    (ResponseCode::FailedUnknownSession, None)
                } else if !matching {
                    (ResponseCode::Failed, None)
                } else {
                    match self.secc.ac_loop(&req, bpt) {
                        Some(a) => (a.response_code, Some(a)),
                        None => (ResponseCode::Failed, None),
                    }
                };
                let code = self.code(M::AcChargeLoop, code);
                let header = self.header();
                let a = answer.unwrap_or(AcLoopAnswer::empty());
                let wanted = req.meter_info_requested || matches!(a.status, Some(s) if s.evse_notification == iso15118_20::common_types::EvseNotification::MeteringConfirmation);
                let res = ac::AcChargeLoopRes {
                    header,
                    response_code: code,
                    evse_status: a.status,
                    meter_info: self.secc.meter_info(wanted),
                    receipt: self.secc.receipt(),
                    evse_target_frequency: a.target_frequency,
                    cl_res_control_mode: a.control_mode,
                };
                let framed = frame(out, PayloadType::AC_MAINSTREAM, |o| res.encode(o));
                let close = !is_ok(code);
                if close {
                    self.goto(State::Closed);
                }
                reply(framed, close)
            }
            (State::ChargeLoop, Request::Common(C::PowerDeliveryReq(req))) => {
                let code = if self.unknown_session(req.header.session_id) {
                    ResponseCode::FailedUnknownSession
                } else {
                    match (req.charge_progress, self.standby) {
                        (ChargeProgress::Stop, _) | (ChargeProgress::Standby, false) | (ChargeProgress::Start, true) => self.secc.power_delivery(&req),
                        _ => ResponseCode::FailedPowerDeliveryNotApplied,
                    }
                };
                let code = self.code(M::PowerDelivery, code);
                let header = self.header();
                if is_ok(code) {
                    match req.charge_progress {
                        // V2G20-1385, 1387: a standby period; V2G20-1386: Start ends it.
                        ChargeProgress::Standby => self.standby = true,
                        ChargeProgress::Start => self.standby = false,
                        ChargeProgress::Stop => self.goto(if self.ac() { State::AwaitSessionStopReq } else { State::AwaitWeldingReq }),
                        _ => {}
                    }
                }
                let refused = code == ResponseCode::WarningStandbyNotAllowed;
                let res = common::PowerDeliveryRes { header, response_code: code, evse_status: None };
                self.main_reply(out, main, |o| res.encode(o), !is_ok(code) && !refused)
            }
            (State::AwaitWeldingReq, Request::Dc(dc::Message::DcWeldingDetectionReq(req))) => {
                let code = if self.unknown_session(req.header.session_id) { ResponseCode::FailedUnknownSession } else { ResponseCode::Ok };
                let code = self.code(M::DcWeldingDetection, code);
                let voltage = self.secc.welding_detection();
                let header = self.header();
                if is_ok(code) && req.ev_processing == Processing::Finished {
                    self.goto(State::AwaitSessionStopReq);
                }
                let res = dc::DcWeldingDetectionRes { header, response_code: code, evse_present_voltage: voltage };
                self.main_reply(out, PayloadType::DC_MAINSTREAM, |o| res.encode(o), !is_ok(code))
            }
            (State::AwaitSessionStopReq, Request::Common(C::SessionStopReq(req))) => {
                let renegotiation = req.charging_session == ChargingSession::ServiceRenegotiation;
                let mut code = if self.unknown_session(req.header.session_id) { ResponseCode::FailedUnknownSession } else { ResponseCode::Ok };
                // V2G20-1643, V2G20-1955.
                if renegotiation && is_ok(code) && !self.secc.accept_service_renegotiation() {
                    code = ResponseCode::FailedNoServiceRenegotiationSupported;
                }
                let code = self.code(M::SessionStop, code);
                let header = self.header();
                let res = common::SessionStopRes { header, response_code: code };
                if is_ok(code) {
                    self.secc.session_ended(Ended::Stopped(req.charging_session));
                    if renegotiation {
                        // The connection and the authorization stay; the service flow starts over.
                        self.standby = false;
                        self.goto(State::AwaitServiceDiscoveryReq);
                        return reply(frame(out, main, |o| res.encode(o)), false);
                    }
                    self.goto(if req.charging_session == ChargingSession::Pause { State::Paused } else { State::Stopped });
                    return reply(frame(out, main, |o| res.encode(o)), true);
                }
                self.main_reply(out, main, |o| res.encode(o), true)
            }
            (_, other) => self.out_of_turn(&other, out),
        }
    }

    /// A failing response with a code of the caller's choosing, ending the connection.
    fn out_of_turn_code(&mut self, request: &Request<'_>, code: ResponseCode, out: &mut [u8]) -> Reply {
        let framed = self.failing(request, code, out);
        self.goto(State::Closed);
        Reply { len: framed.unwrap_or(0), close: true }
    }

    /// Whether the power delivery request selects what the session was set up for: in Scheduled mode one of
    /// the offered tuples, in Dynamic mode no tuple. Notes the selection.
    fn selects_offered_tuple(&mut self, req: &common::PowerDeliveryReq<'_>) -> bool {
        let Some(profile) = &req.ev_power_profile else { return true };
        match (&profile.evppt_control_mode, self.scheduled) {
            (EvpptControlModeChoice::Scheduled(m), true) if self.secc.schedule_tuples().iter().any(|t| t.schedule_tuple_id == m.selected_schedule_tuple_id) => {
                self.selected_tuple = m.selected_schedule_tuple_id;
                true
            }
            (EvpptControlModeChoice::Dynamic(_), false) => true,
            _ => false,
        }
    }

    fn dc_charge_parameters(&mut self, req: &dc::DcChargeParameterDiscoveryReq<'_>, payload_type: PayloadType, side: bool, out: &mut [u8]) -> Reply {
        let bpt = self.bpt();
        let wrong_mode = matches!(req.dc_cpd_req_energy_transfer_mode, dc::DcCpdReqEnergyTransferModeChoice::Bpt(_)) != bpt;
        let answer = if self.unknown_session(req.header.session_id) {
            Err(ResponseCode::FailedUnknownSession)
        } else if wrong_mode {
            // Table 226: the mode has to be the one of the selected service.
            Err(ResponseCode::FailedWrongChargeParameter)
        } else {
            self.secc.dc_charge_parameters(bpt, req)
        };
        let (code, mode) = match answer {
            Ok(mode) => (ResponseCode::Ok, Some(mode)),
            Err(code) => (code, None),
        };
        let code = self.code(M::DcChargeParameterDiscovery, code);
        let header = self.header();
        if !side && is_ok(code) {
            self.goto(State::AwaitScheduleExchangeReq);
        }
        let mode = mode.unwrap_or(dc::DcCpdResEnergyTransferModeChoice::Base(dc::DcCpdResEnergyTransferMode {
            evse_maximum_charge_power: ZERO,
            evse_minimum_charge_power: ZERO,
            evse_maximum_charge_current: ZERO,
            evse_minimum_charge_current: ZERO,
            evse_maximum_voltage: ZERO,
            evse_minimum_voltage: ZERO,
            evse_power_ramp_limitation: None,
        }));
        let res = dc::DcChargeParameterDiscoveryRes { header, response_code: code, dc_cpd_res_energy_transfer_mode: mode };
        self.main_reply(out, payload_type, |o| res.encode(o), !is_ok(code))
    }

    fn ac_charge_parameters(&mut self, req: &ac::AcChargeParameterDiscoveryReq<'_>, payload_type: PayloadType, side: bool, out: &mut [u8]) -> Reply {
        let bpt = self.bpt();
        let wrong_mode = matches!(req.ac_cpd_req_energy_transfer_mode, ac::AcCpdReqEnergyTransferModeChoice::Bpt(_)) != bpt;
        let answer = if self.unknown_session(req.header.session_id) {
            Err(ResponseCode::FailedUnknownSession)
        } else if wrong_mode {
            Err(ResponseCode::FailedWrongChargeParameter)
        } else {
            self.secc.ac_charge_parameters(bpt, req)
        };
        let (code, mode) = match answer {
            Ok(mode) => (ResponseCode::Ok, Some(mode)),
            Err(code) => (code, None),
        };
        let code = self.code(M::AcChargeParameterDiscovery, code);
        let header = self.header();
        if !side && is_ok(code) {
            self.goto(State::AwaitScheduleExchangeReq);
        }
        let mode = mode.unwrap_or(ac::AcCpdResEnergyTransferModeChoice::Base(ac::AcCpdResEnergyTransferMode {
            evse_maximum_charge_power: ZERO,
            evse_maximum_charge_power_l2: None,
            evse_maximum_charge_power_l3: None,
            evse_minimum_charge_power: ZERO,
            evse_minimum_charge_power_l2: None,
            evse_minimum_charge_power_l3: None,
            evse_nominal_frequency: ZERO,
            maximum_power_asymmetry: None,
            evse_power_ramp_limitation: None,
            evse_present_active_power: None,
            evse_present_active_power_l2: None,
            evse_present_active_power_l3: None,
        }));
        let res = ac::AcChargeParameterDiscoveryRes { header, response_code: code, ac_cpd_res_energy_transfer_mode: mode };
        self.main_reply(out, payload_type, |o| res.encode(o), !is_ok(code))
    }

    fn schedule_exchange(&mut self, req: &common::ScheduleExchangeReq<'_>, payload_type: PayloadType, side: bool, out: &mut [u8]) -> Reply {
        let scheduled_request = matches!(req.se_req_control_mode, common::SeReqControlModeChoice::Scheduled(_));
        let (processing, code) = if self.unknown_session(req.header.session_id) {
            (Processing::Finished, ResponseCode::FailedUnknownSession)
        } else if !side && scheduled_request != self.scheduled {
            // The mode of the selected parameter set is the one to use.
            (Processing::Finished, ResponseCode::Failed)
        } else {
            match self.secc.schedule_ready(side) {
                Progress::Ongoing => (Processing::Ongoing, ResponseCode::Ok),
                Progress::Done(()) => (Processing::Finished, ResponseCode::Ok),
            }
        };
        let code = self.code(M::ScheduleExchange, code);
        let header = self.header();
        let ongoing = processing == Processing::Ongoing;
        // V2G20-2121, 2122: no power. GoToPause is set, and the vehicle may skip the cable check and the pre-charge.
        let go_to_pause = !side && !ongoing && is_ok(code) && self.secc.no_power();
        if !side && !ongoing && is_ok(code) {
            self.no_power = go_to_pause;
            // AC has no cable check or pre-charge: the power delivery follows.
            self.goto(if self.ac() || go_to_pause { State::AwaitPowerDeliveryReq } else { State::AwaitCableCheckReq });
        }
        let needs = if !ongoing && is_ok(code) { self.secc.schedule_requested(req) } else { Default::default() };
        let control = if self.scheduled {
            common::SeResControlModeChoice::Scheduled(common::ScheduledSeResControlMode { schedule_tuple: self.secc.schedule_tuples() })
        } else {
            common::SeResControlModeChoice::Dynamic(common::DynamicSeResControlMode { departure_time: needs.departure_time, minimum_soc: needs.minimum_soc, target_soc: needs.target_soc, schedule: None })
        };
        let res = common::ScheduleExchangeRes { header, response_code: code, evse_processing: processing, go_to_pause: go_to_pause.then_some(true), se_res_control_mode: control };
        let framed = frame(out, payload_type, |o| res.encode(o));
        let close = !ongoing && !is_ok(code);
        if close && !side {
            self.goto(State::Closed);
        }
        reply(framed, close)
    }

    /// The schedule renegotiation side stream (V2G20-1046 ff.), answered next to the charge loop.
    fn on_side(&mut self, request: Request<'_>, out: &mut [u8]) -> Reply {
        use common::Message as C;
        if self.state != State::ChargeLoop {
            return Reply::default();
        }
        let side = PayloadType::SCHEDULE_RENEGOTIATION;
        match request {
            Request::Dc(dc::Message::DcChargeParameterDiscoveryReq(req)) if !self.ac() => self.dc_charge_parameters(&req, side, true, out),
            Request::Ac(ac::Message::AcChargeParameterDiscoveryReq(req)) if self.ac() => self.ac_charge_parameters(&req, side, true, out),
            Request::Common(C::ScheduleExchangeReq(req)) => self.schedule_exchange(&req, side, true, out),
            Request::Common(C::PowerDeliveryReq(req)) => {
                let code = if self.unknown_session(req.header.session_id) {
                    ResponseCode::FailedUnknownSession
                } else if req.charge_progress != ChargeProgress::ScheduleRenegotiation || req.ev_power_profile.is_none() {
                    ResponseCode::FailedPowerDeliveryNotApplied
                } else if !self.selects_offered_tuple(&req) {
                    ResponseCode::FailedScheduleSelectionInvalid
                } else {
                    self.secc.schedule_renegotiated(&req)
                };
                let code = self.code(M::PowerDelivery, code);
                let header = self.header();
                let res = common::PowerDeliveryRes { header, response_code: code, evse_status: None };
                self.main_reply(out, side, |o| res.encode(o), !is_ok(code))
            }
            _ => Reply::default(),
        }
    }

    /// The metering confirmation side stream (V2G20-1084, 1085).
    fn on_metering(&mut self, request: Request<'_>, scratch: &mut [u8], out: &mut [u8]) -> Reply {
        let Request::Common(common::Message::MeteringConfirmationReq(req)) = request else { return Reply::default() };
        if self.state != State::ChargeLoop {
            return Reply::default();
        }
        let code = if self.unknown_session(req.header.session_id) {
            ResponseCode::FailedUnknownSession
        } else {
            // The stream the signature covers (V2G20-904); a host that does not verify ignores it.
            match iso15118_20::signature::encode_fragment(scratch, iso15118_20::signature::fragment::SIGNED_METERING_DATA, |e| req.signed_metering_data.encode_content(e)) {
                Ok(len) => self.secc.confirm_metering(&req, Signed { header: &req.header, fragment: &scratch[..len] }),
                Err(_) => ResponseCode::FailedMeteringSignatureNotValid,
            }
        };
        let code = self.code(M::MeteringConfirmation, code);
        let header = self.header();
        let res = common::MeteringConfirmationRes { header, response_code: code };
        // The answer on a refusal is the charger's own business to follow with a notification.
        self.main_reply(out, PayloadType::METERING_CONFIRMATION, |o| res.encode(o), code == ResponseCode::FailedUnknownSession)
    }
}

impl DcLoopAnswer {
    /// What the machine says when the host has no answer: nothing delivered.
    fn empty() -> Self {
        Self {
            response_code: ResponseCode::Failed,
            present_current: ZERO,
            present_voltage: ZERO,
            power_limit_achieved: false,
            current_limit_achieved: false,
            voltage_limit_achieved: false,
            control_mode: dc::ClResControlModeChoice::DynamicDc(dc::DynamicDcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_maximum_charge_power: ZERO,
                evse_minimum_charge_power: ZERO,
                evse_maximum_charge_current: ZERO,
                evse_maximum_voltage: ZERO,
            }),
            status: None,
        }
    }
}

impl AcLoopAnswer {
    fn empty() -> Self {
        Self {
            response_code: ResponseCode::Failed,
            target_frequency: None,
            control_mode: ac::ClResControlModeChoice::DynamicAc(ac::DynamicAcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_target_active_power: ZERO,
                evse_target_active_power_l2: None,
                evse_target_active_power_l3: None,
                evse_target_reactive_power: None,
                evse_target_reactive_power_l2: None,
                evse_target_reactive_power_l3: None,
                evse_present_active_power: None,
                evse_present_active_power_l2: None,
                evse_present_active_power_l3: None,
            }),
            status: None,
        }
    }
}

use crate::secc::{AcLoopAnswer, DcLoopAnswer};
