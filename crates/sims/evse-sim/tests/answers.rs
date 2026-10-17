//! What the simulated charger puts in its headers and says to a request out of turn.

use app_protocol::{AppProtocol, SupportedAppProtocolReq};
use evse_sim::{Config, Evse};
use exi::{Arena, EncodeError};
use iso15118_20::common::{self, AuthorizationSetupReq, ServiceDiscoveryReq, SessionSetupReq};
use iso15118_20::common_types::{MessageHeader, ResponseCode};
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

fn framed(payload_type: PayloadType, encode: impl FnOnce(&mut [u8]) -> Result<usize, EncodeError>) -> Vec<u8> {
    let mut buf = vec![0u8; 2048];
    let n = encode(&mut buf[HEADER_LEN..]).unwrap();
    write_header(&mut buf, payload_type, n).unwrap();
    buf.truncate(HEADER_LEN + n);
    buf
}

fn session_setup() -> Vec<u8> {
    framed(PayloadType::MAINSTREAM, |o| SessionSetupReq { header: MessageHeader::new([0; 8], 0), evccid: "DE8VAA0000453C4D58Y2" }.encode(o))
}

fn start(evse: &mut Evse) -> [u8; 8] {
    let offer = [AppProtocol { protocol_namespace: "urn:iso:std:iso:15118:-20:DC", version_number_major: 1, version_number_minor: 0, schema_id: 1, priority: 1 }];
    let sap = framed(PayloadType::SAP, |o| SupportedAppProtocolReq { app_protocol: &offer }.encode(o));
    assert!(evse.handle(&sap)[0].send.is_some());
    let res = evse.handle(&session_setup());
    decode(&res[0].send.clone().unwrap(), |m| match m {
        common::Message::SessionSetupRes(r) => r.header.session_id,
        other => panic!("{other:?}"),
    })
    .0
}

/// Decodes the payload of a frame as a common message and lets `f` take it apart.
fn decode<T>(frame: &[u8], f: impl FnOnce(common::Message<'_>) -> T) -> (T, usize) {
    let Ok(Parse::Frame(p)) = parse(frame, 1 << 20) else { panic!("not a frame") };
    let mut mem = vec![0u8; 16384];
    let arena = Arena::new(&mut mem);
    (f(common::decode(p.payload, &arena).unwrap()), p.consumed)
}

#[test]
fn time_stamps_are_microseconds_and_increase_with_every_message() {
    let mut evse = Evse::new(Config::default());
    let offer = [AppProtocol { protocol_namespace: "urn:iso:std:iso:15118:-20:DC", version_number_major: 1, version_number_minor: 0, schema_id: 1, priority: 1 }];
    evse.handle(&framed(PayloadType::SAP, |o| SupportedAppProtocolReq { app_protocol: &offer }.encode(o)));
    let setup = evse.handle(&session_setup());
    let (session_id, first, _) = decode(&setup[0].send.clone().unwrap(), |m| match m {
        common::Message::SessionSetupRes(r) => (r.header.session_id, r.header.time_stamp, r.response_code),
        other => panic!("{other:?}"),
    })
    .0;
    // V2G20-1534: microseconds since the epoch, which is around 1.8e15 this decade.
    assert!(first > 1_000_000_000_000_000, "{first} is not microseconds");
    let mut last = first;
    let header = || MessageHeader::new(session_id, 0);
    let requests = [
        framed(PayloadType::MAINSTREAM, |o| AuthorizationSetupReq { header: header() }.encode(o)),
        framed(PayloadType::MAINSTREAM, |o| {
            common::AuthorizationReq { header: header(), selected_authorization_service: common::Authorization::Eim, a_req_authorization_mode: common::AReqAuthorizationModeChoice::Eim(common::EimAReqAuthorizationMode {}) }.encode(o)
        }),
        framed(PayloadType::MAINSTREAM, |o| ServiceDiscoveryReq { header: header(), supported_service_ids: None }.encode(o)),
    ];
    for req in requests {
        let res = evse.handle(&req);
        let stamp = decode(&res[0].send.clone().unwrap(), |m| match m {
            common::Message::AuthorizationSetupRes(r) => r.header.time_stamp,
            common::Message::AuthorizationRes(r) => r.header.time_stamp,
            common::Message::ServiceDiscoveryRes(r) => r.header.time_stamp,
            other => panic!("{other:?}"),
        })
        .0;
        assert!(stamp > last, "{stamp} does not follow {last} (V2G20-1537)");
        last = stamp;
    }
}

#[test]
fn a_request_out_of_turn_is_answered_with_a_sequence_error_of_its_own_type() {
    let mut evse = Evse::new(Config::default());
    let session_id = start(&mut evse);
    // The charger waits for AuthorizationSetupReq; a ServiceDiscoveryReq is out of sequence.
    let req = framed(PayloadType::MAINSTREAM, |o| ServiceDiscoveryReq { header: MessageHeader::new(session_id, 0), supported_service_ids: None }.encode(o));
    let out = evse.handle(&req);
    let (code, _) = decode(&out[0].send.clone().unwrap(), |m| match m {
        common::Message::ServiceDiscoveryRes(r) => r.response_code,
        other => panic!("answered with {other:?}"),
    });
    assert_eq!(code, ResponseCode::FailedSequenceError);
    assert!(out[0].close, "a failed response ends the session");
    assert!(evse.take_log().iter().any(|l| l.contains("FAILED_SequenceError")));
}

#[test]
fn a_wrong_session_id_is_answered_with_the_response_of_the_request() {
    let mut evse = Evse::new(Config::default());
    start(&mut evse);
    // AuthorizationSetupReq with another id: the answer is AuthorizationSetupRes with FAILED_UnknownSession.
    let req = framed(PayloadType::MAINSTREAM, |o| AuthorizationSetupReq { header: MessageHeader::new([9; 8], 0) }.encode(o));
    let out = evse.handle(&req);
    let (code, _) = decode(&out[0].send.clone().unwrap(), |m| match m {
        common::Message::AuthorizationSetupRes(r) => r.response_code,
        other => panic!("answered with {other:?}"),
    });
    assert_eq!(code, ResponseCode::FailedUnknownSession);
}
