//! Failed worker requests release only the matching browser operation.
use super::*;
use crate::store::volume_index::{EntryAddress, HistoryCursor};

#[derive(Default)]
pub(super) struct PermissionRefresh {
    pub catalog: bool,
    pub volume: bool,
    pub entry: bool,
}

pub(super) fn permission_refresh_required(error: &str) -> bool {
    matches!(
        error,
        "access denied: current authority requires authenticated catch-up"
            | "access denied: entry authority requires authenticated catch-up"
            | "access denied: dependency authority requires authenticated catch-up"
    )
}

pub(in crate::desktop) struct Failure(Target);
enum Target {
    Lookup(Request),
    PayloadSize {
        volume: Id,
        entry: Option<EntryId>,
    },
    VolumePermissions {
        volume: Id,
    },
    Rename {
        volume: Id,
        entry: EntryId,
        name: String,
    },
    Create {
        volume: Id,
        pending: creation::PendingCreate,
        name: String,
        parent: Option<EntryId>,
        name_collision: bool,
    },
    Record {
        database: EntryAddress,
        kind: Kind,
    },
    File(files::Pending),
    History {
        address: EntryAddress,
        after: Option<HistoryCursor>,
    },
    Permissions {
        volume: Id,
        entry: EntryId,
    },
}

enum Kind {
    Create(Id),
    Edit(Id, Vec<Id>),
    Remove(Id),
    Read(Id),
    Page(Id, Option<Id>),
    Schema,
}

impl Failure {
    pub(in crate::desktop) fn for_name_collision(
        request: &Request,
        error: &(dyn std::error::Error + 'static),
    ) -> Option<Self> {
        // These four store operations reject this exact name conflict before
        // committing an entry. An uncertain exchange with the same text is not
        // a rejection and must never authorize another creation attempt.
        if !error.is::<crate::engine::ipc::RejectedReply>()
            || error.to_string() != "entry name already exists"
        {
            return None;
        }
        let mut failure = Self::for_request(request)?;
        let Target::Create { name_collision, .. } = &mut failure.0 else { return None };
        *name_collision = true;
        Some(failure)
    }

    pub(in crate::desktop) fn is_payload_size(&self) -> bool {
        matches!(self.0, Target::PayloadSize { .. })
    }

    pub(in crate::desktop) fn is_volume_permissions(&self) -> bool {
        matches!(self.0, Target::VolumePermissions { .. })
    }

    pub(in crate::desktop) fn is_history(&self) -> bool {
        matches!(self.0, Target::History { .. })
    }

    pub(in crate::desktop) fn for_request(request: &Request) -> Option<Self> {
        if let Request::PayloadSize { volume, entry } = request {
            return Some(Self(Target::PayloadSize { volume: *volume, entry: *entry }));
        }
        if let Request::Permissions { volume, .. } = request {
            return Some(Self(Target::VolumePermissions { volume: *volume }));
        }
        if let Request::EntryPermissions { volume, entry, .. } = request {
            return Some(Self(Target::Permissions { volume: *volume, entry: *entry }));
        }
        if let Request::RenameEntry { volume, entry, name }
        | Request::RenameEntryComponent { volume, entry, name } = request
        {
            return Some(Self(Target::Rename {
                volume: *volume,
                entry: *entry,
                name: name.clone(),
            }));
        }
        if let Request::CreateDatabase { volume, name, kind } = request {
            return Some(Self(Target::Create {
                volume: *volume,
                pending: creation::PendingCreate::Database(*kind.clone()),
                name: name.clone(),
                parent: None,
                name_collision: false,
            }));
        }
        if let Request::BloblogCreate { volume, name } = request {
            return Some(Self(Target::Create {
                volume: *volume,
                pending: creation::PendingCreate::AppendLog,
                name: name.clone(),
                parent: None,
                name_collision: false,
            }));
        }
        if let Request::CreateDatabaseIn { volume, parent, name, kind } = request {
            return Some(Self(Target::Create {
                volume: *volume,
                parent: Some(*parent),
                name: name.clone(),
                name_collision: false,
                pending: creation::PendingCreate::Database(*kind.clone()),
            }));
        }
        if let Request::BloblogCreateIn { volume, parent, name } = request {
            return Some(Self(Target::Create {
                volume: *volume,
                parent: Some(*parent),
                name: name.clone(),
                name_collision: false,
                pending: creation::PendingCreate::AppendLog,
            }));
        }
        if matches!(
            request,
            Request::DirectoryEntries { .. }
                | Request::VolumeDetails { .. }
                | Request::Entries { .. }
                | Request::EntryDetails { .. }
        ) {
            return Some(Self(Target::Lookup(request.clone())));
        }
        if let Request::EntryRevisions { address, after, .. } = request {
            return Some(Self(Target::History { address: *address, after: *after }));
        }
        if let Some(file) = files::Pending::for_request(request) {
            return Some(Self(Target::File(file)));
        }
        let (database, kind) = match request {
            Request::CreateObject { database, type_id, .. } => (*database, Kind::Create(*type_id)),
            Request::EditObject { database, object, fields } => {
                (*database, Kind::Edit(*object, fields.keys().copied().collect()))
            }
            Request::RemoveObject { database, object } => (*database, Kind::Remove(*object)),
            Request::ReadObject { database, object } => (*database, Kind::Read(*object)),
            Request::Objects { database, type_id, after, .. } => {
                (*database, Kind::Page(*type_id, *after))
            }
            Request::Schema { database, .. } => (*database, Kind::Schema),
            _ => return None,
        };
        Some(Self(Target::Record { database, kind }))
    }
}

impl Browser {
    pub(in crate::desktop) fn failed(
        &mut self,
        failure: Failure,
        error: &str,
        cx: &mut Context<Self>,
    ) {
        let (database, kind) = match failure.0 {
            Target::PayloadSize { volume, entry } => {
                self.receive_payload_size(
                    volume,
                    entry,
                    Err(error.chars().take(256).collect()),
                    cx,
                );
                return;
            }
            Target::VolumePermissions { volume } => {
                if volume == self.volume.id
                    && self.selected.is_none()
                    && self.volume_permissions_admission.take().is_some()
                {
                    self.permissions_pending = None;
                    self.error = error.into();
                    cx.notify();
                }
                return;
            }
            Target::Permissions { volume, entry } => {
                if volume == self.volume.id
                    && self.selected == Some(entry)
                    && self.permissions_pending.take().is_some()
                {
                    self.error = error.into();
                    cx.notify();
                }
                return;
            }
            Target::Rename { volume, entry, name } => {
                if volume == self.volume.id {
                    self.failed_rename(entry, &name, error, cx);
                }
                return;
            }
            Target::Create { volume, pending, name, parent, name_collision } => {
                if volume == self.volume.id
                    && self.creating_entry.as_ref() == Some(&pending)
                    && self.creating_name.as_ref() == Some(&name)
                    && self.creating_parent == parent
                {
                    self.creating_entry = None;
                    self.creating_name = None;
                    if name_collision
                        && let Some(number) = creation::default_name_number(&name)
                        && self.can_create_entry_in(parent)
                    {
                        self.submit_creation(parent, pending, number + 1, cx);
                    } else {
                        self.error = error.into();
                    }
                    cx.notify();
                }
                return;
            }
            Target::Lookup(request) => return self.failed_lookup(request, error, cx),
            Target::File(file) => return self.failed_file(file, error, cx),
            Target::History { address, after } => {
                return self.failed_history(address, after, error, cx);
            }
            Target::Record { database, kind } => (database, kind),
        };
        let picker = if let Kind::Page(type_id, after) = &kind {
            self.picker_failed(&Request::Objects {
                database,
                type_id: *type_id,
                after: *after,
                limit: 64,
            })
        } else {
            false
        };
        if database.volume != self.volume.id || Some(database.entry) != self.selected {
            if picker {
                self.error = error.into();
                cx.notify();
            }
            return;
        }
        match kind {
            Kind::Create(type_id) if self.records.type_id == Some(type_id) => {
                self.records.creating = false;
            }
            Kind::Edit(object, fields) if self.records.selected == Some(object) => {
                for field in fields {
                    if let Some(draft) = self.records.drafts.get_mut(&field) {
                        draft.submitted = None;
                    }
                    if let Some(editor) = &mut self.records.values
                        && editor.field == field
                    {
                        editor.submitted = None;
                    }
                }
            }
            Kind::Remove(object) if self.records.selected == Some(object) => {}
            Kind::Read(object) if self.records.selected == Some(object) => {
                // Every failed content read drops its previous result. Fetch
                // current metadata before requesting that content again.
                self.retire_record_selection(cx);
                self.refresh();
            }
            Kind::Page(type_id, after)
                if self.records.type_id == Some(type_id) && self.records.after == after =>
            {
                self.records.page = None;
            }
            Kind::Schema => {}
            _ => return,
        }
        if !self.retired_kv_draft_pending() && !self.retired_record_draft_pending() {
            self.error = error.into();
        }
        self.settle_detail_loading();
        cx.notify();
    }
    fn failed_lookup(&mut self, request: Request, error: &str, cx: &mut Context<Self>) {
        let mut matched = self.picker_failed(&request);
        let refreshing = permission_refresh_required(error);
        let mut read_waiting = false;
        match request {
            Request::VolumeDetails { volume } if volume == self.volume.id => {
                self.volume_details = None;
                self.permission_refresh.volume = refreshing;
                read_waiting = refreshing;
                matched = true;
            }
            Request::DirectoryEntries { volume, target, cut, after, .. } => {
                if self.failed_directory(volume, target, cut, after, error, cx) {
                    self.permission_refresh.catalog = refreshing;
                    read_waiting = refreshing;
                    matched = true;
                }
            }
            Request::EntryDetails { volume, entry }
                if volume == self.volume.id && Some(entry) == self.selected =>
            {
                self.clear_blob_preview(cx);
                self.retire_record_selection(cx);
                self.retire_kv_selection(cx);
                self.details = None;
                self.open_target = None;
                self.permission_refresh.entry = refreshing;
                read_waiting = refreshing;
                matched = true;
            }
            _ => {}
        }
        if matched {
            if !read_waiting
                && !self.retired_kv_draft_pending()
                && !self.retired_record_draft_pending()
            {
                self.error = error.into();
            }
            self.settle_detail_loading();
            cx.notify();
        }
    }
}
