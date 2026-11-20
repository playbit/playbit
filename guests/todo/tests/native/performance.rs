//! TODO performance measurements: host-open/input-to-scene latency for native
//! and Wasm execution, with optional recording. Excludes OS launch and display.
//! Both adapters map explicitly owned ordinary data to Session role metadata;
//! these fixtures do not qualify automatic-scope derivation or synchronization.
use super::*;
use playbit::trace;
use std::{io::Write, time::Duration};

#[test]
#[ignore = "performance profile provisioning; isolated profile only"]
fn prepare_optimized_todo_profile() {
    let profile = std::env::var("PLAYBIT_TODO_PERF_PROFILE").unwrap();
    let rows: usize = std::env::var("PLAYBIT_TODO_PERF_ROWS").unwrap().parse().unwrap();
    assert!([1, 32, 64].contains(&rows));
    let services = Services::open(&profile, || {}).unwrap();
    let mut sessions = Vec::new();
    for _ in 0..4 {
        let volume = root(&services, rows);
        sessions.push(services.create_session(package(&services), volume).unwrap().id);
    }
    std::fs::write(
        Path::new(&profile).join("sessions.json"),
        serde_json::to_vec(&sessions).unwrap(),
    )
    .unwrap();
}

struct Samples {
    file: std::fs::File,
    failure: std::path::PathBuf,
}
impl Samples {
    fn new(profile: &Path, name: &str, header: &str) -> Self {
        let mut file = std::fs::File::create(profile.join(name)).unwrap();
        writeln!(file, "{header}").unwrap();
        file.flush().unwrap();
        Self { file, failure: profile.join(format!("{name}.failed.json")) }
    }
    fn capture<T>(&mut self, index: usize, execute: impl FnOnce() -> T) -> (T, u64) {
        let start = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(execute));
        let elapsed: u64 = start.elapsed().as_nanos().try_into().unwrap();
        match result {
            Ok(value) => {
                // Qualification I/O happens after the measured boundary. It
                // retains the completed prefix even if a later sample fails.
                writeln!(self.file, "{elapsed}").unwrap();
                self.file.flush().unwrap();
                (value, elapsed)
            }
            Err(panic) => {
                let failure = serde_json::json!({
                    "sample_index": index, "elapsed_ns": elapsed,
                    "completed_scene": false,
                });
                std::fs::write(&self.failure, serde_json::to_vec_pretty(&failure).unwrap())
                    .unwrap();
                std::panic::resume_unwind(panic);
            }
        }
    }
}
#[test]
fn failed_measurement_preserves_completed_prefix_and_partial_duration() {
    let directory = tempfile::tempdir().unwrap();
    let mut samples = Samples::new(directory.path(), "inputs.csv", "host_input_to_scene_ns");
    assert_eq!(samples.capture(0, || 7).0, 7);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        samples.capture(1, || panic!("intentional incomplete scene"));
    }));
    assert!(failed.is_err(), "measurement retains the original failure");
    let completed = std::fs::read_to_string(directory.path().join("inputs.csv")).unwrap();
    let lines: Vec<_> = completed.lines().collect();
    assert_eq!(lines.len(), 2, "only the completed sample belongs in the timing cohort");
    assert_eq!(lines[0], "host_input_to_scene_ns");
    assert!(lines[1].parse::<u64>().is_ok());
    let failure: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.path().join("inputs.csv.failed.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(failure["sample_index"], 1);
    assert_eq!(failure["completed_scene"], false);
    assert!(failure["elapsed_ns"].as_u64().is_some());
}
fn event(index: usize) -> Event {
    if index.is_multiple_of(10) {
        input(EventKind::Click, 100, &[])
    } else {
        input(
            EventKind::TextChanged,
            1,
            if index.is_multiple_of(2) { b"Draft A" } else { b"Draft B" },
        )
    }
}
fn measured(profile: &Path, mut execute: impl FnMut(usize, Event), count: usize) -> Vec<u64> {
    let mut samples = Vec::with_capacity(1000);
    let mut retained = Samples::new(profile, "inputs.csv", "host_input_to_scene_ns");
    // The first sample is deliberately after idle. No wake/energy conclusion
    // follows from this sleep; those require separate process observations.
    std::thread::sleep(Duration::from_millis(250));
    for index in 0..1000 {
        let start = Instant::now();
        let (_, elapsed) = retained.capture(index, || execute(index % count, event(index)));
        samples.push(elapsed);
        if let Some(rest) = Duration::from_millis(16).checked_sub(start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    samples
}

fn settle_native_evidence(apps: &mut Apps, generation: u64, ready: &std::sync::mpsc::Receiver<()>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let now = Instant::now();
        assert!(now < deadline, "native qualification completion deadline");
        let step = apps.step(generation, now).unwrap();
        if step.ready {
            apps.advance(generation, |_| {}).unwrap();
            continue;
        }
        if !apps.drain_pending(generation).unwrap() {
            return;
        }
        let remaining = deadline.saturating_duration_since(now);
        match ready.recv_timeout(step.wake_after.map_or(remaining, |delay| delay.min(remaining))) {
            Ok(()) => (),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if step.wake_after.is_some() => (),
            error => panic!("native completion unavailable: {error:?}"),
        }
    }
}

#[test]
#[ignore = "optimized TODO performance; run alone without builds/proofs/fuzz"]
fn optimized_todo_input() {
    assert!(!std::hint::black_box(cfg!(debug_assertions)), "requires --release");
    let profile = std::env::var("PLAYBIT_TODO_PERF_PROFILE").unwrap();
    let profile = Path::new(&profile);
    let sessions: Vec<String> =
        serde_json::from_slice(&std::fs::read(profile.join("sessions.json")).unwrap()).unwrap();
    let count: usize = std::env::var("PLAYBIT_TODO_PERF_APPS").unwrap().parse().unwrap();
    assert!([1, 4].contains(&count) && sessions.len() == 4);
    let mode = std::env::var("PLAYBIT_TODO_PERF_MODE").unwrap();
    assert!(matches!(mode.as_str(), "native" | "wasm"));
    trace::initialize(8, &[]).unwrap();
    let on = std::env::var("PLAYBIT_TODO_PERF_RECORDING").as_deref() == Ok("1");
    let recorder = if on {
        trace::Recorder::start(profile, &profile.join("logs/trace"), false).unwrap()
    } else {
        trace::Recorder::for_profile(profile, &profile.join(playbit::paths::TRACE))
            .expect("disabled product owner remains available")
    };
    let mut startup_samples = Samples::new(profile, "startup.csv", "host_open_to_scene_ns");
    let startup;
    let samples;
    if mode == "native" {
        let ((mut apps, generations, ready, _connected), elapsed) =
            startup_samples.capture(0, || {
                let connected =
                    playbit::engine::connected::Connected::open(profile, || {}).unwrap();
                let (wake, ready) = std::sync::mpsc::sync_channel(1);
                let mut apps = Apps::new(std::sync::Arc::new(move || {
                    let _ = wake.try_send(());
                }));
                let mut generations = Vec::new();
                for session in &sessions[..count] {
                    // SAFETY: static TODO callbacks and bundle match and outlive the host.
                    let generation = unsafe {
                        apps.attach_fixture_role(
                            &connected.services,
                            connected.services.store.session(session).unwrap().root_volume,
                            playbit_api::native_app(),
                            playbit_api::context::VolumeRole::Session,
                        )
                        .unwrap()
                    };
                    settle_native_evidence(&mut apps, generation, &ready);
                    assert!(apps.scene(generation).unwrap().is_some());
                    generations.push(generation);
                }
                (apps, generations, ready, connected)
            });
        startup = elapsed;
        samples = measured(
            profile,
            |index, event| {
                apps.input(generations[index], event).unwrap();
                settle_native_evidence(&mut apps, generations[index], &ready);
                std::hint::black_box(apps.scene(generations[index]).unwrap().unwrap());
            },
            count,
        );
        apps.clear();
    } else {
        let ((mut owners, _engine), elapsed) = startup_samples.capture(0, || {
            let engine = Engine::open(profile).unwrap();
            let mut owners: Vec<_> = sessions[..count]
                .iter()
                .map(|session| {
                    WasmOwner::open_with_role(
                        &engine,
                        session,
                        playbit_api::context::VolumeRole::Session as u32,
                    )
                })
                .collect();
            for owner in &mut owners {
                assert!(!owner.scene().is_empty());
            }
            (owners, engine)
        });
        startup = elapsed;
        samples = measured(
            profile,
            |index, event| {
                owners[index].input(event).unwrap();
                std::hint::black_box(owners[index].scene());
            },
            count,
        );
        for owner in &mut owners {
            owner.quit();
        }
    }
    drop(recorder);
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let report = serde_json::json!({"mode":mode,"apps":count,"recording":on,"samples":samples.len(),"startup_ns":startup,"first_after_idle_ns":samples[0],"p50_ns":sorted[499],"p99_ns":sorted[989],"max_ns":sorted[999]});
    std::fs::write(profile.join("timing.json"), serde_json::to_vec_pretty(&report).unwrap())
        .unwrap();
    eprintln!("{report}");
}
