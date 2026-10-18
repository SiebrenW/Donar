//! The charger's ISO 15118-2 machine (DC and AC, external payment or contract): the SECC sequence with the
//! response codes of the standard, behind the [`Iso2Secc`] trait.
//!
//! Like [`crate::din`] it starts after the `SupportedAppProtocol` negotiation, which the host does: the first
//! frame the machine sees is the `SessionSetupReq`.

use exi::Arena;
use iso15118_2::body::*;
use iso15118_2::body::BodyElementChoice as B;
use iso15118_2::def::V2gMessage;
use iso15118_2::header::MessageHeader;
use iso15118_2::types::{self as t, PhysicalValue, UnitSymbol};
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use crate::secc::{Action, Ended, Progress};
use crate::session::SEQUENCE_TIMEOUT_MS;
use crate::{Event, Step, Timer, TimerCmd};

pub use iso15118_2::types::ResponseCode;

/// Memory the host provides.
pub struct Buffers<'a> {
    pub rx: &'a mut [u8],
    pub tx: &'a mut [u8],
    /// Scratch for decoded messages, reset for every received frame (a few tens of kilobytes with a certificate chain).
    pub arena: &'a mut [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    AwaitSessionSetupReq,
    AwaitServiceDiscoveryReq,
    /// Also the state in which `ServiceDetailReq` is answered.
    AwaitServicePaymentSelectionReq,
    /// Plug & Charge.
    AwaitPaymentDetailsReq,
    AwaitAuthorizationReq,
    AwaitChargeParameterDiscoveryReq,
    AwaitCableCheckReq,
    AwaitPreChargeReq,
    /// AC: the power delivery follows the charge parameters; also after a renegotiation.
    AwaitPowerDeliveryReq,
    ChargeLoop,
    AwaitWeldingReq,
    AwaitSessionStopReq,
    Stopped,
    /// The vehicle paused the session; it can join again on a new connection.
    Paused,
    Closed,
}

impl State {
    /// The request the charger waits for in this state.
    pub fn expects(self) -> &'static str {
        match self {
            State::AwaitSessionSetupReq => "SessionSetupReq",
            State::AwaitServiceDiscoveryReq => "ServiceDiscoveryReq",
            State::AwaitServicePaymentSelectionReq => "PaymentServiceSelectionReq",
            State::AwaitPaymentDetailsReq => "PaymentDetailsReq",
            State::AwaitAuthorizationReq => "AuthorizationReq",
            State::AwaitChargeParameterDiscoveryReq => "ChargeParameterDiscoveryReq",
            State::AwaitCableCheckReq => "CableCheckReq",
            State::AwaitPreChargeReq => "PreChargeReq or PowerDeliveryReq",
            State::AwaitPowerDeliveryReq => "PowerDeliveryReq",
            State::ChargeLoop => "CurrentDemandReq, ChargingStatusReq, MeteringReceiptReq or PowerDeliveryReq",
            State::AwaitWeldingReq => "WeldingDetectionReq or SessionStopReq",
            State::AwaitSessionStopReq => "SessionStopReq",
            State::Stopped | State::Paused | State::Closed => "nothing",
        }
    }
}

pub fn is_ok(c: ResponseCode) -> bool {
    matches!(c, ResponseCode::Ok | ResponseCode::OkNewSessionEstablished | ResponseCode::OkOldSessionJoined | ResponseCode::OkCertificateExpiresSoon)
}

/// The answer to a `SessionSetupReq`: the code and the id of the session (a new one, or the one of a paused session that joins again).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSetup {
    pub code: ResponseCode,
    pub session_id: [u8; 8],
}

/// The charge parameters the charger offers and the most power its schedule allows (24 hours).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Offer {
    pub parameter: EvseChargeParameterChoice,
    pub p_max: PhysicalValue,
}

/// The answer to a `CurrentDemandReq`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DcLoopAnswer {
    pub response_code: ResponseCode,
    /// Status code `EVSE_Shutdown` instead of `EVSE_Ready`.
    pub shutdown: bool,
    /// `ReNegotiation` or `StopCharging` asks the vehicle for it (5 s to do it).
    pub notification: t::EvseNotification,
    pub present_voltage: PhysicalValue,
    pub present_current: PhysicalValue,
    pub current_limit_achieved: bool,
    pub voltage_limit_achieved: bool,
    pub power_limit_achieved: bool,
    pub maximum_voltage: Option<PhysicalValue>,
    pub maximum_current: Option<PhysicalValue>,
    pub maximum_power: Option<PhysicalValue>,
    /// The meter reading, with `receipt_required` when the vehicle is to confirm it (Plug & Charge).
    pub meter_info: Option<t::MeterInfo<'static>>,
    pub receipt_required: Option<bool>,
}

/// The answer to a `ChargingStatusReq`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcLoopAnswer {
    pub response_code: ResponseCode,
    pub notification: t::EvseNotification,
    pub max_current: Option<PhysicalValue>,
    pub meter_info: Option<t::MeterInfo<'static>>,
    pub receipt_required: Option<bool>,
}

/// The charger, as the ISO 15118-2 machine sees it. Every method has a default.
pub trait Iso2Secc {
    fn evse_id(&self) -> &str {
        "ZZ00000"
    }
    fn now_secs(&self) -> i64 {
        0
    }
    /// The `SessionSetupReq`; `asked` is the session id in its header (a paused session asks to join again).
    fn session_setup(&mut self, _asked: &[u8], _evccid: &[u8]) -> SessionSetup {
        SessionSetup { code: ResponseCode::OkNewSessionEstablished, session_id: [1, 2, 3, 4, 5, 6, 7, 8] }
    }
    fn payment_options(&self) -> &[t::PaymentOption] {
        &[t::PaymentOption::ExternalPayment]
    }
    fn energy_modes(&self) -> &[t::EnergyTransferMode] {
        &[t::EnergyTransferMode::DcExtended]
    }
    /// The value-added services offered (id 1 is the charging).
    fn value_added_services(&self) -> &[t::Service<'_>] {
        &[]
    }
    /// The parameter sets of a value-added service; `None` for one that is not offered.
    fn service_parameters(&self, _service_id: u16) -> Option<&[t::ParameterSet<'_>]> {
        None
    }
    /// A valid payment and service selection (`contract` when the vehicle chose the contract): the code to answer with.
    fn payment_selected(&mut self, _request: &PaymentServiceSelectionReq<'_>, _contract: bool) -> ResponseCode {
        ResponseCode::Ok
    }
    /// The contract chain of a `PaymentDetailsReq`: `Err` carries the code that refuses it.
    fn payment_details(&mut self, _request: &PaymentDetailsReq<'_>) -> Result<(), ResponseCode> {
        Ok(())
    }
    /// The 128 random bits of the `PaymentDetailsRes` (the vehicle signs them in the `AuthorizationReq`).
    fn gen_challenge(&mut self) -> [u8; 16] {
        [0; 16]
    }
    /// Authorization (`Ongoing` while the user has not identified); with a contract the host verifies the signature in
    /// `header` and the challenge in `request`.
    fn authorize(&mut self, _header: &MessageHeader<'_>, _request: &AuthorizationReq<'_>) -> Progress<ResponseCode> {
        Progress::Done(ResponseCode::Ok)
    }
    /// The charge parameters for the request (`renegotiating` when the vehicle asked for new ones during the charge), or `Ongoing`.
    fn charge_parameters(&mut self, request: &ChargeParameterDiscoveryReq, renegotiating: bool) -> Progress<(ResponseCode, Offer)>;
    /// The schedule tuples to offer (at most the vehicle asked for) with their sales tariffs; empty: one tuple with the
    /// offer's maximum power for 24 hours.
    fn schedule_tuples(&self) -> &[t::SaScheduleTuple<'_>] {
        &[]
    }
    fn cable_check(&mut self) -> Progress<ResponseCode> {
        Progress::Done(ResponseCode::Ok)
    }
    fn pre_charge(&mut self, target: PhysicalValue) -> PhysicalValue {
        target
    }
    /// The power starts or stops; the schedule selection and the profile were checked. The code to answer with.
    fn power_delivery(&mut self, _request: &PowerDeliveryReq<'_>) -> ResponseCode {
        ResponseCode::Ok
    }
    /// The vehicle asks for new charge parameters and a new schedule (`ChargeProgress` Renegotiate).
    fn renegotiation(&mut self) {}
    /// A DC charge loop request; `announced_power` the `EVMaximumPowerLimit` of the discovery.
    fn dc_loop(&mut self, request: &CurrentDemandReq, announced_power: Option<PhysicalValue>) -> DcLoopAnswer;
    fn ac_loop(&mut self, request: &ChargingStatusReq) -> AcLoopAnswer;
    /// A `MeteringReceiptReq`: the host verifies the signature in `header` against the contract certificate and the reading it asked to confirm.
    fn metering_receipt(&mut self, _header: &MessageHeader<'_>, _request: &MeteringReceiptReq<'_>) -> ResponseCode {
        ResponseCode::FailedMeteringSignatureNotValid
    }
    fn welding_detection(&mut self) -> (ResponseCode, PhysicalValue) {
        (ResponseCode::Ok, PhysicalValue::from_f64(0.0, UnitSymbol::V))
    }
    fn session_ended(&mut self, _how: Ended) {}
    fn intercept(&mut self, _request: &B<'_>) -> Action {
        Action::Pass
    }
    fn note(&mut self, _text: core::fmt::Arguments<'_>) {}
}

pub struct Session<S: Iso2Secc> {
    rx_len: usize,
    m: Machine<S>,
}

struct Machine<S> {
    secc: S,
    state: State,
    session_id: [u8; 8],
    /// The mode of the session is AC.
    ac: bool,
    /// A renegotiation is under way: the charge parameters are answered, then power again.
    renegotiating: bool,
    announced_power: Option<PhysicalValue>,
    /// The most power the schedule offered, in watts.
    p_max_w: f64,
}

#[derive(Default)]
struct Reply {
    len: usize,
    close: bool,
}

fn frame(out: &mut [u8], encode: impl FnOnce(&mut [u8]) -> Result<usize, exi::EncodeError>) -> Result<usize, ()> {
    let n = encode(out.get_mut(HEADER_LEN..).ok_or(())?).map_err(|_| ())?;
    write_header(out, PayloadType::SAP, n).map_err(|_| ())?;
    Ok(HEADER_LEN + n)
}

fn pv(x: f64, unit: UnitSymbol) -> PhysicalValue {
    PhysicalValue::from_f64(x, unit)
}

fn is_ac(m: t::EnergyTransferMode) -> bool {
    matches!(m, t::EnergyTransferMode::AcSinglePhaseCore | t::EnergyTransferMode::AcThreePhaseCore)
}

/// A copy of the charger's id on the stack, so that an answer can carry it while the machine writes the log.
struct Id {
    bytes: [u8; 40],
    len: usize,
}

impl Id {
    fn of(text: &str) -> Id {
        let mut bytes = [0u8; 40];
        let len = text.len().min(40);
        bytes[..len].copy_from_slice(&text.as_bytes()[..len]);
        Id { bytes, len }
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl<S: Iso2Secc> Session<S> {
    pub fn new(secc: S) -> Self {
        Self { rx_len: 0, m: Machine { secc, state: State::AwaitSessionSetupReq, session_id: [0; 8], ac: false, renegotiating: false, announced_power: None, p_max_w: 0.0 } }
    }

    pub fn state(&self) -> State {
        self.m.state
    }

    pub fn secc(&self) -> &S {
        &self.m.secc
    }

    pub fn secc_mut(&mut self) -> &mut S {
        &mut self.m.secc
    }

    /// Free space of `rx` to read the next bytes from the stream into.
    pub fn rx_space<'b>(&self, rx: &'b mut [u8]) -> &'b mut [u8] {
        &mut rx[self.rx_len..]
    }

    pub fn handle(&mut self, event: Event, buffers: &mut Buffers<'_>) -> Step {
        let Self { rx_len, m } = self;
        let Buffers { rx, tx, arena } = buffers;
        let mut arena = Arena::new(arena);
        let ended = |s: State| matches!(s, State::Stopped | State::Paused | State::Closed);
        match event {
            Event::Connected { .. } => Step { timer: Some(TimerCmd::Arm { timer: Timer::Sequence, ms: SEQUENCE_TIMEOUT_MS }), ..Step::default() },
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
                let (mut sent, mut close) = (0, false);
                loop {
                    match parse(&rx[..*rx_len], rx.len() - HEADER_LEN) {
                        Ok(Parse::Incomplete { .. }) => break,
                        Ok(Parse::Frame(f)) => {
                            arena.reset();
                            let before = m.state;
                            m.secc.note(format_args!("rx {} bytes, payload type {:#06x}, waiting for {}", f.payload.len(), f.payload_type.0, before.expects()));
                            let reply = m.on_frame(f.payload_type, f.payload, &arena, &mut tx[sent..]);
                            sent += reply.len;
                            close |= reply.close;
                            if m.state != before {
                                m.secc.note(format_args!("state {before:?} -> {:?}", m.state));
                            }
                            let consumed = f.consumed;
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

impl<S: Iso2Secc> Machine<S> {
    fn dc_status(&self, code: t::DcEvseStatusCode, isolation: Option<t::IsolationLevel>) -> t::DcEvseStatus {
        t::DcEvseStatus { notification_max_delay: 0, evse_notification: t::EvseNotification::None, evse_isolation_status: isolation, evse_status_code: code }
    }

    fn ac_status(&self, notification: t::EvseNotification) -> t::AcEvseStatus {
        t::AcEvseStatus { notification_max_delay: if notification == t::EvseNotification::None { 0 } else { 5 }, evse_notification: notification, rcd: false }
    }

    fn status(&self) -> EvseStatusChoice {
        if self.ac {
            EvseStatusChoice::Ac(self.ac_status(t::EvseNotification::None))
        } else {
            EvseStatusChoice::Dc(self.dc_status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid)))
        }
    }

    /// Frames a response with the session id in the header, and notes it.
    fn send(&mut self, name: &str, code: ResponseCode, body: B<'_>, close: bool, out: &mut [u8]) -> Reply {
        self.secc.note(format_args!("tx {name} {}{}", code.xml_name(), if close { ", then closing" } else { "" }));
        let message = V2gMessage { header: MessageHeader { session_id: &self.session_id, notification: None, signature: None }, body: Body { body_element: Some(body) } };
        match frame(out, |o| message.encode(o)) {
            Ok(len) => Reply { len, close },
            Err(()) => Reply { len: 0, close: true },
        }
    }

    /// A failing response of the type the request asks for (wrong session or out of sequence), and the end of the session.
    fn failing(&mut self, request: &B<'_>, c: ResponseCode, out: &mut [u8]) -> Reply {
        let dc = self.dc_status(t::DcEvseStatusCode::EvseReady, None);
        let ac = self.ac_status(t::EvseNotification::None);
        let zero = pv(0.0, UnitSymbol::V);
        let tuple = [t::SaScheduleTuple {
            sa_schedule_tuple_id: 1,
            p_max_schedule: t::PMaxSchedule { p_max_schedule_entry: &[t::PMaxScheduleEntry { time_interval: t::TimeIntervalChoice::Relative(t::RelativeTimeInterval { start: 0, duration: None }), p_max: pv(0.0, UnitSymbol::W) }] },
            sales_tariff: None,
        }];
        let modes = [t::EnergyTransferMode::DcExtended];
        let options = [t::PaymentOption::ExternalPayment];
        let body = match request {
            B::SessionSetupReq(_) => B::SessionSetupRes(SessionSetupRes { response_code: c, evseid: "ZZ00000", evse_time_stamp: None }),
            B::ServiceDiscoveryReq(_) => B::ServiceDiscoveryRes(ServiceDiscoveryRes {
                response_code: c,
                payment_option_list: t::PaymentOptionList { payment_option: &options },
                charge_service: t::ChargeService { service_id: 1, service_name: None, service_category: t::ServiceCategory::EvCharging, service_scope: None, free_service: false, supported_energy_transfer_mode: t::SupportedEnergyTransferMode { energy_transfer_mode: &modes } },
                service_list: None,
            }),
            B::PaymentServiceSelectionReq(_) => B::PaymentServiceSelectionRes(PaymentServiceSelectionRes { response_code: c }),
            B::AuthorizationReq(_) => B::AuthorizationRes(AuthorizationRes { response_code: c, evse_processing: t::EvseProcessing::Finished }),
            B::ChargeParameterDiscoveryReq(_) => B::ChargeParameterDiscoveryRes(ChargeParameterDiscoveryRes {
                response_code: c,
                evse_processing: t::EvseProcessing::Finished,
                sa_schedules: Some(SaSchedulesChoice::SaScheduleList(t::SaScheduleList { sa_schedule_tuple: &tuple })),
                evse_charge_parameter: EvseChargeParameterChoice::Ac(t::AcEvseChargeParameter { ac_evse_status: ac, evse_nominal_voltage: zero, evse_max_current: pv(0.0, UnitSymbol::A) }),
            }),
            B::CableCheckReq(_) => B::CableCheckRes(CableCheckRes { response_code: c, dc_evse_status: dc, evse_processing: t::EvseProcessing::Finished }),
            B::PreChargeReq(_) => B::PreChargeRes(PreChargeRes { response_code: c, dc_evse_status: dc, evse_present_voltage: zero }),
            B::PowerDeliveryReq(_) => B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: EvseStatusChoice::Dc(dc) }),
            B::CurrentDemandReq(_) => B::CurrentDemandRes(CurrentDemandRes {
                response_code: c,
                dc_evse_status: dc,
                evse_present_voltage: zero,
                evse_present_current: pv(0.0, UnitSymbol::A),
                evse_current_limit_achieved: false,
                evse_voltage_limit_achieved: false,
                evse_power_limit_achieved: false,
                evse_maximum_voltage_limit: None,
                evse_maximum_current_limit: None,
                evse_maximum_power_limit: None,
                evseid: "ZZ00000",
                sa_schedule_tuple_id: 1,
                meter_info: None,
                receipt_required: None,
            }),
            B::ChargingStatusReq(_) => B::ChargingStatusRes(ChargingStatusRes { response_code: c, evseid: "ZZ00000", sa_schedule_tuple_id: 1, evse_max_current: None, meter_info: None, receipt_required: None, ac_evse_status: ac }),
            B::WeldingDetectionReq(_) => B::WeldingDetectionRes(WeldingDetectionRes { response_code: c, dc_evse_status: dc, evse_present_voltage: zero }),
            B::SessionStopReq(_) => B::SessionStopRes(SessionStopRes { response_code: c }),
            _ => return Reply { len: 0, close: true },
        };
        self.send("response", c, body, true, out)
    }

    /// Whether the tuple id is one of those offered (the one tuple 1 when the host offers none).
    fn offers_tuple(&self, id: u8) -> bool {
        let offered = self.secc.schedule_tuples();
        if offered.is_empty() { id == 1 } else { offered.iter().any(|t| t.sa_schedule_tuple_id == id) }
    }

    fn on_frame(&mut self, payload_type: PayloadType, payload: &[u8], arena: &Arena<'_>, out: &mut [u8]) -> Reply {
        let closed = Reply { len: 0, close: true };
        if payload_type != PayloadType::SAP {
            return closed;
        }
        let Ok(message) = V2gMessage::decode(payload, arena) else {
            self.secc.note(format_args!("closing without an answer: the frame is not a decodable {} (state {:?})", self.state.expects(), self.state));
            return closed;
        };
        let Some(request) = message.body.body_element else { return closed };
        match self.secc.intercept(&request) {
            Action::Pass => {}
            Action::Silent => {
                self.secc.note(format_args!("silent: not answering"));
                return Reply::default();
            }
            Action::Close => return closed,
        }
        let session_ok = message.header.session_id == self.session_id;
        let state = self.state;
        // A SessionStopReq ends the session wherever the power is not on (and after the stop).
        if let (B::SessionStopReq(req), true) = (&request, state != State::AwaitSessionSetupReq) {
            if !session_ok {
                return self.failing(&request, ResponseCode::FailedUnknownSession, out);
            }
            if !matches!(state, State::ChargeLoop) {
                let pause = req.charging_session == t::ChargingSession::Pause;
                self.state = if pause { State::Paused } else { State::Stopped };
                self.secc.session_ended(Ended::Stopped(if pause { iso15118_20::common::ChargingSession::Pause } else { iso15118_20::common::ChargingSession::Terminate }));
                return self.send("SessionStopRes", ResponseCode::Ok, B::SessionStopRes(SessionStopRes { response_code: ResponseCode::Ok }), true, out);
            }
        }
        match (state, &request) {
            (State::AwaitSessionSetupReq, B::SessionSetupReq(req)) => {
                let asked = message.header.session_id;
                self.secc.note(format_args!("SessionSetupReq from EVCC {:02x?}, session {:02x?}", req.evccid, asked));
                let SessionSetup { code, session_id } = self.secc.session_setup(asked, req.evccid);
                self.session_id = session_id;
                if is_ok(code) {
                    self.state = State::AwaitServiceDiscoveryReq;
                }
                let id = Id::of(self.secc.evse_id());
                let stamp = self.secc.now_secs();
                self.send("SessionSetupRes", code, B::SessionSetupRes(SessionSetupRes { response_code: code, evseid: id.as_str(), evse_time_stamp: Some(stamp) }), !is_ok(code), out)
            }
            (State::AwaitServiceDiscoveryReq, B::ServiceDiscoveryReq(_)) => {
                if !session_ok {
                    return self.failing(&request, ResponseCode::FailedUnknownSession, out);
                }
                let (options, modes, services) = (self.secc.payment_options(), self.secc.energy_modes(), self.secc.value_added_services());
                let body = B::ServiceDiscoveryRes(ServiceDiscoveryRes {
                    response_code: ResponseCode::Ok,
                    payment_option_list: t::PaymentOptionList { payment_option: options },
                    charge_service: t::ChargeService {
                        service_id: 1,
                        service_name: None,
                        service_category: t::ServiceCategory::EvCharging,
                        service_scope: None,
                        free_service: false,
                        supported_energy_transfer_mode: t::SupportedEnergyTransferMode { energy_transfer_mode: modes },
                    },
                    service_list: (!services.is_empty()).then_some(t::ServiceList { service: services }),
                });
                // The answer borrows the charger; the log line is written after it is framed.
                let message = V2gMessage { header: MessageHeader { session_id: &self.session_id, notification: None, signature: None }, body: Body { body_element: Some(body) } };
                let framed = frame(out, |o| message.encode(o));
                self.state = State::AwaitServicePaymentSelectionReq;
                self.secc.note(format_args!("tx ServiceDiscoveryRes OK"));
                match framed {
                    Ok(len) => Reply { len, close: false },
                    Err(()) => closed,
                }
            }
            (State::AwaitServicePaymentSelectionReq, B::ServiceDetailReq(req)) if session_ok => {
                let known = self.secc.service_parameters(req.service_id).is_some();
                let c = if known { ResponseCode::Ok } else { ResponseCode::FailedServiceIdInvalid };
                let body = B::ServiceDetailRes(ServiceDetailRes {
                    response_code: c,
                    service_id: req.service_id,
                    service_parameter_list: self.secc.service_parameters(req.service_id).map(|sets| t::ServiceParameterList { parameter_set: sets }),
                });
                let message = V2gMessage { header: MessageHeader { session_id: &self.session_id, notification: None, signature: None }, body: Body { body_element: Some(body) } };
                let framed = frame(out, |o| message.encode(o));
                self.secc.note(format_args!("tx ServiceDetailRes {}", c.xml_name()));
                match framed {
                    Ok(len) => Reply { len, close: false },
                    Err(()) => closed,
                }
            }
            (State::AwaitServicePaymentSelectionReq, B::PaymentServiceSelectionReq(req)) if session_ok => {
                let contract = req.selected_payment_option == t::PaymentOption::Contract && self.secc.payment_options().contains(&t::PaymentOption::Contract);
                let payment_ok = req.selected_payment_option == t::PaymentOption::ExternalPayment || contract;
                let service_ok = !req.selected_service_list.selected_service.is_empty()
                    && req.selected_service_list.selected_service.iter().all(|s| s.service_id == 1 || self.secc.service_parameters(s.service_id).is_some());
                let c = if !payment_ok {
                    ResponseCode::FailedPaymentSelectionInvalid
                } else if !service_ok {
                    ResponseCode::FailedServiceSelectionInvalid
                } else {
                    self.secc.payment_selected(req, contract)
                };
                if is_ok(c) {
                    self.state = if contract { State::AwaitPaymentDetailsReq } else { State::AwaitAuthorizationReq };
                }
                self.send("PaymentServiceSelectionRes", c, B::PaymentServiceSelectionRes(PaymentServiceSelectionRes { response_code: c }), !is_ok(c), out)
            }
            (State::AwaitPaymentDetailsReq, B::PaymentDetailsReq(req)) if session_ok => {
                let (c, challenge, stamp) = match self.secc.payment_details(req) {
                    Ok(()) => {
                        self.state = State::AwaitAuthorizationReq;
                        (ResponseCode::Ok, self.secc.gen_challenge(), self.secc.now_secs())
                    }
                    Err(c) => (c, [0; 16], 0),
                };
                self.send("PaymentDetailsRes", c, B::PaymentDetailsRes(PaymentDetailsRes { response_code: c, gen_challenge: challenge, evse_time_stamp: stamp }), !is_ok(c), out)
            }
            (State::AwaitAuthorizationReq, B::AuthorizationReq(auth)) if session_ok => {
                let (c, processing) = match self.secc.authorize(&message.header, auth) {
                    Progress::Ongoing => (ResponseCode::Ok, t::EvseProcessing::Ongoing),
                    Progress::Done(c) => (c, t::EvseProcessing::Finished),
                };
                let ongoing = processing == t::EvseProcessing::Ongoing;
                if !ongoing && is_ok(c) {
                    self.state = State::AwaitChargeParameterDiscoveryReq;
                }
                self.send("AuthorizationRes", c, B::AuthorizationRes(AuthorizationRes { response_code: c, evse_processing: processing }), !ongoing && !is_ok(c), out)
            }
            (State::AwaitChargeParameterDiscoveryReq, B::ChargeParameterDiscoveryReq(req)) if session_ok => {
                let offered = self.secc.energy_modes().contains(&req.requested_energy_transfer_mode);
                let ac = is_ac(req.requested_energy_transfer_mode);
                let matches = matches!((&req.ev_charge_parameter, ac), (EvChargeParameterChoice::Ac(_), true) | (EvChargeParameterChoice::Dc(_), false));
                if !offered || !matches {
                    let c = if !offered { ResponseCode::FailedWrongEnergyTransferMode } else { ResponseCode::FailedWrongChargeParameter };
                    let count = self.secc.energy_modes().len();
                    self.secc.note(format_args!("the vehicle asks for {:?}, the charger offers {count} modes", req.requested_energy_transfer_mode));
                    return self.failing(&request, c, out);
                }
                self.ac = ac;
                self.announced_power = match &req.ev_charge_parameter {
                    EvChargeParameterChoice::Dc(p) => p.ev_maximum_power_limit,
                    _ => None,
                };
                let (c, offer, processing) = match self.secc.charge_parameters(req, self.renegotiating) {
                    Progress::Ongoing => (ResponseCode::Ok, None, t::EvseProcessing::Ongoing),
                    Progress::Done((c, offer)) => (c, Some(offer), t::EvseProcessing::Finished),
                };
                let ongoing = offer.is_none();
                let parameter = offer.map_or(
                    EvseChargeParameterChoice::Ac(t::AcEvseChargeParameter { ac_evse_status: self.ac_status(t::EvseNotification::None), evse_nominal_voltage: pv(0.0, UnitSymbol::V), evse_max_current: pv(0.0, UnitSymbol::A) }),
                    |o| o.parameter,
                );
                let p_max = offer.map_or(pv(0.0, UnitSymbol::W), |o| o.p_max);
                self.p_max_w = p_max.to_f64();
                let entry = [t::PMaxScheduleEntry { time_interval: t::TimeIntervalChoice::Relative(t::RelativeTimeInterval { start: 0, duration: Some(86_400) }), p_max }];
                let tuple = [t::SaScheduleTuple { sa_schedule_tuple_id: 1, p_max_schedule: t::PMaxSchedule { p_max_schedule_entry: &entry }, sales_tariff: None }];
                let offered = self.secc.schedule_tuples();
                let tuples: &[t::SaScheduleTuple<'_>] = if offered.is_empty() { &tuple } else { offered };
                let body = B::ChargeParameterDiscoveryRes(ChargeParameterDiscoveryRes {
                    response_code: c,
                    evse_processing: processing,
                    sa_schedules: (!ongoing).then_some(SaSchedulesChoice::SaScheduleList(t::SaScheduleList { sa_schedule_tuple: tuples })),
                    evse_charge_parameter: parameter,
                });
                if !ongoing && is_ok(c) {
                    // After a renegotiation the cable is checked already: power is asked for again.
                    self.state = if ac || self.renegotiating { State::AwaitPowerDeliveryReq } else { State::AwaitCableCheckReq };
                }
                // The answer borrows the charger's tuples; the log line is written after it is framed.
                let message = V2gMessage { header: MessageHeader { session_id: &self.session_id, notification: None, signature: None }, body: Body { body_element: Some(body) } };
                let framed = frame(out, |o| message.encode(o));
                let close = !ongoing && !is_ok(c);
                self.secc.note(format_args!("tx ChargeParameterDiscoveryRes {}{}", c.xml_name(), if close { ", then closing" } else { "" }));
                match framed {
                    Ok(len) => Reply { len, close },
                    Err(()) => closed,
                }
            }
            (State::AwaitCableCheckReq, B::CableCheckReq(_)) if session_ok => {
                let (c, ongoing) = match self.secc.cable_check() {
                    Progress::Ongoing => (ResponseCode::Ok, true),
                    Progress::Done(c) => (c, false),
                };
                let (status, isolation, processing) = if ongoing {
                    (t::DcEvseStatusCode::EvseIsolationMonitoringActive, t::IsolationLevel::Invalid, t::EvseProcessing::Ongoing)
                } else {
                    (t::DcEvseStatusCode::EvseReady, t::IsolationLevel::Valid, t::EvseProcessing::Finished)
                };
                if !ongoing && is_ok(c) {
                    self.state = State::AwaitPreChargeReq;
                }
                let body = B::CableCheckRes(CableCheckRes { response_code: c, dc_evse_status: self.dc_status(status, Some(isolation)), evse_processing: processing });
                self.send("CableCheckRes", c, body, !ongoing && !is_ok(c), out)
            }
            (State::AwaitPreChargeReq, B::PreChargeReq(req)) if session_ok => {
                let voltage = self.secc.pre_charge(req.ev_target_voltage);
                let body = B::PreChargeRes(PreChargeRes {
                    response_code: ResponseCode::Ok,
                    dc_evse_status: self.dc_status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid)),
                    evse_present_voltage: voltage,
                });
                self.send("PreChargeRes", ResponseCode::Ok, body, false, out)
            }
            (State::AwaitPreChargeReq | State::AwaitPowerDeliveryReq, B::PowerDeliveryReq(req)) if session_ok && req.charge_progress == t::ChargeProgress::Start => {
                // The tuple the vehicle follows must be one of those offered; the profile within the schedule.
                let mut c = ResponseCode::Ok;
                if !self.offers_tuple(req.sa_schedule_tuple_id) {
                    c = ResponseCode::FailedTariffSelectionInvalid;
                } else if let Some(profile) = &req.charging_profile {
                    if profile.profile_entry.iter().any(|e| e.charging_profile_entry_max_power.to_f64() > self.p_max_w * 1.0001) {
                        c = ResponseCode::FailedChargingProfileInvalid;
                    }
                }
                if is_ok(c) {
                    c = self.secc.power_delivery(req);
                }
                if is_ok(c) {
                    self.state = State::ChargeLoop;
                    self.renegotiating = false;
                }
                let status = self.status();
                self.send("PowerDeliveryRes", c, B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: status }), !is_ok(c), out)
            }
            (State::ChargeLoop, B::CurrentDemandReq(req)) if session_ok && !self.ac => {
                let a = self.secc.dc_loop(req, self.announced_power);
                let mut status = self.dc_status(if a.shutdown { t::DcEvseStatusCode::EvseShutdown } else { t::DcEvseStatusCode::EvseReady }, Some(t::IsolationLevel::Valid));
                if a.notification != t::EvseNotification::None {
                    status.evse_notification = a.notification;
                    status.notification_max_delay = 5;
                }
                let id = Id::of(self.secc.evse_id());
                let body = B::CurrentDemandRes(CurrentDemandRes {
                    response_code: a.response_code,
                    dc_evse_status: status,
                    evse_present_voltage: a.present_voltage,
                    evse_present_current: a.present_current,
                    evse_current_limit_achieved: a.current_limit_achieved,
                    evse_voltage_limit_achieved: a.voltage_limit_achieved,
                    evse_power_limit_achieved: a.power_limit_achieved,
                    evse_maximum_voltage_limit: a.maximum_voltage,
                    evse_maximum_current_limit: a.maximum_current,
                    evse_maximum_power_limit: a.maximum_power,
                    evseid: id.as_str(),
                    sa_schedule_tuple_id: 1,
                    meter_info: a.meter_info,
                    receipt_required: a.receipt_required,
                });
                self.send("CurrentDemandRes", a.response_code, body, !is_ok(a.response_code), out)
            }
            (State::ChargeLoop, B::ChargingStatusReq(req)) if session_ok && self.ac => {
                let a = self.secc.ac_loop(req);
                let id = Id::of(self.secc.evse_id());
                let body = B::ChargingStatusRes(ChargingStatusRes {
                    response_code: a.response_code,
                    evseid: id.as_str(),
                    sa_schedule_tuple_id: 1,
                    evse_max_current: a.max_current,
                    meter_info: a.meter_info,
                    receipt_required: a.receipt_required,
                    ac_evse_status: self.ac_status(a.notification),
                });
                self.send("ChargingStatusRes", a.response_code, body, false, out)
            }
            (State::ChargeLoop, B::PowerDeliveryReq(req)) if session_ok && req.charge_progress == t::ChargeProgress::Renegotiate => {
                // New charge parameters and a new schedule; the contactors stay closed.
                self.state = State::AwaitChargeParameterDiscoveryReq;
                self.renegotiating = true;
                self.secc.note(format_args!("renegotiation: new charge parameters, the power stays connected"));
                self.secc.renegotiation();
                let status = self.status();
                self.send("PowerDeliveryRes", ResponseCode::Ok, B::PowerDeliveryRes(PowerDeliveryRes { response_code: ResponseCode::Ok, evse_status: status }), false, out)
            }
            (State::ChargeLoop, B::MeteringReceiptReq(req)) if session_ok => {
                let verdict = self.secc.metering_receipt(&message.header, req);
                let status = self.status();
                self.send("MeteringReceiptRes", verdict, B::MeteringReceiptRes(MeteringReceiptRes { response_code: verdict, evse_status: status }), !is_ok(verdict), out)
            }
            (State::ChargeLoop, B::PowerDeliveryReq(req)) if session_ok && req.charge_progress == t::ChargeProgress::Stop => {
                let c = self.secc.power_delivery(req);
                if is_ok(c) {
                    self.state = if self.ac { State::AwaitSessionStopReq } else { State::AwaitWeldingReq };
                }
                let status = self.status();
                self.send("PowerDeliveryRes", c, B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: status }), !is_ok(c), out)
            }
            (State::AwaitWeldingReq, B::WeldingDetectionReq(_)) if session_ok => {
                let (c, voltage) = self.secc.welding_detection();
                let body = B::WeldingDetectionRes(WeldingDetectionRes {
                    response_code: c,
                    dc_evse_status: self.dc_status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid)),
                    evse_present_voltage: voltage,
                });
                self.send("WeldingDetectionRes", c, body, !is_ok(c), out)
            }
            (_, req) => {
                let c = if session_ok { ResponseCode::FailedSequenceError } else { ResponseCode::FailedUnknownSession };
                self.secc.note(format_args!("a request out of sequence (state {state:?}): {}", c.xml_name()));
                self.failing(req, c, out)
            }
        }
    }
}
