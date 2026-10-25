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
use crate::secc_din::SimDin;
use crate::secc_iso2::SimIso2;

const DC_NAMESPACE: &str = "urn:iso:std:iso:15118:-20:DC";
const AC_NAMESPACE: &str = "urn:iso:std:iso:15118:-20:AC";
const ACDP_NAMESPACE: &str = "urn:iso:std:iso:15118:-20:ACDP";
const DIN_NAMESPACE: &str = "urn:din:70121:2012:MsgDef";
const ISO2_NAMESPACE: &str = "urn:iso:15118:2:2013:MsgDef";


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    AwaitSupportedAppProtocolReq,
    AwaitSessionSetupReq,
    /// ACDP: the vehicle is positioned before it is authorized.
    AwaitVehiclePositioningReq,
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
    /// ACDP: the pantograph is connected before the cable check.
    AwaitConnectReq,
    AwaitCableCheckReq,
    AwaitPreChargeReq,
    AwaitPowerDeliveryReq,
    /// Energy flows; charge loop requests and the stop are accepted.
    ChargeLoop,
    /// Welding detection after the stop (V2G20-1448); ends with a `Finished` request.
    AwaitWeldingReq,
    /// ACDP: the pantograph is disconnected after the stop, in place of the welding detection.
    AwaitDisconnectReq,
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

/// The memory a session machine works in.
struct Mem {
    rx: Vec<u8>,
    tx: Vec<u8>,
    arena: Vec<u8>,
    scratch: Vec<u8>,
}

impl Mem {
    fn new() -> Self {
        Mem { rx: vec![0; 1 << 16], tx: vec![0; 1 << 16], arena: vec![0; 1 << 16], scratch: vec![0; 1 << 14] }
    }
}

/// One of the session machines of `evse-core` with this charger behind it.
trait Pump {
    fn space<'b>(&self, rx: &'b mut [u8]) -> &'b mut [u8];
    /// `n` bytes are in the receive space: the number of bytes to send and whether to close.
    fn step(&mut self, n: usize, mem: &mut Mem) -> (usize, bool);
    fn state(&self) -> State;
    fn log(&mut self) -> &mut Vec<String>;
    fn set_config(&mut self, config: Config);
    /// The host measured a change of the control pilot: the bytes to send (none) and whether the session ended.
    fn pilot(&mut self, reading: control_pilot::Reading, mem: &mut Mem) -> (usize, bool);
    /// Charge loop requests seen so far.
    fn loops(&self) -> u32;
    /// What the host is to watch on the control pilot now.
    fn guard(&self) -> control_pilot::Guard;
}

impl Pump for evse_core::Session<SimSecc> {
    fn space<'b>(&self, rx: &'b mut [u8]) -> &'b mut [u8] {
        self.rx_space(rx)
    }
    fn step(&mut self, n: usize, mem: &mut Mem) -> (usize, bool) {
        let mut buffers = evse_core::Buffers { rx: &mut mem.rx, tx: &mut mem.tx, arena: &mut mem.arena, scratch: &mut mem.scratch };
        let step = self.handle(evse_core::Event::Received(n), &mut buffers);
        (step.send, step.close)
    }
    fn state(&self) -> State {
        evse_core::Session::state(self).into()
    }
    fn log(&mut self) -> &mut Vec<String> {
        &mut self.secc_mut().log
    }
    fn set_config(&mut self, config: Config) {
        self.secc_mut().set_config(config);
    }
    fn pilot(&mut self, reading: control_pilot::Reading, mem: &mut Mem) -> (usize, bool) {
        let mut buffers = evse_core::Buffers { rx: &mut mem.rx, tx: &mut mem.tx, arena: &mut mem.arena, scratch: &mut mem.scratch };
        let step = self.handle(evse_core::Event::Pilot(reading), &mut buffers);
        (step.send, step.close)
    }
    fn loops(&self) -> u32 {
        self.secc().loops_seen()
    }
    fn guard(&self) -> control_pilot::Guard {
        self.pilot_guard()
    }
}

impl Pump for evse_core::din::Session<SimDin> {
    fn space<'b>(&self, rx: &'b mut [u8]) -> &'b mut [u8] {
        self.rx_space(rx)
    }
    fn step(&mut self, n: usize, mem: &mut Mem) -> (usize, bool) {
        let mut buffers = evse_core::din::Buffers { rx: &mut mem.rx, tx: &mut mem.tx, arena: &mut mem.arena };
        let step = self.handle(evse_core::Event::Received(n), &mut buffers);
        (step.send, step.close)
    }
    fn state(&self) -> State {
        evse_core::din::Session::state(self).into()
    }
    fn log(&mut self) -> &mut Vec<String> {
        &mut self.secc_mut().log
    }
    fn set_config(&mut self, config: Config) {
        self.secc_mut().set_config(config);
    }
    fn pilot(&mut self, reading: control_pilot::Reading, mem: &mut Mem) -> (usize, bool) {
        let mut buffers = evse_core::din::Buffers { rx: &mut mem.rx, tx: &mut mem.tx, arena: &mut mem.arena };
        let step = self.handle(evse_core::Event::Pilot(reading), &mut buffers);
        (step.send, step.close)
    }
    fn loops(&self) -> u32 {
        self.secc().loops_seen()
    }
    fn guard(&self) -> control_pilot::Guard {
        self.pilot_guard()
    }
}

impl Pump for evse_core::iso2::Session<SimIso2> {
    fn space<'b>(&self, rx: &'b mut [u8]) -> &'b mut [u8] {
        self.rx_space(rx)
    }
    fn step(&mut self, n: usize, mem: &mut Mem) -> (usize, bool) {
        let mut buffers = evse_core::iso2::Buffers { rx: &mut mem.rx, tx: &mut mem.tx, arena: &mut mem.arena };
        let step = self.handle(evse_core::Event::Received(n), &mut buffers);
        (step.send, step.close)
    }
    fn state(&self) -> State {
        evse_core::iso2::Session::state(self).into()
    }
    fn log(&mut self) -> &mut Vec<String> {
        &mut self.secc_mut().log
    }
    fn set_config(&mut self, config: Config) {
        self.secc_mut().set_config(config);
    }
    fn pilot(&mut self, reading: control_pilot::Reading, mem: &mut Mem) -> (usize, bool) {
        let mut buffers = evse_core::iso2::Buffers { rx: &mut mem.rx, tx: &mut mem.tx, arena: &mut mem.arena };
        let step = self.handle(evse_core::Event::Pilot(reading), &mut buffers);
        (step.send, step.close)
    }
    fn loops(&self) -> u32 {
        self.secc().loops_seen()
    }
    fn guard(&self) -> control_pilot::Guard {
        self.pilot_guard()
    }
}

/// The session machine of the protocol the vehicle chose, and its memory.
struct Run {
    pump: Box<dyn Pump>,
    mem: Mem,
}

pub struct Evse {
    pub(crate) config: Config,
    /// Before the machine takes over (the `SupportedAppProtocol` negotiation) and after a close.
    state: State,
    rx: Vec<u8>,
    /// The protocol the vehicle and the charger agreed on.
    protocol: Protocol,
    /// The session machine, once the protocol is chosen.
    run: Option<Run>,
    live: Option<Arc<EvseLive>>,
    log: Vec<String>,
    /// The last control pilot reading, for a session machine that is not started yet.
    pilot: Option<control_pilot::Reading>,
    /// The guard last written to the log.
    guard: control_pilot::Guard,
}

impl From<evse_core::State> for State {
    fn from(s: evse_core::State) -> State {
        use evse_core::State as C;
        match s {
            C::AwaitSupportedAppProtocolReq => State::AwaitSupportedAppProtocolReq,
            C::AwaitSessionSetupReq => State::AwaitSessionSetupReq,
            C::AwaitVehiclePositioningReq => State::AwaitVehiclePositioningReq,
            C::AwaitAuthorizationSetupReq => State::AwaitAuthorizationSetupReq,
            C::AwaitAuthorizationReq => State::AwaitAuthorizationReq,
            C::AwaitServiceDiscoveryReq => State::AwaitServiceDiscoveryReq,
            C::AwaitServiceDetailReq => State::AwaitServiceDetailReq,
            C::AwaitServiceSelectionReq => State::AwaitServiceSelectionReq,
            C::AwaitChargeParameterDiscoveryReq => State::AwaitChargeParameterDiscoveryReq,
            C::AwaitScheduleExchangeReq => State::AwaitScheduleExchangeReq,
            C::AwaitConnectReq => State::AwaitConnectReq,
            C::AwaitCableCheckReq => State::AwaitCableCheckReq,
            C::AwaitPreChargeReq => State::AwaitPreChargeReq,
            C::AwaitPowerDeliveryReq => State::AwaitPowerDeliveryReq,
            C::ChargeLoop => State::ChargeLoop,
            C::AwaitWeldingReq => State::AwaitWeldingReq,
            C::AwaitDisconnectReq => State::AwaitDisconnectReq,
            C::AwaitSessionStopReq => State::AwaitSessionStopReq,
            C::Stopped => State::Stopped,
            C::Paused => State::Paused,
            C::Closed => State::Closed,
        }
    }
}

impl From<evse_core::din::State> for State {
    fn from(s: evse_core::din::State) -> State {
        use evse_core::din::State as C;
        match s {
            C::AwaitSessionSetupReq => State::AwaitSessionSetupReq,
            C::AwaitServiceDiscoveryReq => State::AwaitServiceDiscoveryReq,
            C::AwaitServicePaymentSelectionReq => State::AwaitServicePaymentSelectionReq,
            C::AwaitContractAuthenticationReq => State::AwaitContractAuthenticationReq,
            C::AwaitChargeParameterDiscoveryReq => State::AwaitChargeParameterDiscoveryReq,
            C::AwaitCableCheckReq => State::AwaitCableCheckReq,
            C::AwaitPreChargeReq => State::AwaitPreChargeReq,
            C::ChargeLoop => State::ChargeLoop,
            C::AwaitWeldingReq => State::AwaitWeldingReq,
            C::Stopped => State::Stopped,
            C::Closed => State::Closed,
        }
    }
}

impl From<evse_core::iso2::State> for State {
    fn from(s: evse_core::iso2::State) -> State {
        use evse_core::iso2::State as C;
        match s {
            C::AwaitSessionSetupReq => State::AwaitSessionSetupReq,
            C::AwaitServiceDiscoveryReq => State::AwaitServiceDiscoveryReq,
            C::AwaitServicePaymentSelectionReq => State::AwaitServicePaymentSelectionReq,
            C::AwaitPaymentDetailsReq => State::AwaitPaymentDetailsReq,
            C::AwaitAuthorizationReq => State::AwaitContractAuthenticationReq,
            C::AwaitChargeParameterDiscoveryReq => State::AwaitChargeParameterDiscoveryReq,
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
        Self { config, state: State::AwaitSupportedAppProtocolReq, rx: Vec::new(), protocol: Protocol::Iso20, run: None, live: None, log: Vec::new(), pilot: None, guard: control_pilot::Guard::disarmed(control_pilot::Side::Charger) }
    }

    /// Reports the charge loop measurements to a test platform and takes its edits.
    pub fn with_live(mut self, live: Arc<EvseLive>) -> Self {
        self.live = Some(live);
        self
    }

    pub fn state(&self) -> State {
        self.run.as_ref().map_or(self.state, |r| r.pump.state())
    }

    /// The host measured the control pilot: a change (also before the session machine exists, which is told at once
    /// when it starts). The output is the end of the session if the reading means an emergency: nothing to send.
    pub fn pilot(&mut self, reading: control_pilot::Reading) -> Vec<Output> {
        self.pilot = Some(reading);
        let Some(run) = &mut self.run else { return Vec::new() };
        let (len, close) = run.pump.pilot(reading, &mut run.mem);
        let sent = run.mem.tx[..len].to_vec();
        self.note_guard();
        if close || len > 0 { vec![Output { send: (!sent.is_empty()).then_some(sent), close }] } else { Vec::new() }
    }

    /// What the host is to watch on the control pilot now.
    pub fn pilot_guard(&self) -> control_pilot::Guard {
        self.guard
    }

    /// Whether the charger is to have an emergency of its own now (`pilot_emergency_after_loops`).
    pub fn pilot_emergency_due(&self) -> bool {
        match (&self.run, self.config.pilot_emergency_after_loops) {
            (Some(run), Some(n)) => run.pump.loops() >= n,
            _ => false,
        }
    }

    /// Writes a change of what the host is to watch on the pilot to the log.
    fn note_guard(&mut self) {
        let Some(run) = &self.run else { return };
        let guard = run.pump.guard();
        if guard != self.guard {
            self.guard = guard;
            self.note(format!("control pilot guard: {guard:?}"));
        }
    }

    /// A session machine that starts learns the pilot at once.
    fn replay_pilot(&mut self) {
        if let (Some(reading), Some(run)) = (self.pilot, &mut self.run) {
            run.pump.pilot(reading, &mut run.mem);
        }
    }

    /// What the charger did since the last call, one line each: what it received, what it
    /// offered and answered, why it refused or closed.
    pub fn take_log(&mut self) -> Vec<String> {
        let mut log = std::mem::take(&mut self.log);
        if let Some(run) = &mut self.run {
            log.extend(std::mem::take(run.pump.log()));
        }
        log
    }

    fn note(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
    }

    /// Feeds bytes read from the stream; returns the messages to send, in order.
    pub fn handle(&mut self, bytes: &[u8]) -> Vec<Output> {
        // Settings changed by a test platform during the session count from this message on.
        if let Some(config) = self.live.as_ref().and_then(|l| l.take_patch()) {
            if let Some(run) = &mut self.run {
                run.pump.set_config(config.clone());
            }
            self.config = config;
        }
        if self.run.is_some() {
            return self.handle_run(bytes);
        }
        self.rx.extend_from_slice(bytes);
        if self.state == State::AwaitSupportedAppProtocolReq && self.chose_iso20() {
            // The vehicle chose ISO 15118-20: its session machine takes over from the first frame on.
            self.protocol = Protocol::Iso20;
            self.run = Some(Run { pump: Box::new(evse_core::Session::new(SimSecc::new(self.config.clone(), self.live.clone()))), mem: Mem::new() });
            self.replay_pilot();
            let all = std::mem::take(&mut self.rx);
            return self.handle_run(&all);
        }
        // The negotiation: one frame, then the machine of the protocol chosen takes over.
        let (payload_type, payload, consumed) = match parse(&self.rx, 1 << 20) {
            Ok(Parse::Frame(f)) => (f.payload_type, f.payload.to_vec(), f.consumed),
            Ok(Parse::Incomplete { .. }) => return Vec::new(),
            Err(_) => return self.close(Vec::new()),
        };
        self.rx.drain(..consumed);
        let out = self.negotiate(payload_type, &payload);
        let close = out.close;
        let mut outputs = vec![out];
        if close {
            self.state = State::Closed;
            return outputs;
        }
        if self.run.is_some() && !self.rx.is_empty() {
            let rest = std::mem::take(&mut self.rx);
            outputs.extend(self.handle_run(&rest));
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
        pick.is_some_and(|p| matches!(p.protocol_namespace, DC_NAMESPACE | AC_NAMESPACE | ACDP_NAMESPACE))
    }

    fn speaks(&self, speaks: impl Fn(Protocol) -> bool, p: &app_protocol::AppProtocol<'_>) -> bool {
        (speaks(Protocol::Iso20) && (p.protocol_namespace == DC_NAMESPACE || p.protocol_namespace == AC_NAMESPACE || (p.protocol_namespace == ACDP_NAMESPACE && self.config.acdp.is_some())) && p.version_number_major == 1)
            || (speaks(Protocol::Din) && p.protocol_namespace == DIN_NAMESPACE && p.version_number_major == 2)
            || (speaks(Protocol::Iso2) && p.protocol_namespace == ISO2_NAMESPACE && p.version_number_major == 2)
    }

    /// Feeds bytes to the session machine.
    fn handle_run(&mut self, bytes: &[u8]) -> Vec<Output> {
        let iso20 = self.protocol == Protocol::Iso20;
        let run = self.run.as_mut().expect("the session machine is running");
        let mut sent: Vec<u8> = Vec::new();
        let mut close = false;
        let mut rest = bytes;
        while !rest.is_empty() && !close {
            let space = run.pump.space(&mut run.mem.rx);
            if space.is_empty() {
                close = true;
                break;
            }
            let n = space.len().min(rest.len());
            space[..n].copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            let (len, closed) = run.pump.step(n, &mut run.mem);
            sent.extend_from_slice(&run.mem.tx[..len]);
            close |= closed;
        }
        if iso20 {
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
            run.pump.log().extend(lines);
        }
        self.note_guard();
        vec![Output { send: (!sent.is_empty()).then_some(sent), close }]
    }

    fn close(&mut self, mut outputs: Vec<Output>) -> Vec<Output> {
        self.state = State::Closed;
        outputs.push(Output { send: None, close: true });
        outputs
    }

    /// The `SupportedAppProtocolReq`: the best protocol among those the charger speaks, or a refusal. On
    /// success the session machine of that protocol (DIN SPEC 70121 or ISO 15118-2) is started.
    fn negotiate(&mut self, payload_type: PayloadType, payload: &[u8]) -> Output {
        self.note(format!("rx {} bytes, payload type {:#06x}, waiting for SupportedAppProtocolReq", payload.len(), payload_type.0));
        if self.config.silent {
            self.note("silent: not answering");
            return Output::default();
        }
        let mut mem = vec![0u8; 8192];
        let arena = Arena::new(&mut mem);
        let closed = Output { send: None, close: true };
        if payload_type != PayloadType::SAP {
            return closed;
        }
        let Ok(app_protocol::Message::SupportedAppProtocolReq(req)) = app_protocol::decode(payload, &arena) else {
            self.note("closing without an answer: the frame is not a decodable SupportedAppProtocolReq");
            return closed;
        };
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
                let (config, live) = (self.config.clone(), self.live.clone());
                let pump: Box<dyn Pump> = match self.protocol {
                    Protocol::Din => Box::new(evse_core::din::Session::new(SimDin::new(config, live))),
                    _ => Box::new(evse_core::iso2::Session::new(SimIso2::new(config, live))),
                };
                self.run = Some(Run { pump, mem: Mem::new() });
                self.replay_pilot();
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

pub(crate) fn now_micros() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
}

pub(crate) fn frame(payload_type: PayloadType, encode: impl FnOnce(&mut [u8]) -> Result<usize, EncodeError>) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    let n = encode(&mut buf[HEADER_LEN..]).ok()?;
    write_header(&mut buf, payload_type, n).ok()?;
    buf.truncate(HEADER_LEN + n);
    Some(buf)
}
