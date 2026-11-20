//! Explicit cross-process IPC diagnostic; never run by CI. Use only an isolated
//! profile. The engine cases are read-only; the synthetic peer never opens data.
use ipc::codec;
use playbit::engine::{Request, ipc, reply::Reply};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::Path,
    process::{Child, Command, Stdio},
    sync::Barrier,
    time::{Duration, Instant},
};

const SAMPLES: usize = 1000;
const WARMUP: usize = 100;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--peer") && args.len() == 3 {
        return peer(Path::new(&args[2]));
    }
    if args.len() != 2 {
        return Err("Usage: benchmark-ipc ISOLATED_PROFILE".into());
    }
    let profile = Path::new(&args[1]);
    playbit::trace::initialize(8, &[])?;
    if playbit::trace::is_recording() {
        playbit::trace::attach(profile)?;
    }
    let directory = tempfile::Builder::new().prefix("pb-ipc-").tempdir_in("/tmp")?;
    let mut child = Peer(
        Command::new(std::env::current_exe()?)
            .arg("--peer")
            .arg(directory.path())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut ready = String::new();
    BufReader::new(child.0.stdout.take().ok_or("missing peer readiness pipe")?)
        .read_line(&mut ready)?;
    if ready != "ready\n" {
        return Err("peer did not become ready".into());
    }
    let mut results = Vec::new();
    for clients in [1, 4] {
        results.push(measure(
            "transport_codec_256_4096",
            directory.path(),
            &peer_request()?,
            clients,
        )?);
        results.push(measure("engine_local_protocol", profile, &Request::LocalProtocol, clients)?);
        results.push(measure("engine_status", profile, &Request::Status, clients)?);
    }
    println!(
        "{}",
        json!({"boundary":"native client before connect through verified archived response (owned conversion and correctness checks excluded); fresh connection per request, no process startup; synthetic peer excludes engine admission/guest/store work", "warmup_per_client":WARMUP, "samples_per_client":SAMPLES, "trace_enabled":playbit::trace::is_recording(), "results":results})
    );
    Ok(())
}

struct Peer(Child);
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn peer_request() -> Result<Request> {
    let mut request = Request::SetDisplayName { name: String::new() };
    let empty = codec::encode(&request, codec::MAX_ARCHIVE)?.len() + codec::PREFIX_LEN;
    let Request::SetDisplayName { name } = &mut request else { unreachable!() };
    *name = "x".repeat(256usize.checked_sub(empty).ok_or("request metadata exceeds fixture size")?);
    Ok(request)
}

fn peer_response() -> Result<Reply> {
    let mut response = Reply::DisplayName { display_name: String::new() };
    let empty = codec::encode(&response, codec::MAX_ARCHIVE)?.len() + codec::PREFIX_LEN;
    let Reply::DisplayName { display_name } = &mut response else { unreachable!() };
    *display_name =
        "x".repeat(4096usize.checked_sub(empty).ok_or("reply metadata exceeds fixture size")?);
    Ok(response)
}

fn peer(profile: &Path) -> Result<()> {
    let listener = UnixListener::bind(profile.join("control/engine.sock"))?;
    let response = peer_response()?;
    let expected = codec::encode(&peer_request()?, codec::MAX_ARCHIVE)?;
    assert_eq!(
        expected.len() + codec::PREFIX_LEN,
        256,
        "probe request matches the declared workload"
    );
    assert_eq!(
        codec::encode(&response, codec::MAX_ARCHIVE)?.len() + codec::PREFIX_LEN,
        4096,
        "probe response matches the declared workload"
    );
    println!("ready");
    std::io::stdout().flush()?;
    loop {
        let (mut stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        ipc::verify_peer(&stream)?;
        let bytes = ipc::read(&mut stream)?;
        let request = codec::access::<playbit::engine::ArchivedRequest>(&bytes)?;
        if !matches!(request, playbit::engine::ArchivedRequest::SetDisplayName { .. })
            || bytes.as_slice() != expected.as_slice()
        {
            return Err("unexpected diagnostic request".into());
        }
        // Serialize on every request, as the production reply path does.
        ipc::write(&mut stream, &codec::encode(&response, codec::MAX_ARCHIVE)?)?;
    }
}

fn measure(name: &str, profile: &Path, request: &Request, clients: usize) -> Result<Value> {
    let expected_reply = ipc::call_archive(profile, request)?.into_owned()?;
    let response_bytes =
        codec::encode(&expected_reply, codec::MAX_ARCHIVE)?.len() + codec::PREFIX_LEN;
    let expected = serde_json::to_value(expected_reply)?;
    if name == "transport_codec_256_4096" && expected != serde_json::to_value(peer_response()?)? {
        return Err("wrong diagnostic response".into());
    }
    if name == "engine_local_protocol"
        && expected != json!({"protocol":ipc::LOCAL_PROTOCOL,"version":playbit::VERSION})
    {
        return Err("incompatible engine".into());
    }
    let barrier = Barrier::new(clients);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..clients)
            .map(|_| {
                scope.spawn(|| -> Result<Vec<u64>> {
                    let warmup = (0..WARMUP)
                        .try_for_each(|_| ipc::call_archive(profile, request).map(|_| ()));
                    barrier.wait();
                    warmup?;
                    let mut samples = Vec::with_capacity(SAMPLES);
                    for _ in 0..SAMPLES {
                        let start = Instant::now();
                        let response = ipc::call_archive(profile, request)?;
                        let elapsed = start.elapsed().as_nanos() as u64;
                        // Response correctness is checked after timing, for every call.
                        if serde_json::to_value(response.into_owned()?)? != expected {
                            return Err("read-only response changed during probe".into());
                        }
                        samples.push(elapsed);
                    }
                    Ok(samples)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle.join().map_err(|_| "probe worker panicked".into()).and_then(|result| result)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    let all: Vec<_> = results.iter().flatten().copied().collect();
    Ok(
        json!({"case":name, "clients":clients, "request_bytes":codec::encode(request, codec::MAX_ARCHIVE)?.len() + codec::PREFIX_LEN, "response_bytes":response_bytes, "summary":summary(&all), "clients_samples_ns":results}),
    )
}

fn summary(samples: &[u64]) -> Value {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let percentile = |p: usize| sorted[(sorted.len() * p).div_ceil(100) - 1] as f64 / 1000.;
    json!({"count":sorted.len(), "p50_us":percentile(50), "p95_us":percentile(95), "p99_us":percentile(99), "max_us":sorted.last().copied().unwrap_or(0) as f64 / 1000.})
}

#[test]
fn synthetic_peer_preserves_the_declared_frame_sizes_and_reply_type() -> Result<()> {
    let request = peer_request()?;
    let request_bytes = codec::encode(&request, codec::MAX_ARCHIVE)?;
    let reply = peer_response()?;
    let reply_bytes = codec::encode(&reply, codec::MAX_ARCHIVE)?;
    assert_eq!(codec::PREFIX_LEN + request_bytes.len(), 256);
    assert_eq!(codec::PREFIX_LEN + reply_bytes.len(), 4096);
    assert!(
        codec::access::<playbit::engine::reply::ArchivedReply>(&reply_bytes)?
            .matches_request(&request)
    );
    Ok(())
}
