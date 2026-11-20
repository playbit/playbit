//! Exercise whole-event ownership and the real local control/recording/tail path.
use super::*;
use playbit_api::abi::trace::{self as wire, TraceEvent};
use std::{
    io::Read,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::Path,
};

fn event(header: TraceEvent, metadata: &[u8]) -> Vec<u8> {
    let mut header = header;
    header.metadata_len = metadata.len() as u32;
    let mut bytes = vec![0; wire::event_bytes(metadata.len()).expect("bounded metadata")];
    wire::encode_header(header, &mut bytes).expect("event header");
    bytes[wire::HEADER_BYTES..wire::HEADER_BYTES + metadata.len()].copy_from_slice(metadata);
    bytes
}
fn command(
    profile: &Path,
    command: control::Command,
    selector: Option<[u8; 16]>,
) -> std::io::Result<control::Status> {
    request(profile, command, selector, false)
}
fn request(
    profile: &Path,
    command: control::Command,
    selector: Option<[u8; 16]>,
    clean: bool,
) -> std::io::Result<control::Status> {
    let mut stream = control::connect(profile, command, selector, clean)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    let mut bytes = [0; control::STATUS_BYTES];
    stream.read_exact(&mut bytes)?;
    control::Status::decode(&bytes)
}

#[test]
fn clean_controls_preserve_other_instances_and_apply_to_start_and_stop() {
    const TEST: &str = "tests::clean_controls_preserve_other_instances_and_apply_to_start_and_stop";
    if std::env::var_os("PLAYBIT_CLEAN_TEST_CHILD").is_none() {
        assert!(
            std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", TEST, "--nocapture"])
                .env("PLAYBIT_CLEAN_TEST_CHILD", "1")
                .status()
                .expect("isolated source")
                .success()
        );
        return;
    }
    initialize(8, &[]).expect("trace bootstrap");
    let profile = std::env::temp_dir().join(format!("pt-clean-{}", std::process::id()));
    std::fs::create_dir(&profile).expect("profile");
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700))
        .expect("private profile");
    let names = [(AppNameKind::Mark, "test.point")];
    let mut a = AppScope::new("A", AppIdentity::default(), &names, [1; 16]);
    let mut b = AppScope::new("B", AppIdentity::default(), &names, [2; 16]);
    let recorder = Recorder::start(&profile, &profile.join("traces"), false).expect("recorder");
    command(&profile, control::Command::Start, None).expect("start profile");
    let point =
        |timestamp| event(TraceEvent { timestamp, kind: wire::INSTANT, ..Default::default() }, &[]);
    a.record(&point(1), a.generation(), 0).expect("A point");
    b.record(&point(2), b.generation(), 0).expect("B point");
    request(&profile, control::Command::Stop, Some([1; 16]), true).expect("stop and clean A");
    assert_eq!(a.generation(), 0);
    assert_ne!(b.generation(), 0);
    let directory = profile.join("traces").join(identifier::encode(&[1; 16]));
    match std::fs::read_dir(directory) {
        Ok(mut files) => assert!(files.next().is_none(), "stopped clean scope kept trace files"),
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
    }
    // A control acknowledgment fences queued old-generation data and stored
    // bytes; B's admitted point must survive the same operation.
    command(&profile, control::Command::Stop, None).expect("stop profile");
    let retained = |id| {
        let mut stream = tail::connect(&profile, Some(0), Some(id)).expect("retained tail");
        let mut timestamps = Vec::new();
        for _ in 0..256 {
            let (kind, bytes) = frame(&mut stream);
            if kind == 6 {
                return timestamps;
            }
            if matches!(kind, 2 | 3 | 5) {
                continue;
            }
            assert!(matches!(kind, 1 | 7), "unexpected frame {kind}");
            let (envelope, payload) = format::Envelope::decode(&bytes).expect("block");
            if envelope.kind == format::BlockKind::Events {
                timestamps
                    .push(format::Events::decode(payload).expect("event").event.header.timestamp);
            }
        }
        panic!("retained tail did not end");
    };
    assert!(retained([1; 16]).is_empty());
    assert_eq!(retained([2; 16]), vec![2]);
    command(&profile, control::Command::Start, Some([1; 16])).expect("restart A");
    a.record(&point(3), a.generation(), 0).expect("A old point");
    request(&profile, control::Command::Start, Some([1; 16]), true).expect("restart and clean A");
    a.record(&point(4), a.generation(), 0).expect("A replacement point");
    command(&profile, control::Command::Stop, None).expect("stop profile");
    assert_eq!(retained([1; 16]), vec![4]);
    assert_eq!(retained([2; 16]), vec![2]);
    request(&profile, control::Command::Start, None, true).expect("start and clean all");
    b.record(&point(5), b.generation(), 0).expect("B replacement point");
    request(&profile, control::Command::Stop, None, true).expect("stop and clean all");
    assert!(retained([1; 16]).is_empty());
    assert!(retained([2; 16]).is_empty());
    command(&profile, control::Command::Start, Some([1; 16])).expect("start only A after clear");
    a.record(&point(6), a.generation(), 0).expect("A fresh point");
    command(&profile, control::Command::Stop, None).expect("stop profile");
    assert_eq!(retained([1; 16]), vec![6]);
    assert!(retained([2; 16]).is_empty());
    let directory = profile.join("traces").join(identifier::encode(&[2; 16]));
    match std::fs::read_dir(directory) {
        Ok(mut files) => assert!(files.next().is_none(), "off sibling recreated cleared metadata"),
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
    }
    drop(a);
    drop(b);
    drop(recorder);
    std::fs::remove_dir_all(profile).expect("cleanup");
}
fn frame(stream: &mut UnixStream) -> (u8, Vec<u8>) {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).expect("tail deadline");
    let mut size = [0; 4];
    stream.read_exact(&mut size).expect("frame header");
    let size = u32::from_le_bytes(size) as usize;
    assert!((1..tail::MAX_FRAME_BYTES).contains(&size));
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).expect("frame payload");
    (bytes[0], bytes[1..].to_vec())
}
fn next_event(stream: &mut UnixStream) -> (u32, u32, TraceEvent, Vec<u8>) {
    for _ in 0..256 {
        let (kind, bytes) = frame(stream);
        assert_ne!(kind, 9, "{}", String::from_utf8_lossy(&bytes));
        if !matches!(kind, 1 | 7) {
            continue;
        }
        let (envelope, payload) = format::Envelope::decode(&bytes).expect("recorded block");
        if envelope.kind == format::BlockKind::Events {
            let event = format::Events::decode(payload).expect("complete event");
            return (event.scope, event.tid, event.event.header, event.event.metadata.to_vec());
        }
    }
    panic!("event was not retained within bounded control metadata");
}
#[test]
fn byte_queue_wrap_and_overload_preserve_complete_metadata() {
    // Maximum metadata occupies one complete loss unit through wrap and saturation.
    let region = region::Region::create(8, &["test.queue"]).expect("bounded mapping");
    let queue = &region.header().queues[0];
    let metadata = vec![b'x'; wire::MAX_METADATA_BYTES];
    let event = event(
        TraceEvent { timestamp: 1, name: 0, kind: wire::INSTANT, ..Default::default() },
        &metadata,
    );
    let mut prefix = [0; 16];
    prefix[4] = 1;
    prefix[8] = 1;
    for _ in 0..3 {
        assert!(queue.push(&prefix, &event));
    }
    assert!(!queue.push(&prefix, &event));
    assert_eq!(queue.lost.load(std::sync::atomic::Ordering::Relaxed), 1);
    let mut output = [0; queue::EVENT_MAX_BYTES];
    for _ in 0..3 {
        let n = queue.pop(&mut output).expect("whole event");
        let decoded = format::Events::decode(&output[..n]).expect("inline metadata");
        assert_eq!(decoded.event.metadata, metadata);
    }
    assert_eq!(queue.pop(&mut output), Ok(0));
    assert!(queue.push(&prefix, &event));
    let n = queue.pop(&mut output).expect("wrapped event");
    assert_eq!(
        format::Events::decode(&output[..n]).expect("wrapped payload").event.metadata,
        metadata
    );
}
#[test]
fn scoped_controls_record_independent_spans_and_reject_stale_batches() {
    // One real recorder hosts two instances of the same session. The test uses
    // authenticated sockets and the retained file reader, not mocked controls.
    const TEST: &str = "tests::scoped_controls_record_independent_spans_and_reject_stale_batches";
    if std::env::var_os("PLAYBIT_EVENT_TEST_CHILD").is_none() {
        assert!(
            std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", TEST, "--nocapture"])
                .env("PLAYBIT_EVENT_TEST_CHILD", "1")
                .status()
                .expect("isolated source")
                .success()
        );
        return;
    }
    initialize(8, &[]).expect("trace bootstrap");
    let profile = std::env::temp_dir().join(format!("pte-{}", std::process::id()));
    std::fs::create_dir(&profile).expect("private test profile");
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700))
        .expect("private profile");
    let names = [(AppNameKind::Span, "test.work"), (AppNameKind::Mark, "test.point")];
    let identity = AppIdentity { session: Some([7; 32]), build: None };
    let mut a = AppScope::new("A", identity, &names, [1; 16]);
    let mut b = AppScope::new("B", identity, &names, [2; 16]);
    let recorder =
        Recorder::start(&profile, &profile.join("logs/trace"), false).expect("ready recorder");
    command(&profile, control::Command::Stop, None).expect("off by default control");
    assert_eq!(a.generation(), 0);
    assert_eq!(b.generation(), 0);
    assert_eq!(
        command(&profile, control::Command::Start, Some([99; 16]))
            .expect_err("unknown instance")
            .kind(),
        std::io::ErrorKind::NotFound
    );
    let status = command(&profile, control::Command::Start, Some([1; 16])).expect("start A");
    assert!(status.recording);
    let first_generation = a.generation();
    assert_ne!(first_generation, 0);
    assert_eq!(b.generation(), 0);
    let mut tail_a = tail::connect(&profile, None, Some([1; 16])).expect("scoped tail A");
    let end = event(
        TraceEvent { timestamp: 20, reference_eid: 10, kind: wire::END, ..Default::default() },
        br#"{"result":"done"}"#,
    );
    let start = event(
        TraceEvent { timestamp: 10, name: 0, kind: wire::START, ..Default::default() },
        br#"{"actor_type":"funny_monster","position":{"x":12.5,"y":8,"z":0}}"#,
    );
    a.record(&end, first_generation, 0).expect("end before start accepted");
    let (a_scope, tid, header, metadata) = next_event(&mut tail_a);
    assert_eq!((tid, header.kind, header.reference_eid), (1, wire::END, 10));
    assert_eq!(metadata, br#"{"result":"done"}"#);
    let mut malformed = start.clone();
    malformed.extend_from_slice(&[1, 2, 3]);
    assert_eq!(a.record(&malformed, first_generation, 0), Err(AppTraceError::Invalid));
    a.record(&start, first_generation, 0).expect("later start no host history");
    let (scope, stable_tid, header, _) = next_event(&mut tail_a);
    assert_eq!((scope, stable_tid, header.timestamp), (a_scope, tid, 10));
    command(&profile, control::Command::Start, Some([2; 16])).expect("independent B");
    assert_eq!(a.generation(), first_generation);
    let mut tail_b = tail::connect(&profile, None, Some([2; 16])).expect("scoped tail B");
    let point = event(
        TraceEvent {
            timestamp: 30,
            name: 1,
            kind: wire::INSTANT,
            value: u64::MAX,
            ..Default::default()
        },
        b"{invalid-json",
    );
    b.record(&point, b.generation(), 0).expect("B instant");
    a.record(&point, first_generation, 0).expect("A instant");
    let (scope, _, header, metadata) = next_event(&mut tail_a);
    assert_eq!((scope, header.value), (a_scope, u64::MAX));
    assert_eq!(metadata, b"{invalid-json");
    let (b_scope, _, _, _) = next_event(&mut tail_b);
    assert_ne!(a_scope, b_scope);
    assert_eq!(a.record(&[], 0, 0), Err(AppTraceError::Invalid));
    a.record(&[], first_generation, 5).expect("loss-only submission");
    a.record(&[], first_generation, 5).expect("unchanged cumulative loss");
    assert_eq!(a.record(&[], first_generation, 4), Err(AppTraceError::Invalid));
    let app = crate::source::SOURCE
        .get()
        .expect("source")
        .region
        .header()
        .apps
        .iter()
        .find(|app| app.scope.load(std::sync::atomic::Ordering::Acquire) == u64::from(a_scope))
        .expect("app scope");
    assert_eq!(app.lost.load(std::sync::atomic::Ordering::Acquire), 5);
    command(&profile, control::Command::Stop, Some([1; 16])).expect("stop A only");
    assert_eq!(a.generation(), 0);
    assert_ne!(b.generation(), 0);
    assert_eq!(a.record(&point, first_generation, 0), Err(AppTraceError::Stale));
    command(&profile, control::Command::Start, Some([1; 16])).expect("restart A only");
    let second_generation = a.generation();
    assert_ne!(second_generation, first_generation);
    assert_eq!(a.record(&point, first_generation, 0), Err(AppTraceError::Stale));
    a.record(&point, second_generation, 0).expect("new generation accepts observations");
    let (scope, _, _, _) = next_event(&mut tail_a);
    assert_eq!(scope, a_scope);
    command(&profile, control::Command::Stop, None).expect("stop all clears overrides");
    assert_eq!((a.generation(), b.generation()), (0, 0));
    command(&profile, control::Command::Start, None).expect("start all clears overrides");
    assert_ne!(a.generation(), 0);
    assert_ne!(b.generation(), 0);
    command(&profile, control::Command::Stop, Some([1; 16])).expect("target override");
    assert_eq!(a.generation(), 0);
    command(&profile, control::Command::Start, None).expect("profile resets override");
    assert_ne!(a.generation(), 0);
    drop(a);
    command(&profile, control::Command::Observe, None).expect("retirement processed");
    assert_eq!(
        command(&profile, control::Command::Observe, Some([1; 16]))
            .expect_err("retired ownership")
            .kind(),
        std::io::ErrorKind::NotFound
    );
    drop(b);
    drop(tail_a);
    drop(tail_b);
    drop(recorder);
    std::fs::remove_dir_all(profile).expect("remove isolated capture");
}

#[test]
fn creating_a_log_preserves_unrecognized_retained_bytes() {
    // Opening for a new capture must reject an unsupported existing segment
    // before any reset/truncation, including when the segment header is short.
    for bytes in [b"foreign recording format".to_vec(), vec![42; format::SEGMENT_HEADER_BYTES]] {
        let path =
            std::env::temp_dir().join(format!("pt-unknown-{}-{}", std::process::id(), bytes.len()));
        std::fs::create_dir(&path).expect("test directory");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("private directory");
        let segment = path.join("segment-0.pbt");
        std::fs::write(&segment, &bytes).expect("foreign retained bytes");
        std::fs::set_permissions(&segment, std::fs::Permissions::from_mode(0o600))
            .expect("private segment");
        assert!(crate::log::Log::create(&path, false, 0).is_err());
        assert_eq!(std::fs::read(&segment).expect("original bytes"), bytes);
        std::fs::remove_dir_all(path).expect("test cleanup");
    }
}
