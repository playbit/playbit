//! One ordered QUIC stream per merge, using the ordinary kvdb byte protocol.
//! Fixture setup/receipts have a separate bounded benchmark control encoding.
use super::*;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use std::{io::Write, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;

const NAME: &str = "kvdb-benchmark";
const CONTROL_LIMIT: usize = 4096;
const DEADLINE: Duration = Duration::from_secs(120);

fn transport() -> Arc<quinn::TransportConfig> {
    let mut config = quinn::TransportConfig::default();
    config.max_concurrent_bidi_streams(1u32.into());
    config.max_concurrent_uni_streams(0u32.into());
    // Match native AP window sizes without depending on the Playbit runtime.
    config.stream_receive_window((512u32 * 1024).into());
    config.receive_window((2u32 * 1024 * 1024).into());
    config.send_window(2 * 1024 * 1024);
    config.max_idle_timeout(Some(quinn::VarInt::from_u32(30_000).into()));
    config.datagram_receive_buffer_size(None);
    Arc::new(config)
}

async fn write_control(send: &mut SendStream, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > CONTROL_LIMIT {
        return Err("benchmark control is too large".into());
    }
    send.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read_control<T: serde::de::DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let mut header = [0; 4];
    recv.read_exact(&mut header).await?;
    let length = u32::from_le_bytes(header) as usize;
    if length > CONTROL_LIMIT {
        return Err("benchmark control is too large".into());
    }
    let mut bytes = vec![0; length];
    recv.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn eof(recv: &mut RecvStream) -> Result<()> {
    if recv.read(&mut [0]).await?.is_some() {
        return Err("unexpected trailing benchmark data".into());
    }
    Ok(())
}

pub(super) async fn run<const MEASURE: bool>(options: &Options) -> Result<()> {
    if options.peer.is_some() { client::<MEASURE>(options).await } else { server(options).await }
}

fn server_endpoint(bind: std::net::SocketAddr) -> Result<(Endpoint, tempfile::TempDir)> {
    let directory = tempfile::tempdir()?;
    let cert = rcgen::generate_simple_self_signed(vec![NAME.into()])?;
    let cert_path = directory.path().join("server.der");
    std::fs::write(&cert_path, cert.cert.der())?;
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let mut config =
        quinn::ServerConfig::with_single_cert(vec![cert.cert.der().clone()], key.into())?;
    config.transport_config(transport());
    config.max_incoming(1);
    config.incoming_buffer_size(64 * 1024);
    config.incoming_buffer_size_total(64 * 1024);
    Ok((Endpoint::server(config, bind)?, directory))
}

async fn server(options: &Options) -> Result<()> {
    let (endpoint, directory) = server_endpoint(options.bind)?;
    let cert_path = directory.path().join("server.der");
    println!(
        "{}",
        json!({"listening":endpoint.local_addr()?,"certificate":cert_path,
        "medium":if options.memory { "memory" } else { "fsync" },
        "note":"copy this certificate to the client; replace unspecified listen IP with this machine's reachable address; synthetic data only"})
    );
    std::io::stdout().flush()?;
    // Single active connection/source, no per-client worker or unbounded task queue.
    while let Some(incoming) = endpoint.accept().await {
        let connection = match tokio::time::timeout(DEADLINE, incoming).await {
            Ok(Ok(connection)) => connection,
            result => {
                eprintln!("connection failed: {result:?}");
                continue;
            }
        };
        let result = serve(&connection, options.memory).await;
        if let Err(error) = result {
            eprintln!("benchmark failed: {error}");
            connection.close(1u32.into(), b"benchmark failed");
        }
        // Successful client closes after observing the final receipt. Closing
        // here would discard queued stream data before it reaches the client.
        let _ = tokio::time::timeout(Duration::from_secs(5), connection.closed()).await;
        connection.close(0u32.into(), b"complete");
    }
    Ok(())
}

async fn serve(connection: &Connection, memory: bool) -> Result<()> {
    let workload = tokio::time::timeout(DEADLINE, async {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let workload = read_control::<Workload>(&mut recv).await?.validate()?;
        eof(&mut recv).await?;
        write_control(
            &mut send,
            &json!({"server_medium":if memory { "memory" } else { "fsync" },
            "protocol_version":kvdb::sync::PROTOCOL_VERSION}),
        )
        .await?;
        send.finish()?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(workload)
    })
    .await??;
    let directory = tempfile::tempdir()?;
    let source = create(&directory.path().join("source.kvdb"), memory, 1)?;
    workload.seed(&source)?;
    for case in CASES {
        for iteration in 0..=workload.samples {
            if case == Case::Catchup {
                workload.change(&source, iteration + 1)?;
            }
            tokio::time::timeout(DEADLINE, serve_merge(connection, &source)).await??;
        }
    }
    Ok(())
}

async fn serve_merge(connection: &Connection, source: &Database) -> Result<()> {
    let (mut send, mut recv) = connection.accept_bi().await?;
    if recv.read_u8().await? != 1 {
        return Err("invalid merge request".into());
    }
    let mut sender = SyncSender::new(source)?;
    write_control(&mut send, &sender.root_hash()?).await?;
    let start = Instant::now();
    let cpu = cpu_ns()?;
    let mut output = vec![0; BUFFER];
    let mut request = vec![0; kvdb::sync::MAX_REQUEST_SIZE];
    let mut done = false;
    // SyncSender accepts exactly one framed request per process() call. A QUIC
    // read may split/coalesce writes, so decode the bounded frame length here.
    loop {
        let Some(_) = recv.read(&mut request[..1]).await? else { break };
        recv.read_exact(&mut request[1..8]).await?;
        let length = u32::from_le_bytes(request[..4].try_into()?) as usize;
        if !(8..=request.len()).contains(&length) {
            return Err("invalid sync request length".into());
        }
        recv.read_exact(&mut request[8..length]).await?;
        done = sender.process(&request[..length])? == SyncStatus::Done;
        loop {
            let length = sender.write_output(&mut output)?;
            if length == 0 {
                break;
            }
            send.write_all(&output[..length]).await?;
        }
    }
    if !done {
        return Err("client closed an incomplete sync".into());
    }
    write_control(
        &mut send,
        &json!({"sender_elapsed_ns":start.elapsed().as_nanos() as u64,
        "sender_thread_cpu_ns":cpu_ns()? - cpu}),
    )
    .await?;
    send.finish()?;
    Ok(())
}

async fn client<const MEASURE: bool>(options: &Options) -> Result<()> {
    let peer = tokio::net::lookup_host(options.peer.as_deref().ok_or("missing peer")?)
        .await?
        .next()
        .ok_or("peer has no address")?;
    let file = std::fs::File::open(options.cert.as_ref().ok_or("missing certificate")?)?;
    let mut certificate = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, (CONTROL_LIMIT + 1) as u64),
        &mut certificate,
    )?;
    if certificate.len() > CONTROL_LIMIT {
        return Err("certificate is too large".into());
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(certificate))?;
    let mut config = quinn::ClientConfig::with_root_certificates(Arc::new(roots))?;
    config.transport_config(transport());
    let bind = if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let mut endpoint = Endpoint::client(bind.parse()?)?;
    endpoint.set_default_client_config(config);
    let start = Instant::now();
    let connection = tokio::time::timeout(DEADLINE, endpoint.connect(peer, NAME)?).await??;
    let handshake_ns = start.elapsed().as_nanos() as u64;
    let (mut send, mut recv) = connection.open_bi().await?;
    write_control(&mut send, &options.workload).await?;
    send.finish()?;
    let setup: serde_json::Value = tokio::time::timeout(DEADLINE, async {
        let setup = read_control(&mut recv).await?;
        eof(&mut recv).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(setup)
    })
    .await??;
    if setup["protocol_version"] != kvdb::sync::PROTOCOL_VERSION {
        return Err("server protocol version differs".into());
    }
    println!("{}", json!({"connected":peer,"handshake_ns":handshake_ns,"server":setup}));
    let result = receive_cases::<MEASURE>(&connection, options).await;
    connection.close(if result.is_ok() { 0u32 } else { 1u32 }.into(), b"finished");
    tokio::time::timeout(Duration::from_secs(5), endpoint.wait_idle()).await?;
    result
}

async fn receive_cases<const MEASURE: bool>(
    connection: &Connection,
    options: &Options,
) -> Result<()> {
    let mut directory = tempfile::tempdir()?;
    let mut target = create(&directory.path().join("target.kvdb"), options.memory, 2)?;
    for case in CASES {
        let mut samples = Vec::with_capacity(options.workload.samples);
        for iteration in 0..=options.workload.samples {
            if case == Case::Cold {
                drop(target);
                directory = tempfile::tempdir()?;
                target = create(&directory.path().join("target.kvdb"), options.memory, 2)?;
            }
            let (root, sample) =
                tokio::time::timeout(DEADLINE, receive_merge::<MEASURE>(connection, &target))
                    .await??;
            check_sample(options.workload, case, &sample)?;
            options.workload.verify(
                &target,
                root,
                if case == Case::Catchup { iteration + 1 } else { 0 },
            )?;
            if iteration > 0 {
                samples.push(sample);
            }
        }
        report(options, case, &samples)?;
    }
    Ok(())
}

async fn receive_merge<const MEASURE: bool>(
    connection: &Connection,
    target: &Database,
) -> Result<(Option<(u8, [u8; 32])>, Sample)> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&[1]).await?;
    let root = read_control(&mut recv).await?;
    let calls = Cell::new(StagingCalls::default());
    let staging =
        Staging::<MEASURE> { file: FileStaging::new(tempfile::tempfile()?)?, calls: &calls };
    let mut request = Vec::with_capacity(BUFFER);
    let mut input = vec![0; BUFFER];
    let mut wire = Wire::default();
    let before = connection.stats();
    let start = Instant::now();
    let cpu = cpu_ns()?;
    let mut receiver = SyncReceiver::new(target, Lww, staging, 1, &mut request)?;
    wire.requests.feed(&request)?;
    send.write_all(&request).await?;
    request.clear();
    loop {
        let length = recv.read(&mut input).await?.ok_or("source closed before sync completed")?;
        wire.responses.feed(&input[..length])?;
        let done = receiver.process(&input[..length], &mut request)? == SyncStatus::Done;
        if !request.is_empty() {
            wire.requests.feed(&request)?;
            send.write_all(&request).await?;
            request.clear();
        }
        if done {
            break;
        }
    }
    let finishing = start.elapsed().as_nanos() as u64;
    let outcome = receiver.finish()?;
    let thread_cpu_ns = cpu_ns()? - cpu;
    let total_ns = start.elapsed().as_nanos() as u64;
    send.finish()?;
    let sender: serde_json::Value = read_control(&mut recv).await?;
    eof(&mut recv).await?;
    let peer_complete_ns = start.elapsed().as_nanos() as u64;
    let after = connection.stats();
    Ok((
        root,
        Sample {
            total_ns,
            protocol_and_apply_ns: finishing,
            finish_ns: total_ns - finishing,
            thread_cpu_ns,
            wire,
            applied: outcome.applied,
            staging: MEASURE.then(|| calls.get()),
            quic: Some(json!({"peer_complete_ns":peer_complete_ns,"sender":sender,
            "rtt_ns":connection.rtt().as_nanos() as u64,"path_mtu":after.path.current_mtu,
            "udp_tx_datagrams":after.udp_tx.datagrams - before.udp_tx.datagrams,
            "udp_rx_datagrams":after.udp_rx.datagrams - before.udp_rx.datagrams,
            "udp_tx_bytes":after.udp_tx.bytes - before.udp_tx.bytes,
            "udp_rx_bytes":after.udp_rx.bytes - before.udp_rx.bytes,
            "lost_packets":after.path.lost_packets - before.path.lost_packets})),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quic_cold_equal_and_catchup() {
        // Separate runtimes exercise real UDP, TLS, stream framing and FIN/receipt
        // delivery. Every transfer checks its pinned root and all fixture values.
        let (ready, address) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(async {
                let (endpoint, directory) = server_endpoint("127.0.0.1:0".parse()?)?;
                ready.send((endpoint.local_addr()?, directory.path().join("server.der")))?;
                let connection = tokio::time::timeout(DEADLINE, async {
                    let incoming = endpoint.accept().await.ok_or("server stopped")?;
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(incoming.await?)
                })
                .await??;
                serve(&connection, true).await?;
                tokio::time::timeout(DEADLINE, connection.closed()).await?;
                Ok(())
            })
        });
        let (peer, cert) = address.recv_timeout(DEADLINE).unwrap();
        let options = Options::parse([
            "--quic".into(),
            "--pull-from".into(),
            peer.to_string(),
            "--cert".into(),
            cert.to_str().unwrap().into(),
            "--keys".into(),
            "32".into(),
            "--changes".into(),
            "3".into(),
            "--samples".into(),
            "1".into(),
        ])
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(client::<false>(&options)).unwrap();
        server.join().unwrap().unwrap();
    }
}
