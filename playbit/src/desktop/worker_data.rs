//! Interactive KV work shares the bounded management worker, never the AP lane.
use super::{WorkerResponse, ipc, worker_queue::Credit};
use crate::{
    engine::tool_client::DataContext,
    runtime::host::{DataCall, DataOperation, DataResult},
    store::{EntryId, Id},
};
use std::path::Path;

mod rename;
pub(super) use rename::RenameSource;

pub struct QueuedData {
    pub owner: u64,
    pub sequence: u64,
    pub call: DataCall,
    pub rename: Option<RenameSource>,
    pub _credit: Credit,
    pub epoch: u64,
}
pub struct DataReply {
    pub owner: u64,
    pub sequence: u64,
    pub volume: Id,
    pub entry: EntryId,
    pub result: Result<DataResult, DataFailure>,
    pub committed_root: Option<crate::store::key_value::Root>,
}
pub struct DataFailure {
    pub status: Option<playbit_api::abi::syscall::Error>,
    pub message: String,
}

pub fn retained_bytes(call: &DataCall) -> Option<usize> {
    if !matches!(
        call.operation,
        DataOperation::KvKeys { .. }
            | DataOperation::KvRead { .. }
            | DataOperation::KvPut { .. }
            | DataOperation::KvDelete { .. }
    ) {
        return None;
    }
    call.operation.validate(Default::default()).ok()?;
    // The queued source, context correlation and wire request can coexist.
    call.retained_bytes()?
        .checked_mul(3)?
        .checked_add(call.completion_bytes())?
        .checked_add(std::mem::size_of::<QueuedData>())
}

pub fn execute(profile: &Path, queued: QueuedData, engine_lost: bool) -> WorkerResponse {
    let _timing = crate::trace::Span::new("ui.worker.kv");
    let _kind = crate::trace::Span::new(match queued.call.operation {
        DataOperation::KvKeys { .. } => "ui.worker.kv.keys",
        DataOperation::KvRead { .. } => "ui.worker.kv.read",
        DataOperation::KvPut { .. } => "ui.worker.kv.put",
        DataOperation::KvDelete { .. } => "ui.worker.kv.delete",
        _ => "ui.worker.kv.invalid",
    });
    let volume = Id::from_bytes(queued.call.volume);
    let entry = EntryId::from_bytes(
        queued.call.operation.entry_id().expect("bounded KV admission requires an entry"),
    )
    .expect("validated entry ID");
    let mut committed_root = None;
    let result = if engine_lost {
        Err(DataFailure { status: None, message: super::worker_queue::RETIRED.into() })
    } else if let Some(source) = &queued.rename {
        rename::execute(profile, &queued.call, source).map(|root| {
            committed_root = Some(root);
            DataResult::KvWritten
        })
    } else {
        let exchange = |request| ipc::call_archive(profile, &request)?.into_owned();
        let result: crate::engine::Result<DataResult> = (|| {
            let mut context = DataContext::open_volume(volume, &exchange)?;
            let result = context.execute(queued.call);
            context.finish(result)
        })();
        result.map_err(|error| {
            crate::platform::failure::log(profile, "interactive data IPC", &format!("{error:?}"));
            DataFailure {
                status: error
                    .downcast_ref::<crate::engine::tool_client::DataError>()
                    .map(|error| error.0),
                message: error.to_string(),
            }
        })
    };
    let mut response = WorkerResponse::new(Ok(()));
    response.credit = Some(queued._credit);

    response.data_reply = Some(DataReply {
        owner: queued.owner,
        sequence: queued.sequence,
        volume,
        entry,
        result,
        committed_root,
    });
    response
}
