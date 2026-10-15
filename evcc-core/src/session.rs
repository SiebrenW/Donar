//! The V2G session machine: SupportedAppProtocol handshake, SessionSetup, EIM authorization
//! and service discovery, detail and selection. (More of the DC dynamic flow is added slice
//! by slice.)
//!
//! Host contract:
//! 1. Open the TCP/TLS connection found by SDP, then send [`Event::Start`].
//! 2. Read from the socket straight into [`Session::rx_space`], then send
//!    [`Event::Received`] with the byte count.
//! 3. After every call, carry out the returned [`Step`]: write `tx[..send]` to the stream,
//!    apply the timer commands, handle `notice`, and close the connection once `end` is set.
//! 4. When an armed timer fires, send [`Event::TimerExpired`] naming it.

use app_protocol::{self, AppProtocol, SupportedAppProtocolReq};
use exi::{Arena, DecodeError};
use iso15118_20::common::{
    self, AReqAuthorizationModeChoice, Authorization, AuthorizationReq, AuthorizationSetupReq, EimAReqAuthorizationMode,
    ParameterChoice, ParameterSet, SelectedService, ServiceDetailReq, ServiceDiscoveryReq, ServiceSelectionReq,
    SessionSetupReq,
};
use iso15118_20::common_types::{MessageHeader, Processing, ResponseCode};
use iso15118_20::dc;
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use crate::ev::Ev;
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd, Timers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The protocol offered in `SupportedAppProtocolReq`.
    pub offer: AppProtocol<'static>,
    /// How long to wait for a response: `V2G_EVCC_Msg_Timeout(MessageType)` is 2 s for every
    /// message we send except `ServiceDetailReq` and `CertificateInstallationReq`
    /// (ISO 15118-20:2022 Table 215). The charge loop has its own timing (added with it).
    pub response_timeout_ms: u32,
    /// `V2G_EVCC_Msg_Timeout(ServiceDetailReq)`: 5 s (Table 215).
    pub service_detail_timeout_ms: u32,
    /// `V2G_EVCC_Ongoing_Timeout`: the bound on a repeated `Ongoing` exchange such as
    /// authorization polling; 60 s (Table 215).
    pub ongoing_timeout_ms: u32,
    /// Pause between repeats of a request while the EVSE answers `Ongoing` (0: repeat at
    /// once). The standard only bounds the time (V2G20-1582); this keeps the line quiet.
    pub poll_interval_ms: u32,
    /// `V2G_EVCC_DC_CableCheck_Timeout`: 40 s from the first request (Table 223, V2G20-700).
    pub cable_check_timeout_ms: u32,
    /// `V2G_EVCC_DC_PreCharge_Timeout`: 10 s from the first request (Table 223, V2G20-704).
    pub precharge_timeout_ms: u32,
    /// Pre-charge is done when the voltage at the inlet differs less than this from the battery
    /// voltage (IEC 61851-23:2023 CC.5.1 / t9: 20 V).
    pub precharge_tolerance_v: u16,
    /// `V2G_EVCC_Msg_Timeout(DC_ChargeLoopReq)`: 0.5 s (Table 217).
    pub charge_loop_timeout_ms: u32,
    /// Pause between a `DC_ChargeLoopRes` and the next request. The next request has to go out
    /// within `V2G_EVCC_Sequence_Performance_Time(DC_ChargeLoopReq)`, 0.25 s (Table 217), so
    /// keep this well below that.
    pub charge_loop_interval_ms: u32,
    /// `MaximumSupportingPoints` of the schedule exchange: how many entries of a schedule the
    /// EV can take (12..=1024).
    pub maximum_supporting_points: u16,
    /// The energy transfer service to select (`services::service_id`).
    pub energy_service: u16,
    /// A parameter set is acceptable when it holds every `(name, value)` listed here as an
    /// integer parameter. The first acceptable set is selected.
    pub parameter_filter: &'static [(&'static str, i32)],
}

impl Config {
    /// ISO 15118-20 DC, version 1.0, schema id 1, top priority.
    pub const DC_20: Config = Config {
        offer: AppProtocol {
            protocol_namespace: "urn:iso:std:iso:15118:-20:DC",
            version_number_major: 1,
            version_number_minor: 0,
            schema_id: 1,
            priority: 1,
        },
        response_timeout_ms: 2000,
        service_detail_timeout_ms: 5000,
        ongoing_timeout_ms: 60_000,
        poll_interval_ms: 200,
        cable_check_timeout_ms: 40_000,
        precharge_timeout_ms: 10_000,
        precharge_tolerance_v: 20,
        charge_loop_timeout_ms: 500,
        charge_loop_interval_ms: 100,
        maximum_supporting_points: 12,
        energy_service: SERVICE_ID_DC,
        // ISO 15118-20:2022 Table 207: ControlMode 2 = Dynamic, MobilityNeedsMode 1 = mobility
        // needs provided by the EVCC. `Connector` follows the vehicle's inlet
        // ([`Ev::dc_connector`]) and is matched separately.
        parameter_filter: &[("ControlMode", 2), ("MobilityNeedsMode", 1)],
    };
}

/// A `RationalNumber` (value * 10^exponent) in volts, for comparisons.
fn rational_to_volts(r: iso15118_20::common_types::RationalNumber) -> f32 {
    f32::from(r.value) * pow10(r.exponent)
}

fn pow10(exp: i8) -> f32 {
    let mut v = 1.0f32;
    for _ in 0..exp.unsigned_abs() {
        v = if exp < 0 { v / 10.0 } else { v * 10.0 };
    }
    v
}

/// Service id of DC energy transfer (ISO 15118-20:2022 Table 204, V2G20-1355).
pub const SERVICE_ID_DC: u16 = 2;

/// Host-provided memory. Nothing is allocated by the core.
pub struct Buffers<'a> {
    /// Receives V2GTP frames; must hold the largest expected frame (header + payload).
    pub rx: &'a mut [u8],
    /// Holds the one outgoing frame.
    pub tx: &'a mut [u8],
    /// Scratch for decoded messages, reset for every received frame.
    pub arena: &'a mut [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    AwaitSupportedAppProtocolRes,
    AwaitSessionSetupRes,
    AwaitAuthorizationSetupRes,
    AwaitAuthorizationRes,
    AwaitServiceDiscoveryRes,
    AwaitServiceDetailRes,
    AwaitServiceSelectionRes,
    AwaitChargeParameterDiscoveryRes,
    AwaitScheduleExchangeRes,
    AwaitCableCheckRes,
    AwaitPreChargeRes,
    /// Pre-charge is finished; the host closes the contactors and reports
    /// [`Event::ContactorsClosed`].
    AwaitContactors,
    /// `PowerDeliveryReq(Start)` is out.
    AwaitPowerDeliveryRes,
    /// Energy is flowing: `DC_ChargeLoopReq`s go out in a cycle.
    AwaitChargeLoopRes,
    /// Shutting down: the host stops drawing current and reports
    /// [`Event::EnergyTransferStopped`].
    AwaitCurrentStopped,
    /// `PowerDeliveryReq(Stop)` is out.
    AwaitPowerDeliveryStopRes,
    AwaitSessionStopRes,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The flow implemented so far finished normally.
    Completed,
    /// The EVSE rejected or did not accept our protocol offer.
    NoCommonProtocol,
    /// The EVSE offers no authorization service this EV can use (only EIM is implemented).
    NoCompatibleAuthorization,
    /// The EVSE does not offer the energy transfer service this EV wants.
    NoCompatibleService,
    /// None of the EVSE's parameter sets for the service matches `Config::parameter_filter`.
    NoCompatibleParameterSet,
    /// The EVSE answered with a failing response code.
    Rejected(ResponseCode),
    Timeout,
    /// Bad V2GTP framing, or a frame larger than the RX buffer.
    Framing,
    Malformed(DecodeError),
    /// A message the current state does not expect.
    UnexpectedMessage,
    /// The TX buffer cannot hold the outgoing message.
    TxBufferTooSmall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The transport is connected. `secured` is true for TLS.
    Start { secured: bool },
    /// `n` bytes were written into [`Session::rx_space`].
    Received(usize),
    TimerExpired(Timer),
    /// The host closed the vehicle's contactors after [`Notice::CloseContactors`].
    ContactorsClosed,
    /// The vehicle wants to stop charging (user, full battery, fault). Only acted on while the
    /// charge loop runs.
    StopRequested,
    /// The host reduced the current to zero after [`Notice::StopEnergyTransfer`]
    /// (V2G20-1310 asks for this before `PowerDeliveryReq(Stop)`).
    EnergyTransferStopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// The EVSE accepted the session; `session_id` is the id it assigned.
    SessionSetUp { session_id: [u8; 8] },
    /// Authorization finished successfully.
    Authorized,
    /// The EVSE accepted the service selection.
    ServiceSelected { service_id: u16, parameter_set_id: u16 },
    /// The EVSE's DC limits, from `DC_ChargeParameterDiscoveryRes`.
    ChargeParametersDiscovered(dc::DcChargeParameterDiscoveryRes),
    /// The EVSE finished the schedule exchange.
    ScheduleExchanged,
    /// The cable check finished successfully.
    CableChecked,
    /// Pre-charge is finished: close the EV's contactors, then send
    /// [`Event::ContactorsClosed`] (IEC 61851-23 t9).
    CloseContactors,
    /// The EVSE enabled the energy transfer: the vehicle may draw current.
    EnergyTransferStarted,
    /// What the EVSE reported in the latest `DC_ChargeLoopRes`.
    ChargeLoop(ChargeLoopStatus),
    /// Stop drawing current (ramp down), then report [`Event::EnergyTransferStopped`]. When the
    /// EVSE asked for the stop, `within_s` is its `NotificationMaxDelay` (V2G20-1416).
    StopEnergyTransfer { within_s: Option<u16> },
    /// The EVSE acknowledged the stop: the contactors may open.
    OpenContactors,
}

/// The EVSE's side of one charge loop exchange (Dynamic control mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeLoopStatus {
    pub present_current: iso15118_20::common_types::RationalNumber,
    pub present_voltage: iso15118_20::common_types::RationalNumber,
    pub power_limit_achieved: bool,
    pub current_limit_achieved: bool,
    pub voltage_limit_achieved: bool,
    pub maximum_charge_power: iso15118_20::common_types::RationalNumber,
    pub maximum_charge_current: iso15118_20::common_types::RationalNumber,
    pub maximum_voltage: iso15118_20::common_types::RationalNumber,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// Length of the framed message at the start of the TX buffer to write to the stream.
    pub send: Option<usize>,
    pub timers: Timers,
    pub notice: Option<Notice>,
    /// Set when the session is over; the host closes the connection.
    pub end: Option<EndReason>,
}

pub struct Session<'a, E: Ev, L: LogSink> {
    rx: &'a mut [u8],
    rx_len: usize,
    tx: &'a mut [u8],
    arena: Arena<'a>,
    m: Machine<E, L>,
}

struct Machine<E, L> {
    config: Config,
    ev: E,
    log: L,
    state: State,
    secured: bool,
    session_id: [u8; 8],
    /// The `Ongoing` timer is armed once, when the first `Ongoing` response arrives.
    ongoing_armed: bool,
    /// A poll pause is running; when it ends the request of the current state is repeated.
    poll_pending: bool,
    /// The last `DC_PreChargeReq` carried `EVProcessing = Finished`.
    precharge_finishing: bool,
    /// The vehicle asked to stop while a charge loop request was outstanding.
    stop_requested: bool,
    /// SECC time minus the vehicle's clock, learned from `SessionSetupRes` (V2G20-1536).
    clock_offset: i128,
    last_timestamp: u64,
    selected: SelectedService,
}

impl<'a, E: Ev, L: LogSink> Session<'a, E, L> {
    pub fn new(config: Config, ev: E, log: L, buffers: Buffers<'a>) -> Self {
        Self {
            rx: buffers.rx,
            rx_len: 0,
            tx: buffers.tx,
            arena: Arena::new(buffers.arena),
            m: Machine {
                config,
                ev,
                log,
                state: State::Idle,
                secured: false,
                session_id: [0; 8],
                ongoing_armed: false,
                poll_pending: false,
                precharge_finishing: false,
                stop_requested: false,
                clock_offset: 0,
                last_timestamp: 0,
                selected: SelectedService { service_id: 0, parameter_set_id: 0 },
            },
        }
    }

    /// Free space to read the next bytes from the stream into.
    pub fn rx_space(&mut self) -> &mut [u8] {
        &mut self.rx[self.rx_len..]
    }

    /// The outgoing message buffer; valid up to `Step::send` after a step.
    pub fn tx(&self) -> &[u8] {
        self.tx
    }

    pub fn state(&self) -> State {
        self.m.state
    }

    pub fn handle(&mut self, event: Event) -> Step {
        let Self { rx, rx_len, tx, arena, m } = self;
        if m.state == State::Ended {
            return Step::default();
        }
        match event {
            Event::Start { secured } => {
                if m.state != State::Idle {
                    return Step::default();
                }
                m.secured = secured;
                m.log.record(&Record::Started { secured });
                m.send_supported_app_protocol_req(tx)
            }
            Event::TimerExpired(timer) => match (timer, m.state) {
                (Timer::Response, s) if m.is_awaiting(s) => m.end(EndReason::Timeout),
                (
                    Timer::Ongoing,
                    State::AwaitAuthorizationRes
                    | State::AwaitScheduleExchangeRes
                    | State::AwaitCableCheckRes
                    | State::AwaitPreChargeRes,
                ) => m.end(EndReason::Timeout),
                (Timer::Poll, _) if m.poll_pending => {
                    m.poll_pending = false;
                    m.repeat(tx)
                }
                // A timer that was cancelled in the same moment it fired.
                _ => Step::default(),
            },
            Event::ContactorsClosed => {
                if m.state != State::AwaitContactors {
                    return Step::default();
                }
                m.send_power_delivery(common::ChargeProgress::Start, tx)
            }
            Event::StopRequested => match m.state {
                State::AwaitChargeLoopRes if m.poll_pending => {
                    m.poll_pending = false;
                    m.begin_stop(None)
                }
                State::AwaitChargeLoopRes => {
                    m.stop_requested = true;
                    Step::default()
                }
                _ => Step::default(),
            },
            Event::EnergyTransferStopped => {
                if m.state != State::AwaitCurrentStopped {
                    return Step::default();
                }
                m.send_power_delivery(common::ChargeProgress::Stop, tx)
            }
            Event::Received(n) => {
                *rx_len = (*rx_len + n).min(rx.len());
                let mut step = Step::default();
                loop {
                    let consumed = match parse(&rx[..*rx_len], rx.len() - HEADER_LEN) {
                        Ok(Parse::Incomplete { .. }) => break,
                        Ok(Parse::Frame(frame)) => {
                            arena.reset();
                            m.log.record(&Record::RxPayload { payload_type: frame.payload_type.0, payload: frame.payload });
                            step = m.on_frame(frame.payload_type, frame.payload, arena, tx);
                            frame.consumed
                        }
                        Err(_) => return m.end(EndReason::Framing),
                    };
                    rx.copy_within(consumed..*rx_len, 0);
                    *rx_len -= consumed;
                    if step.end.is_some() {
                        break;
                    }
                }
                // A full buffer that still holds no complete frame can never make progress.
                if step.end.is_none() && *rx_len == rx.len() {
                    return m.end(EndReason::Framing);
                }
                step
            }
        }
    }
}

/// The `OK*` response codes (ISO 15118-20:2022 Annex A, response code table): which message
/// may carry which is checked by the EVSE side; the EVCC accepts all of them.
pub(crate) fn is_ok(code: ResponseCode) -> bool {
    matches!(
        code,
        ResponseCode::Ok
            | ResponseCode::OkCertificateExpiresSoon
            | ResponseCode::OkNewSessionEstablished
            | ResponseCode::OkOldSessionJoined
            | ResponseCode::OkPowerToleranceConfirmed
    )
}

impl<E: Ev, L: LogSink> Machine<E, L> {
    fn is_awaiting(&self, state: State) -> bool {
        matches!(
            state,
            State::AwaitSupportedAppProtocolRes
                | State::AwaitSessionSetupRes
                | State::AwaitAuthorizationSetupRes
                | State::AwaitAuthorizationRes
                | State::AwaitServiceDiscoveryRes
                | State::AwaitServiceDetailRes
                | State::AwaitServiceSelectionRes
                | State::AwaitChargeParameterDiscoveryRes
                | State::AwaitScheduleExchangeRes
                | State::AwaitCableCheckRes
                | State::AwaitPreChargeRes
                | State::AwaitPowerDeliveryRes
                | State::AwaitChargeLoopRes
                | State::AwaitPowerDeliveryStopRes
                | State::AwaitSessionStopRes
        )
    }

    fn goto(&mut self, to: State) {
        let from = core::mem::replace(&mut self.state, to);
        self.log.record(&Record::StateChange { from, to });
    }

    fn end(&mut self, reason: EndReason) -> Step {
        self.goto(State::Ended);
        self.log.record(&Record::Ended { reason });
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Cancel(Timer::Ongoing)),
            end: Some(reason),
            ..Step::default()
        }
    }

    /// The header `TimeStamp`: microseconds in SECC time (V2G20-1534), increasing with every
    /// message (V2G20-1537). Until the EVSE's clock is known (its `SessionSetupRes`) it is the
    /// vehicle's own best guess of UTC, or zero (V2G20-1535).
    fn timestamp(&mut self) -> u64 {
        let local = i128::from(self.ev.now_micros());
        let t = u64::try_from((local + self.clock_offset).max(0)).unwrap_or(u64::MAX);
        let t = t.max(self.last_timestamp.saturating_add(1));
        self.last_timestamp = t;
        t
    }

    fn header(&mut self) -> MessageHeader {
        MessageHeader { session_id: self.session_id, time_stamp: self.timestamp() }
    }

    /// Records the transmit and arms the response timer for a message framed by [`frame`].
    fn sent(&mut self, msg: MsgKind, framed: Result<usize, ()>) -> Step {
        let Ok(len) = framed else { return self.end(EndReason::TxBufferTooSmall) };
        self.log.record(&Record::MsgTx { msg });
        Step {
            send: Some(len),
            timers: Timers::one(TimerCmd::Arm {
                timer: Timer::Response,
                ms: if msg == MsgKind::ServiceDetailReq {
                    self.config.service_detail_timeout_ms
                } else if msg == MsgKind::DcChargeLoopReq {
                    self.config.charge_loop_timeout_ms
                } else {
                    self.config.response_timeout_ms
                },
            }),
            ..Step::default()
        }
    }

    /// Sends `msg` and, unless the send failed, moves to `next`.
    fn send_then(&mut self, next: State, msg: MsgKind, framed: Result<usize, ()>) -> Step {
        let step = self.sent(msg, framed);
        if step.end.is_none() {
            self.goto(next);
        }
        step
    }

    fn send_supported_app_protocol_req(&mut self, tx: &mut [u8]) -> Step {
        let offer = [self.config.offer];
        let msg = SupportedAppProtocolReq { app_protocol: &offer };
        let framed = frame(tx, PayloadType::SAP, |out| msg.encode(out));
        self.send_then(State::AwaitSupportedAppProtocolRes, MsgKind::SupportedAppProtocolReq, framed)
    }

    fn on_frame(&mut self, payload_type: PayloadType, payload: &[u8], arena: &Arena<'_>, tx: &mut [u8]) -> Step {
        use common::Message as M;
        match self.state {
            State::AwaitSupportedAppProtocolRes if payload_type == PayloadType::SAP => {
                match app_protocol::decode(payload, arena) {
                    Ok(app_protocol::Message::SupportedAppProtocolRes(res)) => {
                        self.log.record(&Record::MsgRx { msg: MsgKind::SupportedAppProtocolRes });
                        self.on_supported_app_protocol_res(res, tx)
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitSessionSetupRes if payload_type == PayloadType::MAINSTREAM => {
                match common::decode(payload, arena) {
                    Ok(M::SessionSetupRes(res)) => {
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
                        step
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitAuthorizationSetupRes if payload_type == PayloadType::MAINSTREAM => {
                match common::decode(payload, arena) {
                    Ok(M::AuthorizationSetupRes(res)) => {
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
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitAuthorizationRes if payload_type == PayloadType::MAINSTREAM => {
                match common::decode(payload, arena) {
                    Ok(M::AuthorizationRes(res)) => {
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
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitServiceDiscoveryRes if payload_type == PayloadType::MAINSTREAM => {
                match common::decode(payload, arena) {
                    Ok(M::ServiceDiscoveryRes(res)) => {
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
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitServiceDetailRes if payload_type == PayloadType::MAINSTREAM => {
                match common::decode(payload, arena) {
                    Ok(M::ServiceDetailRes(res)) => {
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
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitServiceSelectionRes if payload_type == PayloadType::MAINSTREAM => {
                match common::decode(payload, arena) {
                    Ok(M::ServiceSelectionRes(res)) => {
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
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitChargeParameterDiscoveryRes if payload_type == PayloadType::DC_MAINSTREAM => {
                match dc::decode(payload, arena) {
                    Ok(dc::Message::DcChargeParameterDiscoveryRes(res)) => {
                        if let Some(end) = self.received(MsgKind::DcChargeParameterDiscoveryRes, res.response_code) {
                            return end;
                        }
                        let mut step = self.send_schedule_exchange_req(tx);
                        if step.end.is_none() {
                            step.notice = Some(Notice::ChargeParametersDiscovered(res));
                        }
                        step
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitScheduleExchangeRes if payload_type == PayloadType::MAINSTREAM => {
                match common::decode(payload, arena) {
                    Ok(M::ScheduleExchangeRes(res)) => {
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
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitCableCheckRes if payload_type == PayloadType::DC_MAINSTREAM => {
                match dc::decode(payload, arena) {
                    Ok(dc::Message::DcCableCheckRes(res)) => {
                        if let Some(end) = self.received(MsgKind::DcCableCheckRes, res.response_code) {
                            return end;
                        }
                        self.log.record(&Record::EvseProcessing { processing: res.evse_processing });
                        match res.evse_processing {
                            // V2G20-1397: wait for ResponseCode OK and EVSEProcessing Finished.
                            Processing::Finished => {
                                self.ongoing_armed = false;
                                let mut step = self.send_precharge_req(false, tx);
                                if step.end.is_none() {
                                    step.notice = Some(Notice::CableChecked);
                                }
                                step
                            }
                            _ => self.ongoing_repeat(tx),
                        }
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitPreChargeRes if payload_type == PayloadType::DC_MAINSTREAM => {
                match dc::decode(payload, arena) {
                    Ok(dc::Message::DcPreChargeRes(res)) => {
                        if let Some(end) = self.received(MsgKind::DcPreChargeRes, res.response_code) {
                            return end;
                        }
                        if self.precharge_finishing {
                            // The EVSE acknowledged the end of pre-charge: close the contactors.
                            self.precharge_finishing = false;
                            self.ongoing_armed = false;
                            self.goto(State::AwaitContactors);
                            return Step {
                                timers: Timers::one(TimerCmd::Cancel(Timer::Response))
                                    .with(TimerCmd::Cancel(Timer::Ongoing)),
                                notice: Some(Notice::CloseContactors),
                                ..Step::default()
                            };
                        }
                        let inlet = rational_to_volts(self.ev.inlet_voltage());
                        let battery = rational_to_volts(self.ev.battery_voltage());
                        if (inlet - battery).abs() < f32::from(self.config.precharge_tolerance_v) {
                            self.precharge_finishing = true;
                            return self.send_precharge_req(true, tx);
                        }
                        self.ongoing_repeat(tx)
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitPowerDeliveryRes if payload_type == PayloadType::MAINSTREAM => {
                match common::decode(payload, arena) {
                    Ok(M::PowerDeliveryRes(res)) => {
                        if let Some(end) = self.received(MsgKind::PowerDeliveryRes, res.response_code) {
                            return end;
                        }
                        // V2G20-1615/1616: with OK the energy transfer starts; the charge loop
                        // begins at once.
                        let mut step = self.send_charge_loop_req(tx);
                        if step.end.is_none() {
                            step.notice = Some(Notice::EnergyTransferStarted);
                        }
                        step
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitChargeLoopRes if payload_type == PayloadType::DC_MAINSTREAM => {
                match dc::decode(payload, arena) {
                    Ok(dc::Message::DcChargeLoopRes(res)) => {
                        if let Some(end) = self.received(MsgKind::DcChargeLoopRes, res.response_code) {
                            return end;
                        }
                        let dc::ClResControlModeChoice::DynamicDc(mode) = res.cl_res_control_mode else {
                            // We asked for Dynamic control.
                            return self.end(EndReason::UnexpectedMessage);
                        };
                        let status = ChargeLoopStatus {
                            present_current: res.evse_present_current,
                            present_voltage: res.evse_present_voltage,
                            power_limit_achieved: res.evse_power_limit_achieved,
                            current_limit_achieved: res.evse_current_limit_achieved,
                            voltage_limit_achieved: res.evse_voltage_limit_achieved,
                            maximum_charge_power: mode.evse_maximum_charge_power,
                            maximum_charge_current: mode.evse_maximum_charge_current,
                            maximum_voltage: mode.evse_maximum_voltage,
                        };
                        // V2G20-1416, V2G20-1960: Terminate means shut down and stop the session.
                        // The other notifications (pause, renegotiation, metering) are not
                        // handled yet (TODO) and are ignored.
                        let terminate = res.evse_status.filter(|s| {
                            s.evse_notification == iso15118_20::common_types::EvseNotification::Terminate
                        });
                        if let Some(status) = terminate {
                            return self.begin_stop(Some(status.notification_max_delay));
                        }
                        if core::mem::take(&mut self.stop_requested) {
                            return self.begin_stop(None);
                        }
                        let mut step = self.pace_charge_loop(tx);
                        step.notice = Some(Notice::ChargeLoop(status));
                        step
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitPowerDeliveryStopRes if payload_type == PayloadType::MAINSTREAM => {
                match common::decode(payload, arena) {
                    Ok(M::PowerDeliveryRes(res)) => {
                        if let Some(end) = self.received(MsgKind::PowerDeliveryRes, res.response_code) {
                            return end;
                        }
                        // The energy flow is over: the contactors may open, the session ends.
                        let msg = common::SessionStopReq {
                            header: self.header(),
                            charging_session: common::ChargingSession::Terminate,
                            ev_termination_code: None,
                            ev_termination_explanation: None,
                        };
                        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
                        let mut step = self.send_then(State::AwaitSessionStopRes, MsgKind::SessionStopReq, framed);
                        if step.end.is_none() {
                            step.notice = Some(Notice::OpenContactors);
                        }
                        step
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitSessionStopRes if payload_type == PayloadType::MAINSTREAM => {
                match common::decode(payload, arena) {
                    Ok(M::SessionStopRes(res)) => {
                        if let Some(end) = self.received(MsgKind::SessionStopRes, res.response_code) {
                            return end;
                        }
                        // V2G20-1463: after SessionStopRes OK the EVCC terminates the connection.
                        self.end(EndReason::Completed)
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            _ => self.end(EndReason::UnexpectedMessage),
        }
    }

    /// The EVSE answered `Ongoing`: pause for the poll interval (if any), then repeat the
    /// request of the current state.
    fn ongoing_repeat(&mut self, tx: &mut [u8]) -> Step {
        if self.config.poll_interval_ms == 0 {
            return self.repeat(tx);
        }
        self.poll_pending = true;
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Response))
                .with(TimerCmd::Arm { timer: Timer::Poll, ms: self.config.poll_interval_ms }),
            ..Step::default()
        }
    }

    /// Sends the request of the current state again and makes sure the `Ongoing` bound runs.
    fn repeat(&mut self, tx: &mut [u8]) -> Step {
        let step = match self.state {
            State::AwaitAuthorizationRes => self.send_authorization_req(State::AwaitAuthorizationRes, tx),
            State::AwaitScheduleExchangeRes => self.send_schedule_exchange_req(tx),
            State::AwaitCableCheckRes => self.send_cable_check_req(tx),
            State::AwaitPreChargeRes => self.send_precharge_req(false, tx),
            // The charge loop has its own cycle and no `Ongoing` bound.
            State::AwaitChargeLoopRes => return self.send_charge_loop_req(tx),
            _ => return Step::default(),
        };
        self.arm_ongoing(step)
    }

    fn send_cable_check_req(&mut self, tx: &mut [u8]) -> Step {
        let msg = dc::DcCableCheckReq { header: self.header() };
        let framed = frame(tx, PayloadType::DC_MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitCableCheckRes {
            return self.sent(MsgKind::DcCableCheckReq, framed);
        }
        let mut step = self.send_then(State::AwaitCableCheckRes, MsgKind::DcCableCheckReq, framed);
        // V2G20-700: the cable check timer starts with the first request.
        if step.end.is_none() {
            self.ongoing_armed = true;
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Ongoing, ms: self.config.cable_check_timeout_ms });
        }
        step
    }

    /// One `DC_PreChargeReq`; `finished` is the EV's `EVProcessing`.
    fn send_precharge_req(&mut self, finished: bool, tx: &mut [u8]) -> Step {
        let battery = self.ev.battery_voltage();
        let msg = dc::DcPreChargeReq {
            header: self.header(),
            ev_processing: if finished { Processing::Finished } else { Processing::Ongoing },
            ev_present_voltage: battery,
            ev_target_voltage: battery,
        };
        let framed = frame(tx, PayloadType::DC_MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitPreChargeRes {
            return self.sent(MsgKind::DcPreChargeReq, framed);
        }
        let mut step = self.send_then(State::AwaitPreChargeRes, MsgKind::DcPreChargeReq, framed);
        // V2G20-704: the pre-charge timer starts with the first request (arming replaces the
        // cable check timer).
        if step.end.is_none() {
            self.ongoing_armed = true;
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Ongoing, ms: self.config.precharge_timeout_ms });
        }
        step
    }

    fn send_charge_loop_req(&mut self, tx: &mut [u8]) -> Step {
        let r = self.ev.schedule_request();
        let l = self.ev.dc_limits();
        let msg = dc::DcChargeLoopReq {
            header: self.header(),
            display_parameters: None,
            meter_info_requested: false,
            ev_present_voltage: self.ev.battery_voltage(),
            cl_req_control_mode: dc::ClReqControlModeChoice::DynamicDc(dc::DynamicDcClReqControlMode {
                departure_time: None,
                ev_target_energy_request: r.target_energy,
                ev_maximum_energy_request: r.maximum_energy,
                ev_minimum_energy_request: r.minimum_energy,
                ev_maximum_charge_power: l.maximum_charge_power,
                ev_minimum_charge_power: l.minimum_charge_power,
                ev_maximum_charge_current: l.maximum_charge_current,
                ev_maximum_voltage: l.maximum_voltage,
                ev_minimum_voltage: l.minimum_voltage,
            }),
        };
        let framed = frame(tx, PayloadType::DC_MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitChargeLoopRes {
            return self.sent(MsgKind::DcChargeLoopReq, framed);
        }
        self.send_then(State::AwaitChargeLoopRes, MsgKind::DcChargeLoopReq, framed)
    }

    /// After a `DC_ChargeLoopRes`: wait the loop interval, then send the next request.
    fn pace_charge_loop(&mut self, tx: &mut [u8]) -> Step {
        if self.config.charge_loop_interval_ms == 0 {
            return self.send_charge_loop_req(tx);
        }
        self.poll_pending = true;
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Response))
                .with(TimerCmd::Arm { timer: Timer::Poll, ms: self.config.charge_loop_interval_ms }),
            ..Step::default()
        }
    }

    /// Starts the shutdown: the host ramps the current down first (V2G20-1310).
    fn begin_stop(&mut self, within_s: Option<u16>) -> Step {
        self.goto(State::AwaitCurrentStopped);
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Cancel(Timer::Poll)),
            notice: Some(Notice::StopEnergyTransfer { within_s }),
            ..Step::default()
        }
    }

    fn send_power_delivery(&mut self, progress: common::ChargeProgress, tx: &mut [u8]) -> Step {
        let r = self.ev.schedule_request();
        let limits = self.ev.dc_limits();
        // A single-entry profile at the vehicle's maximum charge power until departure: the
        // "fastest charging" simulation the standard asks for is simplified (TODO).
        let entries = [common::PowerScheduleEntry {
            duration: r.departure_time,
            power: limits.maximum_charge_power,
            power_l2: None,
            power_l3: None,
        }];
        let time_anchor = self.timestamp();
        let msg = common::PowerDeliveryReq {
            header: self.header(),
            ev_processing: Processing::Finished,
            charge_progress: progress,
            ev_power_profile: Some(common::EvPowerProfile {
                time_anchor,
                evppt_control_mode: common::EvpptControlModeChoice::Dynamic(common::DynamicEvpptControlMode {}),
                ev_power_profile_entries: common::EvPowerProfileEntryList { ev_power_profile_entry: &entries },
            }),
            bpt_channel_selection: None,
        };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        let next = if progress == common::ChargeProgress::Stop {
            State::AwaitPowerDeliveryStopRes
        } else {
            State::AwaitPowerDeliveryRes
        };
        self.send_then(next, MsgKind::PowerDeliveryReq, framed)
    }

    /// Arms the overall `Ongoing` bound once, when the first `Ongoing` answer arrives.
    fn arm_ongoing(&mut self, mut step: Step) -> Step {
        if step.end.is_none() && !self.ongoing_armed {
            self.ongoing_armed = true;
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Ongoing, ms: self.config.ongoing_timeout_ms });
        }
        step
    }

    fn send_schedule_exchange_req(&mut self, tx: &mut [u8]) -> Step {
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
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitScheduleExchangeRes {
            return self.sent(MsgKind::ScheduleExchangeReq, framed);
        }
        self.send_then(State::AwaitScheduleExchangeRes, MsgKind::ScheduleExchangeReq, framed)
    }

    /// Logs a received response and its code; returns the ending step if the code is a failure.
    fn received(&mut self, msg: MsgKind, code: ResponseCode) -> Option<Step> {
        self.log.record(&Record::MsgRx { msg });
        self.log.record(&Record::Response { msg, code });
        if is_ok(code) { None } else { Some(self.end(EndReason::Rejected(code))) }
    }

    fn pick_parameter_set(&self, sets: &[ParameterSet<'_>]) -> Option<u16> {
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
    }

    fn send_authorization_req(&mut self, next: State, tx: &mut [u8]) -> Step {
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

    fn malformed(&mut self, error: DecodeError) -> Step {
        self.log.record(&Record::DecodeFailed { state: self.state, error });
        self.end(EndReason::Malformed(error))
    }

    fn on_supported_app_protocol_res(&mut self, res: app_protocol::SupportedAppProtocolRes, tx: &mut [u8]) -> Step {
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
}

/// Encodes a payload behind a V2GTP header at the start of `tx`; returns the framed length.
fn frame(
    tx: &mut [u8],
    payload_type: PayloadType,
    encode: impl FnOnce(&mut [u8]) -> Result<usize, exi::EncodeError>,
) -> Result<usize, ()> {
    let n = encode(tx.get_mut(HEADER_LEN..).ok_or(())?).map_err(|_| ())?;
    write_header(tx, payload_type, n).map_err(|_| ())?;
    Ok(HEADER_LEN + n)
}
