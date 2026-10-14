//! The V2G session machine: SupportedAppProtocol handshake, then SessionSetup. (More of the
//! DC dynamic EIM flow is added slice by slice.)
//!
//! Host contract:
//! 1. Open the TCP/TLS connection found by SDP, then send [`Event::Start`].
//! 2. Read from the socket straight into [`Session::rx_space`], then send
//!    [`Event::Received`] with the byte count.
//! 3. After every call, carry out the returned [`Step`]: write `tx[..send]` to the stream,
//!    apply the timer command, handle `notice`, and close the connection once `end` is set.
//! 4. When the armed timer fires, send [`Event::TimerExpired`].

use iso15118_20::app_protocol::{self, AppProtocol, SupportedAppProtocolReq};
use iso15118_20::common::{self, MessageHeader, ResponseCode, SessionSetupReq};
use iso15118_20::{Arena, DecodeError};
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use crate::TimerCmd;
use crate::ev::Ev;
use crate::log::{LogSink, MsgKind, Record};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The protocol offered in `SupportedAppProtocolReq`.
    pub offer: AppProtocol<'static>,
    /// How long to wait for any response. 2 s is the SupportedAppProtocolReq timeout used by
    /// EcoG's stack (requirement id: TODO, not yet identified). It is reused for every other
    /// message until their individual ISO 15118-20:2022 timeouts are looked up.
    pub response_timeout_ms: u32,
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
    SessionSetUp,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The flow implemented so far finished normally.
    Completed,
    /// The EVSE rejected or did not accept our protocol offer.
    NoCommonProtocol,
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
    TimerExpired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    SessionSetUp { session_id: [u8; 8] },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// Length of the framed message at the start of the TX buffer to write to the stream.
    pub send: Option<usize>,
    pub timer: TimerCmd,
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
}

impl<'a, E: Ev, L: LogSink> Session<'a, E, L> {
    pub fn new(config: Config, ev: E, log: L, buffers: Buffers<'a>) -> Self {
        Self {
            rx: buffers.rx,
            rx_len: 0,
            tx: buffers.tx,
            arena: Arena::new(buffers.arena),
            m: Machine { config, ev, log, state: State::Idle, secured: false, session_id: [0; 8] },
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
            Event::TimerExpired => match m.state {
                State::AwaitSupportedAppProtocolRes | State::AwaitSessionSetupRes => m.end(EndReason::Timeout),
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
    fn goto(&mut self, to: State) {
        let from = core::mem::replace(&mut self.state, to);
        self.log.record(&Record::StateChange { from, to });
    }

    fn end(&mut self, reason: EndReason) -> Step {
        self.goto(State::Ended);
        self.log.record(&Record::Ended { reason });
        Step { timer: TimerCmd::Cancel, end: Some(reason), ..Step::default() }
    }

    /// Records the transmit and arms the response timer for a message framed by [`frame`].
    fn sent(&mut self, msg: MsgKind, framed: Result<usize, ()>) -> Step {
        let Ok(len) = framed else { return self.end(EndReason::TxBufferTooSmall) };
        self.log.record(&Record::MsgTx { msg });
        Step { send: Some(len), timer: TimerCmd::Arm { ms: self.config.response_timeout_ms }, ..Step::default() }
    }

    fn send_supported_app_protocol_req(&mut self, tx: &mut [u8]) -> Step {
        let offer = [self.config.offer];
        let framed = frame(tx, PayloadType::SAP, |out| {
            app_protocol::encode_req(&SupportedAppProtocolReq { app_protocols: &offer }, out)
        });
        let step = self.sent(MsgKind::SupportedAppProtocolReq, framed);
        if step.end.is_none() {
            self.goto(State::AwaitSupportedAppProtocolRes);
        }
        step
    }

    fn on_frame(&mut self, payload_type: PayloadType, payload: &[u8], arena: &Arena<'_>, tx: &mut [u8]) -> Step {
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
                    Ok(common::Message::SessionSetupRes(res)) => {
                        self.log.record(&Record::MsgRx { msg: MsgKind::SessionSetupRes });
                        self.log.record(&Record::Response { msg: MsgKind::SessionSetupRes, code: res.response_code });
                        if !is_ok(res.response_code) {
                            return self.end(EndReason::Rejected(res.response_code));
                        }
                        self.session_id = res.header.session_id;
                        self.log.record(&Record::SessionSetUp { session_id: self.session_id, evse_id: res.evse_id });
                        self.goto(State::SessionSetUp);
                        // This slice ends here; the next one continues with authorization.
                        Step {
                            timer: TimerCmd::Cancel,
                            notice: Some(Notice::SessionSetUp { session_id: self.session_id }),
                            ..Step::default()
                        }
                    }
                    Ok(_) => self.end(EndReason::UnexpectedMessage),
                    Err(e) => self.malformed(e),
                }
            }
            _ => self.end(EndReason::UnexpectedMessage),
        }
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
        let step = self.sent(MsgKind::SessionSetupReq, framed);
        if step.end.is_none() {
            self.goto(State::AwaitSessionSetupRes);
        }
        step
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
