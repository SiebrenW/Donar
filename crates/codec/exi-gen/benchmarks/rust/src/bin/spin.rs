//! Decodes (or encodes) the DIN corpus over and over for a profiler: `spin decode|encode corpus.txt seconds`.
use exi::Arena;
use din70121::def::V2gMessage;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let docs: Vec<Vec<u8>> = std::fs::read_to_string(&a[2]).unwrap().lines().filter_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        (f[1] == "din").then(|| (0..f[2].len() / 2).map(|i| u8::from_str_radix(&f[2][2 * i..2 * i + 2], 16).unwrap()).collect())
    }).collect();
    let secs: f64 = a[3].parse().unwrap();
    let mut mem = vec![0u8; 1 << 16];
    let mut arena = Arena::new(&mut mem);
    let start = std::time::Instant::now();
    let mut n = 0u64;
    let encode = a[1] == "encode";
    let mut out = [0u8; 1024];
    while start.elapsed().as_secs_f64() < secs {
        for d in &docs {
            arena.reset();
            let m = V2gMessage::decode(d, &arena);
            if encode { if let Ok(m) = &m { std::hint::black_box(m.encode(&mut out).ok()); } } else { std::hint::black_box(m.is_ok()); }
            n += 1;
        }
    }
    println!("{:.1} ns/doc", start.elapsed().as_nanos() as f64 / n as f64);
}
