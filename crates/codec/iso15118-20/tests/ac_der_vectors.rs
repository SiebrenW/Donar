//! The AC DER messages (payload types 0x8010, 0x8011) against EXIficient (`cargo xtask vectors`): each vector is decoded by
//! the codec of its schema, must be the alternative its name says, and must encode to the same bytes. The inputs are made by
//! `crates/xtask/tools/ac_der_vectors.py` (every member of the substitution group of each message, with only the mandatory
//! elements and with all of them).

use exi::Arena;

fn vectors(variant: &str) -> Vec<(String, Vec<u8>)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data/vectors");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|_| panic!("missing {dir:?}; run `cargo xtask setup && cargo xtask vectors`")) {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        if path.extension().is_some_and(|e| e == "hex") && name.starts_with(&format!("acder_{variant}_")) {
            let hex = std::fs::read_to_string(&path).unwrap();
            let bytes = (0..hex.trim().len() / 2).map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap()).collect();
            found.push((name, bytes));
        }
    }
    found.sort();
    found
}

/// `name` is `acder_<iec|sae>_<cpd_req|cpd_res|cl_req|cl_res>_<alternative>_<min|max>`; the Rust variant of the alternative.
fn variant(name: &str) -> &'static str {
    let rest = name.splitn(4, '_').nth(3).unwrap();
    let alt = rest.rsplit_once('_').unwrap().0;
    let alt = alt.split_once('_').unwrap().1; // after req/res
    let cpd = name.contains("_cpd_");
    match (cpd, alt) {
        (true, "ac") => "Base(",
        (true, "bpt_ac") => "Bpt(",
        (true, "der_ac") => "Der(",
        (false, "dynamic_ac") => "DynamicAc(",
        (false, "scheduled_ac") => "ScheduledAc(",
        (false, "bpt_dynamic_ac") => "BptDynamicAc(",
        (false, "bpt_scheduled_ac") => "BptScheduledAc(",
        (false, "der_dynamic_ac") => "DerDynamicAc(",
        (false, "der_scheduled_ac") => "DerScheduledAc(",
        _ => panic!("{name}: {alt}"),
    }
}

/// `want` as a whole word in the Debug text (`DynamicAc(` must not match inside `BptDynamicAc(`).
fn has(text: &str, want: &str) -> bool {
    text.match_indices(want).any(|(i, _)| i == 0 || !text.as_bytes()[i - 1].is_ascii_alphabetic())
}

macro_rules! check_all {
    ($test:ident, $variant:literal, $module:ident) => {
        #[test]
        fn $test() {
            let all = vectors($variant);
            assert_eq!(all.len(), 36, "{:?}", all.iter().map(|(n, _)| n).collect::<Vec<_>>());
            for (name, bytes) in all {
                let mut mem = vec![0u8; 65536];
                let arena = Arena::new(&mut mem);
                let msg = iso15118_20::$module::decode(&bytes, &arena).unwrap_or_else(|e| panic!("{name}: decode failed: {e:?}"));
                let text = format!("{msg:?}");
                assert!(has(&text, variant(&name)), "{name}: not the alternative its name says: {}", &text[..text.len().min(300)]);
                let mut out = vec![0u8; 8192];
                let n = msg.encode(&mut out).unwrap_or_else(|e| panic!("{name}: encode failed: {e:?}"));
                assert_eq!(&out[..n], &bytes[..], "{name}: encode differs from EXIficient");
            }
        }
    };
}

check_all!(ac_der_iec, "iec", ac_der_iec);
check_all!(ac_der_sae, "sae", ac_der_sae);
