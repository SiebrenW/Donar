//! The charger's DIN SPEC 70121 machine (DC, external payment or contract): the SECC sequence of 9.7.4.2.4
//! with the response codes of 9.7.3, behind the [`DinSecc`] trait.
//!
//! It starts after the `SupportedAppProtocol` negotiation, which the host does (the protocol was chosen and
//! answered): the first frame the machine sees is the `SessionSetupReq`. Host contract as for
//! [`crate::session::Session`]: [`Session::rx_space`], [`Session::handle`], the same [`Event`], [`Step`] and
//! sequence timer.

use din70121::body::*;
use din70121::def::{Body, BodyElementChoice as B, V2gMessage};
use din70121::header::MessageHeader;
use din70121::types::{self as t, PhysicalValue, UnitSymbol};
use exi::Arena;
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use crate::secc::{Action, Ended, Progress};
use crate::session::SEQUENCE_TIMEOUT_MS;
use crate::{Event, Step, Timer, TimerCmd};

pub use din70121::types::ResponseCode;

/// Memory the host provides.
pub struct Buffers<'a> {
    pub rx: &'a mut [u8],
    pub tx: &'a mut [u8],
    /// Scratch for decoded messages, reset for every received frame.
    pub arena: &'a mut [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    AwaitSessionSetupReq,
    AwaitServiceDiscoveryReq,
    AwaitServicePaymentSelectionReq,
    AwaitContractAuthenticationReq,
    AwaitChargeParameterDiscoveryReq,
    AwaitCableCheckReq,
    /// Pre-charge rounds, then the `PowerDeliveryReq` that starts the power.
    AwaitPreChargeReq,
    /// Energy flows; `CurrentDemandReq` and the stop are accepted.
    ChargeLoop,
    /// Welding detection after the stop.
    AwaitWeldingReq,
    Stopped,
    Closed,
}

impl State {
    /// The request the charger waits for in this state.
    pub fn expects(self) -> &'static str {
        match self {
            State::AwaitSessionSetupReq => "SessionSetupReq",
            State::AwaitServiceDiscoveryReq => "ServiceDiscoveryReq",
            State::AwaitServicePaymentSelectionReq => "ServicePaymentSelectionReq",
            State::AwaitContractAuthenticationReq => "ContractAuthenticationReq",
            State::AwaitChargeParameterDiscoveryReq => "ChargeParameterDiscoveryReq",
            State::AwaitCableCheckReq => "CableCheckReq",
            State::AwaitPreChargeReq => "PreChargeReq or PowerDeliveryReq",
            State::ChargeLoop => "CurrentDemandReq or PowerDeliveryReq",
            State::AwaitWeldingReq => "WeldingDetectionReq or SessionStopReq",
            State::Stopped | State::Closed => "nothing",
        }
    }
}

pub fn is_ok(c: ResponseCode) -> bool {
    matches!(c, ResponseCode::Ok | ResponseCode::OkNewSessionEstablished | ResponseCode::OkOldSessionJoined | ResponseCode::OkCertificateExpiresSoon)
}

/// The answer to a `SessionSetupReq`: the code and the id of the session (V2G-DC-393: a new session has an id of its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSetup {
    pub code: ResponseCode,
    pub session_id: [u8; 8],
}

/// What the charger offers in its `ChargeParameterDiscoveryRes`: the limits, and the most power its schedule allows
/// (watts; the schedule covers 24 hours, V2G-DC-559).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Offer {
    pub parameter: t::DcEvseChargeParameter,
    pub p_max_w: i16,
}

/// A charger's answer to a `CurrentDemandReq`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoopAnswer {
    pub response_code: ResponseCode,
    /// Status code `EVSE_Shutdown` instead of `EVSE_Ready`: the charger ends the charge.
    pub shutdown: bool,
    pub present_voltage: PhysicalValue,
    pub present_current: PhysicalValue,
    pub current_limit_achieved: bool,
    pub voltage_limit_achieved: bool,
    pub power_limit_achieved: bool,
    pub maximum_voltage: Option<PhysicalValue>,
    pub maximum_current: Option<PhysicalValue>,
    pub maximum_power: Option<PhysicalValue>,
}

/// The charger, as the DIN SPEC 70121 machine sees it. Every method has a default: a charger that accepts
/// and does nothing.
pub trait DinSecc {
    /// `EVSEID` of the `SessionSetupRes` as hexBinary (V2G-DC-621).
    fn evse_id(&self) -> &[u8] {
        &[0xFF]
    }
    /// The charger's clock in seconds.
    fn now_secs(&self) -> i64 {
        0
    }
    /// The `SessionSetupReq`: whether to accept it and the session id.
    fn session_setup(&mut self, _evccid: &[u8]) -> SessionSetup {
        SessionSetup { code: ResponseCode::OkNewSessionEstablished, session_id: [1, 2, 3, 4, 5, 6, 7, 8] }
    }
    /// Offer `Contract` next to `ExternalPayment`.
    fn contract_payment(&self) -> bool {
        false
    }
    fn energy_transfer(&self) -> t::EvseSupportedEnergyTransfer {
        t::EvseSupportedEnergyTransfer::DcExtended
    }
    /// A valid payment and service selection: the code to answer with.
    fn payment_selected(&mut self, _request: &ServicePaymentSelectionReq<'_>) -> ResponseCode {
        ResponseCode::Ok
    }
    /// Authorization (`Ongoing` while the user has not identified); the code once it is decided.
    fn authorize(&mut self) -> Progress<ResponseCode> {
        Progress::Done(ResponseCode::Ok)
    }
    /// The charge parameters for the vehicle's announcement, or `Ongoing`; the code to answer with is `Ok` or a failure.
    fn charge_parameters(&mut self, _ev: &t::DcEvChargeParameter) -> Progress<(ResponseCode, Offer)> {
        Progress::Done((ResponseCode::Ok, zero_offer()))
    }
    /// One round of the cable check: `Ongoing` while the insulation is tested, then the code.
    fn cable_check(&mut self) -> Progress<ResponseCode> {
        Progress::Done(ResponseCode::Ok)
    }
    /// One round of pre-charge: the voltage at the charger's output for the vehicle's target.
    fn pre_charge(&mut self, target: PhysicalValue) -> PhysicalValue {
        target
    }
    /// The vehicle starts the power (`ready` true; the profile has been checked against the schedule) or stops it. The code to answer with.
    fn power_delivery(&mut self, _request: &PowerDeliveryReq<'_>) -> ResponseCode {
        ResponseCode::Ok
    }
    /// A `CurrentDemandReq`; `announced` is what the vehicle announced in its charge parameter discovery.
    fn current_demand(&mut self, _request: &CurrentDemandReq, _announced: &t::DcEvChargeParameter) -> LoopAnswer {
        let zero = |u| PhysicalValue::from_f64(0.0, u);
        LoopAnswer {
            response_code: ResponseCode::Ok,
            shutdown: false,
            present_voltage: zero(UnitSymbol::V),
            present_current: zero(UnitSymbol::A),
            current_limit_achieved: false,
            voltage_limit_achieved: false,
            power_limit_achieved: false,
            maximum_voltage: None,
            maximum_current: None,
            maximum_power: None,
        }
    }
    /// One round of welding detection: the code and the voltage seen at the output.
    fn welding_detection(&mut self) -> (ResponseCode, PhysicalValue) {
        (ResponseCode::Ok, PhysicalValue::from_f64(0.0, UnitSymbol::V))
    }
    /// The session ended.
    fn session_ended(&mut self, _how: Ended) {}
    /// Called with every request before it is looked at.
    fn intercept(&mut self, _request: &B<'_>) -> Action {
        Action::Pass
    }
    /// A step that was taken or refused, for a log.
    fn note(&mut self, _text: core::fmt::Arguments<'_>) {}
}

fn zero_offer() -> Offer {
    Offer {
        parameter: t::DcEvseChargeParameter {
            dc_evse_status: t::DcEvseStatus { evse_isolation_status: None, evse_status_code: t::DcEvseStatusCode::EvseReady, notification_max_delay: 0, evse_notification: t::EvseNotification::None },
            evse_maximum_current_limit: pv(0.0, UnitSymbol::A),
            evse_maximum_power_limit: None,
            evse_maximum_voltage_limit: pv(0.0, UnitSymbol::V),
            evse_minimum_current_limit: pv(0.0, UnitSymbol::A),
            evse_minimum_voltage_limit: pv(0.0, UnitSymbol::V),
            evse_current_regulation_tolerance: None,
            evse_peak_current_ripple: pv(0.0, UnitSymbol::A),
            evse_energy_to_be_delivered: None,
        },
        p_max_w: 0,
    }
}

pub struct Session<S: DinSecc> {
    rx_len: usize,
    m: Machine<S>,
}

struct Machine<S> {
    secc: S,
    state: State,
    session_id: [u8; 8],
    announced: Option<t::DcEvChargeParameter>,
    /// The most power the schedule offered, in watts.
    p_max_w: i16,
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

impl<S: DinSecc> Session<S> {
    pub fn new(secc: S) -> Self {
        Self { rx_len: 0, m: Machine { secc, state: State::AwaitSessionSetupReq, session_id: [0; 8], announced: None, p_max_w: 0 } }
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
        let ended = |s: State| matches!(s, State::Stopped | State::Closed);
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

impl<S: DinSecc> Machine<S> {
    fn status(&self, status: t::DcEvseStatusCode, isolation: Option<t::IsolationLevel>) -> t::DcEvseStatus {
        t::DcEvseStatus { evse_isolation_status: isolation, evse_status_code: status, notification_max_delay: 0, evse_notification: t::EvseNotification::None }
    }

    fn transfer(&self) -> t::EvseSupportedEnergyTransfer {
        self.secc.energy_transfer()
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

    /// The response a request asks for, with a failing code: the answer to a request out of sequence or with the
    /// wrong session id (V2G-DC-390, V2G-DC-391).
    fn failing(&mut self, request: &B<'_>, c: ResponseCode, out: &mut [u8]) -> Reply {
        let status = self.status(t::DcEvseStatusCode::EvseReady, None);
        let zero = pv(0.0, UnitSymbol::V);
        let tuple = [t::SaScheduleTuple {
            sa_schedule_tuple_id: 1,
            p_max_schedule: t::PMaxSchedule {
                p_max_schedule_id: 1,
                p_max_schedule_entry: &[t::PMaxScheduleEntry { time_interval: t::TimeIntervalChoice::Relative(t::RelativeTimeInterval { start: 0, duration: None }), p_max: 0 }],
            },
            sales_tariff: None,
        }];
        let service = t::ServiceCharge {
            service_tag: t::ServiceTag { service_id: 1, service_name: None, service_category: t::ServiceCategory::EvCharging, service_scope: None },
            free_service: false,
            energy_transfer_type: self.transfer(),
        };
        let evse_parameter = t::DcEvseChargeParameter {
            dc_evse_status: status,
            evse_maximum_current_limit: pv(0.0, UnitSymbol::A),
            evse_maximum_power_limit: None,
            evse_maximum_voltage_limit: zero,
            evse_minimum_current_limit: pv(0.0, UnitSymbol::A),
            evse_minimum_voltage_limit: zero,
            evse_current_regulation_tolerance: None,
            evse_peak_current_ripple: pv(0.0, UnitSymbol::A),
            evse_energy_to_be_delivered: None,
        };
        let body = match request {
            B::SessionSetupReq(_) => B::SessionSetupRes(SessionSetupRes { response_code: c, evseid: &[0], date_time_now: None }),
            B::ServiceDiscoveryReq(_) => B::ServiceDiscoveryRes(ServiceDiscoveryRes {
                response_code: c,
                payment_options: t::PaymentOptions { payment_option: &[t::PaymentOption::ExternalPayment] },
                charge_service: service,
                service_list: None,
            }),
            B::ServicePaymentSelectionReq(_) => B::ServicePaymentSelectionRes(ServicePaymentSelectionRes { response_code: c }),
            B::ContractAuthenticationReq(_) => B::ContractAuthenticationRes(ContractAuthenticationRes { response_code: c, evse_processing: t::EvseProcessing::Finished }),
            B::ChargeParameterDiscoveryReq(_) => B::ChargeParameterDiscoveryRes(ChargeParameterDiscoveryRes {
                response_code: c,
                evse_processing: t::EvseProcessing::Finished,
                sa_schedules: SaSchedulesChoice::SaScheduleList(t::SaScheduleList { sa_schedule_tuple: &tuple }),
                evse_charge_parameter: EvseChargeParameterChoice::Dc(evse_parameter),
            }),
            B::CableCheckReq(_) => B::CableCheckRes(CableCheckRes { response_code: c, dc_evse_status: status, evse_processing: t::EvseProcessing::Finished }),
            B::PreChargeReq(_) => B::PreChargeRes(PreChargeRes { response_code: c, dc_evse_status: status, evse_present_voltage: zero }),
            B::PowerDeliveryReq(_) => B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: EvseStatusChoice::Dc(status) }),
            B::CurrentDemandReq(_) => B::CurrentDemandRes(CurrentDemandRes {
                response_code: c,
                dc_evse_status: status,
                evse_present_voltage: zero,
                evse_present_current: pv(0.0, UnitSymbol::A),
                evse_current_limit_achieved: false,
                evse_voltage_limit_achieved: false,
                evse_power_limit_achieved: false,
                evse_maximum_voltage_limit: None,
                evse_maximum_current_limit: None,
                evse_maximum_power_limit: None,
            }),
            B::WeldingDetectionReq(_) => B::WeldingDetectionRes(WeldingDetectionRes { response_code: c, dc_evse_status: status, evse_present_voltage: zero }),
            B::SessionStopReq(_) => B::SessionStopRes(SessionStopRes { response_code: c }),
            _ => return Reply { len: 0, close: true },
        };
        self.send("response", c, body, true, out)
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
        // SessionStopReq is accepted wherever the session has not yet started the power (V2G-DC-664) and in the stopping states.
        if let (B::SessionStopReq(_), true) = (&request, state != State::AwaitSessionSetupReq) {
            if !session_ok {
                return self.failing(&request, ResponseCode::FailedUnknownSession, out);
            }
            if !matches!(state, State::ChargeLoop) {
                self.state = State::Stopped;
                self.secc.session_ended(Ended::Stopped(iso15118_20::common::ChargingSession::Terminate));
                return self.send("SessionStopRes", ResponseCode::Ok, B::SessionStopRes(SessionStopRes { response_code: ResponseCode::Ok }), true, out);
            }
        }
        match (state, &request) {
            (State::AwaitSessionSetupReq, B::SessionSetupReq(req)) => {
                if req.evccid.is_empty() {
                    self.secc.note(format_args!("SessionSetupReq without an EVCCID"));
                }
                self.secc.note(format_args!("SessionSetupReq from EVCC {:02x?}", req.evccid));
                let SessionSetup { code, session_id } = self.secc.session_setup(req.evccid);
                self.session_id = session_id;
                if is_ok(code) {
                    self.state = State::AwaitServiceDiscoveryReq;
                }
                let now = self.secc.now_secs();
                // The id is copied: the answer is sent through `self`.
                let (mut id, mut n) = ([0u8; 32], 0);
                for b in self.secc.evse_id().iter().take(32) {
                    id[n] = *b;
                    n += 1;
                }
                let body = B::SessionSetupRes(SessionSetupRes { response_code: code, evseid: &id[..n], date_time_now: Some(now) });
                let close = !is_ok(code);
                // The id is in the answer's header already (V2G-DC-393).
                self.send("SessionSetupRes", code, body, close, out)
            }
            (State::AwaitServiceDiscoveryReq, B::ServiceDiscoveryReq(_)) if session_ok => {
                let options = [t::PaymentOption::ExternalPayment, t::PaymentOption::Contract];
                let options = &options[..if self.secc.contract_payment() { 2 } else { 1 }];
                let body = B::ServiceDiscoveryRes(ServiceDiscoveryRes {
                    response_code: ResponseCode::Ok,
                    payment_options: t::PaymentOptions { payment_option: options },
                    charge_service: t::ServiceCharge {
                        service_tag: t::ServiceTag { service_id: 1, service_name: None, service_category: t::ServiceCategory::EvCharging, service_scope: None },
                        free_service: false,
                        energy_transfer_type: self.transfer(),
                    },
                    service_list: None,
                });
                self.state = State::AwaitServicePaymentSelectionReq;
                self.send("ServiceDiscoveryRes", ResponseCode::Ok, body, false, out)
            }
            (State::AwaitServicePaymentSelectionReq, B::ServicePaymentSelectionReq(req)) if session_ok => {
                let payment_ok = req.selected_payment_option == t::PaymentOption::ExternalPayment || (self.secc.contract_payment() && req.selected_payment_option == t::PaymentOption::Contract);
                let service_ok = req.selected_service_list.selected_service.iter().all(|s| s.service_id == 1);
                // V2G-DC-395, V2G-DC-396.
                let c = if !payment_ok {
                    ResponseCode::FailedPaymentSelectionInvalid
                } else if !service_ok || req.selected_service_list.selected_service.is_empty() {
                    ResponseCode::FailedServiceSelectionInvalid
                } else {
                    self.secc.payment_selected(req)
                };
                if is_ok(c) {
                    self.state = State::AwaitContractAuthenticationReq;
                }
                self.send("ServicePaymentSelectionRes", c, B::ServicePaymentSelectionRes(ServicePaymentSelectionRes { response_code: c }), !is_ok(c), out)
            }
            (State::AwaitContractAuthenticationReq, B::ContractAuthenticationReq(_)) if session_ok => {
                let (c, processing) = match self.secc.authorize() {
                    Progress::Ongoing => (ResponseCode::Ok, t::EvseProcessing::Ongoing),
                    Progress::Done(c) => (c, t::EvseProcessing::Finished),
                };
                let ongoing = processing == t::EvseProcessing::Ongoing;
                if !ongoing && is_ok(c) {
                    self.state = State::AwaitChargeParameterDiscoveryReq;
                }
                self.send("ContractAuthenticationRes", c, B::ContractAuthenticationRes(ContractAuthenticationRes { response_code: c, evse_processing: processing }), !ongoing && !is_ok(c), out)
            }
            (State::AwaitChargeParameterDiscoveryReq, B::ChargeParameterDiscoveryReq(req)) if session_ok => {
                let EvChargeParameterChoice::Dc(ev) = req.ev_charge_parameter else {
                    // V2G-DC-398.
                    return self.failing(&request, ResponseCode::FailedWrongChargeParameter, out);
                };
                let wanted_ok = matches!(
                    (req.ev_requested_energy_transfer_type, self.transfer()),
                    (t::EvRequestedEnergyTransfer::DcCore, t::EvseSupportedEnergyTransfer::DcCore) | (t::EvRequestedEnergyTransfer::DcExtended, t::EvseSupportedEnergyTransfer::DcExtended)
                );
                if !wanted_ok {
                    // V2G-DC-397.
                    self.secc.note(format_args!("the vehicle asks for {:?}, the charger offers {:?}", req.ev_requested_energy_transfer_type, self.transfer()));
                    return self.failing(&request, ResponseCode::FailedWrongEnergyTransferType, out);
                }
                self.announced = Some(ev);
                let (c, offer, processing) = match self.secc.charge_parameters(&ev) {
                    Progress::Ongoing => {
                        // The values do not matter while `Ongoing`; the message has the same shape.
                        (ResponseCode::Ok, zero_offer(), t::EvseProcessing::Ongoing)
                    }
                    Progress::Done((c, offer)) => (c, offer, t::EvseProcessing::Finished),
                };
                let ongoing = processing == t::EvseProcessing::Ongoing;
                self.p_max_w = offer.p_max_w;
                // V2G-DC-559: the schedule covers at least 24 hours.
                let entry = [t::PMaxScheduleEntry { time_interval: t::TimeIntervalChoice::Relative(t::RelativeTimeInterval { start: 0, duration: Some(86_400) }), p_max: offer.p_max_w }];
                let tuple = [t::SaScheduleTuple { sa_schedule_tuple_id: 1, p_max_schedule: t::PMaxSchedule { p_max_schedule_id: 1, p_max_schedule_entry: &entry }, sales_tariff: None }];
                let body = B::ChargeParameterDiscoveryRes(ChargeParameterDiscoveryRes {
                    response_code: c,
                    evse_processing: processing,
                    sa_schedules: SaSchedulesChoice::SaScheduleList(t::SaScheduleList { sa_schedule_tuple: &tuple }),
                    evse_charge_parameter: EvseChargeParameterChoice::Dc(offer.parameter),
                });
                if !ongoing && is_ok(c) {
                    self.state = State::AwaitCableCheckReq;
                }
                self.send("ChargeParameterDiscoveryRes", c, body, !ongoing && !is_ok(c), out)
            }
            (State::AwaitCableCheckReq, B::CableCheckReq(_)) if session_ok => {
                let (c, ongoing) = match self.secc.cable_check() {
                    Progress::Ongoing => (ResponseCode::Ok, true),
                    Progress::Done(c) => (c, false),
                };
                // V2G-DC-499, V2G-DC-455.
                let (status, isolation, processing) = if ongoing {
                    (t::DcEvseStatusCode::EvseIsolationMonitoringActive, t::IsolationLevel::Invalid, t::EvseProcessing::Ongoing)
                } else {
                    (t::DcEvseStatusCode::EvseReady, t::IsolationLevel::Valid, t::EvseProcessing::Finished)
                };
                if !ongoing && is_ok(c) {
                    self.state = State::AwaitPreChargeReq;
                }
                let body = B::CableCheckRes(CableCheckRes { response_code: c, dc_evse_status: self.status(status, Some(isolation)), evse_processing: processing });
                self.send("CableCheckRes", c, body, !ongoing && !is_ok(c), out)
            }
            (State::AwaitPreChargeReq, B::PreChargeReq(req)) if session_ok => {
                let voltage = self.secc.pre_charge(req.ev_target_voltage);
                let body = B::PreChargeRes(PreChargeRes {
                    response_code: ResponseCode::Ok,
                    dc_evse_status: self.status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid)),
                    evse_present_voltage: voltage,
                });
                self.send("PreChargeRes", ResponseCode::Ok, body, false, out)
            }
            (State::AwaitPreChargeReq, B::PowerDeliveryReq(req)) if session_ok && req.ready_to_charge_state => {
                // V2G-DC-399, V2G-DC-400: the profile the vehicle chose, against the schedule.
                let mut c = ResponseCode::Ok;
                if let Some(profile) = &req.charging_profile {
                    if profile.sa_schedule_tuple_id != 1 {
                        c = ResponseCode::FailedTariffSelectionInvalid;
                    } else if profile.profile_entry.iter().any(|e| e.charging_profile_entry_max_power > self.p_max_w) {
                        c = ResponseCode::FailedChargingProfileInvalid;
                    }
                }
                if is_ok(c) {
                    c = self.secc.power_delivery(req);
                }
                if is_ok(c) {
                    self.state = State::ChargeLoop;
                }
                let status = self.status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid));
                self.send("PowerDeliveryRes", c, B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: EvseStatusChoice::Dc(status) }), !is_ok(c), out)
            }
            (State::ChargeLoop, B::CurrentDemandReq(req)) if session_ok => {
                let Some(announced) = self.announced else { return self.failing(&request, ResponseCode::FailedSequenceError, out) };
                let a = self.secc.current_demand(req, &announced);
                let status = self.status(if a.shutdown { t::DcEvseStatusCode::EvseShutdown } else { t::DcEvseStatusCode::EvseReady }, Some(t::IsolationLevel::Valid));
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
                });
                self.send("CurrentDemandRes", a.response_code, body, !is_ok(a.response_code), out)
            }
            (State::ChargeLoop, B::PowerDeliveryReq(req)) if session_ok && !req.ready_to_charge_state => {
                let c = self.secc.power_delivery(req);
                if is_ok(c) {
                    self.state = State::AwaitWeldingReq;
                }
                let status = self.status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid));
                self.send("PowerDeliveryRes", c, B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: EvseStatusChoice::Dc(status) }), !is_ok(c), out)
            }
            (State::AwaitWeldingReq, B::WeldingDetectionReq(_)) if session_ok => {
                let (c, voltage) = self.secc.welding_detection();
                let body = B::WeldingDetectionRes(WeldingDetectionRes {
                    response_code: c,
                    dc_evse_status: self.status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid)),
                    evse_present_voltage: voltage,
                });
                self.send("WeldingDetectionRes", c, body, !is_ok(c), out)
            }
            // A request with the wrong session id (V2G-DC-391), or out of sequence (V2G-DC-390): the answer of its own type and the end
            // of the session (V2G-DC-666).
            (_, req) => {
                let c = if session_ok { ResponseCode::FailedSequenceError } else { ResponseCode::FailedUnknownSession };
                self.secc.note(format_args!("a request out of sequence (state {state:?}): {}", c.xml_name()));
                self.failing(req, c, out)
            }
        }
    }
}
