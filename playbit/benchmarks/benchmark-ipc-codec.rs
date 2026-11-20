//! Explicit optimized codec comparison; no sockets, renderer or timed store work.
//! Current management, shared data and local GUI control messages. Operator-only timing.
#[path = "ipc-codec-allocations.rs"]
mod allocations;
use playbit::{
    engine::{Request, ipc::codec, reply::Reply},
    store::Id,
    ui::app_control,
};
use rkyv::{
    api::low::{LowSerializer, LowValidator},
    bytecheck::CheckBytes,
    rancor::Error,
    ser::allocator::SubAllocator,
};
use serde_json::{Value, json};
use std::{hint::black_box, time::Instant};

const SAMPLES: usize = 1000;
const WARMUP: usize = 100;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> Result<()> {
    if std::env::args_os().len() != 1 {
        return Err("Usage: benchmark-ipc-codec".into());
    }
    let id = Id::from_bytes([1; 16]);
    let mut results = vec![measure::<Request>("status_request", &Request::Status, |bytes| {
        black_box(serde_json::from_slice::<Request>(bytes)?);
        Ok(())
    })?];
    results.push(measure::<app_control::Request>(
        "local_input",
        &app_control::Request {
            instance: id,
            generation: 1,
            operation: app_control::Operation::Input { kind: 2, target: 100, data: vec![] },
        },
        json_value,
    )?);
    results.push(measure::<app_control::Reply>(
        "local_logs_64k",
        &app_control::Reply::Logs(app_control::LogBatch {
            cursor: app_control::LogCursor { instance: id, generation: 1, next: 256 },
            lines: vec![
                app_control::LogEntry::recorded(
                    "x".repeat(256),
                    app_control::LogSeverity::Info
                );
                256
            ],
            dropped: 0,
            reset: false,
        }),
        json_value,
    )?);
    results.push(measure::<Reply>(
        "protocol_reply",
        &Reply::Protocol { protocol: codec::ABI, version: env!("CARGO_PKG_VERSION").into() },
        json_value,
    )?);
    results.push(measure::<Request>(
        "blobs_32k",
        &Request::Syscall {
            context: playbit::engine::service_call::Context::Tool { handle: id, incarnation: id },
            operation: 1,
            mutation: 1,
            call: Box::new(playbit::runtime::host::DataCall {
                volume: *id.as_bytes(),
                operation: playbit::runtime::host::DataOperation::BloblogAppend {
                    append_id: [1; 16],
                    log: [3; 15],
                    blobs: vec![(0..32768).map(|i| i as u8).collect()],
                },
            }),
        },
        |bytes| {
            black_box(serde_json::from_slice::<Request>(bytes)?);
            Ok(())
        },
    )?);
    // Opaque synthetic payload exercises only the GUI-control codec ceiling.
    // It makes no claim about a valid rendered scene or guest execution.
    results.push(measure::<app_control::Reply>(
        "local_raw_1m_scene",
        &app_control::Reply::Frame {
            instance: id,
            generation: 1,
            build: playbit::identifier::encode(&[0xab; 32]),
            scene: (0..1024 * 1024).map(|i| i as u8).collect(),
        },
        json_value,
    )?);
    println!(
        "{}",
        json!({
        "boundary":"in-memory encode and decode only; JSON parses owned values, rkyv validates borrowed archives; no sockets, guest execution, store, scene decoding or rendering in timing",
        "allocation_boundary":"one separate untimed pass per stage on the benchmark thread; requested heap bytes include reallocations, not peak live memory or RSS; timing retains the disabled allocator-hook branch",
            "warmup":WARMUP, "samples":SAMPLES, "results":results,
        })
    );
    Ok(())
}

fn json_value(bytes: &[u8]) -> Result<()> {
    black_box(serde_json::from_slice::<Value>(bytes)?);
    Ok(())
}
fn measure<T>(name: &str, value: &T, decode_json: impl Fn(&[u8]) -> Result<()>) -> Result<Value>
where
    T: serde::Serialize
        + rkyv::Archive
        + for<'a> rkyv::Serialize<LowSerializer<codec::Output, SubAllocator<'a>, Error>>,
    T::Archived: rkyv::Portable
        + for<'a> CheckBytes<LowValidator<'a, Error>>
        + rkyv::Deserialize<T, rkyv::rancor::Strategy<(), Error>>,
{
    let json_bytes = serde_json::to_vec(value)?;
    let archive = codec::encode(value, codec::MAX_ARCHIVE)?;
    let archived = codec::access::<T::Archived>(&archive)?;
    let restored: T = rkyv::api::low::deserialize::<T, Error>(archived)?;
    if serde_json::to_value(restored)? != serde_json::to_value(value)? {
        return Err("codec round trip changed payload or metadata".into());
    }
    let mut samples: [Vec<u64>; 4] = std::array::from_fn(|_| Vec::with_capacity(SAMPLES));
    let stage_action = |stage| -> Result<()> {
        match stage {
            0 => {
                black_box(serde_json::to_vec(black_box(value))?);
            }
            1 => {
                decode_json(black_box(&json_bytes))?;
            }
            2 => {
                black_box(codec::encode(black_box(value), codec::MAX_ARCHIVE)?);
            }
            3 => {
                black_box(codec::access::<T::Archived>(black_box(&archive))?);
            }
            _ => unreachable!(),
        }
        Ok(())
    };
    for i in 0..WARMUP + SAMPLES {
        // Rotate order to avoid giving one codec all cold or all warm runs.
        for offset in 0..4 {
            let stage = (i + offset) % 4;
            let start = Instant::now();
            stage_action(stage)?;
            let nanos = start.elapsed().as_nanos() as u64;
            if i >= WARMUP {
                samples[stage].push(nanos);
            }
        }
    }
    let mut allocations = Vec::new();
    for stage in 0..4 {
        let (result, counts) = allocations::measure(|| stage_action(stage));
        result?;
        allocations.push(counts);
    }
    let stages: Vec<_> = ["json_encode", "json_decode", "rkyv_encode", "rkyv_validate_borrow"]
        .into_iter()
        .zip(samples)
        .zip(allocations)
        .map(|((name, samples), allocations)| {
            let mut sorted = samples.clone();
            sorted.sort_unstable();
            json!({"stage":name, "p50_us":sorted[499] as f64 / 1000.,
                "p99_us":sorted[989] as f64 / 1000., "max_us":sorted[999] as f64 / 1000.,
                "samples_ns":samples, "allocations":allocations})
        })
        .collect();
    Ok(json!({"case":name, "json_body_bytes":json_bytes.len(),
        "rkyv_body_bytes":codec::PREFIX_LEN + archive.len(),
        "rkyv_archive_capacity":archive.capacity(), "stages":stages}))
}
