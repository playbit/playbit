//! Reserve owned request storage before deserializing a validated archive.
use crate::{
    engine::{ArchivedRequest as R, Request, Result},
    store::{self, browser_drafts as drafts, object_contract as object},
};

pub const LIMIT: usize = super::codec::MAX_MESSAGE;

struct Budget(usize);
impl Budget {
    fn data_values(
        &mut self,
        values: &rkyv::vec::ArchivedVec<crate::runtime::host::ArchivedValue>,
    ) -> Result<()> {
        use crate::runtime::host::{ArchivedValue as V, Value};
        self.array::<Value>(values.len())?;
        for value in values.iter() {
            match value {
                V::Text(text) => self.add(text.len())?,
                V::Bytes(bytes) => self.add(bytes.len())?,
                V::Boolean(_)
                | V::Integer(_)
                | V::Float(_)
                | V::Reference { .. }
                | V::File { .. } => {}
            }
        }
        Ok(())
    }
    fn data_fields(
        &mut self,
        fields: &rkyv::vec::ArchivedVec<crate::runtime::host::ArchivedFieldChange>,
    ) -> Result<()> {
        self.array::<crate::runtime::host::FieldChange>(fields.len())?;
        for field in fields.iter() {
            if let Some(values) = field.values.as_ref() {
                self.data_values(values)?;
            }
        }
        Ok(())
    }
    fn data(&mut self, call: &crate::runtime::host::ArchivedDataCall) -> Result<()> {
        use crate::runtime::host::{self as h, ArchivedDataOperation as O};
        self.add(std::mem::size_of::<h::DataCall>())?;
        match &call.operation {
            O::KvKeys { after, .. } => {
                if let Some(key) = after.as_ref() {
                    self.add(key.len())?;
                }
            }
            O::KvRead { key, .. } | O::KvDelete { key, .. } => self.add(key.len())?,
            O::KvPut { key, value, .. } => {
                self.add(key.len())?;
                self.add(value.len())?;
            }
            O::ResolveEntry { name } => self.add(name.len())?,
            O::SchemaRead { .. }
            | O::FileMetadata { .. }
            | O::ChannelEvents { .. }
            | O::AuthenticatedField { .. }
            | O::FileRead { .. }
            | O::FileTransferStart { .. }
            | O::FileTransferCommit { .. } => {}
            O::SchemaEstablish { schema, .. } => {
                self.array::<h::TypeDeclaration>(schema.types.len())?;
                for ty in schema.types.iter() {
                    self.add(ty.name.len())?;
                    self.array::<h::FieldDeclaration>(ty.fields.len())?;
                    for field in ty.fields.iter() {
                        self.add(field.name.len())?;
                    }
                }
            }
            O::Create { fields, .. } | O::Edit { fields, .. } => self.data_fields(fields)?,
            O::Read { fields, .. } => self.array::<playbit_api::abi::syscall::Id>(fields.len())?,
            O::Query { projection, .. } => {
                if let Some(fields) = projection.as_ref() {
                    self.array::<playbit_api::abi::syscall::Id>(fields.len())?;
                }
            }
            O::FileWrite { bytes, .. }
            | O::FileTransferChunk { bytes, .. }
            | O::ChannelCommand { data: bytes, .. } => self.add(bytes.len())?,
            O::BloblogAppend { blobs, .. } => {
                self.array::<Vec<u8>>(blobs.len())?;
                for blob in blobs.iter() {
                    self.add(blob.len())?;
                }
            }
            O::BloblogRead { cursor, .. } => {
                if let Some(cursor) = cursor.as_ref() {
                    self.add(cursor.as_str().len())?;
                }
            }
        }
        Ok(())
    }
    fn add(&mut self, bytes: usize) -> Result<()> {
        self.0 = self
            .0
            .checked_add(bytes)
            .filter(|n| *n <= LIMIT)
            .ok_or("Local request exceeds its owned-memory limit; request did not start")?;
        Ok(())
    }
    fn array<T>(&mut self, count: usize) -> Result<()> {
        self.add(count.checked_mul(std::mem::size_of::<T>()).ok_or("request size overflow")?)
    }
    fn map<K, V>(&mut self, count: usize) -> Result<()> {
        // Rust's BTreeMap nodes have up to 11 key/value slots. Charge a full
        // 12-slot node plus 512 bytes of links/header per entry: every allocated
        // node contains an entry, including partially occupied internal nodes.
        // This intentionally overcounts; no node-layout casts are involved.
        let per_entry = std::mem::size_of::<(K, V)>()
            .checked_mul(12)
            .and_then(|n| n.checked_add(512))
            .ok_or("request size overflow")?;
        self.add(count.checked_mul(per_entry).ok_or("request size overflow")?)
    }
    fn acl(&mut self, acl: &object::ArchivedAcl) -> Result<()> {
        self.array::<object::Grant>(acl.grants.len())?;
        for grant in acl.grants.iter() {
            self.array::<object::Permission>(grant.permissions.len())?;
        }
        Ok(())
    }
    fn draft_values(
        &mut self,
        values: Option<&rkyv::vec::ArchivedVec<object::ArchivedValue>>,
        scalar: bool,
    ) -> Result<()> {
        if let Some(values) = values {
            if values.len() > store::browser_drafts::MAX_VALUES {
                return Err("Too many browser draft values".into());
            }
            let mut bytes = 0usize;
            for value in values.iter() {
                let payload = match value {
                    object::ArchivedValue::Text(text) => {
                        text.len().checked_mul(if scalar { 1 } else { 6 })
                    }
                    object::ArchivedValue::Bytes(bytes) => {
                        bytes.len().checked_mul(if scalar { 1 } else { 4 })
                    }
                    object::ArchivedValue::File { .. } => Some(if scalar { 31 } else { 128 }),
                    object::ArchivedValue::Reference { .. } => Some(if scalar { 47 } else { 192 }),
                    object::ArchivedValue::Boolean(_) => Some(if scalar { 1 } else { 32 }),
                    _ => Some(if scalar { 8 } else { 32 }),
                }
                .ok_or("Browser draft value size overflow")?;
                bytes = bytes
                    .checked_add(payload)
                    .and_then(|bytes| bytes.checked_add(if scalar { 0 } else { 32 }))
                    .filter(|bytes| *bytes <= store::browser_drafts::MAX_FIELD_TEXT)
                    .ok_or("Browser draft value payload exceeds its bound")?;
            }
            self.values(values)?;
        }
        Ok(())
    }
    fn draft_acl(&mut self, acl: &object::ArchivedAcl) -> Result<()> {
        if acl.grants.len() > 64 || acl.grants.iter().any(|grant| grant.permissions.len() > 4) {
            return Err("Browser draft ACL exceeds its bound".into());
        }
        self.acl(acl)
    }
    fn browser_draft(&mut self, draft: &drafts::ArchivedBrowserDraft) -> Result<()> {
        use drafts::ArchivedBrowserDraftBody as Body;
        if draft.ancestors.len() > drafts::MAX_ANCESTORS {
            return Err("Too many browser draft ancestors".into());
        }
        self.array::<store::EntryId>(draft.ancestors.len())?;
        match &draft.body {
            Body::Kv(value) => self.draft_kv(value),
            Body::Permissions(value) => self.draft_permissions(value),
            Body::Record(value) => self.draft_record(value),
        }
    }
    fn draft_kv(&mut self, value: &drafts::ArchivedKvDraft) -> Result<()> {
        if value.key_text.len() > drafts::MAX_KEY_TEXT
            || value.value_text.len() > drafts::MAX_VALUE_TEXT
        {
            return Err("Browser draft text exceeds its bound".into());
        }
        self.add(value.key_text.len())?;
        self.add(value.value_text.len())?;
        if let Some(key) = value.original_key.as_ref() {
            if key.len() > kvdb::KEY_SIZE_MAX {
                return Err("Browser draft key exceeds its bound".into());
            }
            self.add(key.len())?;
        }
        if let Some(base) = value.base.as_ref() {
            if base.bytes.len() > store::key_value::MAX_BYTES {
                return Err("Browser draft baseline exceeds its bound".into());
            }
            self.add(base.bytes.len())?;
        }
        Ok(())
    }
    fn draft_permissions(&mut self, value: &drafts::ArchivedPermissionDraft) -> Result<()> {
        if value.grant_input.len() > drafts::MAX_PRINCIPAL_TEXT {
            return Err("Browser draft principal text exceeds its bound".into());
        }
        self.add(value.grant_input.len())?;
        for active in [&value.base_active, &value.active] {
            if let object::ArchivedEntryAcl::Replace(acl) = active {
                self.draft_acl(acl)?;
            }
        }
        for acl in [value.base_retained.as_ref(), value.retained.as_ref()].into_iter().flatten() {
            self.draft_acl(acl)?;
        }
        Ok(())
    }
    fn draft_record(&mut self, value: &drafts::ArchivedRecordDraft) -> Result<()> {
        if value.fields.len() > drafts::MAX_RECORD_FIELDS
            || value.values.len() > drafts::MAX_RECORD_FIELDS
        {
            return Err("Too many browser draft fields".into());
        }
        self.array::<drafts::RecordFieldDraft>(value.fields.len())?;
        for field in value.fields.iter() {
            if field.text.len() > drafts::MAX_FIELD_TEXT {
                return Err("Browser draft field exceeds its bound".into());
            }
            self.add(field.text.len())?;
            self.draft_values(field.base_values.as_ref(), true)?;
        }
        self.array::<drafts::RecordValuesDraft>(value.values.len())?;
        for values in value.values.iter() {
            if values.input.len() > drafts::MAX_FIELD_TEXT {
                return Err("Browser draft input exceeds its bound".into());
            }
            self.add(values.input.len())?;
            self.draft_values(values.base_values.as_ref(), false)?;
            self.draft_values(values.values.as_ref(), false)?;
        }
        Ok(())
    }
    fn schema(&mut self, schema: &object::ArchivedSchema) -> Result<()> {
        self.map::<store::Id, object::TypeDeclaration>(schema.types.len())?;
        for (_, ty) in schema.types.iter() {
            self.add(ty.name.len())?;
            self.map::<store::Id, object::FieldDeclaration>(ty.fields.len())?;
            for (_, field) in ty.fields.iter() {
                self.add(field.name.len())?;
            }
        }
        Ok(())
    }
    fn values(&mut self, values: &rkyv::vec::ArchivedVec<object::ArchivedValue>) -> Result<()> {
        self.array::<object::Value>(values.len())?;
        for value in values.iter() {
            match value {
                object::ArchivedValue::Text(value) => self.add(value.len())?,
                object::ArchivedValue::Bytes(value) => self.add(value.len())?,
                object::ArchivedValue::Float(_)
                | object::ArchivedValue::Boolean(_)
                | object::ArchivedValue::Integer(_)
                | object::ArchivedValue::File { .. }
                | object::ArchivedValue::Reference { .. } => {}
            }
        }
        Ok(())
    }
    fn payloads(&mut self, lengths: &[usize]) -> Result<()> {
        for length in lengths {
            self.add(*length)?;
        }
        Ok(())
    }
    fn strings(
        &mut self,
        texts: &rkyv::vec::ArchivedVec<rkyv::string::ArchivedString>,
    ) -> Result<()> {
        self.array::<String>(texts.len())?;
        for text in texts.iter() {
            self.add(text.len())?;
        }
        Ok(())
    }
    fn personal_volumes(&mut self, text: &str, count: usize) -> Result<()> {
        self.add(text.len())?;
        self.array::<store::PersonalVolumeGrant>(count)
    }
    fn reconstruction(
        &mut self,
        target: &rkyv::Archived<crate::engine::session_instance::ReconstructionTarget>,
    ) -> Result<()> {
        self.array::<crate::engine::session_instance::ReconstructionTarget>(1)?;
        self.add(target.build.len())
    }
    fn file_read(
        &mut self,
        session: &rkyv::option::ArchivedOption<rkyv::string::ArchivedString>,
        path: &rkyv::vec::ArchivedVec<u8>,
    ) -> Result<()> {
        if let Some(session) = session.as_ref() {
            self.add(session.len())?;
        }
        self.add(path.len())
    }
    fn database(
        &mut self,
        name: &str,
        kind: &store::volume_index::ArchivedEntryKind,
    ) -> Result<()> {
        self.add(name.len())?;
        self.array::<store::volume_index::EntryKind>(1)?;
        if let store::volume_index::ArchivedEntryKind::ObjectDb { schema } = kind {
            self.schema(schema)?;
        }
        Ok(())
    }
    fn fields(
        &mut self,
        fields: &rkyv::Archived<std::collections::BTreeMap<store::Id, Option<Vec<object::Value>>>>,
    ) -> Result<()> {
        self.map::<store::Id, Option<Vec<object::Value>>>(fields.len())?;
        for (_, values) in fields.iter() {
            if let Some(values) = values.as_ref() {
                self.values(values)?;
            }
        }
        Ok(())
    }
    fn initialization(
        &mut self,
        name: &str,
        declaration: &rkyv::Archived<store::RootInitialization>,
    ) -> Result<()> {
        self.add(name.len())?;
        self.array::<store::RootInitialization>(1)?;
        self.acl(&declaration.permissions)?;
        self.array::<store::InitialBlob>(declaration.blobs.len())?;
        for blob in declaration.blobs.iter() {
            self.add(blob.name.len())?;
            self.add(blob.bytes.len())?;
        }
        self.array::<store::InitialDatabase>(declaration.databases.len())?;
        for database in declaration.databases.iter() {
            self.add(database.name.len())?;
            if let store::initialization::ArchivedInitialDatabaseKind::ObjectDb {
                schema,
                objects,
            } = &database.kind
            {
                self.schema(schema)?;
                self.array::<store::InitialObject>(objects.len())?;
                for object in objects.iter() {
                    self.map::<store::Id, Vec<object::Value>>(object.fields.len())?;
                    for (_, values) in object.fields.iter() {
                        self.values(values)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Includes the Request itself and all owned allocations, before any are made.
/// Exhaustive matching makes new request shapes require an explicit review.
pub fn required(request: &R) -> Result<usize> {
    let mut b = Budget(std::mem::size_of::<Request>());
    // Ordinary management reads share the compact embedded request queue.
    // Only a request bearing a selected Shared session allocates its identifier.
    match request {
        R::Entries { session, .. }
        | R::DirectoryEntries { session, .. }
        | R::VolumeDetails { session, .. }
        | R::EntryDetails { session, .. }
        | R::EntryRevisions { session, .. }
        | R::CreateDatabase { session, .. }
        | R::CreateDatabaseIn { session, .. }
        | R::RenameEntry { session, .. }
        | R::RemoveEntry { session, .. }
        | R::EntryPermissions { session, .. }
        | R::BloblogCreate { session, .. }
        | R::BloblogCreateIn { session, .. }
        | R::FileCreate { session, .. }
        | R::FileCreateIn { session, .. }
        | R::PayloadSize { session, .. }
        | R::Schema { session, .. }
        | R::CreateObject { session, .. }
        | R::ReadObject { session, .. }
        | R::EditObject { session, .. }
        | R::RemoveObject { session, .. }
        | R::Objects { session, .. }
        | R::RenameEntryComponent { session, .. }
        | R::KvRename { session, .. }
            if session.is_some() =>
        {
            b.add(std::mem::size_of::<[u8; 32]>())?;
        }
        _ => {}
    }
    match request {
        R::GetAppDisplay { app, .. }
        | R::SetAppEnabled { app, .. }
        | R::DeleteApp { app }
        | R::CreateAppSession { app, .. }
        | R::AppRemovalImpact { app, .. }
        | R::RemoveApp { app, .. }
        | R::AppVolumes { app }
        | R::OpenAppVolume { app, .. } => b.add(app.app_id.len())?,
        R::InitializeApp { app_id, .. } => b.add(app_id.len())?,
        R::ListApps { after } => {
            if let Some(after) = after.as_ref() {
                b.add(after.len())?;
            }
        }
        _ => {}
    }
    match request {
        R::BrowserDrafts { operation, .. } => {
            if let store::browser_drafts::ArchivedBrowserDraftOperation::Upsert { draft, .. } =
                operation.as_ref()
            {
                b.add(std::mem::size_of::<drafts::BrowserDraft>())?;
                b.browser_draft(draft)?;
            }
        }
        R::FileCall { request, .. } => {
            use crate::{
                engine::file_service::ArchivedRequest as F,
                runtime::host::files::ArchivedAction as A,
            };
            b.add(std::mem::size_of::<crate::engine::file_service::Request>())?;
            if let F::Operation(call) = request.as_ref() {
                match &call.action {
                    A::Open { path, .. } => b.add(path.len())?,
                    A::Write { bytes, .. } => b.add(bytes.len())?,
                    A::Read { .. }
                    | A::Status
                    | A::Close { .. }
                    | A::ListOpen
                    | A::ListNext { .. } => {}
                }
            }
        }

        #[rustfmt::skip]
        R::InstanceList { session: text } | R::InstanceConfigurations { session: text } | R::SetDisplayName { name: text }
        | R::CreateVolume { name: text, .. } | R::RequestCode { email: text }
        | R::GrantVolume { email: text, .. } | R::SignIn { code: text, .. }
        | R::RecoverCredentials { secret: text }
        | R::Join { invitation: text } | R::PreviewInvitation { invitation: text }
        | R::PreviewDesktopInvitation { invitation: text }
        | R::RetainDesktopInvitation { session: text }
        | R::Provision { version: text, .. } | R::PersonalVolumes { session: text }
        | R::LabelVolume { label: text, .. }
        | R::RecoverEntry { name: text, .. } | R::RenameEntry { name: text, .. }
        | R::BloblogCreate { name: text, .. }
        | R::GetAppDisplay { build: text, .. } | R::CreateBuildSession { build: text, .. }
        | R::ForgetSession { session: text }
        | R::StartJoin { session: text } | R::PrepareSession { session: text }
        | R::RetrySessionInfo { session: text }
        | R::WatchInvitation { session: text } | R::ResumeInvitation { session: text }
        | R::PauseInvitation { session: text } | R::CheckUpdates { session: text }
        | R::SessionReference { session: text } | R::AutomaticVolumes { session: text } | R::OpenAutomaticVolume { session: text, .. } | R::Invitation { session: text }
        | R::ToolContextOpen { scope: crate::engine::service_call::ArchivedToolScope::Session(text) }
        => b.add(text.len())?,
        R::RenameEntryComponent { name, .. } | R::BloblogCreateIn { name, .. } => {
            if name.len() > store::volume_index::namespace::MAX_COMPONENT_BYTES {
                return Err("Entry component exceeds its name limit; request did not start".into());
            }
            b.add(name.len())?;
        }
        R::PrepareApp { bundle: bytes }
        | R::SignPackage { bundle: bytes }
        | R::AuthenticatePackage { bundle: bytes }
        | R::FileWrite { path: bytes, .. }
        | R::CreateSession { bundle: bytes, .. } => b.add(bytes.len())?,
        R::Syscall { call, .. } => b.data(call)?,
        R::InstanceReserve { session, extra_volumes, host_mounts, .. } => {
            b.personal_volumes(session, extra_volumes.len())?;
            b.array::<crate::engine::host_mount::HostMount>(host_mounts.len())?;
            for mount in host_mounts.iter() {
                b.payloads(&[mount.path.len(), mount.label.len()])?;
            }
        }
        R::InstanceMounts { expected, mounts, .. } => {
            for roots in [expected, mounts] {
                if roots.len() > crate::engine::host_mount::MAX_MOUNTS {
                    return Err("Too many host mount records".into());
                }
                b.array::<crate::engine::host_mount::HostMount>(roots.len())?;
                for mount in roots.iter() {
                    b.payloads(&[mount.path.len(), mount.label.len()])?;
                }
            }
        }
        R::InstanceCompiled { .. } => {
            b.add(std::mem::size_of::<crate::engine::session_instance::Context>())?
        }
        R::InstanceReconstruct { target, .. } => b.reconstruction(target)?,
        R::OrderSessions { sessions } => b.strings(sessions)?,
        R::SignInWithKey { email, account_key } => b.payloads(&[email.len(), account_key.len()])?,
        R::Permissions { acl, .. } => b.acl(acl)?,
        R::Revoke { reason, .. } => b.payloads(&[reason.len()])?,
        R::InitializeRoot { name, declaration, .. }
        | R::InitializeApp { name, declaration, .. } => b.initialization(name, declaration)?,
        R::CreateDatabase { name, kind, .. } => b.database(name, kind)?,
        R::CreateDatabaseIn { name, kind, .. } => {
            if name.len() > store::volume_index::namespace::MAX_COMPONENT_BYTES {
                return Err("Entry component exceeds its name limit; request did not start".into());
            }
            b.database(name, kind)?;
        }
        R::EntryPermissions { permissions, retained_permissions, .. } => {
            if let object::ArchivedEntryAcl::Replace(acl) = permissions {
                b.acl(acl)?;
            }
            if let Some(acl) = retained_permissions.as_ref() {
                b.acl(acl)?;
            }
        }
        R::FileCreate { name, path, .. } => b.payloads(&[name.len(), path.len()])?,
        R::FileCreateIn { name, path, .. } => {
            if name.len() > store::volume_index::namespace::MAX_COMPONENT_BYTES {
                return Err("Entry component exceeds its name limit; request did not start".into());
            }
            b.payloads(&[name.len(), path.len()])?;
        }
        R::FileRead { session, path, .. } => b.file_read(session, path)?,
        R::Schema { schema, .. } => b.schema(schema)?,
        R::CreateObject { fields, .. } | R::EditObject { fields, .. } => b.fields(fields)?,
        R::PublishApp { bundle, notes, .. } => b.payloads(&[bundle.len(), notes.len()])?,
        R::SetAppBuildNotes { app, build, notes } => {
            if !crate::package::valid_app_id(app.app_id.as_str())
                || !crate::identifier::valid(build.as_str(), 32)
                || !crate::network::ap::operation::valid_app_notes(notes.as_str())
            {
                return Err("invalid build ID, app ID or release notes".into());
            }
            b.payloads(&[app.app_id.len(), build.len(), notes.len()])?
        }
        R::KvRename { mutation, .. } => {
            let (old_key, new_key, value) = (&mutation.old_key, &mutation.new_key, &mutation.value);
            b.add(std::mem::size_of::<crate::engine::KvRenameMutation>())?;
            if old_key.is_empty()
                || new_key.is_empty()
                || old_key.len() > kvdb::KEY_SIZE_MAX
                || new_key.len() > kvdb::KEY_SIZE_MAX
                || value.len() > store::key_value::MAX_BYTES
            {
                return Err("KV rename exceeds its bounded key or value limits".into());
            }
            b.payloads(&[old_key.len(), new_key.len(), value.len()])?;
        }
        R::UpdateSession { session, bundle } => b.payloads(&[session.len(), bundle.len()])?,
        R::GetAccountDisplayNames { account_ids, .. } => {
            if account_ids.is_empty()
                || account_ids.len() > crate::network::ap::operation::ACCOUNT_DISPLAY_NAMES_MAX
            {
                return Err("invalid account display-name batch size".into());
            }
            b.add(std::mem::size_of_val(account_ids.as_slice()))?;
        }
        R::WatchAppBuilds { app } => b.add(app.app_id.len())?,
        R::ListAppBuilds { app, after } => {
            b.add(app.app_id.len())?;
            if let Some(cursor) = after.as_ref() {
                b.add(cursor.len())?;
            }
        }
        R::SelectPersonalVolumes { session: version, grants }
        | R::AddPersonalVolumes { session: version, grants } => {
            b.personal_volumes(version, grants.len())?
        }
        #[rustfmt::skip]
        R::InstanceInspect { .. } | R::InstanceStopReason { .. } | R::InstanceStartup { .. } | R::InstanceShow { .. } | R::InstanceCompile { .. } | R::InstanceClaim { .. } | R::InstanceRestore { .. } | R::InstanceConfiguration { .. }
        | R::InstanceLaunchFailed { .. } | R::InstanceChannelsAck { .. } | R::InstanceChannelsClose { .. } | R::InstanceEvents { .. } | R::ToolContextClose { .. }
        | R::ToolContextOpen { scope: crate::engine::service_call::ArchivedToolScope::Account }
        | R::ToolContextOpen { scope: crate::engine::service_call::ArchivedToolScope::Volume(_) | crate::engine::service_call::ArchivedToolScope::BrowserVolume(_) }
        | R::DecideVolumeInvitation { .. } | R::DeleteVolume { .. } | R::DuplicateVolume { .. } | R::DuplicateEntry { .. }
        | R::LocalProtocol | R::AcceptAgreement { .. } | R::Entries { .. } | R::DirectoryEntries { .. } | R::VolumeDetails { .. } | R::EntryDetails { .. }
        | R::EntryRevisions { .. } | R::RejectedEntries { .. } | R::Objects { .. } | R::ReadObject { .. }
        | R::RemoveObject { .. } | R::RemoveEntry { .. } | R::Status | R::DesktopLaunch | R::WaitChanges { .. }
        | R::UiZoom { .. } | R::CacheBudget { .. } | R::Usage { .. } | R::PayloadSize { .. }
        | R::PrepareSignIn | R::SavedAccount | R::RetryAccountKeyBackup
        | R::RecoveryStatus | R::PrepareRecovery | R::SaveRecovery | R::Sync | R::StartSync
        | R::TransferStatus | R::CancelTransfer | R::Disconnect | R::WorkOffline { .. } | R::ListApps { .. }
        | R::SetAppEnabled { .. } | R::DeleteApp { .. }
        | R::AppRemovalImpact { .. } | R::RemoveApp { .. }
        | R::CreateAppSession { .. } | R::AppVolumes { .. } | R::OpenAppVolume { .. } => {}
    }
    Ok(b.0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests construct bounded archive fixtures.")]
mod tests {
    use super::*;
    use crate::engine::ipc::codec;

    #[test]
    fn kv_rename_bounds_are_checked_before_owned_payload_materialization() {
        let address = store::volume_index::EntryAddress {
            volume: store::Id::from_bytes([1; 16]),
            entry: store::EntryId::from_bytes([2; 15]).unwrap(),
        };
        for (old, new, value, accepted) in [
            (kvdb::KEY_SIZE_MAX, kvdb::KEY_SIZE_MAX, store::key_value::MAX_BYTES, true),
            (0, 1, 0, false),
            (1, kvdb::KEY_SIZE_MAX + 1, 0, false),
            (1, 1, store::key_value::MAX_BYTES + 1, false),
        ] {
            let request = Request::KvRename {
                session: None,
                mutation: Box::new(crate::engine::KvRenameMutation {
                    address,
                    old_key: vec![b'a'; old],
                    new_key: vec![b'b'; new],
                    expected: store::key_value::Root { level: 0, digest: [3; 32] },
                    value: vec![0; value],
                }),
            };
            let bytes = codec::encode(&request, codec::MAX_ARCHIVE).unwrap();
            assert_eq!(required(codec::access::<R>(&bytes).unwrap()).is_ok(), accepted);
        }
    }

    #[test]
    fn browser_draft_bounds_are_checked_before_owned_payload_materialization() {
        use store::browser_drafts::*;
        for length in [MAX_VALUE_TEXT, MAX_VALUE_TEXT + 1] {
            let request = Request::BrowserDrafts {
                account: store::Id::from_bytes([1; 16]),
                operation: BrowserDraftOperation::Upsert {
                    expected_revision: None,
                    draft: Box::new(BrowserDraft {
                        id: store::Id::from_bytes([2; 16]),
                        revision: 1,
                        volume: store::Id::from_bytes([3; 16]),
                        entry: Some(store::EntryId::from_bytes([4; 15]).unwrap()),
                        ancestors: vec![],
                        body: BrowserDraftBody::Kv(KvDraft {
                            creating: true,
                            original_key: None,
                            key_text: String::new(),
                            value_text: "f".repeat(length),
                            key_encoding: DraftEncoding::Text,
                            value_encoding: DraftEncoding::Hex,
                            base: None,
                            changed_during_edit: false,
                            uncertain: false,
                            read_unavailable: false,
                            complete_put: false,
                        }),
                    }),
                },
            };
            let bytes = codec::encode(&request, codec::MAX_ARCHIVE).unwrap();
            let archived = codec::access::<R>(&bytes).unwrap();
            assert_eq!(required(archived).is_ok(), length == MAX_VALUE_TEXT);
        }
    }

    #[test]
    fn browser_draft_principal_text_is_charged_and_bounded_before_ownership() {
        // Exact-bound unfinished input survives IPC; reject excess UTF-8 bytes before decoding.
        use store::browser_drafts::*;
        for (input, admitted) in [
            ("x".repeat(MAX_PRINCIPAL_TEXT), true),
            ("é".repeat(MAX_PRINCIPAL_TEXT / 2), true),
            ("x".repeat(MAX_PRINCIPAL_TEXT + 1), false),
            ("é".repeat(MAX_PRINCIPAL_TEXT / 2 + 1), false),
        ] {
            let draft = BrowserDraft {
                id: store::Id::from_bytes([2; 16]),
                revision: 1,
                volume: store::Id::from_bytes([3; 16]),
                entry: Some(store::EntryId::from_bytes([4; 15]).unwrap()),
                ancestors: vec![],
                body: BrowserDraftBody::Permissions(PermissionDraft {
                    base_active: object::EntryAcl::Inherit,
                    base_retained: None,
                    active: object::EntryAcl::Inherit,
                    retained: None,
                    adding_grant: true,
                    grant_input: input.clone(),
                    grant_target: Some(object::Subject::Principal(object::Principal::Service(
                        store::Id::from_bytes([5; 16]),
                    ))),
                    grant_service: true,
                }),
            };
            let request = Request::BrowserDrafts {
                account: store::Id::from_bytes([1; 16]),
                operation: BrowserDraftOperation::Upsert {
                    expected_revision: None,
                    draft: Box::new(draft.clone()),
                },
            };
            let bytes = codec::encode(&request, codec::MAX_ARCHIVE).unwrap();
            let budget = required(codec::access::<R>(&bytes).unwrap());
            if admitted {
                assert_eq!(
                    budget.unwrap(),
                    std::mem::size_of::<Request>()
                        + std::mem::size_of::<BrowserDraft>()
                        + input.len()
                );
                let recovered =
                    rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&bytes).unwrap();
                let Request::BrowserDrafts {
                    operation: BrowserDraftOperation::Upsert { draft: recovered, .. },
                    ..
                } = recovered
                else {
                    panic!("expected the browser draft request")
                };
                assert_eq!(*recovered, draft);
            } else {
                assert!(budget.is_err());
            }
        }
    }

    #[test]
    fn directory_creation_component_lengths_are_bounded_before_ownership() {
        let volume = store::Id::from_bytes([1; 16]);
        let parent = store::EntryId::from_bytes([2; 15]).unwrap();
        let maximum = store::volume_index::namespace::MAX_COMPONENT_BYTES;
        for length in [maximum, maximum + 1] {
            for request in [
                Request::CreateDatabaseIn {
                    session: None,
                    volume,
                    parent,
                    name: "x".repeat(length),
                    kind: Box::new(store::volume_index::EntryKind::KvDb {}),
                },
                Request::BloblogCreateIn {
                    session: None,
                    volume,
                    parent,
                    name: "x".repeat(length),
                },
                Request::FileCreateIn {
                    session: None,
                    volume,
                    parent,
                    name: "x".repeat(length),
                    path: "/tmp/source".into(),
                },
            ] {
                let bytes = codec::encode(&request, codec::MAX_ARCHIVE).unwrap();
                let archived = codec::access::<crate::engine::ArchivedRequest>(&bytes).unwrap();
                assert_eq!(required(archived).is_ok(), length == maximum);
            }
        }
    }

    #[test]
    fn component_name_is_bounded_before_owned_request_materialization() {
        // Valid archives still cannot allocate a component larger than the namespace permits.
        let limit = store::volume_index::namespace::MAX_COMPONENT_BYTES;
        for (length, session) in [(limit, None), (limit, Some([3; 32])), (limit + 1, None)] {
            let request = Request::RenameEntryComponent {
                session: session.map(Box::new),
                volume: store::Id::from_bytes([1; 16]),
                entry: store::EntryId::from_bytes([2; 15]).unwrap(),
                name: "x".repeat(length),
            };
            let bytes = codec::encode(&request, codec::MAX_ARCHIVE).unwrap();
            let budget = required(codec::access::<R>(&bytes).unwrap());
            if length == limit {
                let id = session.as_ref().map_or(0, std::mem::size_of_val);
                assert_eq!(budget.unwrap(), std::mem::size_of::<Request>() + length + id);
            } else {
                assert!(budget.is_err());
            }
        }
    }

    #[test]
    fn empty_strings_cannot_hide_an_oversized_owned_vector() {
        let count = LIMIT / std::mem::size_of::<String>() + 1;
        let request = Request::OrderSessions { sessions: vec![String::new(); count] };
        // A hostile peer need not use our bounded serializer. Give the fixture
        // extra scratch to produce a valid archive which must still be refused.
        let mut scratch = vec![std::mem::MaybeUninit::<u8>::uninit(); 1024 * 1024];
        let bytes = rkyv::api::low::to_bytes_in_with_alloc::<_, _, rkyv::rancor::Error>(
            &request,
            codec::Bytes::new(),
            rkyv::ser::allocator::SubAllocator::new(&mut scratch),
        )
        .unwrap();
        assert!(bytes.len() < codec::MAX_ARCHIVE);
        let archived = codec::access::<R>(&bytes).unwrap();
        assert!(required(archived).is_err());
    }

    #[test]
    fn payload_bytes_and_initialization_collections_are_charged() {
        let request = Request::Syscall {
            context: crate::engine::service_call::Context::Tool {
                handle: store::Id::from_bytes([1; 16]),
                incarnation: store::Id::from_bytes([2; 16]),
            },
            operation: 1,
            mutation: 1,
            call: Box::new(crate::runtime::host::DataCall {
                volume: [3; 16],
                operation: crate::runtime::host::DataOperation::FileWrite {
                    entry_id: [5; 15],
                    bytes: vec![0; 64 * 1024],
                },
            }),
        };
        let bytes = codec::encode(&request, codec::MAX_ARCHIVE).unwrap();
        assert_eq!(
            required(codec::access::<R>(&bytes).unwrap()).unwrap(),
            std::mem::size_of::<Request>()
                + std::mem::size_of::<crate::runtime::host::DataCall>()
                + 64 * 1024
        );
        let request = Request::InitializeRoot {
            volume: store::Id::from_bytes([1; 16]),
            name: "TODO".into(),
            declaration: Box::new(
                crate::package::VolumeInitializers::from_json(include_bytes!(
                    "../../../../guests/todo/playbit-volumes.json"
                ))
                .unwrap()
                .for_role(crate::package::AutomaticVolumeRole::Session),
            ),
        };
        let bytes = codec::encode(&request, codec::MAX_ARCHIVE).unwrap();
        let charge = required(codec::access::<R>(&bytes).unwrap()).unwrap();
        assert!(charge > bytes.len(), "owned maps are charged beyond their archived storage");
        assert!(charge < 64 * 1024);
    }

    #[test]
    fn float_accounting_is_value_independent_and_store_validation_rejects_nonfinite() {
        use std::collections::BTreeMap;
        let volume = store::Id::from_bytes([1; 16]);
        let type_id = store::Id::from_bytes([2; 16]);
        let field_id = store::Id::from_bytes([3; 16]);
        let object_id = store::Id::from_bytes([4; 16]);
        let schema = object::Schema {
            types: BTreeMap::from([(
                type_id,
                object::TypeDeclaration {
                    name: "Measurement".into(),
                    deprecated: false,
                    fields: BTreeMap::from([(
                        field_id,
                        object::FieldDeclaration {
                            name: "value".into(),
                            value_type: object::ValueType::Float,
                            repeated: false,
                            deprecated: false,
                        },
                    )]),
                },
            )]),
        };
        let mut charge = None;
        for value in [0.0, f64::MAX, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let values = vec![object::Value::Float(value)];
            let request = Request::Syscall {
                context: crate::engine::service_call::Context::Tool {
                    handle: volume,
                    incarnation: volume,
                },
                operation: 1,
                mutation: 1,
                call: Box::new(crate::runtime::host::DataCall {
                    volume: *volume.as_bytes(),
                    operation: crate::runtime::host::DataOperation::Create {
                        entry_id: [5; 15],
                        type_id: *type_id.as_bytes(),
                        fields: vec![crate::runtime::host::FieldChange {
                            field_id: *field_id.as_bytes(),
                            values: Some(
                                values
                                    .clone()
                                    .into_iter()
                                    .map(crate::runtime::store_bridge::from_owned_value)
                                    .collect(),
                            ),
                        }],
                    },
                }),
            };
            let bytes = codec::encode(&request, codec::MAX_ARCHIVE).unwrap();
            let required = required(codec::access::<R>(&bytes).unwrap()).unwrap();
            assert_eq!(*charge.get_or_insert(required), required);
            let mut object = object::ObjectFields::new(object_id, type_id);
            let edit = object::FieldEdit {
                object_id,
                type_id,
                field_id,
                stamp: object::EditStamp { clock: 1, device_id: volume },
                values: Some(values),
            };
            assert_eq!(object.apply(&schema, &edit).is_ok(), value.is_finite());
            assert_eq!(object.fields.len(), usize::from(value.is_finite()));
        }
    }
}
