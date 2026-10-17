//! Checks the generated codec byte for byte against reference vectors made by EXIficient
//! (`cargo xtask vectors`), in both directions.

use din70121::body::*;
use din70121::def::{BodyElementChoice, Body, V2gMessage};
use din70121::header::MessageHeader;
use exi::Arena;

fn vector(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../../data/vectors/{name}.hex"));
    let hex = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {path:?}; run `cargo xtask setup && cargo xtask vectors`"));
    (0..hex.trim().len() / 2).map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap()).collect()
}

const SESSION: [u8; 8] = [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18];

fn message(session: &'static [u8], body: BodyElementChoice<'static>) -> V2gMessage<'static> {
    V2gMessage { header: MessageHeader { session_id: session, notification: None, signature: None }, body: Body { body_element: Some(body) } }
}

/// Encodes with the codec, compares with the vector, decodes the vector and compares with `msg`.
fn check(name: &str, msg: V2gMessage<'static>) {
    let want = vector(name);
    let mut out = [0u8; 2048];
    let n = msg.encode(&mut out).unwrap_or_else(|e| panic!("{name}: encode failed: {e:?}"));
    assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
    let mut mem = [0u8; 16384];
    let arena = Arena::new(&mut mem);
    assert_eq!(V2gMessage::decode(&want, &arena).unwrap_or_else(|e| panic!("{name}: {e:?}")), msg, "{name}: decode");
}

#[test]
fn session_setup_req() {
    check("din_session_setup_req", message(&[0], BodyElementChoice::SessionSetupReq(SessionSetupReq { evccid: &[0x0A, 0x1B, 0x2C, 0x3D, 0x4E, 0x5F] })));
}

/// Every vector EXIficient made for the DC session decodes, and encoding what was decoded gives
/// the same bytes: the grammar of every message, option and repeated element matches.
#[test]
fn every_vector_round_trips() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data/vectors");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str().and_then(|n| n.strip_suffix(".hex").map(str::to_string)))
        .filter(|n| n.starts_with("din_"))
        .collect();
    names.sort();
    assert!(names.len() >= 41, "run `cargo xtask vectors`");
    let mut failures = Vec::new();
    for name in &names {
        let want = vector(name);
        let mut mem = [0u8; 32768];
        let arena = Arena::new(&mut mem);
        let msg = match V2gMessage::decode(&want, &arena) {
            Ok(m) => m,
            Err(e) => {
                failures.push(format!("{name}: decode {e:?}"));
                continue;
            }
        };
        let mut out = [0u8; 4096];
        match msg.encode(&mut out) {
            Ok(n) if out[..n] == want[..] => {}
            Ok(n) => failures.push(format!("{name}: re-encoded differs ({n} bytes, want {})", want.len())),
            Err(e) => failures.push(format!("{name}: encode {e:?}")),
        }
    }
    assert!(failures.is_empty(), "{} of {} vectors failed:\n{}", failures.len(), names.len(), failures.join("\n"));
}
