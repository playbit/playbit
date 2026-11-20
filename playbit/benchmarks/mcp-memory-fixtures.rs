//! Typed synthetic replies for the operator MCP memory diagnostic, not engine data.
use playbit::{
    engine::{Request, Result, ipc, reply::Reply},
    store::{
        EntryId, Id,
        bloblog::{Cursor, Entry, Page},
    },
};
use std::path::Path;

fn write(directory: &Path, name: &str, bytes: &ipc::codec::Bytes) -> Result<()> {
    let mut file = std::fs::File::create(directory.join(name))?;
    ipc::codec::write(&mut file, bytes)?;
    Ok(())
}

fn main() -> Result<()> {
    let directory = std::env::args_os().nth(1).ok_or("expected fixture directory")?;
    let directory = Path::new(&directory);
    std::fs::create_dir(directory)?;
    let limit = ipc::codec::MAX_ARCHIVE;
    let protocol =
        Reply::Protocol { protocol: ipc::LOCAL_PROTOCOL, version: playbit::VERSION.into() };
    write(directory, "protocol.frame", &ipc::codec::encode(&protocol, limit)?)?;
    write(directory, "protocol.request", &ipc::codec::encode(&Request::LocalProtocol, limit)?)?;
    let id = Id::from_bytes([0; 16]);
    let log = EntryId::from_bytes([1; 15])?;
    let context = playbit::engine::service_call::Context::Tool { handle: id, incarnation: id };
    write(
        directory,
        "bytes.request",
        &ipc::codec::encode(
            &Request::Syscall {
                context,
                operation: 1,
                mutation: 0,
                call: Box::new(playbit::runtime::host::DataCall {
                    volume: *id.as_bytes(),
                    operation: playbit::runtime::host::DataOperation::BloblogRead {
                        log: *log.as_bytes(),
                        cursor: None,
                    },
                }),
            },
            limit,
        )?,
    )?;
    // Leave room for archive metadata. These deliberately exercise near-frame-limit
    // codec responses, beyond the ordinary engine's per-operation data limits.
    let size = limit - 1024;
    let bytes = Reply::Syscall(playbit::engine::service_call::Outcome {
        context,
        volume: id,
        operation: 1,
        mutation: 0,
        kind: playbit::runtime::host::DataKind::BloblogRead as u32,
        revision: Some((id, 0)),
        status: 0,
        result: Some(playbit::runtime::host::DataResult::BloblogPage(Page {
            entries: vec![Entry { log, id, nonce: id, clock: 0, data: vec![255; size] }],
            cursor: Cursor::parse(&format!(
                "02{}{}{}",
                "0".repeat(32),
                playbit::identifier::encode(&[0x01; 15]),
                "0".repeat(64)
            ))?,
            more: false,
        })),
    });
    let text = Reply::Protocol { protocol: ipc::LOCAL_PROTOCOL, version: "\n".repeat(size) };
    for (name, reply) in [("bytes", bytes), ("text", text)] {
        let encoded = ipc::codec::encode(&reply, limit)?;
        // Verify the same validated/owned path that MCP will consume.
        ipc::codec::access::<playbit::engine::reply::ArchivedReply>(&encoded)?;
        write(directory, &format!("{name}.frame"), &encoded)?;
    }
    println!("{size}");
    Ok(())
}
