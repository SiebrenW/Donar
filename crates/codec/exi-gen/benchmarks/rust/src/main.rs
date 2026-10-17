//! Decodes every document of a corpus (name, group, hex per line), encodes it again, compares the
//! bytes and times both directions. Prints the lines the other harnesses print:
//! `RESULT lib group ok total decode_ns encode_ns arena_bytes` and `MISMATCH lib group name why`.

use std::hint::black_box;
use std::time::Instant;

use exi::Arena;

struct Vector {
    name: String,
    group: String,
    bytes: Vec<u8>,
    schema: String,
}

fn corpus(path: &str) -> Vec<Vector> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (name, group, hex, schema) = (f.next()?, f.next()?, f.next()?, f.next()?);
            let bytes = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
            Some(Vector { name: name.into(), group: group.into(), bytes, schema: schema.into() })
        })
        .collect()
}

/// Nanoseconds per call: runs `f` for about 3 ms.
fn time(mut f: impl FnMut()) -> f64 {
    let start = Instant::now();
    let mut calls = 0u64;
    loop {
        for _ in 0..16 {
            f();
        }
        calls += 16;
        let elapsed = start.elapsed();
        if elapsed.as_secs_f64() >= 3e-3 {
            return elapsed.as_nanos() as f64 / calls as f64;
        }
    }
}

#[derive(Default)]
struct Tally {
    ok: u32,
    total: u32,
    decode_ns: f64,
    encode_ns: f64,
    arena: usize,
}

impl Tally {
    fn report(&self, group: &str) {
        let n = f64::from(self.ok.max(1));
        println!("RESULT\texi-rs\t{group}\t{}\t{}\t{:.1}\t{:.1}\t{}", self.ok, self.total, self.decode_ns / n, self.encode_ns / n, self.arena);
    }
}

/// Round-trips and times the vectors of one group with the `decode` function in scope: decode with
/// an arena, encode the message into a buffer, compare with the input. Decoding is timed on its
/// own (the arena is reset, not cleared), encoding on its own from a message decoded once.
macro_rules! run_group {
    ($group:expr, $vectors:expr) => {{
        let mut t = Tally::default();
        let mut mem = vec![0u8; 1 << 16];
        for v in $vectors.iter().filter(|v| v.group == $group) {
            t.total += 1;
            let mut out = [0u8; 1024];
            let mut arena = Arena::new(&mut mem);
            let message = match decode(&v.bytes, &v.schema, &arena) {
                Ok(m) => m,
                Err(e) => {
                    println!("MISMATCH\texi-rs\t{}\t{}\tdecode: {e}", $group, v.name);
                    continue;
                }
            };
            match message.encode(&mut out) {
                Ok(n) if out[..n] == v.bytes[..] => {}
                Ok(n) => {
                    println!("MISMATCH\texi-rs\t{}\t{}\tre-encoded differently ({n} bytes, was {})", $group, v.name, v.bytes.len());
                    continue;
                }
                Err(e) => {
                    println!("MISMATCH\texi-rs\t{}\t{}\tencode: {e:?}", $group, v.name);
                    continue;
                }
            }
            t.arena = t.arena.max(arena.used());
            t.encode_ns += time(|| {
                black_box(message.encode(black_box(&mut out)).ok());
            });
            t.ok += 1;
            t.decode_ns += time(|| {
                arena.reset();
                black_box(decode(black_box(&v.bytes), &v.schema, &arena).is_ok());
            });
        }
        t.report($group);
    }};
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "corpus.txt".into());
    let vectors = corpus(&path);
    let _ = &vectors;
    #[cfg(feature = "din")]
    {
        use din70121::def::V2gMessage;
        fn decode<'a>(b: &[u8], _schema: &str, a: &'a Arena<'_>) -> Result<V2gMessage<'a>, String> {
            V2gMessage::decode(b, a).map_err(|e| format!("{e:?}"))
        }
        run_group!("din", vectors);
    }
    #[cfg(feature = "iso2")]
    {
        use iso15118_2::def::V2gMessage;
        fn decode<'a>(b: &[u8], _schema: &str, a: &'a Arena<'_>) -> Result<V2gMessage<'a>, String> {
            V2gMessage::decode(b, a).map_err(|e| format!("{e:?}"))
        }
        run_group!("iso2", vectors);
    }
    #[cfg(feature = "iso20")]
    {
        use iso15118_20::{ac, common, dc};
        // The schema of a document is told by the payload type on the wire; the corpus carries it.
        enum Any<'a> {
            Common(common::Message<'a>),
            Dc(dc::Message<'a>),
            Ac(ac::Message<'a>),
        }
        impl Any<'_> {
            fn encode(&self, out: &mut [u8]) -> Result<usize, exi::EncodeError> {
                match self {
                    Any::Common(m) => m.encode(out),
                    Any::Dc(m) => m.encode(out),
                    Any::Ac(m) => m.encode(out),
                }
            }
        }
        fn decode<'a>(b: &[u8], schema: &str, a: &'a Arena<'_>) -> Result<Any<'a>, String> {
            match schema {
                "dc" => dc::decode(b, a).map(Any::Dc),
                "ac" => ac::decode(b, a).map(Any::Ac),
                _ => common::decode(b, a).map(Any::Common),
            }
            .map_err(|e| format!("{e:?}"))
        }
        run_group!("iso20", vectors);
    }
}
