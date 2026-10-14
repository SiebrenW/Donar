//! Checks the hand-written `common` codec against EXIficient reference vectors.

use exi::Arena;
use iso15118_20::common::*;

fn vector(name: &str) -> std::vec::Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../vectors/{name}.hex"));
    let hex = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {path:?}; run `cargo xtask setup && cargo xtask vectors`"));
    (0..hex.trim().len() / 2)
        .map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

const TS: u64 = 1_700_000_000;

#[test]
fn session_setup_req() {
    let want = vector("session_setup_req");
    let msg = SessionSetupReq { header: MessageHeader { session_id: [0; 8], timestamp: TS }, evcc_id: "WMIV1234567890ABCDEX" };
    let mut out = [0u8; 128];
    let n = encode_session_setup_req(&msg, &mut out).unwrap();
    assert_eq!(&out[..n], &want[..]);
    let mut mem = [0u8; 512];
    let arena = Arena::new(&mut mem);
    assert_eq!(decode(&want, &arena).unwrap(), Message::SessionSetupReq(msg));
}

#[test]
fn session_setup_res() {
    for (name, code, evse) in [
        ("ok", ResponseCode::Ok, "DE*EVSE*0001"),
        ("joined", ResponseCode::OkOldSessionJoined, "DE*EVSE*0001"),
        ("unknown", ResponseCode::FailedUnknownSession, "DE*EVSE*0002"),
        ("wrongcp", ResponseCode::FailedWrongChargeParameter, "X"),
    ] {
        let want = vector(&format!("session_setup_res_{name}"));
        let msg = SessionSetupRes {
            header: MessageHeader { session_id: [1, 2, 3, 4, 5, 6, 7, 8], timestamp: TS },
            response_code: code,
            evse_id: evse,
        };
        let mut out = [0u8; 128];
        let n = encode_session_setup_res(&msg, &mut out).unwrap();
        assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
        let mut mem = [0u8; 512];
        let arena = Arena::new(&mut mem);
        assert_eq!(decode(&want, &arena).unwrap(), Message::SessionSetupRes(msg), "{name}: decode");
    }
}

#[test]
fn truncated_input_never_panics() {
    for name in ["session_setup_req", "session_setup_res_ok"] {
        let full = vector(name);
        for cut in 0..full.len() {
            let mut mem = [0u8; 512];
            let arena = Arena::new(&mut mem);
            assert!(decode(&full[..cut], &arena).is_err(), "{name} cut at {cut} decoded");
        }
    }
}

#[test]
fn oversized_identifier_is_rejected() {
    let long = "x".repeat(256);
    let msg = SessionSetupReq { header: MessageHeader { session_id: [0; 8], timestamp: 0 }, evcc_id: &long };
    assert!(encode_session_setup_req(&msg, &mut [0u8; 1024]).is_err());
}

/// Open question (see PLAN.md): EXIficient's reference for an empty string element
/// (`session_setup_res_empty`) uses an event code this codec does not understand yet, so
/// both directions must fail loudly instead of producing a wrong stream.
#[test]
fn empty_identifier_is_unsupported_not_wrong() {
    let msg = SessionSetupReq { header: MessageHeader { session_id: [0; 8], timestamp: 0 }, evcc_id: "" };
    assert!(encode_session_setup_req(&msg, &mut [0u8; 64]).is_err());
    let reference = vector("session_setup_res_empty");
    let mut mem = [0u8; 512];
    let arena = Arena::new(&mut mem);
    assert_eq!(decode(&reference, &arena), Err(exi::DecodeError::Unsupported("empty string element")));
}

// ---------------------------------------------------------------- authorization

const EVSE_SESSION: [u8; 8] = [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18];

fn hdr() -> MessageHeader {
    MessageHeader { session_id: EVSE_SESSION, timestamp: TS }
}

fn round_trip(name: &str, msg: Message<'_>, encode: impl FnOnce(&mut [u8]) -> usize) {
    let want = vector(name);
    let mut out = [0u8; 128];
    let n = encode(&mut out);
    assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
    let mut mem = [0u8; 256];
    let arena = Arena::new(&mut mem);
    assert_eq!(decode(&want, &arena).unwrap(), msg, "{name}: decode");
}

#[test]
fn authorization_setup_req() {
    let m = AuthorizationSetupReq { header: hdr() };
    round_trip("authorization_setup_req", Message::AuthorizationSetupReq(m), |o| encode_authorization_setup_req(&m, o).unwrap());
}

#[test]
fn authorization_setup_res() {
    use Authorization::{Eim, Pnc};
    for (name, services, cert) in [
        ("eim", &[Eim][..], false),
        ("eim_cert", &[Eim][..], true),
        ("eim_pnc_listed", &[Eim, Pnc][..], false),
        ("pnc_eim_listed", &[Pnc, Eim][..], true),
    ] {
        let m = AuthorizationSetupRes {
            header: hdr(),
            response_code: ResponseCode::Ok,
            authorization_services: AuthorizationServices::new(services).unwrap(),
            certificate_installation_service: cert,
        };
        round_trip(&format!("authorization_setup_res_{name}"), Message::AuthorizationSetupRes(m), |o| encode_authorization_setup_res(&m, o).unwrap());
    }
}

#[test]
fn authorization_req_eim() {
    let m = AuthorizationReq { header: hdr() };
    round_trip("authorization_req_eim", Message::AuthorizationReq(m), |o| encode_authorization_req(&m, o).unwrap());
}

#[test]
fn authorization_res() {
    for (name, code, p) in [
        ("finished", ResponseCode::Ok, Processing::Finished),
        ("ongoing", ResponseCode::Ok, Processing::Ongoing),
        ("waiting", ResponseCode::Ok, Processing::OngoingWaitingForCustomerInteraction),
        ("failed", ResponseCode::WarningEIMAuthorizationFailure, Processing::Finished),
    ] {
        let m = AuthorizationRes { header: hdr(), response_code: code, evse_processing: p };
        round_trip(&format!("authorization_res_{name}"), Message::AuthorizationRes(m), |o| encode_authorization_res(&m, o).unwrap());
    }
}

#[test]
fn authorization_truncations_never_panic() {
    for name in ["authorization_setup_res_eim_pnc_listed", "authorization_req_eim", "authorization_res_ongoing"] {
        let full = vector(name);
        for cut in 0..full.len() {
            let mut mem = [0u8; 256];
            let arena = Arena::new(&mut mem);
            assert!(decode(&full[..cut], &arena).is_err(), "{name} cut at {cut} decoded");
        }
    }
}
