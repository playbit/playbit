//! Operator-only CPU benchmark; no timing assertion or CI execution.
//! cargo run --release -p kvdb --example combine_deltas
//! Measures the publisher's 32-input fold, including validation/allocation,
//! excluding fixture creation, database I/O, projection and network transit.
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use kvdb::{
    CreateOptions, Db, Lww,
    storage::mem::MemStorage,
    sync::{CommittedDelta, DeltaReceiver, combine_deltas},
};
use std::{hint::black_box, time::Instant};

fn main() {
    for repeated in [false, true] {
        let options = CreateOptions::syncable([42; 32], 1);
        let source = Db::create(MemStorage::new(), &options).unwrap();
        let mut frames = Vec::new();
        for i in 0..32u64 {
            let key = if repeated { 0 } else { i }.to_be_bytes();
            let mut tx = source.begin_write_with_delta(1 + i, 32768).unwrap();
            tx.put(&key, &vec![i as u8; 937]).unwrap();
            let CommittedDelta::Bytes(frame) = tx.commit_with_delta().unwrap() else { panic!() };
            frames.push(frame);
        }
        let bytes: usize = frames.iter().map(Vec::len).sum();
        assert!(bytes <= 32768 && bytes > 31000, "fixture bytes: {bytes}");
        let mut times = Vec::new();
        let mut final_frame = Vec::new();
        for iteration in 0..1100 {
            let start = Instant::now();
            let mut combined = None;
            for frame in &frames[1..] {
                combined = Some(
                    combine_deltas(
                        black_box(combined.as_deref().unwrap_or(&frames[0])),
                        black_box(frame),
                        32768,
                    )
                    .unwrap(),
                );
            }
            let elapsed = start.elapsed().as_nanos();
            final_frame = black_box(combined.unwrap());
            if iteration >= 100 {
                times.push(elapsed);
            }
        }
        let replica = Db::create(MemStorage::new(), &options).unwrap();
        let mut receiver = DeltaReceiver::new(&replica, Lww, 100, 32768).unwrap();
        receiver.process(&final_frame).unwrap();
        receiver.finish().unwrap();
        assert_eq!(
            replica.begin_read().unwrap().root_hash().unwrap(),
            source.begin_read().unwrap().root_hash().unwrap()
        );
        times.sort_unstable();
        println!(
            "{{\"repeated_key\":{repeated},\"input_bytes\":{bytes},\"records\":32,\"samples\":1000,\"p50_ns\":{},\"p90_ns\":{},\"p99_ns\":{},\"max_ns\":{}}}",
            times[499], times[899], times[989], times[999]
        );
    }
}
