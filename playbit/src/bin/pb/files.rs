//! Host paths belong to the CLI. The engine only receives bounded owned bytes.
use playbit::{
    engine::{Request, Result, reply::Reply, tool_client::DataContext},
    store::Id,
};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

struct Output {
    path: PathBuf,
}
impl Drop for Output {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            playbit::diagnostic!("Cannot remove incomplete tool output: {error}");
        }
    }
}
pub(super) fn run(
    args: &[String],
    call: &dyn Fn(Request) -> Result<Reply>,
    channel: playbit::release_channel::ReleaseChannel,
) -> Result<serde_json::Value> {
    if (args[0] == "file-write" && args.len() != 4)
        || (args[0] == "file-read" && !(4..=5).contains(&args.len()))
    {
        return Err(
            "expected pb file-write VOLUME ENTRY PATH or file-read VOLUME ENTRY PATH [SESSION]"
                .into(),
        );
    }
    let volume = *Id::parse(&args[1])?.as_bytes();
    let entry = *playbit::store::EntryId::parse(&args[2])?.as_bytes();
    let session = args.get(4).map(|s| super::session_argument(s, channel)).transpose()?;
    let mut context = DataContext::open(session, call)?;
    let result: Result<serde_json::Value> = (|| {
        if args[0] == "file-write" {
            let mut input = File::open(&args[3])?;
            let total = input.metadata()?.len();
            context.write_file(volume, entry, total, &mut input)?;
            Ok(serde_json::json!({"bytes": total}))
        } else {
            let destination = std::path::absolute(&args[3])?;
            let parent = destination.parent().ok_or("File destination has no parent")?;
            let path = parent.join(format!(".playbit-read-{}", Id::generate()?));
            let mut output =
                OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
            let staged = Output { path };
            let bytes = context.read_file(volume, entry, &mut output)?;
            // Exports share the native ordinary-fsync contract. Rust's Apple
            // sync_all requests a device-wide F_FULLFSYNC instead.
            rustix::io::retry_on_intr(|| rustix::fs::fsync(&output))?;
            drop(output);
            fs::rename(&staged.path, &destination)?;
            Ok(serde_json::json!({"bytes": bytes}))
        }
    })();
    context.finish(result)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests assert private output remains intact after failed data exchanges."
)]
mod tests {
    use super::*;
    use playbit::{
        engine::service_call::{Context, Outcome},
        runtime::host::{DataOperation, DataResult},
    };
    #[test]
    fn verified_export_replaces_only_after_a_complete_read_and_preserves_failed_destinations() {
        for (lose_reply, valid) in [(false, false), (true, false), (false, true)] {
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("output");
            fs::write(&destination, b"retain this output").unwrap();
            let id = Id::from_bytes([1; 16]);
            let binding = Context::Tool { handle: id, incarnation: id };
            let reads = std::cell::Cell::new(0);
            let call = |request| match request {
                Request::ToolContextOpen { .. } => Ok(Reply::ToolContext { context: binding }),
                Request::ToolContextClose { context } => Ok(Reply::ToolContextClosed { context }),
                Request::Syscall { context, operation, mutation, call } => {
                    let kind = call.operation.kind() as u32;
                    let result = match call.operation {
                        DataOperation::FileMetadata { .. } => DataResult::FileMetadata {
                            len_bytes: 3,
                            digest: *blake3::hash(b"abc").as_bytes(),
                        },
                        DataOperation::FileRead { .. } if lose_reply => {
                            reads.set(reads.get() + 1);
                            return Err("Read reply lost".into());
                        }
                        DataOperation::FileRead { .. } => {
                            reads.set(reads.get() + 1);
                            DataResult::File(playbit::runtime::host::FileChunk {
                                total_bytes: 3,
                                bytes: if valid { b"abc".to_vec() } else { b"abd".to_vec() },
                            })
                        }
                        _ => panic!("unexpected data operation"),
                    };
                    Ok(Reply::Syscall(Outcome {
                        context,
                        volume: Id::from_bytes(call.volume),
                        operation,
                        mutation,
                        kind,
                        revision: Some((id, 1)),
                        status: 0,
                        result: Some(result),
                    }))
                }
                _ => panic!("unexpected management operation"),
            };
            let args = vec![
                "file-read".into(),
                id.to_string(),
                playbit::store::EntryId::from_bytes([2; 15]).unwrap().to_string(),
                destination.to_str().unwrap().into(),
            ];
            let result = run(&args, &call, playbit::release_channel::ReleaseChannel::Public);
            assert_eq!(reads.get(), 1);
            if valid {
                assert_eq!(result.unwrap(), serde_json::json!({"bytes": 3}));
                assert_eq!(fs::read(&destination).unwrap(), b"abc");
            } else {
                assert!(result.is_err());
                assert_eq!(fs::read(&destination).unwrap(), b"retain this output");
            }
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }
}
