//! Bounded UI admission. Deferred work retains its original credits.
use crate::engine::Request;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

const MAX_REQUESTS: usize = 32;
const MAX_BYTES: usize = 2 * 1024 * 1024;
// Arc's flag and two reference counters, rounded to pointer alignment.
const SIZE_CANCELLATION_BYTES: usize = 3 * std::mem::size_of::<usize>();

#[derive(Default)]
struct Budget {
    count: AtomicUsize,
    bytes: AtomicUsize,
    next_admission: AtomicU64,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum InvitationRequest {
    Saved,
    Preview,
    Dialog,
}

pub struct Credit {
    budget: Arc<Budget>,
    bytes: usize,
    pub(super) browser_session: Option<[u8; 32]>,
    admission: u64,
    pub(super) invitation: Option<InvitationRequest>,
    pub(super) read_cancelled: Option<Arc<AtomicBool>>,
    pub(super) draft_erasure: Option<super::drafts::ErasureHold>,
}
impl Credit {
    pub fn admission(&self) -> u64 {
        self.admission
    }
    pub(super) fn cancelled(&self) -> bool {
        self.read_cancelled.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire))
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        self.budget.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget.count.fetch_sub(1, Ordering::AcqRel);
    }
}
/// The admission and its read share one flag until actual completion releases
/// the credit. A full wake channel cannot lose the retained cancellation.
pub(super) struct SizeRead {
    pub admission: u64,
    cancelled: Arc<AtomicBool>,
    wake: async_channel::Sender<Message>,
}
impl SizeRead {
    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            let _ = self.wake.try_send(Message::CopyComplete);
        }
    }
}
pub struct QueuedRequest {
    pub sign_in: Option<crate::ui::desktop::sign_in::flow::Ticket>,
    pub request: Request,
    pub credit: Option<Credit>,
    pub epoch: u64,
}
impl QueuedRequest {
    pub fn internal(request: Request) -> Self {
        Self { request, credit: None, epoch: 0, sign_in: None }
    }
}
pub enum Message {
    CopyComplete,
    Request(QueuedRequest),
    Data(super::worker_data::QueuedData),
    Launch(super::new_session::Queued),
    Changed(Result<crate::engine::ipc::ChangeReply, String>),
    Fatal(crate::ui::desktop::alert::Failure),
}
pub const RESTARTING: &str =
    "Playbit is restarting its background process. Wait a moment, then try again.";
pub const RETIRED: &str = "Playbit restarted before this action began. Try again.";
pub const INTERRUPTED: &str = "Playbit’s background process stopped during this action. Your change may already have been saved; check before trying again.";

/// An admission belongs to one connection lifetime, even if delivery is delayed.
pub struct Gate {
    epoch: AtomicU64,
    available: AtomicBool,
}
impl Default for Gate {
    fn default() -> Self {
        Self { epoch: AtomicU64::new(0), available: AtomicBool::new(true) }
    }
}
impl Gate {
    pub fn pause(&self) {
        if self.available.swap(false, Ordering::AcqRel) {
            let previous = self.epoch.fetch_add(1, Ordering::AcqRel);
            assert!(previous < u64::MAX, "worker admission epoch must not wrap");
        }
    }
    pub fn resume(&self) {
        self.available.store(true, Ordering::Release);
    }
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
    fn admission(&self) -> Result<u64, &'static str> {
        let epoch = self.epoch();
        self.available.load(Ordering::Acquire).then_some(epoch).ok_or(RESTARTING)
    }
    pub fn accepts(&self, epoch: u64) -> bool {
        self.available.load(Ordering::Acquire) && self.epoch.load(Ordering::Acquire) == epoch
    }
}
struct SenderState {
    send: async_channel::Sender<Message>,
    errors: async_channel::Sender<&'static str>,
    budget: Arc<Budget>,
    gate: Arc<Gate>,
}
impl Drop for SenderState {
    fn drop(&mut self) {
        self.send.close();
    }
}
#[derive(Clone)]
pub struct RequestSender(Arc<SenderState>, pub(super) Option<[u8; 32]>);

// Count capacities, not logical lengths. Only requests with an actual GUI
// caller belong here; adding a new one requires its memory accounting.
fn retained_bytes(request: &Request) -> Option<usize> {
    let heap = match request {
        Request::AppRemovalImpact { app, .. } | Request::RemoveApp { app, .. } => {
            app.app_id.capacity()
        }
        Request::SignInWithKey { email, account_key } => {
            email.capacity().checked_add(account_key.capacity())?
        }
        Request::RequestCode { email: value }
        | Request::SignIn { code: value, .. }
        | Request::CreateVolume { name: value, .. }
        | Request::LabelVolume { label: value, .. }
        | Request::SetDisplayName { name: value }
        | Request::RecoverCredentials { secret: value }
        | Request::PreviewDesktopInvitation { invitation: value }
        | Request::PrepareSession { session: value }
        | Request::RetrySessionInfo { session: value }
        | Request::StartJoin { session: value }
        | Request::CheckUpdates { session: value }
        | Request::WatchInvitation { session: value }
        | Request::PauseInvitation { session: value }
        | Request::ResumeInvitation { session: value }
        | Request::PersonalVolumes { session: value }
        | Request::ForgetSession { session: value } => value.capacity(),
        Request::RenameEntry { name, .. }
        | Request::RenameEntryComponent { name, .. }
        | Request::RecoverEntry { name, .. }
        | Request::BloblogCreate { name, .. }
        | Request::BloblogCreateIn { name, .. } => name.capacity(),
        Request::FileCreate { name, path, .. } | Request::FileCreateIn { name, path, .. } => {
            name.capacity().checked_add(path.capacity())?
        }
        Request::FileWrite { path, .. } => path.capacity(),
        Request::FileRead { session, path, .. } => {
            path.capacity().checked_add(session.as_ref().map_or(0, String::capacity))?
        }
        Request::CreateDatabase { name, kind, .. }
        | Request::CreateDatabaseIn { name, kind, .. } => {
            match kind.as_ref() {
                crate::store::volume_index::EntryKind::KvDb {} => {}
                crate::store::volume_index::EntryKind::ObjectDb { schema }
                    if schema.types.is_empty() => {}
                _ => return None,
            }
            name.capacity()
                .checked_add(std::mem::size_of::<crate::store::volume_index::EntryKind>())?
        }
        Request::Permissions { acl, .. } => acl_heap(acl)?,
        Request::EntryPermissions { permissions, retained_permissions, .. } => {
            permissions_heap(permissions)?
                .checked_add(retained_permissions.as_ref().map_or(Some(0), acl_heap)?)?
        }
        Request::Schema { schema, .. } => schema_heap(schema)?,
        Request::CreateObject { fields, .. } | Request::EditObject { fields, .. } => {
            fields_heap(fields)?
        }
        Request::SelectPersonalVolumes { session, grants } => session.capacity().checked_add(
            grants
                .capacity()
                .checked_mul(std::mem::size_of::<crate::store::PersonalVolumeGrant>())?,
        )?,
        Request::CreateSession { bundle, .. } => bundle.capacity(),
        Request::PayloadSize { .. } => SIZE_CANCELLATION_BYTES,
        Request::GetAccountDisplayNames { account_ids, .. } => {
            use crate::network::ap::operation::ACCOUNT_DISPLAY_NAMES_MAX;
            if account_ids.is_empty() || account_ids.capacity() > ACCOUNT_DISPLAY_NAMES_MAX {
                return None;
            }
            account_ids.capacity().checked_mul(
                std::mem::size_of::<crate::store::Id>()
                    + std::mem::size_of::<crate::network::ap::operation::AccountDisplayName>()
                    + playbit_api::abi::environment::DISPLAY_NAME_BYTES,
            )?
        }
        Request::SavedAccount
        | Request::AcceptAgreement { .. }
        | Request::RetryAccountKeyBackup
        | Request::PrepareSignIn
        | Request::Status
        | Request::DeleteVolume { .. }
        | Request::DecideVolumeInvitation { .. }
        | Request::DuplicateVolume { .. }
        | Request::DuplicateEntry { .. }
        | Request::Entries { .. }
        | Request::DirectoryEntries { .. }
        | Request::VolumeDetails { .. }
        | Request::EntryDetails { .. }
        | Request::EntryRevisions { .. }
        | Request::RejectedEntries { .. }
        | Request::Objects { .. }
        | Request::ReadObject { .. }
        | Request::RemoveObject { .. }
        | Request::RemoveEntry { .. }
        | Request::StartSync
        | Request::WorkOffline { .. }
        | Request::TransferStatus
        | Request::CancelTransfer
        | Request::RecoveryStatus
        | Request::PrepareRecovery
        | Request::SaveRecovery => 0,
        _ => return None,
    };
    let session = request.selected_shared_volume().map_or(0, |(_, id)| std::mem::size_of_val(id));
    heap.checked_add(session)?.checked_add(std::mem::size_of::<QueuedRequest>())
}

fn permissions_heap(permissions: &crate::store::object_contract::EntryAcl) -> Option<usize> {
    use crate::store::object_contract::EntryAcl;
    let EntryAcl::Replace(acl) = permissions else { return Some(0) };
    acl_heap(acl)
}

fn acl_heap(acl: &crate::store::object_contract::Acl) -> Option<usize> {
    use crate::store::object_contract::{Grant, Permission};
    let mut bytes = acl.grants.capacity().checked_mul(std::mem::size_of::<Grant>())?;
    for grant in &acl.grants {
        bytes = bytes.checked_add(
            grant.permissions.capacity().checked_mul(std::mem::size_of::<Permission>())?,
        )?;
    }
    Some(bytes)
}

fn map_heap<K, V>(count: usize) -> Option<usize> {
    let node = std::mem::size_of::<(K, V)>().checked_mul(12)?.checked_add(512)?;
    count.checked_mul(node)
}

fn schema_heap(schema: &crate::store::object_contract::Schema) -> Option<usize> {
    use crate::store::object_contract::{FieldDeclaration, TypeDeclaration};
    let mut bytes = map_heap::<crate::store::Id, TypeDeclaration>(schema.types.len())?;
    for ty in schema.types.values() {
        bytes = bytes
            .checked_add(ty.name.capacity())?
            .checked_add(map_heap::<crate::store::Id, FieldDeclaration>(ty.fields.len())?)?;
        for field in ty.fields.values() {
            bytes = bytes.checked_add(field.name.capacity())?;
        }
    }
    Some(bytes)
}

fn fields_heap(
    fields: &std::collections::BTreeMap<
        crate::store::Id,
        Option<Vec<crate::store::object_contract::Value>>,
    >,
) -> Option<usize> {
    use crate::store::object_contract::Value;
    let mut bytes = map_heap::<crate::store::Id, Option<Vec<Value>>>(fields.len())?;
    for values in fields.values().flatten() {
        bytes = bytes.checked_add(values.capacity().checked_mul(std::mem::size_of::<Value>())?)?;
        for value in values {
            bytes = bytes.checked_add(match value {
                Value::Text(value) => value.capacity(),
                Value::Bytes(value) => value.capacity(),
                _ => 0,
            })?;
        }
    }
    Some(bytes)
}

pub fn channel()
-> (RequestSender, async_channel::Receiver<Message>, async_channel::Receiver<&'static str>) {
    // One extra slot belongs to the sole observer, never an extra UI request.
    let (send, receive) = async_channel::bounded(MAX_REQUESTS + 1);
    let (errors, error_receive) = async_channel::bounded(1);
    (
        RequestSender(
            Arc::new(SenderState {
                send,
                errors,
                budget: Arc::new(Budget::default()),
                gate: Arc::new(Gate::default()),
            }),
            None,
        ),
        receive,
        error_receive,
    )
}
impl RequestSender {
    pub(super) fn for_browser(&self, session: Option<[u8; 32]>) -> Self {
        Self(self.0.clone(), session)
    }
    pub fn gate(&self) -> Arc<Gate> {
        self.0.gate.clone()
    }
    pub fn close(&self) {
        self.0.send.close();
        self.0.errors.close();
    }
    pub fn same_worker(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn observer_sender(&self) -> async_channel::Sender<Message> {
        self.0.send.clone()
    }
    pub fn sign_in(
        &self,
        request: Request,
        ticket: crate::ui::desktop::sign_in::flow::Ticket,
    ) -> Result<(), &'static str> {
        let epoch = self.0.gate.admission()?;
        let bytes = retained_bytes(&request).ok_or("Unsupported account operation")?;
        let credit = self.reserve(bytes)?;
        self.0
            .send
            .try_send(Message::Request(QueuedRequest {
                request,
                credit: Some(credit),
                epoch,
                sign_in: Some(ticket),
            }))
            .map_err(|_| "Account operation could not start")
    }
    pub fn send(&self, request: Request) -> Result<(), &'static str> {
        if matches!(request, Request::DesktopLaunch) {
            // The initial snapshot also retires the prior app's transient seed.
            // It belongs to this launch even if engine admission is recovering.
            return self
                .0
                .send
                .try_send(Message::Request(QueuedRequest::internal(request)))
                .map_err(|_| "Playbit's launch request could not be queued");
        }
        self.enqueue(request).map(|_| ())
    }
    pub(super) fn payload_size(
        &self,
        volume: crate::store::Id,
        entry: Option<crate::store::EntryId>,
    ) -> Result<SizeRead, &'static str> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let admission = self.enqueue_request(
            Request::PayloadSize { session: self.1.map(Box::new), volume, entry },
            Some(cancelled.clone()),
            false,
            None,
        )?;
        Ok(SizeRead { admission, cancelled, wake: self.0.send.clone() })
    }
    pub fn prepare_session(&self, session: String) -> Result<u64, &'static str> {
        self.enqueue(Request::PrepareSession { session })
    }
    pub fn data(
        &self,
        owner: u64,
        sequence: u64,
        call: crate::runtime::host::DataCall,
    ) -> Result<(), &'static str> {
        self.data_with_rename(owner, sequence, call, None)
    }
    pub(super) fn data_rename(
        &self,
        owner: u64,
        sequence: u64,
        call: crate::runtime::host::DataCall,
        source: super::worker_data::RenameSource,
    ) -> Result<(), &'static str> {
        self.data_with_rename(owner, sequence, call, Some(source))
    }
    fn data_with_rename(
        &self,
        owner: u64,
        sequence: u64,
        call: crate::runtime::host::DataCall,
        rename: Option<super::worker_data::RenameSource>,
    ) -> Result<(), &'static str> {
        let result = (|| {
            let epoch = self.0.gate.admission()?;
            let bytes = super::worker_data::retained_bytes(&call)
                .ok_or("This data operation exceeds the app window's limits")?;
            let bytes = match &rename {
                Some(source) => source
                    .retained_bytes(&call)
                    .and_then(|extra| bytes.checked_add(extra))
                    .ok_or("This KV rename exceeds the app window's limits")?,
                None => bytes,
            };
            // The selected ID can coexist in the tool request and its wire frame.
            let session_bytes = self.1.map_or(0, |_| 2 * crate::identifier::MAX_TEXT);
            let credit = self.reserve(
                bytes
                    .checked_add(session_bytes)
                    .ok_or("KV session exceeds the app window's limits")?,
            )?;
            self.0
                .send
                .try_send(Message::Data(super::worker_data::QueuedData {
                    owner,
                    sequence,
                    call,
                    rename,
                    epoch,
                    _credit: credit,
                }))
                .map_err(|_| "Playbit's worker is unavailable; this action did not start")
        })();
        if let Err(error) = result {
            let _ = self.0.errors.try_send(error);
        }
        result
    }
    pub fn launch(
        &self,
        launch: super::new_session::Launch,
        form: gpui::EntityId,
    ) -> Result<async_channel::Receiver<super::new_session::Completion>, &'static str> {
        let epoch = self.0.gate.admission()?;
        let bytes =
            launch.retained_bytes().ok_or("The session configuration exceeds its limits")?;
        let credit = self.reserve(bytes)?;
        let (reply, receive) = async_channel::bounded(1);
        self.0
            .send
            .try_send(Message::Launch(super::new_session::Queued {
                launch,
                form,
                epoch,
                credit,
                reply,
            }))
            .map_err(|_| "Playbit’s worker is unavailable; this session did not start")?;
        Ok(receive)
    }
    fn reserve(&self, bytes: usize) -> Result<Credit, &'static str> {
        let budget = &self.0.budget;
        budget
            .count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_REQUESTS).then_some(n + 1)
            })
            .map_err(|_| "Playbit is busy; this action did not start")?;
        if budget
            .bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(bytes).filter(|n| *n <= MAX_BYTES)
            })
            .is_err()
        {
            budget.count.fetch_sub(1, Ordering::AcqRel);
            return Err("Playbit request memory is full; this action did not start");
        }
        let admission = budget.next_admission.fetch_add(1, Ordering::Relaxed);
        assert!(admission < u64::MAX, "management admission identity must not wrap");
        Ok(Credit {
            budget: budget.clone(),
            bytes,
            browser_session: self.1,
            admission,
            read_cancelled: None,
            draft_erasure: None,
            invitation: None,
        })
    }
    pub(super) fn enqueue(&self, request: Request) -> Result<u64, &'static str> {
        self.enqueue_request(request, None, false, None)
    }
    pub(super) fn join_dialog_request(&self, request: Request) -> Result<u64, &'static str> {
        self.enqueue_request(request, None, true, None)
    }
    pub(super) fn remove_app(
        &self,
        request: Request,
        hold: Option<super::drafts::ErasureHold>,
    ) -> Result<u64, &'static str> {
        let receipt = hold.as_ref().map(|hold| hold.receipt.clone());
        let result = self.enqueue_request(request, None, false, hold);
        if result.is_err()
            && let Some(receipt) = receipt
        {
            let _ = receipt.try_send(Vec::new()); // The request was never admitted.
        }
        result
    }
    fn enqueue_request(
        &self,
        request: Request,
        cancelled: Option<Arc<AtomicBool>>,
        join_dialog: bool,
        draft_erasure: Option<super::drafts::ErasureHold>,
    ) -> Result<u64, &'static str> {
        let optional_read =
            cancelled.is_some() || matches!(request, Request::GetAccountDisplayNames { .. });
        let result = (|| {
            let epoch = self.0.gate.admission()?;
            let bytes = retained_bytes(&request)
                .ok_or("This operation is not supported by the app window")?;
            let mut credit = self.reserve(bytes)?;
            credit.read_cancelled = cancelled;
            credit.draft_erasure = draft_erasure;
            credit.invitation = if join_dialog {
                Some(InvitationRequest::Dialog)
            } else if matches!(request, Request::PreviewDesktopInvitation { .. }) {
                Some(InvitationRequest::Preview)
            } else if matches!(request, Request::ResumeInvitation { .. }) {
                Some(InvitationRequest::Saved)
            } else {
                None
            };
            let admission = credit.admission();
            self.0
                .send
                .try_send(Message::Request(QueuedRequest {
                    request,
                    sign_in: None,
                    credit: Some(credit),
                    epoch,
                }))
                .map(|()| admission)
                .map_err(|_| "Playbit's worker is unavailable; this action did not start")
        })();
        if !optional_read
            && !join_dialog
            && let Err(error) = result
        {
            let _ = self.0.errors.try_send(error);
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    #[test]
    fn catalog_column_and_volume_details_share_normal_admission_credits() {
        let (send, receive, _) = channel();
        let volume = crate::store::Id::from_bytes([1; 16]);
        send.send(Request::DirectoryEntries {
            session: None,
            volume,
            target: crate::store::volume_index::DirectoryTarget::Root,
            cut: None,
            after: None,
            limit: 32,
        })
        .unwrap();
        send.send(Request::VolumeDetails { session: None, volume }).unwrap();
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 2);
        for _ in 0..2 {
            let Message::Request(queued) = receive.try_recv().unwrap() else {
                panic!("catalog request")
            };
            assert!(queued.credit.is_some());
            assert!(matches!(
                queued.request,
                Request::DirectoryEntries { .. } | Request::VolumeDetails { .. }
            ));
        }
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn sign_in_admission_keeps_its_ticket_and_obeys_recovery_epochs() {
        // Retire an old sign-in admission and return its ticket before admitting an explicit retry.
        let (send, receive, _) = channel();
        let ticket = crate::ui::desktop::sign_in::flow::Ticket { generation: 4, operation: 2 };
        send.sign_in(Request::Status, ticket).unwrap();
        let Message::Request(old) = receive.try_recv().unwrap() else { panic!("request") };
        assert_eq!(old.sign_in, Some(ticket));
        send.gate().pause();
        assert_eq!(send.sign_in(Request::Status, ticket), Err(RESTARTING));
        assert!(receive.is_empty());
        send.gate().resume();
        assert!(!send.gate().accepts(old.epoch));
        let response = super::super::worker::retired_response(old);
        assert_eq!(response.sign_in, Some(ticket));
        assert_eq!(response.result.as_ref().map_err(String::as_str), Err(RETIRED));
        send.sign_in(Request::Status, ticket).unwrap();
        let Message::Request(new) = receive.try_recv().unwrap() else { panic!("request") };
        assert!(send.gate().accepts(new.epoch));
        assert_eq!(new.sign_in, Some(ticket));
    }

    #[test]
    fn retired_response_keeps_credit_until_delivery_and_releases_it_once() {
        // Saturate admission while a retired response is pending, then release only its credit.
        let (send, receive, _) = channel();
        send.send(Request::PrepareSession { session: "retired".into() }).unwrap();
        let Message::Request(queued) = receive.try_recv().unwrap() else {
            panic!("expected queued request");
        };
        send.gate().pause();
        send.gate().resume();
        assert!(!send.gate().accepts(queued.epoch));
        let response = super::super::worker::retired_response(queued);
        for _ in 1..MAX_REQUESTS {
            send.send(Request::Status).unwrap();
        }
        assert!(send.send(Request::Status).is_err());
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), MAX_REQUESTS);
        drop(response);
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), MAX_REQUESTS - 1);
        send.send(Request::Status).unwrap();
        while let Ok(message) = receive.try_recv() {
            drop(message);
        }
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), 0);
    }
    #[test]
    fn kv_uses_the_same_queue_credit_and_charges_retained_capacity_before_execution() {
        use crate::runtime::host::{DataCall, DataOperation};
        let (send, receive, _) = channel();
        let keys = || DataCall {
            volume: [1; 16],
            operation: DataOperation::KvKeys {
                entry_id: [2; 15],
                after: None,
                root: None,
                limit: 64,
            },
        };
        for _ in 0..MAX_REQUESTS {
            send.send(Request::Status).unwrap();
        }
        assert!(send.data(1, 1, keys()).is_err());
        drop(receive.try_recv().unwrap());
        send.data(1, 1, keys()).unwrap();
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), MAX_REQUESTS);
        while let Ok(message) = receive.try_recv() {
            drop(message);
        }
        let call = DataCall {
            volume: [1; 16],
            operation: DataOperation::KvPut {
                entry_id: [2; 15],
                key: vec![1],
                value: Vec::with_capacity(MAX_BYTES),
            },
        };
        assert!(send.data(1, 2, call).is_err());
        assert!(receive.try_recv().is_err());
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), 0);
    }
    #[test]
    fn kv_credit_survives_failed_ui_delivery() {
        // Failed storage work retains its admission until the UI drops the response.
        use crate::runtime::host::{DataCall, DataOperation};
        let (send, receive, _) = channel();
        send.data(
            1,
            2,
            DataCall {
                volume: [1; 16],
                operation: DataOperation::KvRead {
                    entry_id: [2; 15],
                    key: vec![3],
                    root: None,
                    offset: 0,
                    limit: 4096,
                },
            },
        )
        .unwrap();
        let Message::Data(queued) = receive.try_recv().unwrap() else { panic!() };
        let profile = tempfile::tempdir().unwrap();
        let response = super::super::worker_data::execute(profile.path(), queued, true);
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 1);
        assert!(send.0.budget.bytes.load(Ordering::Acquire) > 0);
        let reply = response.data_reply.as_ref().unwrap();
        assert_eq!((reply.owner, reply.sequence), (1, 2));
        assert!(reply.result.is_err());
        drop(response);
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), 0);
    }
    #[test]
    fn recovery_retires_old_admissions_and_rejects_new_work_until_ready() {
        let (send, receive, _) = channel();
        send.send(Request::Status).unwrap();
        let gate = send.gate();
        gate.pause();
        assert_eq!(send.send(Request::SaveRecovery), Err(RESTARTING));
        gate.resume();
        let Message::Request(old) = receive.try_recv().unwrap() else { panic!() };
        assert!(!gate.accepts(old.epoch));
        drop(old);
        send.send(Request::Status).unwrap();
        let Message::Request(fresh) = receive.try_recv().unwrap() else { panic!() };
        assert!(gate.accepts(fresh.epoch));
        drop(fresh);
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
    }
    #[test]
    fn invitation_observe_pause_and_resume_use_normal_bounded_admission() {
        let (send, receive, errors) = channel();
        for request in [
            Request::WatchInvitation { session: crate::identifier::encode(&[0x11; 32]) },
            Request::PauseInvitation { session: crate::identifier::encode(&[0x11; 32]) },
            Request::ResumeInvitation { session: crate::identifier::encode(&[0x11; 32]) },
        ] {
            send.send(request).unwrap();
        }
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 3);
        assert!(send.0.budget.bytes.load(Ordering::Acquire) >= 3 * 64);
        assert!(errors.try_recv().is_err());
        assert!(matches!(
            receive.try_recv().unwrap(),
            Message::Request(QueuedRequest { request: Request::WatchInvitation { .. }, .. })
        ));
        assert!(matches!(
            receive.try_recv().unwrap(),
            Message::Request(QueuedRequest { request: Request::PauseInvitation { .. }, .. })
        ));
        assert!(matches!(
            receive.try_recv().unwrap(),
            Message::Request(QueuedRequest { request: Request::ResumeInvitation { .. }, .. })
        ));
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), 0);
    }
    #[test]
    fn deferred_requests_keep_credits_and_saturation_reports_an_error() {
        // Exhaust real admissions: optional Size stays local, mutations report failure,
        // and deferred work retains its credit until actual retirement.
        let (send, receive, errors) = channel();
        for _ in 0..MAX_REQUESTS {
            send.send(Request::Status).unwrap();
        }
        let deferred = receive.try_recv().unwrap();
        assert!(send.payload_size(crate::store::Id::from_bytes([1; 16]), None).is_err());
        assert!(errors.try_recv().is_err(), "optional Size has no global error owner");
        assert!(
            send.send(Request::CreateVolume {
                name: "Blocked".into(),
                share_key_with_service: false
            })
            .is_err()
        );
        assert!(errors.try_recv().unwrap().contains("did not start"));
        drop(deferred);
        send.send(Request::Status).unwrap();
        while let Ok(message) = receive.try_recv() {
            drop(message);
        }
        drop(receive);
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), 0);
    }
    #[test]
    fn inherited_entry_permissions_charge_the_complete_queued_request() {
        // Even a heap-free inherited ACL retains the credit-bearing queue item.
        let (send, receive, _) = channel();
        let request = Request::EntryPermissions {
            session: None,
            volume: crate::store::Id::from_bytes([1; 16]),
            entry: crate::store::EntryId::from_bytes([1; 15]).unwrap(),
            permissions: crate::store::object_contract::EntryAcl::Inherit,
            retained_permissions: None,
        };
        send.send(request).unwrap();
        assert_eq!(
            send.0.budget.bytes.load(Ordering::Acquire),
            std::mem::size_of::<QueuedRequest>()
        );
        drop(receive.try_recv().unwrap());
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn retained_entry_acl_charges_original_capacities_until_delivery() {
        // Spare capacity belongs to the queued ACL too; accounting must inspect it without cloning/shrinking.
        use crate::store::object_contract::{Acl, EntryAcl, Grant, Permission, Subject};
        let (send, receive, _) = channel();
        let mut grants = Vec::with_capacity(64);
        let mut permissions = Vec::with_capacity(128);
        permissions.push(Permission::Read);
        grants.push(Grant { subject: Subject::Public, permissions });
        let expected = std::mem::size_of::<QueuedRequest>()
            + grants.capacity() * std::mem::size_of::<Grant>()
            + grants[0].permissions.capacity() * std::mem::size_of::<Permission>();
        send.send(Request::EntryPermissions {
            session: None,
            volume: crate::store::Id::from_bytes([1; 16]),
            entry: crate::store::EntryId::from_bytes([1; 15]).unwrap(),
            permissions: EntryAcl::Inherit,
            retained_permissions: Some(Acl { grants }),
        })
        .unwrap();
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), expected);
        let queued = receive.try_recv().unwrap();
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), expected);
        drop(queued);
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), 0);
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
    }
    #[test]
    fn volume_creation_charges_retained_capacity_until_release() {
        // Admit a short label with spare capacity and retain its full charge until release.
        let (send, receive, errors) = channel();
        let mut name = String::with_capacity(4096);
        name.push_str("Untitled");
        let bytes = std::mem::size_of::<QueuedRequest>() + name.capacity();
        send.send(Request::CreateVolume { name, share_key_with_service: true }).unwrap();
        let Message::Request(queued) = receive.try_recv().unwrap() else {
            panic!("expected volume creation");
        };
        assert!(
            matches!(&queued.request, Request::CreateVolume { name, share_key_with_service: true } if name == "Untitled")
        );
        assert!(queued.credit.is_some());
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 1);
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), bytes);
        assert!(errors.try_recv().is_err());
        drop(queued);
        assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
        assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), 0);
    }
    #[test]
    fn retained_capacity_is_charged_before_admitting_management() {
        let (send, receive, errors) = channel();
        for request in [
            Request::PreviewDesktopInvitation { invitation: String::with_capacity(MAX_BYTES) },
            Request::CreateVolume {
                name: String::with_capacity(MAX_BYTES),
                share_key_with_service: true,
            },
            Request::LabelVolume {
                volume: crate::store::Id::from_bytes([1; 16]),
                label: String::with_capacity(MAX_BYTES),
            },
        ] {
            assert!(send.send(request).unwrap_err().contains("memory is full"));
            assert!(receive.try_recv().is_err());
            assert!(errors.try_recv().unwrap().contains("memory is full"));
            assert_eq!(send.0.budget.count.load(Ordering::Acquire), 0);
            assert_eq!(send.0.budget.bytes.load(Ordering::Acquire), 0);
        }
    }
    #[test]
    fn observer_does_not_keep_a_closed_window_alive() {
        let (send, receive, _) = channel();
        let observer = send.observer_sender();
        drop(send);
        assert!(receive.is_closed());
        assert!(observer.is_closed());
    }
}
