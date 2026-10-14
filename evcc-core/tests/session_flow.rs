//! Drives the session machine against a scripted EVSE built from the same codecs (which are
//! themselves checked against EXIficient vectors).

use evcc_core::TimerCmd;
use evcc_core::ev::Ev;
use evcc_core::log::{Level, LogSink, MsgKind, Record};
use evcc_core::session::*;
use iso15118_20::app_protocol::{self, ResponseCode as SapCode, SupportedAppProtocolRes};
use iso15118_20::common::{self, MessageHeader, ResponseCode, SessionSetupRes};
use iso15118_20::{Arena, DecodeError};
use v2gtp::{HEADER_LEN, PayloadType};

struct TestEv;
impl Ev for TestEv {
    fn evcc_id(&self) -> &str {
        "WMIV1234567890ABCDEX"
    }
    fn unix_time(&self) -> u64 {
        1_700_000_000
    }
}

/// Owned copies of the records we assert on (the real ones borrow).
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Info(std::string::String),
    Error(std::string::String),
    Msg(MsgKind, bool), // (kind, is_tx)
}

#[derive(Default)]
struct Capture(std::rc::Rc<std::cell::RefCell<std::vec::Vec<Seen>>>);
impl LogSink for Capture {
    fn record(&mut self, r: &Record<'_>) {
        let seen = match (r, r.level()) {
            (Record::MsgTx { msg }, _) => Seen::Msg(*msg, true),
            (Record::MsgRx { msg }, _) => Seen::Msg(*msg, false),
            (_, Level::Info) => Seen::Info(std::format!("{r:?}")),
            (_, Level::Error) => Seen::Error(std::format!("{r:?}")),
            _ => return,
        };
        self.0.borrow_mut().push(seen);
    }
}

struct Rig {
    rx: [u8; 512],
    tx: [u8; 512],
    arena: [u8; 2048],
}

impl Rig {
    fn new() -> Self {
        Self { rx: [0; 512], tx: [0; 512], arena: [0; 2048] }
    }
}

fn session<'a>(rig: &'a mut Rig, log: Capture) -> Session<'a, TestEv, Capture> {
    let Rig { rx, tx, arena } = rig;
    Session::new(Config::DC_20, TestEv, log, Buffers { rx, tx, arena })
}

/// Copies `bytes` into the session's RX buffer and reports them, like a socket read would.
fn deliver(s: &mut Session<'_, TestEv, Capture>, bytes: &[u8]) -> Step {
    s.rx_space()[..bytes.len()].copy_from_slice(bytes);
    s.handle(Event::Received(bytes.len()))
}

fn framed(payload_type: PayloadType, encode: impl FnOnce(&mut [u8]) -> usize) -> std::vec::Vec<u8> {
    let mut buf = [0u8; 256];
    let n = encode(&mut buf[HEADER_LEN..]);
    v2gtp::write_header(&mut buf, payload_type, n).unwrap();
    buf[..HEADER_LEN + n].to_vec()
}

fn sap_res(code: SapCode, schema_id: Option<u8>) -> std::vec::Vec<u8> {
    framed(PayloadType::SAP, |out| {
        app_protocol::encode_res(&SupportedAppProtocolRes { response_code: code, schema_id }, out).unwrap()
    })
}

fn setup_res(code: ResponseCode, session_id: [u8; 8]) -> std::vec::Vec<u8> {
    framed(PayloadType::MAINSTREAM, |out| {
        let msg = SessionSetupRes { header: MessageHeader { session_id, timestamp: 5 }, response_code: code, evse_id: "DE*EVSE*0001" };
        common::encode_session_setup_res(&msg, out).unwrap()
    })
}

/// Decodes the framed message the session wants to send.
fn sent_frame(s: &Session<'_, TestEv, Capture>, step: &Step) -> (PayloadType, std::vec::Vec<u8>) {
    let bytes = &s.tx()[..step.send.expect("nothing to send")];
    let v2gtp::Parse::Frame(f) = v2gtp::parse(bytes, 512).unwrap() else { panic!("incomplete frame") };
    assert_eq!(f.consumed, bytes.len());
    (f.payload_type, f.payload.to_vec())
}

const SID: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

#[test]
fn full_handshake_and_session_setup() {
    let log = Capture::default();
    let seen = log.0.clone();
    let mut rig = Rig::new();
    let mut s = session(&mut rig, log);

    let step = s.handle(Event::Start { secured: true });
    assert_eq!(step.timer, TimerCmd::Arm { ms: 2000 });
    let (pt, payload) = sent_frame(&s, &step);
    assert_eq!(pt, PayloadType::SAP);
    let mut mem = [0u8; 1024];
    let arena = Arena::new(&mut mem);
    let app_protocol::Message::Req(req) = app_protocol::decode(&payload, &arena).unwrap() else { panic!() };
    assert_eq!(req.app_protocols, &[Config::DC_20.offer]);
    assert_eq!(s.state(), State::AwaitSupportedAppProtocolRes);

    let step = deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    assert_eq!(step.timer, TimerCmd::Arm { ms: 2000 });
    let (pt, payload) = sent_frame(&s, &step);
    assert_eq!(pt, PayloadType::MAINSTREAM);
    let arena = Arena::new(&mut mem);
    let common::Message::SessionSetupReq(req) = common::decode(&payload, &arena).unwrap() else { panic!() };
    assert_eq!(req.evcc_id, "WMIV1234567890ABCDEX");
    assert_eq!(req.header.session_id, [0; 8]);
    assert_eq!(req.header.timestamp, 1_700_000_000);

    let step = deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    assert_eq!(step.notice, Some(Notice::SessionSetUp { session_id: SID }));
    assert_eq!(step.timer, TimerCmd::Cancel);
    assert_eq!(step.end, None);
    assert_eq!(s.state(), State::SessionSetUp);

    let seen = seen.borrow();
    assert!(seen.contains(&Seen::Msg(MsgKind::SupportedAppProtocolReq, true)));
    assert!(seen.contains(&Seen::Msg(MsgKind::SessionSetupRes, false)));
    assert!(!seen.iter().any(|s| matches!(s, Seen::Error(_))), "{seen:?}");
}

#[test]
fn frames_split_across_reads_are_reassembled() {
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    let res = sap_res(SapCode::OkSuccessfulNegotiation, Some(1));
    for byte in &res[..res.len() - 1] {
        assert_eq!(deliver(&mut s, &[*byte]), Step::default());
    }
    let step = deliver(&mut s, &res[res.len() - 1..]);
    assert!(step.send.is_some());
}

#[test]
fn timeout_ends_the_session() {
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    let step = s.handle(Event::TimerExpired);
    assert_eq!(step.end, Some(EndReason::Timeout));
    assert_eq!(s.state(), State::Ended);
    // Once ended, nothing else happens.
    assert_eq!(s.handle(Event::TimerExpired), Step::default());
    assert_eq!(deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1))), Step::default());
}

#[test]
fn rejected_protocol_ends_with_no_common_protocol() {
    for (code, id) in [(SapCode::FailedNoNegotiation, None), (SapCode::OkSuccessfulNegotiation, Some(9)), (SapCode::OkSuccessfulNegotiation, None)] {
        let mut rig = Rig::new();
        let mut s = session(&mut rig, Capture::default());
        s.handle(Event::Start { secured: false });
        assert_eq!(deliver(&mut s, &sap_res(code, id)).end, Some(EndReason::NoCommonProtocol));
    }
}

#[test]
fn failing_response_code_is_reported() {
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiationWithMinorDeviation, Some(1)));
    let step = deliver(&mut s, &setup_res(ResponseCode::FailedSequenceError, SID));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedSequenceError)));
}

#[test]
fn wrong_payload_type_or_message_ends_the_session() {
    // SessionSetupRes arriving while SupportedAppProtocolRes is expected.
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    let step = deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    assert_eq!(step.end, Some(EndReason::UnexpectedMessage));

    // A request instead of a response.
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    let req = framed(PayloadType::SAP, |out| {
        let offer = [Config::DC_20.offer];
        app_protocol::encode_req(&app_protocol::SupportedAppProtocolReq { app_protocols: &offer }, out).unwrap()
    });
    assert_eq!(deliver(&mut s, &req).end, Some(EndReason::UnexpectedMessage));
}

#[test]
fn garbage_is_rejected_without_panicking() {
    // Bad V2GTP version.
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    assert_eq!(deliver(&mut s, &[0x02, 0xFD, 0x80, 0x01, 0, 0, 0, 0]).end, Some(EndReason::Framing));

    // Valid framing, undecodable payload.
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    let bad = framed(PayloadType::SAP, |out| {
        out[..3].copy_from_slice(&[0x80, 0xff, 0xff]);
        3
    });
    assert!(matches!(deliver(&mut s, &bad).end, Some(EndReason::Malformed(_))));

    // A frame bigger than the RX buffer.
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    assert_eq!(deliver(&mut s, &[0x01, 0xFE, 0x80, 0x01, 0, 0, 0xff, 0xff]).end, Some(EndReason::Framing));
}

#[test]
fn truncated_responses_never_panic_and_never_complete() {
    let full = setup_res(ResponseCode::OkNewSessionEstablished, SID);
    for cut in 0..full.len() {
        let mut rig = Rig::new();
        let mut s = session(&mut rig, Capture::default());
        s.handle(Event::Start { secured: false });
        deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
        let step = deliver(&mut s, &full[..cut]);
        assert_eq!(step, Step::default(), "cut {cut} produced output");
        assert_eq!(s.state(), State::AwaitSessionSetupRes);
    }
}

#[test]
fn tiny_tx_buffer_ends_the_session() {
    let mut rx = [0u8; 256];
    let mut tx = [0u8; 12];
    let mut arena = [0u8; 512];
    let mut s = Session::new(Config::DC_20, TestEv, Capture::default(), Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    assert_eq!(s.handle(Event::Start { secured: false }).end, Some(EndReason::TxBufferTooSmall));
}

#[test]
fn small_arena_is_a_decode_error_not_a_panic() {
    let mut rx = [0u8; 256];
    let mut tx = [0u8; 256];
    let mut arena = [0u8; 8];
    let mut s = Session::new(Config::DC_20, TestEv, Capture::default(), Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    s.handle(Event::Start { secured: false });
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    let step = deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    assert_eq!(step.end, Some(EndReason::Malformed(DecodeError::ArenaFull)));
}
