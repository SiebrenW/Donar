//! The simulated EVSE: a sans-IO state machine from received bytes to frames to send. ISO 15118-20 is
//! the session machine of `evse-core` with this charger's settings and faults behind it
//! ([`crate::secc::SimSecc`]); DIN SPEC 70121 and ISO 15118-2 are handled here.

use std::time::{SystemTime, UNIX_EPOCH};

use app_protocol::{self, ResponseCode as SapCode, SupportedAppProtocolRes};
use exi::{Arena, EncodeError};
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use std::sync::Arc;

use crate::config::{Config, Protocol};
use crate::live::EvseLive;
use crate::secc::SimSecc;

const DC_NAMESPACE: &str = "urn:iso:std:iso:15118:-20:DC";
const AC_NAMESPACE: &str = "urn:iso:std:iso:15118:-20:AC";
const DIN_NAMESPACE: &str = "urn:din:70121:2012:MsgDef";
const ISO2_NAMESPACE: &str = "urn:iso:15118:2:2013:MsgDef";

mod din;
mod iso2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    AwaitSupportedAppProtocolReq,
    AwaitSessionSetupReq,
    AwaitAuthorizationSetupReq,
    AwaitAuthorizationReq,
    AwaitServiceDiscoveryReq,
    AwaitServiceDetailReq,
    AwaitServiceSelectionReq,
    /// DIN SPEC 70121 and ISO 15118-2.
    AwaitServicePaymentSelectionReq,
    /// ISO 15118-2 Plug & Charge.
    AwaitPaymentDetailsReq,
    /// DIN SPEC 70121.
    AwaitContractAuthenticationReq,
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

/// The ISO 15118-20 session machine and the memory it works in.
struct Core {
    session: evse_core::Session<SimSecc>,
    rx: Vec<u8>,
    tx: Vec<u8>,
    arena: Vec<u8>,
    scratch: Vec<u8>,
}

pub struct Evse {
    pub(crate) config: Config,
    state: State,
    rx: Vec<u8>,
    schema_id: u8,
    /// The protocol the vehicle and the charger agreed on.
    protocol: Protocol,
    /// ISO 15118-20: the session machine, once the vehicle chose it.
    core: Option<Core>,
    authorization_reqs_seen: u32,
    schedule_reqs_seen: u32,
    cable_reqs_seen: u32,
    precharge_reqs_seen: u32,
    loops_seen: u32,
    /// The id of this session.
    session_id: [u8; 8],
    live: Option<Arc<EvseLive>>,
    log: Vec<String>,
    /// The challenge of this session's authorization setup (V2G20-697: 128 random bits).
    challenge: [u8; 16],
    /// DIN SPEC 70121: what the vehicle announced in its charge parameter discovery.
    din_ev_announced: Option<din70121::types::DcEvChargeParameter>,
    /// ISO 15118-2: the vehicle's announced power limit, and whether this is an AC session.
    iso2_ev_power: Option<f64>,
    iso2_ac: bool,
    /// Phases of the AC mode of the session (1 or 3).
    iso2_phases: f64,
    /// A renegotiation is under way: the charge parameter discovery is answered, then power again.
    iso2_renegotiating: bool,
    iso2_renegotiation_asked: bool,
    /// The last loop answer asked for a receipt (the reading it carried).
    iso2_receipt_reading: Option<u64>,
    /// ISO 15118-2: the contract certificate of a Plug & Charge session, and whether it expires soon.
    iso2_contract: Option<(openssl::x509::X509, bool)>,
}

fn random_challenge() -> [u8; 16] {
    let mut challenge = [0u8; 16];
    openssl::rand::rand_bytes(&mut challenge).expect("random bytes");
    challenge
}

impl From<evse_core::State> for State {
    fn from(s: evse_core::State) -> State {
        use evse_core::State as C;
        match s {
            C::AwaitSupportedAppProtocolReq => State::AwaitSupportedAppProtocolReq,
            C::AwaitSessionSetupReq => State::AwaitSessionSetupReq,
            C::AwaitAuthorizationSetupReq => State::AwaitAuthorizationSetupReq,
            C::AwaitAuthorizationReq => State::AwaitAuthorizationReq,
            C::AwaitServiceDiscoveryReq => State::AwaitServiceDiscoveryReq,
            C::AwaitServiceDetailReq => State::AwaitServiceDetailReq,
            C::AwaitServiceSelectionReq => State::AwaitServiceSelectionReq,
            C::AwaitChargeParameterDiscoveryReq => State::AwaitChargeParameterDiscoveryReq,
            C::AwaitScheduleExchangeReq => State::AwaitScheduleExchangeReq,
            C::AwaitCableCheckReq => State::AwaitCableCheckReq,
            C::AwaitPreChargeReq => State::AwaitPreChargeReq,
            C::AwaitPowerDeliveryReq => State::AwaitPowerDeliveryReq,
            C::ChargeLoop => State::ChargeLoop,
            C::AwaitWeldingReq => State::AwaitWeldingReq,
            C::AwaitSessionStopReq => State::AwaitSessionStopReq,
            C::Stopped => State::Stopped,
            C::Paused => State::Paused,
            C::Closed => State::Closed,
        }
    }
}

impl Evse {
    pub fn new(config: Config) -> Self {
        Self {
            session_id: config.session_id,
            config,
            state: State::AwaitSupportedAppProtocolReq,
            rx: Vec::new(),
            schema_id: 0,
            protocol: Protocol::Iso20,
            core: None,
            authorization_reqs_seen: 0,
            schedule_reqs_seen: 0,
            cable_reqs_seen: 0,
            precharge_reqs_seen: 0,
            loops_seen: 0,
            live: None,
            log: Vec::new(),
            challenge: random_challenge(),
            din_ev_announced: None,
            iso2_ev_power: None,
            iso2_ac: false,
            iso2_phases: 1.0,
            iso2_renegotiating: false,
            iso2_renegotiation_asked: false,
            iso2_receipt_reading: None,
            iso2_contract: None,
        }
    }

    /// Reports the charge loop measurements to a test platform and takes its edits.
    pub fn with_live(mut self, live: Arc<EvseLive>) -> Self {
        self.live = Some(live);
        self
    }

    pub fn state(&self) -> State {
        self.core.as_ref().map_or(self.state, |c| c.session.state().into())
    }

    /// What the charger did since the last call, one line each: what it received, what it
    /// offered and answered, why it refused or closed.
    pub fn take_log(&mut self) -> Vec<String> {
        let mut log = std::mem::take(&mut self.log);
        if let Some(core) = &mut self.core {
            log.extend(std::mem::take(&mut core.session.secc_mut().log));
        }
        log
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
        // Settings changed by a test platform during the session count from this message on.
        if let Some(config) = self.live.as_ref().and_then(|l| l.take_patch()) {
            if let Some(core) = &mut self.core {
                core.session.secc_mut().set_config(config.clone());
            }
            self.config = config;
        }
        if self.core.is_some() {
            return self.handle_core(bytes);
        }
        self.rx.extend_from_slice(bytes);
        if self.state == State::AwaitSupportedAppProtocolReq && self.chose_iso20() {
            // The vehicle chose ISO 15118-20: its session machine takes over from the first frame on.
            let mut core = Core { session: evse_core::Session::new(SimSecc::new(self.config.clone(), self.live.clone())), rx: vec![0; 1 << 16], tx: vec![0; 1 << 16], arena: vec![0; 1 << 15], scratch: vec![0; 1 << 14] };
            let mut buffers = evse_core::Buffers { rx: &mut core.rx, tx: &mut core.tx, arena: &mut core.arena, scratch: &mut core.scratch };
            core.session.handle(evse_core::Event::Connected { secured: false }, &mut buffers);
            self.core = Some(core);
            let all = std::mem::take(&mut self.rx);
            return self.handle_core(&all);
        }
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

    /// Whether the first frame is a `SupportedAppProtocolReq` whose best protocol, among those the charger
    /// speaks, is ISO 15118-20.
    fn chose_iso20(&self) -> bool {
        let Ok(Parse::Frame(f)) = parse(&self.rx, 1 << 20) else { return false };
        if f.payload_type != PayloadType::SAP || self.config.reject_protocol {
            return false;
        }
        let mut mem = vec![0u8; 8192];
        let arena = Arena::new(&mut mem);
        let Ok(app_protocol::Message::SupportedAppProtocolReq(req)) = app_protocol::decode(f.payload, &arena) else { return false };
        let speaks = |p: Protocol| self.config.protocols.contains(&p);
        let pick = req
            .app_protocol
            .iter()
            .filter(|p| self.speaks(speaks, p))
            .min_by_key(|p| p.priority);
        pick.is_some_and(|p| p.protocol_namespace == DC_NAMESPACE || p.protocol_namespace == AC_NAMESPACE)
    }

    fn speaks(&self, speaks: impl Fn(Protocol) -> bool, p: &app_protocol::AppProtocol<'_>) -> bool {
        (speaks(Protocol::Iso20) && (p.protocol_namespace == DC_NAMESPACE || p.protocol_namespace == AC_NAMESPACE) && p.version_number_major == 1)
            || (speaks(Protocol::Din) && p.protocol_namespace == DIN_NAMESPACE && p.version_number_major == 2)
            || (speaks(Protocol::Iso2) && p.protocol_namespace == ISO2_NAMESPACE && p.version_number_major == 2)
    }

    /// Feeds bytes to the ISO 15118-20 session machine.
    fn handle_core(&mut self, bytes: &[u8]) -> Vec<Output> {
        let core = self.core.as_mut().expect("the session machine is running");
        let mut sent: Vec<u8> = Vec::new();
        let mut close = false;
        let mut rest = bytes;
        while !rest.is_empty() && !close {
            let space = core.session.rx_space(&mut core.rx);
            if space.is_empty() {
                close = true;
                break;
            }
            let n = space.len().min(rest.len());
            space[..n].copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            let mut buffers = evse_core::Buffers { rx: &mut core.rx, tx: &mut core.tx, arena: &mut core.arena, scratch: &mut core.scratch };
            let step = core.session.handle(evse_core::Event::Received(n), &mut buffers);
            sent.extend_from_slice(&core.tx[..step.send]);
            close |= step.close;
        }
        // What went out, for the log: each response frame with its code (and the check against Tables 224, 226).
        let mut at = 0;
        let mut lines = Vec::new();
        while at + HEADER_LEN <= sent.len() {
            let len = HEADER_LEN + u32::from_be_bytes(sent[at + 4..at + 8].try_into().expect("four octets")) as usize;
            let frame = &sent[at..(at + len).min(sent.len())];
            let tail = if close && at + len >= sent.len() { ", then closing" } else { "" };
            match crate::answers::response_of(frame) {
                Some((name, _, code)) => lines.push(format!("tx {name} {} ({} bytes{tail})", code.xml_name(), frame.len())),
                None => lines.push(format!("tx {} bytes{tail}", frame.len())),
            }
            at += len;
        }
        core.session.secc_mut().log.extend(lines);
        vec![Output { send: (!sent.is_empty()).then_some(sent), close }]
    }

    fn close(&mut self, mut outputs: Vec<Output>) -> Vec<Output> {
        self.state = State::Closed;
        outputs.push(Output { send: None, close: true });
        outputs
    }

    /// The request the charger waits for in `state` (DIN SPEC 70121 and ISO 15118-2).
    fn expects(&self, state: State) -> &'static str {
        if self.protocol == Protocol::Din {
            return din::expects(state);
        }
        if self.protocol == Protocol::Iso2 {
            return iso2::expects(state);
        }
        match state {
            State::AwaitSupportedAppProtocolReq => "SupportedAppProtocolReq",
            State::AwaitSessionSetupReq => "SessionSetupReq",
            _ => "a request",
        }
    }

    /// A frame of a DIN SPEC 70121 or ISO 15118-2 session (and the first frame of any).
    fn on_frame(&mut self, payload_type: PayloadType, payload: &[u8]) -> Output {
        let before = self.state;
        self.note(format!("rx {} bytes, payload type {:#06x}, waiting for {}", payload.len(), payload_type.0, self.expects(before)));
        if self.config.silent {
            self.note("silent: not answering");
            return Output::default();
        }
        let out = self.on_frame_inner(payload_type, payload);
        match (&out.send, out.close) {
            (None, true) => self.note(format!("closing without an answer: the frame is not a decodable {} (state {before:?})", self.expects(before))),
            (None, false) => self.note("no answer"),
            (Some(bytes), close) => {
                let tail = if close { ", then closing" } else { "" };
                self.note(format!("tx {} bytes{tail}", bytes.len()));
            }
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
        if self.state != State::AwaitSupportedAppProtocolReq {
            return match self.protocol {
                Protocol::Din => self.on_din_frame(payload_type, payload),
                _ => self.on_iso2_frame(payload_type, payload),
            };
        }
        if payload_type != PayloadType::SAP {
            return closed;
        }
        let Ok(app_protocol::Message::SupportedAppProtocolReq(req)) = app_protocol::decode(payload, &arena) else { return closed };
        // Highest priority = lowest number among the protocols we speak.
        let speaks = |p: Protocol| self.config.protocols.contains(&p);
        let pick = req.app_protocol.iter().filter(|p| self.speaks(speaks, p)).min_by_key(|p| p.priority);
        match pick.filter(|_| !self.config.reject_protocol) {
            Some(p) => {
                self.protocol = match p.protocol_namespace {
                    DIN_NAMESPACE => Protocol::Din,
                    _ => Protocol::Iso2,
                };
                self.note(format!("protocol {}, schema id {}", p.protocol_namespace, p.schema_id));
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
}

fn now_micros() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
}

pub(crate) fn frame(payload_type: PayloadType, encode: impl FnOnce(&mut [u8]) -> Result<usize, EncodeError>) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    let n = encode(&mut buf[HEADER_LEN..]).ok()?;
    write_header(&mut buf, payload_type, n).ok()?;
    buf.truncate(HEADER_LEN + n);
    Some(buf)
}
