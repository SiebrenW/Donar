//! Checks the generated codec against reference vectors made by EXIficient (`cargo xtask
//! vectors`): every vector decodes, and encoding what was decoded gives the same bytes.

use iso15118_2::def::V2gMessage;
use exi::Arena;

fn vector(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../../data/vectors/{name}.hex"));
    let hex = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {path:?}; run `cargo xtask setup && cargo xtask vectors`"));
    (0..hex.trim().len() / 2).map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap()).collect()
}

/// Every vector EXIficient made for the DC session decodes, and encoding what was decoded gives
/// the same bytes: the grammar of every message, option and repeated element matches.
#[test]
fn every_vector_round_trips() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data/vectors");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str().and_then(|n| n.strip_suffix(".hex").map(str::to_string)))
        .filter(|n| n.starts_with("iso2_"))
        .collect();
    names.sort();
    assert!(names.len() >= 52, "run `cargo xtask vectors`");
    let mut failures = Vec::new();
    for name in &names {
        let want = vector(name);
        let mut mem = [0u8; 65536];
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

/// The parts of a message that are signed are EXI fragments: the start event is chosen among all
/// element declarations of the schema, and the stream ends with the end of the document.
/// EXIficient's fragment encoding of an `AuthorizationReq` and a `SignedInfo`.
#[test]
fn signed_fragments() {
    use iso15118_2::body::AuthorizationReq;
    use iso15118_2::signature::{self, Reference, SignedInfo, encode_fragment, signed_info_fragment};
    let challenge: [u8; 16] = std::array::from_fn(|i| i as u8);
    let req = AuthorizationReq { id: Some("ID1"), gen_challenge: Some(challenge) };
    let mut out = [0u8; 256];
    let n = encode_fragment(&mut out, signature::fragment::AUTHORIZATION_REQ, |e| req.encode_content(e)).unwrap();
    assert_eq!(&out[..n], &vector("fragment_iso2_authorization_req")[..]);

    let digest: Vec<u8> = (0..32).collect();
    let reference = Reference { id: None, uri: Some("#ID1"), transform_algorithm: Some(signature::CANONICAL_EXI), digest_algorithm: signature::DIGEST_SHA256, digest_value: &digest };
    let info = SignedInfo { id: None, canonicalization_algorithm: signature::CANONICAL_EXI, signature_algorithm: signature::ECDSA_SHA256, reference: &[reference] };
    let mut out = [0u8; 512];
    let n = signed_info_fragment(&info, &mut out).unwrap();
    assert_eq!(&out[..n], &vector("fragment_iso2_signed_info")[..]);
}
