//! Operator benchmark bridge: JSON stdio outside the measured binary IPC exchange.
use playbit::engine::{ChangeToken, Request, Result, ipc, session_instance::LaunchOwner};
use serde::Deserialize;
use serde_json::json;
use std::{
    io::{self, BufRead, Write},
    os::fd::AsRawFd,
    path::Path,
    time::{Duration, Instant},
};
mod input_to_scene;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Command {
    Management(Management),
    App(App),
    FileExport {
        volume: playbit::store::Id,
        entry: playbit::store::EntryId,
        path: std::path::PathBuf,
    },
}
#[derive(Deserialize)]
struct Management {
    request: Request,
    #[serde(default)]
    waiters: usize,
    after: Option<ChangeToken>,
}
#[derive(Deserialize)]
struct App {
    session: String,
    instance: playbit::store::Id,
    generation: u64,
    operation: AppOperation,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum AppOperation {
    Frame,
    Input { kind: u32, target: u32, data: Vec<u8> },
    Benchmark(input_to_scene::Workload),
    Quit,
}

fn exchange(
    profile: &Path,
    command: Command,
    owner: &mut Option<LaunchOwner>,
) -> Result<serde_json::Value> {
    match command {
        Command::Management(command) => management(profile, command),
        Command::App(command) => app(profile, command, owner),
        Command::FileExport { volume, entry, path } => file_export(profile, volume, entry, &path),
    }
}

/// A representative background transfer at 2 MiB/s. The ordinary data client
/// verifies length/digest; pacing bounds trace volume without dropping records.
fn file_export(
    profile: &Path,
    volume: playbit::store::Id,
    entry: playbit::store::EntryId,
    path: &Path,
) -> Result<serde_json::Value> {
    struct Paced {
        file: std::fs::File,
        next: Instant,
    }
    impl Write for Paced {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let count = self.file.write(bytes)?;
            self.next += Duration::from_secs_f64(count as f64 / (2. * 1024. * 1024.));
            std::thread::sleep(self.next.saturating_duration_since(Instant::now()));
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }
    let call = |request| ipc::call_archive(profile, &request)?.into_owned();
    let mut context = playbit::engine::tool_client::DataContext::open_volume(volume, &call)?;
    let result = (|| {
        let mut output = Paced { file: std::fs::File::create(path)?, next: Instant::now() };
        let bytes = context.read_file(*volume.as_bytes(), *entry.as_bytes(), &mut output)?;
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&output.file))?;
        Ok(json!({"response":{"bytes":bytes,"rate_bytes_per_second":2 * 1024 * 1024}}))
    })();
    context.finish(result)
}
fn app(
    profile: &Path,
    command: App,
    retained: &mut Option<LaunchOwner>,
) -> Result<serde_json::Value> {
    use playbit::{
        engine::reply::Reply,
        ui::app_control::{self, Exchange, Operation},
    };
    // Retain one exact discovered owner. Re-discovery before every frame poll
    // would add engine traffic to the workload being measured. Exchange still
    // verifies the OS process identity and response generation on every call.
    let matches = retained.as_ref().is_some_and(|owner| {
        owner.instance.session == command.session
            && owner.instance.id == command.instance
            && owner.instance.owner_generation == command.generation
    });
    if !matches {
        let Reply::InstanceOwners(owners) =
            ipc::call_archive(profile, &Request::InstanceList { session: command.session })?
                .into_owned()?
        else {
            return Err("expected known instance owners".into());
        };
        let owner = app_control::select(owners, Some(command.instance))?;
        if owner.instance.owner_generation != command.generation {
            return Err("app generation changed; inspect before another input".into());
        }
        *retained = Some(owner);
    }
    let owner = retained.as_ref().ok_or("expected discovered app owner")?;
    let operation = match command.operation {
        AppOperation::Frame => Operation::Frame,
        AppOperation::Input { kind, target, data } => Operation::Input { kind, target, data },
        AppOperation::Quit => Operation::Quit,
        AppOperation::Benchmark(workload) => {
            let start = playbit::trace::clock_ns();
            let measurement = input_to_scene::measure(workload, owner, |operation| {
                Exchange::connect(profile, owner, operation)?.receive()
            })?;
            let end = playbit::trace::clock_ns();
            return Ok(
                json!({"response":{"Benchmark":measurement}, "start_ns":start, "end_ns":end}),
            );
        }
    };
    let start = playbit::trace::clock_ns();
    let response = Exchange::connect(profile, owner, operation)?.receive()?;
    let end = playbit::trace::clock_ns();
    Ok(json!({"response":response, "start_ns":start, "end_ns":end}))
}
fn management(profile: &Path, command: Management) -> Result<serde_json::Value> {
    if command.waiters > 4
        || (command.waiters != 0
            && (command.after.is_none()
                || !matches!(command.request, Request::Syscall { mutation, .. } if mutation != 0)))
    {
        return Err("expected at most four mutation waiters and an initial change token".into());
    }
    let mut waits = Vec::with_capacity(command.waiters);
    for _ in 0..command.waiters {
        let wait = ipc::ChangeWait::connect(profile, command.after)?;
        waits.push(wait);
    }
    let start = playbit::trace::clock_ns();
    let response = ipc::call_archive(profile, &command.request)?;
    let end = playbit::trace::clock_ns();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut after = command.after;
    for wait in waits {
        let token = receive_wait(wait, deadline)?;
        let previous = command.after.ok_or("missing initial token")?;
        if token.generation != previous.generation || token.sequence <= previous.sequence {
            return Err("change wait did not observe the local edit".into());
        }
        if after.is_none_or(|current| token.sequence > current.sequence) {
            after = Some(token);
        }
    }
    // Materialization and external JSON formatting are outside the timed interval.
    Ok(
        json!({"response": response.into_owned()?, "start_ns": start, "end_ns": end, "after": after}),
    )
}

#[allow(unsafe_code, reason = "poll borrows an initialized descriptor owned for the entire call.")]
fn receive_wait(wait: ipc::ChangeWait, deadline: Instant) -> Result<ChangeToken> {
    let handle = wait.cancellation_handle()?;
    let mut descriptor = libc::pollfd { fd: handle.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("benchmark change wait exceeded its 5-second deadline".into());
        }
        let timeout_ms = remaining.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
        // SAFETY: the descriptor is initialized and handle owns the socket throughout poll.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                handle.set_read_timeout(Some(remaining))?;
                return Ok(wait.receive()?.token);
            }
        } else if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }
}

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let profile = arguments.next().ok_or("expected profile path")?;
    if arguments.next().is_some() {
        return Err("expected profile path".into());
    }
    let profile = Path::new(&profile);
    run_bridge(profile, io::stdin().lock(), io::stdout().lock())
}

fn run_bridge(profile: &Path, mut input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut line = Vec::new();
    let mut owner = None;
    loop {
        line.clear();
        // The bridge accepts only bounded operator commands, one per line.
        let count = std::io::Read::take(&mut input, (ipc::codec::MAX_ARCHIVE + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if count > ipc::codec::MAX_ARCHIVE || line.last() != Some(&b'\n') {
            return Err("benchmark command exceeds limit or is incomplete".into());
        }
        let result = serde_json::from_slice::<Command>(&line)
            .map_err(Into::into)
            .and_then(|command| exchange(profile, command, &mut owner))
            .unwrap_or_else(|error: Box<dyn std::error::Error + Send + Sync>| json!({"error":error.to_string()}));
        writeln!(output, "{result}")?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert fixture operations.")]
mod tests {
    use super::*;
    use playbit::engine::reply::Reply;
    use std::os::unix::{fs::DirBuilderExt, net::UnixListener};

    #[test]
    fn measures_real_binary_exchange_and_formats_after_validation() {
        let profile = tempfile::tempdir().unwrap();
        std::fs::DirBuilder::new().mode(0o700).create(profile.path().join("control")).unwrap();
        let listener = UnixListener::bind(profile.path().join("control/engine.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let bytes = ipc::read(&mut stream).unwrap();
            let request =
                rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&bytes).unwrap();
            assert!(matches!(request, Request::LocalProtocol));
            ipc::write(
                &mut stream,
                &ipc::codec::encode(
                    &Reply::Protocol {
                        protocol: ipc::LOCAL_PROTOCOL,
                        version: playbit::VERSION.into(),
                    },
                    ipc::codec::MAX_ARCHIVE,
                )
                .unwrap(),
            )
            .unwrap();
        });
        let before = playbit::trace::clock_ns();
        let result = exchange(
            profile.path(),
            Command::Management(Management {
                request: Request::LocalProtocol,
                waiters: 0,
                after: None,
            }),
            &mut None,
        )
        .unwrap();
        let after = playbit::trace::clock_ns();
        assert_eq!(result["response"]["protocol"], ipc::LOCAL_PROTOCOL);
        assert!(before <= result["start_ns"].as_u64().unwrap());
        assert!(result["start_ns"].as_u64().unwrap() <= result["end_ns"].as_u64().unwrap());
        assert!(result["end_ns"].as_u64().unwrap() <= after);
        server.join().unwrap();
    }

    #[test]
    #[allow(
        unsafe_code,
        reason = "Fixture poll borrows the owned peer until bounded EOF readiness."
    )]
    fn retained_discovery_still_rejects_a_different_endpoint_identity() {
        use playbit::{
            engine::session_instance::{SessionInstance, State},
            store::Id,
            ui::app_control,
        };
        use std::io::Read;
        let profile = tempfile::tempdir().unwrap();
        let id = Id::from_bytes([1; 16]);
        let mut owner = Some(LaunchOwner {
            instance: SessionInstance {
                id,
                launch_request: id,
                account: id,
                session: "session".into(),
                build: "build".into(),
                root: id,

                extra_volumes: Vec::new(),
                host_mounts: Box::default(),
                restore_host_mounts: false,
                owner_generation: 3,
                state: State::Owned,
            },
            process: None, // No discovered process matches this fixture's listener.
            presentation: 0,
        });
        let socket = app_control::socket_path(profile.path(), id).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let error = exchange(
            profile.path(),
            Command::App(App {
                session: "session".into(),
                instance: id,
                generation: 3,
                operation: AppOperation::Frame,
            }),
            &mut owner,
        )
        .unwrap_err();
        assert!(error.to_string().contains("another process identity"));
        let (mut peer, _) = listener.accept().unwrap();
        // macOS may deliver the peer's close after accept. Socket options can
        // also fail after closure, so wait for readiness without changing them.
        peer.set_nonblocking(true).unwrap();
        let mut descriptor =
            libc::pollfd { fd: peer.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: peer owns the initialized descriptor throughout this bounded poll.
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 5000) }, 1, "rejected peer closes");
        assert_eq!(peer.read(&mut [0]).unwrap(), 0, "identity mismatch sends no operation");
        listener.set_nonblocking(true).unwrap();
        for (session, instance, generation) in
            [("another", id, 3), ("session", Id::from_bytes([2; 16]), 3), ("session", id, 4)]
        {
            assert!(
                exchange(
                    profile.path(),
                    Command::App(App {
                        session: session.into(),
                        instance,
                        generation,
                        operation: AppOperation::Input {
                            kind: 3,
                            target: 1,
                            data: b"must not send".to_vec(),
                        },
                    }),
                    &mut owner
                )
                .is_err(),
                "a changed binding requires fresh engine discovery"
            );
            assert_eq!(listener.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        }
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn rejects_invalid_waiters_before_connecting() {
        for count in [1, 5] {
            let error = exchange(
                Path::new("/absent"),
                Command::Management(Management {
                    request: Request::Status,
                    waiters: count,
                    after: None,
                }),
                &mut None,
            )
            .unwrap_err();
            assert!(error.to_string().contains("mutation waiters"));
        }
    }

    #[test]
    fn unchanged_engine_wait_has_a_deadline_and_releases_its_connection() {
        use std::io::Read;
        let profile = tempfile::tempdir().unwrap();
        std::fs::DirBuilder::new().mode(0o700).create(profile.path().join("control")).unwrap();
        let listener = UnixListener::bind(profile.path().join("control/engine.sock")).unwrap();
        let wait = ipc::ChangeWait::connect(profile.path(), None).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let _request = ipc::read(&mut peer).unwrap();
        let start = Instant::now();
        let error = receive_wait(wait, start + Duration::from_millis(30)).unwrap_err();
        assert!(error.to_string().contains("change wait"));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}
