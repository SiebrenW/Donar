//! Codec throughput over all reference vectors (`cargo xtask vectors` first):
//! `cargo bench -p iso15118-20`. Plain `std::time`, no benchmark framework.

use std::hint::black_box;
use std::time::{Duration, Instant};

use exi::Arena;
use iso15118_20::{common, dc};

enum Msg<'a> {
    Common(common::Message<'a>),
    Dc(dc::Message<'a>),
}

impl Msg<'_> {
    fn encode(&self, out: &mut [u8]) -> usize {
        match self {
            Msg::Common(m) => m.encode(out).unwrap(),
            Msg::Dc(m) => m.encode(out).unwrap(),
        }
    }
}

fn decode<'a>(bytes: &[u8], arena: &'a Arena<'_>) -> Option<Msg<'a>> {
    if let Ok(m) = common::decode(bytes, arena) {
        return Some(Msg::Common(m));
    }
    dc::decode(bytes, arena).ok().map(Msg::Dc)
}

fn vectors() -> Vec<(String, Vec<u8>)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/vectors");
    let mut v: Vec<_> = std::fs::read_dir(&dir)
        .expect("run `cargo xtask vectors` first")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "hex"))
        .filter(|e| !e.file_name().to_string_lossy().starts_with("app_protocol"))
        .map(|e| {
            let hex = std::fs::read_to_string(e.path()).unwrap();
            let hex = hex.trim();
            let bytes = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
            (e.path().file_stem().unwrap().to_string_lossy().into_owned(), bytes)
        })
        .collect();
    v.sort();
    v
}

/// Runs `f` for about `budget` and returns the nanoseconds per call.
fn time(budget: Duration, mut f: impl FnMut()) -> f64 {
    for _ in 0..200 {
        f();
    }
    let mut runs = 0u64;
    let start = Instant::now();
    while start.elapsed() < budget {
        for _ in 0..100 {
            f();
        }
        runs += 100;
    }
    start.elapsed().as_nanos() as f64 / runs as f64
}

fn main() {
    let vectors = vectors();
    let mut mem = vec![0u8; 1 << 16];
    let mut out = vec![0u8; 1 << 14];
    // `PROFILE=decode|encode cargo bench ...` only loops for a profiler to sample.
    if let Ok(what) = std::env::var("PROFILE") {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(12) {
            for (_, bytes) in &vectors {
                let arena = Arena::new(&mut mem);
                if what == "decode" {
                    black_box(decode(black_box(bytes), &arena));
                } else if let Some(m) = decode(bytes, &arena) {
                    black_box(m.encode(black_box(&mut out)));
                }
            }
        }
        return;
    }
    let budget = Duration::from_millis(300);
    let (mut total_dec, mut total_enc, mut total_bytes) = (0.0, 0.0, 0usize);
    println!("{:<44} {:>6} {:>10} {:>10}", "vector", "bytes", "decode ns", "encode ns");
    let mut rows = 0;
    for (name, bytes) in &vectors {
        // The per-message numbers: decode, and encode the decoded message again.
        if decode(bytes, &Arena::new(&mut mem)).is_none() {
            continue;
        }
        let dec = time(budget, || {
            let arena = Arena::new(&mut mem);
            black_box(decode(black_box(bytes), &arena));
        });
        let arena = Arena::new(&mut mem);
        let msg = decode(bytes, &arena).unwrap();
        let enc = time(budget, || {
            black_box(msg.encode(black_box(&mut out)));
        });
        total_dec += dec;
        total_enc += enc;
        total_bytes += bytes.len();
        rows += 1;
        if std::env::args().any(|a| a == "--all") || ["dc_charge_loop_res", "dc_cpd_req", "schedule_exchange_res_full", "service_detail_res_kinds", "session_setup_req"].contains(&name.as_str()) {
            println!("{name:<44} {:>6} {dec:>10.0} {enc:>10.0}", bytes.len());
        }
    }
    println!("{:<44} {:>6} {:>10.0} {:>10.0}  (sum over {rows} messages)", "all", total_bytes, total_dec, total_enc);
    println!(
        "decode {:.1} MB/s, encode {:.1} MB/s",
        total_bytes as f64 / total_dec * 1e3,
        total_bytes as f64 / total_enc * 1e3
    );
}
