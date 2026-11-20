//! Warm canonical identifier conversion, independent of storage, transport and UI.
//! Run optimized; retain the individual wall observations, including outliers.
use playbit::identifier;
use serde_json::{Value, json};
use std::{hint::black_box, time::Instant};

const SAMPLES: usize = 4096;

fn measure<const N: usize>() -> Value {
    let mut seed = 0xd1b54a32d192ed03u64;
    let mut cases = vec![[0u8; N]; SAMPLES];
    cases[1].fill(255);
    for bytes in cases.iter_mut().skip(2) {
        for byte in bytes {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *byte = seed as u8;
        }
    }
    let text: Vec<_> = cases.iter().map(|bytes| identifier::encode(bytes)).collect();
    for (bytes, text) in cases.iter().zip(&text) {
        assert_eq!(identifier::decode::<N>(text), Ok(*bytes));
        black_box(identifier::encode(black_box(bytes)));
        black_box(identifier::decode::<N>(black_box(text)).expect("generated canonical ID"));
    }
    let mut encode_ns = Vec::with_capacity(SAMPLES);
    let mut decode_ns = Vec::with_capacity(SAMPLES);
    for bytes in &cases {
        let start = Instant::now();
        black_box(identifier::encode(black_box(bytes)));
        encode_ns.push(start.elapsed().as_nanos());
    }
    for text in &text {
        let start = Instant::now();
        black_box(identifier::decode::<N>(black_box(text)).expect("generated canonical ID"));
        decode_ns.push(start.elapsed().as_nanos());
    }
    json!({"width":N, "encode_ns":encode_ns, "decode_ns":decode_ns})
}

fn main() {
    println!(
        "{}",
        json!({
            "boundary":"single-thread warm wall time; encode includes String allocation/drop, decode returns bytes; Instant overhead/resolution retained; no storage, transport, UI or CPU-only timing",
            "dataset":"deterministic opaque IDs, including zero/max edge values; synthetic and not secret",
            "samples_per_stage":SAMPLES,
            "results":[measure::<15>(),measure::<16>(),measure::<32>()],
        })
    );
}
