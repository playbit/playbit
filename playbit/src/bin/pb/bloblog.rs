//! Bounded log operations use the same scoped tool authority as the other pb data commands.
use crate::usage_error;
use playbit::{
    engine::{Request, Result, ipc, reply::Reply, tool_client::DataContext},
    runtime::host::{DataCall, DataOperation, DataResult},
    store::{EntryId, Id, bloblog},
};
use std::{io::Read, path::Path};

pub(super) fn run(
    args: &[String],
    profile: &Path,
    call: &dyn Fn(Request) -> Result<Reply>,
) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create") if args.len() == 3 => {
            let volume = Id::parse(&args[1]).map_err(usage_error)?;
            playbit::store::volume_index::validate_name(&args[2]).map_err(usage_error)?;
            let Reply::VolumeEntry { entry, .. } =
                call(Request::BloblogCreate { session: None, volume, name: args[2].clone() })?
            else {
                return Err("unexpected append log creation reply".into());
            };
            println!("{}", serde_json::to_string(&entry)?);
            Ok(())
        }
        Some(command @ ("put" | "put-file")) if args.len() >= 5 => append(command, args, call),
        Some(command @ ("get" | "watch")) if (3..=4).contains(&args.len()) => {
            read(command, args, profile, call)
        }
        _ => Err(usage_error(
            "expected pb bloblog create|put|put-file|get|watch; run pb help bloblog",
        )),
    }
}

fn execute(
    volume: Id,
    operation: DataOperation,
    call: &dyn Fn(Request) -> Result<Reply>,
) -> Result<DataResult> {
    let mut context = DataContext::open_volume(volume, call)?;
    let result = context.execute(DataCall { volume: *volume.as_bytes(), operation });
    context.finish(result)
}

fn put(
    volume: Id,
    log: EntryId,
    append_id: Id,
    blobs: Vec<Vec<u8>>,
    call: &dyn Fn(Request) -> Result<Reply>,
) -> Result<bloblog::Cursor> {
    bloblog::validate_blobs(&blobs)?;
    match execute(
        volume,
        DataOperation::BloblogAppend {
            log: *log.as_bytes(),
            append_id: *append_id.as_bytes(),
            blobs,
        },
        call,
    )? {
        DataResult::BloblogAppended(cursor) => Ok(cursor),
        _ => Err("unexpected bloblog append reply".into()),
    }
}

fn get(
    volume: Id,
    log: EntryId,
    cursor: Option<bloblog::Cursor>,
    call: &dyn Fn(Request) -> Result<Reply>,
) -> Result<bloblog::Page> {
    match execute(volume, DataOperation::BloblogRead { log: *log.as_bytes(), cursor }, call)? {
        DataResult::BloblogPage(page) => Ok(page),
        _ => Err("unexpected bloblog page reply".into()),
    }
}

fn append(command: &str, args: &[String], call: &dyn Fn(Request) -> Result<Reply>) -> Result<()> {
    if args.len() - 4 > bloblog::MAX_BLOBS {
        return Err(usage_error("too many blobs"));
    }
    let volume = Id::parse(&args[1]).map_err(usage_error)?;
    let log = EntryId::parse(&args[2]).map_err(usage_error)?;
    let mut blobs = Vec::with_capacity(args.len() - 4);
    let append_id = Id::parse(&args[3]).map_err(usage_error)?;
    for arg in &args[4..] {
        let data = if command == "put" {
            if arg.len() > bloblog::MAX_BLOB_BYTES {
                return Err(usage_error("blob exceeds 32 KiB"));
            }
            arg.as_bytes().to_vec()
        } else {
            let reader: Box<dyn Read> = if arg == "-" {
                Box::new(std::io::stdin())
            } else {
                Box::new(std::fs::File::open(arg)?)
            };
            let mut bytes = Vec::new();
            reader.take(bloblog::MAX_BLOB_BYTES as u64 + 1).read_to_end(&mut bytes)?;
            bytes
        };
        if data.len() > bloblog::MAX_BLOB_BYTES {
            return Err("blob exceeds 32 KiB".into());
        }
        blobs.push(data);
    }
    println!(
        "{}",
        serde_json::json!({"cursor":put(volume, log, append_id, blobs, call)?,"durability":"remote"})
    );
    Ok(())
}

fn read(
    command: &str,
    args: &[String],
    profile: &Path,
    call: &dyn Fn(Request) -> Result<Reply>,
) -> Result<()> {
    let volume = Id::parse(&args[1]).map_err(usage_error)?;
    let log = EntryId::parse(&args[2]).map_err(usage_error)?;
    let mut cursor =
        args.get(3).map(|s| bloblog::Cursor::parse(s)).transpose().map_err(usage_error)?;
    // Capture the change boundary BEFORE reading, including on first
    // watch. An append racing a read makes the next wait return at once.
    // Start the engine before connecting the notification socket.
    if command == "watch" {
        call(Request::Status)?;
    }
    let mut after = None;
    loop {
        let wait =
            if command == "watch" { Some(ipc::ChangeWait::connect(profile, after)?) } else { None };
        loop {
            let page = get(volume, log, cursor, call)?;
            cursor = Some(page.cursor.clone());
            if command == "get" || !page.entries.is_empty() {
                println!("{}", serde_json::to_string(&page)?);
            }
            if command == "get" {
                return Ok(());
            }
            if !page.more {
                break;
            }
        }
        let reply = wait.expect("the watch mode installed a subscription").receive()?;
        after = Some(reply.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use playbit::engine::service_call::{Context, ToolScope};
    use std::cell::Cell;

    #[test]
    fn append_preserves_text_and_file_bytes_without_replaying_uncertain_writes() {
        // Exercise command parsing through scoped admission and failed acknowledgment.
        let volume = Id::from_bytes([2; 16]);
        let log = EntryId::from_bytes([3; 15]).expect("valid entry ID");
        let append_id = Id::from_bytes([4; 16]);
        let context = Context::Tool { handle: volume, incarnation: volume };
        let directory = tempfile::tempdir().expect("temporary directory");
        let file = directory.path().join("payload");
        std::fs::write(&file, [0, 255, 10]).expect("write binary payload");
        for (command, inputs, expected) in [
            ("put", vec!["hello".to_owned(), String::new()], vec![b"hello".to_vec(), vec![]]),
            (
                "put-file",
                vec![file.to_str().expect("temporary path").to_owned()],
                vec![vec![0, 255, 10]],
            ),
        ] {
            let writes = Cell::new(0);
            let closes = Cell::new(0);
            let exchange = |request| match request {
                Request::ToolContextOpen { scope: ToolScope::Volume(target) } => {
                    assert_eq!(target, volume);
                    Ok(Reply::ToolContext { context })
                }
                Request::Syscall { call, .. } => {
                    assert_eq!(call.volume, *volume.as_bytes());
                    let DataOperation::BloblogAppend { log: target, append_id: id, blobs } =
                        call.operation
                    else {
                        panic!("expected append");
                    };
                    assert_eq!(target, *log.as_bytes());
                    assert_eq!(id, *append_id.as_bytes());
                    assert_eq!(blobs, expected);
                    writes.set(writes.get() + 1);
                    Err("append acknowledgment lost".into())
                }
                Request::ToolContextClose { context } => {
                    closes.set(closes.get() + 1);
                    Ok(Reply::ToolContextClosed { context })
                }
                _ => panic!("unexpected request"),
            };
            let mut args = vec![
                command.to_owned(),
                volume.to_string(),
                log.to_string(),
                append_id.to_string(),
            ];
            args.extend(inputs);
            let error = run(&args, directory.path(), &exchange).expect_err("lost acknowledgment");
            assert_eq!(error.to_string(), "append acknowledgment lost");
            assert_eq!((writes.get(), closes.get()), (1, 1));
        }
    }

    #[test]
    fn create_and_get_use_the_selected_volume_and_return_one_page() {
        // Exercise creation and both initial/cursor reads through the command dispatcher.
        use playbit::engine::service_call::Outcome;
        use playbit::store::{Access, LocalStore};
        let directory = tempfile::tempdir().expect("temporary store");
        let store = LocalStore::open(directory.path()).expect("open store");
        let volume = store.create_volume("CLI test", false).expect("create volume").id;
        let access = Access::owner(store.account().id);
        let entry = store.create_bloblog(&access, volume, "Events").expect("create log");
        let page = store.bloblog_get(&access, volume, entry.id, None).expect("read empty log");
        let context = Context::Tool { handle: volume, incarnation: volume };
        let reads = Cell::new(0);
        let exchange = |request| match request {
            Request::BloblogCreate { volume: target, name, .. } => {
                assert_eq!(target, volume);
                assert_eq!(name, "Events");
                Ok(Reply::VolumeEntry { volume, entry: entry.clone() })
            }
            Request::ToolContextOpen { scope: ToolScope::Volume(target) } => {
                assert_eq!(target, volume);
                Ok(Reply::ToolContext { context })
            }
            Request::Syscall { context, operation, mutation, call } => {
                let DataOperation::BloblogRead { log, cursor } = &call.operation else {
                    panic!("expected read");
                };
                assert_eq!(*log, *entry.id.as_bytes());
                assert_eq!(*cursor, (reads.get() != 0).then(|| page.cursor.clone()));
                reads.set(reads.get() + 1);
                Ok(Reply::Syscall(Outcome {
                    context,
                    volume,
                    operation,
                    mutation,
                    kind: call.operation.kind() as u32,
                    revision: Some((volume, 0)),
                    status: 0,
                    result: Some(DataResult::BloblogPage(page.clone())),
                }))
            }
            Request::ToolContextClose { context } => Ok(Reply::ToolContextClosed { context }),
            _ => panic!("unexpected request"),
        };
        run(&["create".into(), volume.to_string(), "Events".into()], directory.path(), &exchange)
            .expect("create command");
        let mut args = vec!["get".into(), volume.to_string(), entry.id.to_string()];
        run(&args, directory.path(), &exchange).expect("initial read");
        args.push(page.cursor.as_str().to_owned());
        run(&args, directory.path(), &exchange).expect("cursor read");
        assert_eq!(reads.get(), 2);
    }

    #[test]
    fn invalid_arguments_and_oversized_payloads_fail_before_engine_access() {
        // Reject malformed commands and resource-limit violations at CLI admission.
        let volume = Id::from_bytes([2; 16]).to_string();
        let log = EntryId::from_bytes([3; 15]).expect("valid entry ID").to_string();
        let append_id = Id::from_bytes([4; 16]).to_string();
        let exchange = |_| -> Result<Reply> { panic!("invalid input must not reach the engine") };
        let mut too_many = vec!["put".into(), volume.clone(), log.clone(), append_id.clone()];
        too_many.extend(vec![String::new(); bloblog::MAX_BLOBS + 1]);
        for args in [
            vec![],
            vec!["create".into(), volume.clone()],
            vec!["get".into(), "invalid".into(), log.clone()],
            vec!["watch".into(), volume.clone(), log.clone(), "invalid".into()],
            vec!["put".into(), volume, log, append_id, "x".repeat(bloblog::MAX_BLOB_BYTES + 1)],
            too_many,
        ] {
            assert!(run(&args, Path::new("/unused"), &exchange).is_err());
        }
    }
}
