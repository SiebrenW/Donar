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
    assert!(names.len() >= 51, "run `cargo xtask vectors`");
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
