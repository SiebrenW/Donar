//! Drives the session machine against a scripted EVSE built from the same codecs (which are
//! themselves checked against EXIficient vectors).

use std::cell::Cell;
use std::rc::Rc;

use evcc_core::ev::Ev;
use evcc_core::log::{Level, LogSink, MsgKind, Record};
use evcc_core::session::*;
use evcc_core::{Timer, TimerCmd, Timers};
use iso15118_20::app_protocol::{self, ResponseCode as SapCode, SupportedAppProtocolRes};
use iso15118_20::common::{
    self, Authorization, AuthorizationRes, AuthorizationServices, AuthorizationSetupRes, MessageHeader, Processing,
    ResponseCode, SessionSetupRes,
};
use iso15118_20::{Arena, DecodeError};
use v2gtp::{HEADER_LEN, PayloadType};

/// The vehicle. Every call to `header_timestamp` returns a larger value so tests can tell
/// messages apart.
#[derive(Clone)]
struct TestEv(Rc<Cell<u64>>);

impl TestEv {
    fn new() -> Self {
        Self(Rc::new(Cell::new(1_700_000_000)))
    }
}

impl Ev for TestEv {
    fn evcc_id(&self) -> &str {
        "WMIV1234567890ABCDEX"
    }
    fn header_timestamp(&self) -> u64 {
        let t = self.0.get();
        self.0.set(t + 1);
        t
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
struct Capture(Rc<std::cell::RefCell<std::vec::Vec<Seen>>>);
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

type Sess<'a> = Session<'a, TestEv, Capture>;

fn session<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let Rig { rx, tx, arena } = rig;
    Session::new(Config::DC_20, TestEv::new(), log, Buffers { rx, tx, arena })
}

/// Copies `bytes` into the session's RX buffer and reports them, like a socket read would.
fn deliver(s: &mut Sess<'_>, bytes: &[u8]) -> Step {
    s.rx_space()[..bytes.len()].copy_from_slice(bytes);
    s.handle(Event::Received(bytes.len()))
}

fn framed(payload_type: PayloadType, encode: impl FnOnce(&mut [u8]) -> usize) -> std::vec::Vec<u8> {
    let mut buf = [0u8; 256];
    let n = encode(&mut buf[HEADER_LEN..]);
    v2gtp::write_header(&mut buf, payload_type, n).unwrap();
    buf[..HEADER_LEN + n].to_vec()
}

const SID: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

fn hdr() -> MessageHeader {
    MessageHeader { session_id: SID, timestamp: 5 }
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

fn auth_setup_res(code: ResponseCode, services: &[Authorization]) -> std::vec::Vec<u8> {
    framed(PayloadType::MAINSTREAM, |out| {
        let msg = AuthorizationSetupRes {
            header: hdr(),
            response_code: code,
            authorization_services: AuthorizationServices::new(services).unwrap(),
            certificate_installation_service: false,
        };
        common::encode_authorization_setup_res(&msg, out).unwrap()
    })
}

fn auth_res(code: ResponseCode, p: Processing) -> std::vec::Vec<u8> {
    framed(PayloadType::MAINSTREAM, |out| {
        common::encode_authorization_res(&AuthorizationRes { header: hdr(), response_code: code, evse_processing: p }, out).unwrap()
    })
}

/// The framed message the session wants to send, decoded by the common codec.
fn sent(s: &Sess<'_>, step: &Step) -> (PayloadType, std::vec::Vec<u8>) {
    let bytes = &s.tx()[..step.send.expect("nothing to send")];
    let v2gtp::Parse::Frame(f) = v2gtp::parse(bytes, 512).unwrap() else { panic!("incomplete frame") };
    assert_eq!(f.consumed, bytes.len());
    (f.payload_type, f.payload.to_vec())
}

fn sent_common(s: &Sess<'_>, step: &Step, f: impl FnOnce(common::Message<'_>)) {
    let (pt, payload) = sent(s, step);
    assert_eq!(pt, PayloadType::MAINSTREAM);
    let mut mem = [0u8; 1024];
    let arena = Arena::new(&mut mem);
    f(common::decode(&payload, &arena).unwrap());
}

fn response_timer() -> Timers {
    Timers::one(TimerCmd::Arm { timer: Timer::Response, ms: 2000 })
}

/// Runs the handshake and session setup; returns the session after the
/// `AuthorizationSetupReq` was sent.
fn up_to_authorization_setup<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = session(rig, log);
    s.handle(Event::Start { secured: false });
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    assert_eq!(s.state(), State::AwaitAuthorizationSetupRes);
    s
}

/// Up to and including the first `AuthorizationReq`.
fn up_to_authorization<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = up_to_authorization_setup(rig, log);
    deliver(&mut s, &auth_setup_res(ResponseCode::Ok, &[Authorization::Eim]));
    assert_eq!(s.state(), State::AwaitAuthorizationRes);
    s
}

#[test]
fn full_flow_to_authorized() {
    let log = Capture::default();
    let seen = log.0.clone();
    let mut rig = Rig::new();
    let mut s = session(&mut rig, log);

    let step = s.handle(Event::Start { secured: true });
    assert_eq!(step.timers, response_timer());
    let (pt, payload) = sent(&s, &step);
    assert_eq!(pt, PayloadType::SAP);
    let mut mem = [0u8; 1024];
    let arena = Arena::new(&mut mem);
    let app_protocol::Message::Req(req) = app_protocol::decode(&payload, &arena).unwrap() else { panic!() };
    assert_eq!(req.app_protocols, &[Config::DC_20.offer]);

    let step = deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    assert_eq!(step.timers, response_timer());
    sent_common(&s, &step, |m| {
        let common::Message::SessionSetupReq(req) = m else { panic!("{m:?}") };
        assert_eq!(req.evcc_id, "WMIV1234567890ABCDEX");
        assert_eq!(req.header.session_id, [0; 8], "a new session starts with an all-zero id");
    });

    // SessionSetupRes: the EV learns the session id and moves on to AuthorizationSetupReq.
    let step = deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    assert_eq!(step.notice, Some(Notice::SessionSetUp { session_id: SID }));
    assert_eq!(step.timers, response_timer());
    sent_common(&s, &step, |m| {
        let common::Message::AuthorizationSetupReq(req) = m else { panic!("{m:?}") };
        assert_eq!(req.header.session_id, SID, "later messages carry the id the EVSE assigned");
    });

    let step = deliver(&mut s, &auth_setup_res(ResponseCode::Ok, &[Authorization::Pnc, Authorization::Eim]));
    sent_common(&s, &step, |m| {
        let common::Message::AuthorizationReq(req) = m else { panic!("{m:?}") };
        assert_eq!(req.header.session_id, SID);
    });

    let step = deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(step.notice, Some(Notice::Authorized));
    assert_eq!(step.send, None);
    assert_eq!(step.timers, Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Cancel(Timer::Ongoing)));
    assert_eq!(s.state(), State::Authorized);

    let seen = seen.borrow();
    for (kind, tx) in [
        (MsgKind::SupportedAppProtocolReq, true),
        (MsgKind::SessionSetupRes, false),
        (MsgKind::AuthorizationSetupReq, true),
        (MsgKind::AuthorizationRes, false),
    ] {
        assert!(seen.contains(&Seen::Msg(kind, tx)), "missing {kind:?} tx={tx}");
    }
    assert!(!seen.iter().any(|s| matches!(s, Seen::Error(_))), "{seen:?}");
}

/// Ref: ISO 15118-20:2022 V2G20-1582 (via EcoG, unchecked): while EVSEProcessing is Ongoing the
/// EVCC repeats the AuthorizationReq, unaltered except for the timestamp.
#[test]
fn ongoing_authorization_repeats_the_request_with_a_new_timestamp() {
    let mut rig = Rig::new();
    let mut s = up_to_authorization(&mut rig, Capture::default());
    let first_ts = {
        // Re-trigger so we can look at a request: the first one was already sent.
        let step = deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Ongoing));
        let mut ts = 0;
        sent_common(&s, &step, |m| {
            let common::Message::AuthorizationReq(req) = m else { panic!("{m:?}") };
            assert_eq!(req.header.session_id, SID);
            ts = req.header.timestamp;
        });
        // First Ongoing: arms the response timer and, once, the ongoing timer.
        assert_eq!(
            step.timers,
            response_timer().with(TimerCmd::Arm { timer: Timer::Ongoing, ms: 60_000 })
        );
        ts
    };
    for processing in [Processing::Ongoing, Processing::OngoingWaitingForCustomerInteraction] {
        let step = deliver(&mut s, &auth_res(ResponseCode::Ok, processing));
        assert_eq!(s.state(), State::AwaitAuthorizationRes);
        assert_eq!(step.timers, response_timer(), "the ongoing timer must not be re-armed");
        let mut ts = 0;
        sent_common(&s, &step, |m| {
            let common::Message::AuthorizationReq(req) = m else { panic!("{m:?}") };
            ts = req.header.timestamp;
        });
        assert!(ts > first_ts);
    }
    let step = deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(step.notice, Some(Notice::Authorized));
}

#[test]
fn ongoing_forever_hits_the_ongoing_timeout() {
    let mut rig = Rig::new();
    let mut s = up_to_authorization(&mut rig, Capture::default());
    for _ in 0..10 {
        deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Ongoing));
    }
    assert_eq!(s.handle(Event::TimerExpired(Timer::Ongoing)).end, Some(EndReason::Timeout));
}

#[test]
fn stale_timers_are_ignored() {
    let mut rig = Rig::new();
    let mut s = up_to_authorization(&mut rig, Capture::default());
    deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(s.state(), State::Authorized);
    // Both timers were cancelled, but an expiry may already have been in flight.
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)), Step::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Ongoing)), Step::default());
    assert_eq!(s.state(), State::Authorized);
}

#[test]
fn authorization_failures_end_the_session() {
    // EVSE offers only PnC, which this EV does not implement.
    let mut rig = Rig::new();
    let mut s = up_to_authorization_setup(&mut rig, Capture::default());
    let step = deliver(&mut s, &auth_setup_res(ResponseCode::Ok, &[Authorization::Pnc]));
    assert_eq!(step.end, Some(EndReason::NoCompatibleAuthorization));

    // AuthorizationSetupRes with a failing code.
    let mut rig = Rig::new();
    let mut s = up_to_authorization_setup(&mut rig, Capture::default());
    let step = deliver(&mut s, &auth_setup_res(ResponseCode::FailedSequenceError, &[Authorization::Eim]));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedSequenceError)));

    // AuthorizationRes: the EVSE refuses the user.
    let mut rig = Rig::new();
    let mut s = up_to_authorization(&mut rig, Capture::default());
    let step = deliver(&mut s, &auth_res(ResponseCode::WarningEIMAuthorizationFailure, Processing::Finished));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::WarningEIMAuthorizationFailure)));
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
    let step = s.handle(Event::TimerExpired(Timer::Response));
    assert_eq!(step.end, Some(EndReason::Timeout));
    assert_eq!(s.state(), State::Ended);
    // Once ended, nothing else happens.
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)), Step::default());
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
fn failing_session_setup_code_is_reported() {
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

    // A different common message than the one awaited.
    let mut rig = Rig::new();
    let mut s = up_to_authorization(&mut rig, Capture::default());
    let step = deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    assert_eq!(step.end, Some(EndReason::UnexpectedMessage));

    // The right message under the wrong payload type.
    let mut rig = Rig::new();
    let mut s = up_to_authorization(&mut rig, Capture::default());
    let wrong = framed(PayloadType::DC_MAINSTREAM, |out| {
        common::encode_authorization_res(&AuthorizationRes { header: hdr(), response_code: ResponseCode::Ok, evse_processing: Processing::Finished }, out).unwrap()
    });
    assert_eq!(deliver(&mut s, &wrong).end, Some(EndReason::UnexpectedMessage));
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
    let full = auth_res(ResponseCode::Ok, Processing::Finished);
    for cut in 0..full.len() {
        let mut rig = Rig::new();
        let mut s = up_to_authorization(&mut rig, Capture::default());
        let step = deliver(&mut s, &full[..cut]);
        assert_eq!(step, Step::default(), "cut {cut} produced output");
        assert_eq!(s.state(), State::AwaitAuthorizationRes);
    }
}

#[test]
fn tiny_tx_buffer_ends_the_session() {
    let mut rx = [0u8; 256];
    let mut tx = [0u8; 12];
    let mut arena = [0u8; 512];
    let mut s = Session::new(Config::DC_20, TestEv::new(), Capture::default(), Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    assert_eq!(s.handle(Event::Start { secured: false }).end, Some(EndReason::TxBufferTooSmall));
}

#[test]
fn small_arena_is_a_decode_error_not_a_panic() {
    let mut rx = [0u8; 256];
    let mut tx = [0u8; 256];
    let mut arena = [0u8; 8];
    let mut s = Session::new(Config::DC_20, TestEv::new(), Capture::default(), Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    s.handle(Event::Start { secured: false });
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    let step = deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    assert_eq!(step.end, Some(EndReason::Malformed(DecodeError::ArenaFull)));
}
