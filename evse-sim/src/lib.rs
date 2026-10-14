//! A simulated EVSE: the mirror image of the EV session machine, built from the same codecs.
//! It is sans-IO like the EV side ([`Evse::handle`]), with a small blocking TCP server
//! ([`serve_one`]) for end-to-end tests. Its knobs inject the faults an EVSE test platform
//! needs: rejections, failing response codes and silence.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{SystemTime, UNIX_EPOCH};

use iso15118_20::app_protocol::{self, ResponseCode as SapCode, SupportedAppProtocolRes};
use iso15118_20::common::{
    self, Authorization, AuthorizationRes, AuthorizationServices, AuthorizationSetupRes, MessageHeader, Processing,
    ResponseCode, SessionSetupRes,
};
use iso15118_20::{Arena, EncodeError};
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

const DC_NAMESPACE: &str = "urn:iso:std:iso:15118:-20:DC";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub evse_id: String,
    pub session_id: [u8; 8],
    /// Answer the SupportedAppProtocolReq with `FailedNoNegotiation` regardless of the offer.
    pub reject_protocol: bool,
    /// Response code for `SessionSetupRes`.
    pub setup_code: ResponseCode,
    /// Never answer a `SessionSetupReq` (the EV should time out).
    pub silent_on_setup: bool,
    /// How many `AuthorizationRes` carry `Ongoing` before one says `Finished`.
    pub authorization_ongoing_responses: u32,
    /// Never finish authorization: answer `Ongoing` forever (the EV's ongoing timeout should fire).
    pub authorization_never_finishes: bool,
    /// Response code of the `AuthorizationRes` that would finish authorization.
    pub authorization_code: ResponseCode,
    /// Authorization services offered in `AuthorizationSetupRes`.
    pub offered_authorization: Vec<Authorization>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            evse_id: "DE*SIM*E0001".into(),
            session_id: [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18],
            reject_protocol: false,
            setup_code: ResponseCode::OkNewSessionEstablished,
            silent_on_setup: false,
            authorization_ongoing_responses: 0,
            authorization_never_finishes: false,
            authorization_code: ResponseCode::Ok,
            offered_authorization: vec![Authorization::Eim],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    AwaitSupportedAppProtocolReq,
    AwaitSessionSetupReq,
    AwaitAuthorizationSetupReq,
    AwaitAuthorizationReq,
    Authorized,
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
}

impl Evse {
    pub fn new(config: Config) -> Self {
        Self { config, state: State::AwaitSupportedAppProtocolReq, rx: Vec::new(), schema_id: 0, authorization_reqs_seen: 0 }
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
            if close {
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
                let Ok(app_protocol::Message::Req(req)) = app_protocol::decode(payload, &arena) else { return closed };
                // Highest priority = lowest number among the protocols we speak.
                let pick = req
                    .app_protocols
                    .iter()
                    .filter(|p| p.namespace == DC_NAMESPACE && p.version_major == 1)
                    .min_by_key(|p| p.priority);
                match pick.filter(|_| !self.config.reject_protocol) {
                    Some(p) => {
                        self.schema_id = p.schema_id;
                        self.state = State::AwaitSessionSetupReq;
                        let res = SupportedAppProtocolRes { response_code: SapCode::OkSuccessfulNegotiation, schema_id: Some(p.schema_id) };
                        Output { send: frame(PayloadType::SAP, |out| app_protocol::encode_res(&res, out)), close: false }
                    }
                    None => {
                        let res = SupportedAppProtocolRes { response_code: SapCode::FailedNoNegotiation, schema_id: None };
                        Output { send: frame(PayloadType::SAP, |out| app_protocol::encode_res(&res, out)), close: true }
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
                    header: MessageHeader { session_id: self.config.session_id, timestamp: now() },
                    response_code: self.config.setup_code,
                    evse_id: &self.config.evse_id,
                };
                if ok {
                    self.state = State::AwaitAuthorizationSetupReq;
                }
                Output {
                    send: frame(PayloadType::MAINSTREAM, |out| common::encode_session_setup_res(&res, out)),
                    close: !ok,
                }
            }
            State::AwaitAuthorizationSetupReq if payload_type == PayloadType::MAINSTREAM => {
                let Ok(common::Message::AuthorizationSetupReq(req)) = common::decode(payload, &arena) else { return closed };
                if req.header.session_id != self.config.session_id {
                    return self.unknown_session(common::Message::AuthorizationSetupReq(req));
                }
                let Some(authorization_services) = AuthorizationServices::new(&self.config.offered_authorization) else {
                    return closed;
                };
                self.state = State::AwaitAuthorizationReq;
                let res = AuthorizationSetupRes {
                    header: self.header(),
                    response_code: ResponseCode::Ok,
                    authorization_services,
                    certificate_installation_service: false,
                };
                Output { send: frame(PayloadType::MAINSTREAM, |out| common::encode_authorization_setup_res(&res, out)), close: false }
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
                    self.state = State::Authorized;
                }
                Output {
                    send: frame(PayloadType::MAINSTREAM, |out| common::encode_authorization_res(&res, out)),
                    close: !ongoing && !finished_ok,
                }
            }
            // The rest of the flow is added with the later slices.
            _ => closed,
        }
    }
}

impl Evse {
    fn header(&self) -> MessageHeader {
        MessageHeader { session_id: self.config.session_id, timestamp: now() }
    }

    /// Answers a message that carries the wrong session id with `FAILED_UnknownSession`.
    fn unknown_session(&self, msg: common::Message<'_>) -> Output {
        let send = match msg {
            common::Message::AuthorizationSetupReq(_) => {
                let res = AuthorizationSetupRes {
                    header: self.header(),
                    response_code: ResponseCode::FailedUnknownSession,
                    authorization_services: AuthorizationServices::new(&[Authorization::Eim]).expect("one service"),
                    certificate_installation_service: false,
                };
                frame(PayloadType::MAINSTREAM, |out| common::encode_authorization_setup_res(&res, out))
            }
            _ => {
                let res = AuthorizationRes {
                    header: self.header(),
                    response_code: ResponseCode::FailedUnknownSession,
                    evse_processing: Processing::Finished,
                };
                frame(PayloadType::MAINSTREAM, |out| common::encode_authorization_res(&res, out))
            }
        };
        Output { send, close: true }
    }
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

/// Accepts one connection and serves it until it closes. Blocking.
pub fn serve_one(listener: &TcpListener, config: Config) -> std::io::Result<State> {
    let (mut stream, _) = listener.accept()?;
    let mut evse = Evse::new(config);
    let mut buf = [0u8; 2048];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(evse.state());
        }
        for out in evse.handle(&buf[..n]) {
            if let Some(bytes) = out.send {
                stream.write_all(&bytes)?;
            }
            if out.close {
                return Ok(evse.state());
            }
        }
    }
}
