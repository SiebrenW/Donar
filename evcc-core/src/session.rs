//! The V2G session machine: SupportedAppProtocol handshake, SessionSetup, then EIM
//! authorization. (More of the DC dynamic flow is added slice by slice.)
//!
//! Host contract:
//! 1. Open the TCP/TLS connection found by SDP, then send [`Event::Start`].
//! 2. Read from the socket straight into [`Session::rx_space`], then send
//!    [`Event::Received`] with the byte count.
//! 3. After every call, carry out the returned [`Step`]: write `tx[..send]` to the stream,
//!    apply the timer commands, handle `notice`, and close the connection once `end` is set.
//! 4. When an armed timer fires, send [`Event::TimerExpired`] naming it.

use iso15118_20::app_protocol::{self, AppProtocol, SupportedAppProtocolReq};
use iso15118_20::common::{
    self, Authorization, AuthorizationReq, AuthorizationSetupReq, MessageHeader, Processing, ResponseCode,
    SessionSetupReq,
};
use iso15118_20::{Arena, DecodeError};
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use crate::ev::Ev;
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd, Timers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The protocol offered in `SupportedAppProtocolReq`.
    pub offer: AppProtocol<'static>,
    /// How long to wait for any response. 2 s is the SupportedAppProtocolReq timeout used by
    /// EcoG's stack (requirement id: TODO, not yet identified). It is reused for every other
    /// message until their individual ISO 15118-20:2022 timeouts are looked up.
    pub response_timeout_ms: u32,
    /// Upper bound on a repeated `Ongoing` exchange such as authorization polling. 60 s is
    /// EcoG's `V2G_EVCC_ONGOING_TIMEOUT` (requirement id: TODO, not yet identified).
    pub ongoing_timeout_ms: u32,
}

impl Config {
    /// ISO 15118-20 DC, version 1.0, schema id 1, top priority.
    pub const DC_20: Config = Config {
        offer: AppProtocol {
            namespace: "urn:iso:std:iso:15118:-20:DC",
            version_major: 1,
            version_minor: 0,
            schema_id: 1,
            priority: 1,
        },
        response_timeout_ms: 2000,
        ongoing_timeout_ms: 60_000,
    };
}

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
    /// Authorized; the rest of the flow is added with the later slices.
    Authorized,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// The EVSE accepted the session; `session_id` is the id it assigned.
    SessionSetUp { session_id: [u8; 8] },
    /// Authorization finished successfully.
    Authorized,
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
                (Timer::Ongoing, State::AwaitAuthorizationRes) => m.end(EndReason::Timeout),
                // A timer that was cancelled in the same moment it fired.
                _ => Step::default(),
            },
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

pub(crate) fn is_ok(code: ResponseCode) -> bool {
    matches!(code, ResponseCode::Ok | ResponseCode::OkNewSessionEstablished | ResponseCode::OkOldSessionJoined)
}

impl<E: Ev, L: LogSink> Machine<E, L> {
    fn is_awaiting(&self, state: State) -> bool {
        matches!(
            state,
            State::AwaitSupportedAppProtocolRes
                | State::AwaitSessionSetupRes
                | State::AwaitAuthorizationSetupRes
                | State::AwaitAuthorizationRes
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

    fn header(&self) -> MessageHeader {
        MessageHeader { session_id: self.session_id, timestamp: self.ev.header_timestamp() }
    }

    /// Records the transmit and arms the response timer for a message framed by [`frame`].
    fn sent(&mut self, msg: MsgKind, framed: Result<usize, ()>) -> Step {
        let Ok(len) = framed else { return self.end(EndReason::TxBufferTooSmall) };
        self.log.record(&Record::MsgTx { msg });
        Step {
            send: Some(len),
            timers: Timers::one(TimerCmd::Arm { timer: Timer::Response, ms: self.config.response_timeout_ms }),
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
        let framed = frame(tx, PayloadType::SAP, |out| {
            app_protocol::encode_req(&SupportedAppProtocolReq { app_protocols: &offer }, out)
        });
        self.send_then(State::AwaitSupportedAppProtocolRes, MsgKind::SupportedAppProtocolReq, framed)
    }

    fn on_frame(&mut self, payload_type: PayloadType, payload: &[u8], arena: &Arena<'_>, tx: &mut [u8]) -> Step {
        use common::Message as M;
        match self.state {
            State::AwaitSupportedAppProtocolRes if payload_type == PayloadType::SAP => {
                match app_protocol::decode(payload, arena) {
                    Ok(app_protocol::Message::Res(res)) => {
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
                        self.log.record(&Record::SessionSetUp { session_id: self.session_id, evse_id: res.evse_id });
                        let framed = frame(tx, PayloadType::MAINSTREAM, |out| {
                            common::encode_authorization_setup_req(&AuthorizationSetupReq { header: self.header() }, out)
                        });
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
                        if !res.authorization_services.as_slice().contains(&Authorization::Eim) {
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
                                self.goto(State::Authorized);
                                Step {
                                    timers: Timers::one(TimerCmd::Cancel(Timer::Response))
                                        .with(TimerCmd::Cancel(Timer::Ongoing)),
                                    notice: Some(Notice::Authorized),
                                    ..Step::default()
                                }
                            }
                            // Ref: ISO 15118-20:2022 V2G20-1582 (as cited by EcoG, unchecked):
                            // while EVSEProcessing is Ongoing the EVCC sends another AuthorizationReq,
                            // unaltered except for the timestamp. The same is done for
                            // Ongoing_WaitingForCustomerInteraction, which EcoG does not distinguish
                            // (TODO: check the standard for any difference, e.g. pacing).
                            Processing::Ongoing | Processing::OngoingWaitingForCustomerInteraction => {
                                let mut step = self.send_authorization_req(State::AwaitAuthorizationRes, tx);
                                if step.end.is_none() && !self.ongoing_armed {
                                    self.ongoing_armed = true;
                                    step.timers =
                                        step.timers.with(TimerCmd::Arm { timer: Timer::Ongoing, ms: self.config.ongoing_timeout_ms });
                                }
                                step
                            }
                        }
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            _ => self.end(EndReason::UnexpectedMessage),
        }
    }

    fn send_authorization_req(&mut self, next: State, tx: &mut [u8]) -> Step {
        // EIM is the only authorization mode implemented, so the request carries no data
        // beyond the header; a repeat is therefore identical except for its timestamp.
        let msg = AuthorizationReq { header: self.header() };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| common::encode_authorization_req(&msg, out));
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
        use app_protocol::ResponseCode as R;
        if res.response_code == R::FailedNoNegotiation {
            return self.end(EndReason::NoCommonProtocol);
        }
        // The EVSE must pick the schema id we offered.
        if res.schema_id != Some(self.config.offer.schema_id) {
            return self.end(EndReason::NoCommonProtocol);
        }
        let msg = SessionSetupReq {
            header: MessageHeader { session_id: [0; 8], timestamp: self.ev.header_timestamp() },
            evcc_id: self.ev.evcc_id(),
        };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| common::encode_session_setup_req(&msg, out));
        self.send_then(State::AwaitSessionSetupRes, MsgKind::SessionSetupReq, framed)
    }
}

/// Encodes a payload behind a V2GTP header at the start of `tx`; returns the framed length.
fn frame(
    tx: &mut [u8],
    payload_type: PayloadType,
    encode: impl FnOnce(&mut [u8]) -> Result<usize, iso15118_20::EncodeError>,
) -> Result<usize, ()> {
    let n = encode(tx.get_mut(HEADER_LEN..).ok_or(())?).map_err(|_| ())?;
    write_header(tx, payload_type, n).map_err(|_| ())?;
    Ok(HEADER_LEN + n)
}
