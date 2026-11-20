//! Explicit release-build diagnostic; never run by CI. Uses an already-running
//! engine in the supplied isolated profile. No account or storage mutations.
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use playbit::engine::{ipc, launch};
use std::{path::Path, time::Instant};

fn main() -> playbit::engine::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 {
        return Err("Usage: benchmark-startup-check ISOLATED_PROFILE ENGINE_EXECUTABLE".into());
    }
    let profile = Path::new(&args[1]);
    let executable = Path::new(&args[2]);
    let mut baseline = Vec::with_capacity(1000);
    let mut protocol = Vec::with_capacity(1000);
    let mut checked = Vec::with_capacity(1000);
    for index in 0..1100 {
        // Alternate order. Baseline is the previous launcher's actual work:
        // connect a nonblocking Unix socket and validate the OS-account peer.
        for check in if index % 2 == 0 { [0, 1, 2] } else { [2, 1, 0] } {
            let start = Instant::now();
            if check == 2 {
                assert!(launch::ensure(profile, executable)?.is_none());
            } else if check == 1 {
                assert!(launch::ensure(profile, Path::new("/no-engine-may-be-started"))?.is_none());
            } else {
                let socket =
                    socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
                socket.set_nonblocking(true)?;
                socket.connect(&socket2::SockAddr::unix(profile.join("control/engine.sock"))?)?;
                ipc::verify_peer(&socket.into())?;
            }
            let elapsed = start.elapsed().as_nanos() as u64;
            if index >= 100 {
                match check {
                    2 => checked.push(elapsed),
                    1 => protocol.push(elapsed),
                    _ => baseline.push(elapsed),
                }
            }
        }
    }
    fn summary(mut values: Vec<u64>) -> serde_json::Value {
        let raw = values.clone();
        values.sort_unstable();
        serde_json::json!({"count":values.len(), "p50_us":values[499] as f64 / 1000.,
            "p99_us":values[989] as f64 / 1000., "max_us":values[999] as f64 / 1000., "samples_ns":raw})
    }
    println!(
        "{}",
        serde_json::json!({"boundary":"launcher endpoint check, including connect and peer authorization; excludes process launch, GUI and physical presentation",
        "baseline":summary(baseline), "protocol_checked":summary(protocol), "executable_checked":summary(checked)})
    );
    Ok(())
}
