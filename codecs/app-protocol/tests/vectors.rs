//! Checks the hand-written codec against EXIficient reference vectors.
//! Run `cargo xtask setup && cargo xtask vectors` first.

use exi::Arena;
use app_protocol::*;

fn vector(name: &str) -> std::vec::Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../vectors/{name}.hex"));
    let hex = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {path:?}; run `cargo xtask setup && cargo xtask vectors`"));
    (0..hex.trim().len() / 2)
        .map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn dc(schema_id: u8, priority: u8) -> AppProtocol<'static> {
    AppProtocol { namespace: "urn:iso:std:iso:15118:-20:DC", version_major: 1, version_minor: 0, schema_id, priority }
}

fn check_req(name: &str, entries: &[AppProtocol<'_>]) {
    let want = vector(name);
    let req = SupportedAppProtocolReq { app_protocols: entries };
    let mut out = [0u8; 512];
    let n = encode_req(&req, &mut out).unwrap();
    assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");

    let mut mem = [0u8; 4096];
    let arena = Arena::new(&mut mem);
    assert_eq!(decode(&want, &arena).unwrap(), Message::Req(req), "{name}: decode");
}

fn check_res(name: &str, res: SupportedAppProtocolRes) {
    let want = vector(name);
    let mut out = [0u8; 16];
    let n = encode_res(&res, &mut out).unwrap();
    assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
    let mut mem = [0u8; 64];
    let arena = Arena::new(&mut mem);
    assert_eq!(decode(&want, &arena).unwrap(), Message::Res(res), "{name}: decode");
}

#[test]
fn req_one_entry() {
    check_req("app_protocol_req", &[dc(1, 1)]);
}

#[test]
fn req_two_entries() {
    let ac = AppProtocol { namespace: "urn:iso:std:iso:15118:-20:AC", schema_id: 2, priority: 2, ..dc(2, 2) };
    check_req("app_protocol_req_2", &[dc(1, 1), ac]);
}

#[test]
fn req_twenty_entries() {
    let entries: std::vec::Vec<_> = (1..=20).map(|i| dc(i, i)).collect();
    check_req("app_protocol_req_20", &entries);
}

#[test]
fn res_variants() {
    check_res("app_protocol_res_ok", SupportedAppProtocolRes { response_code: ResponseCode::OkSuccessfulNegotiation, schema_id: Some(1) });
    check_res("app_protocol_res_minor", SupportedAppProtocolRes { response_code: ResponseCode::OkSuccessfulNegotiationWithMinorDeviation, schema_id: Some(2) });
    check_res("app_protocol_res_failed", SupportedAppProtocolRes { response_code: ResponseCode::FailedNoNegotiation, schema_id: None });
}

#[test]
fn truncated_input_never_panics() {
    for name in ["app_protocol_req", "app_protocol_req_2", "app_protocol_req_20", "app_protocol_res_ok", "app_protocol_res_failed"] {
        let full = vector(name);
        for cut in 0..full.len() {
            let mut mem = [0u8; 4096];
            let arena = Arena::new(&mut mem);
            assert!(decode(&full[..cut], &arena).is_err(), "{name} cut at {cut} decoded");
        }
    }
}

#[test]
fn small_arena_is_an_error_not_a_panic() {
    let full = vector("app_protocol_req_2");
    let mut mem = [0u8; 16];
    let arena = Arena::new(&mut mem);
    assert_eq!(decode(&full, &arena), Err(exi::DecodeError::ArenaFull));
}

#[test]
fn arena_is_reusable_after_reset() {
    let full = vector("app_protocol_req_20");
    let mut mem = [0u8; 2048];
    let mut arena = Arena::new(&mut mem);
    for _ in 0..3 {
        assert!(matches!(decode(&full, &arena), Ok(Message::Req(_))));
        arena.reset();
    }
}
