//! TODO state and typed operations, shared by the Wasm and native launchers.
mod trace_names {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/target/playbit/metadata.rs"));
}
use playbit_api::abi::{
    data,
    environment::Surface,
    syscall::{Error, EventKind},
};
use playbit_api::context::{
    Completion, Context, EntryHandle, FieldChange, HostLimit, OperationHandle, PageStorage, Query,
    QueryError, ReadError, Value, ValueRef, VolumeRole,
};
use playbit_api::ui::{ButtonStyle, CheckState, TextStyle, ViewEncoder};

const TYPE: [u8; 16] = [1; 16];
const TITLE: [u8; 16] = [2; 16];
const DONE: [u8; 16] = [3; 16];
const INPUT: u32 = 1;
const ADD: u32 = 2;
const MAX_TITLE_BYTES: usize = 8192;
const MAX_ROWS: usize = 64;
const MAX_VOLUMES: usize = 256;
const MAX_VOLUME_TAGS: usize = 16384;
const MAX_VOLUME_BYTES: usize = 65536;
struct Todo {
    control_id: u32,
    id: [u8; 16],
    title: String,
    done: bool,
}
struct Querying {
    handle: OperationHandle,
    rows: Vec<Todo>,
    next_control_id: u32,
    completing_save: bool,
}
enum Pending {
    Edit { handle: OperationHandle, id: [u8; 16] },
    Read { handle: OperationHandle, id: [u8; 16] },
}
impl Pending {
    fn handle(&self) -> OperationHandle {
        match self {
            Self::Edit { handle, .. } | Self::Read { handle, .. } => *handle,
        }
    }
}
#[derive(Default)]
pub struct App {
    root: Option<EntryHandle>,
    resolving: Option<OperationHandle>,
    surface: Option<Surface>,
    rows: Vec<Todo>,
    next_control_id: u32,
    error: String,
    draft: String,
    query_objects: Vec<data::ObjectInfo>,
    query_fields: Vec<data::Field>,
    query_values: Vec<data::Value>,
    query_bytes: Vec<u8>,
    title_bytes_max: usize,
    query: Option<Querying>,
    pending: Option<Pending>,
    creation: Option<(OperationHandle, String)>,
    refresh_needed: bool,
}
impl App {
    pub fn new() -> Self {
        Self { next_control_id: 100, title_bytes_max: MAX_TITLE_BYTES, ..Self::default() }
    }
    fn root(&mut self, context: &mut Context<'_>) -> Result<EntryHandle, Error> {
        if let Some(root) = self.root {
            return Ok(root);
        }
        if self.resolving.is_some() {
            return Err(Error::WouldBlock);
        }
        // Probe exact metadata sizes before allocating; a concurrent change may
        // fail the subsequent read, which the next data event can retry.
        let (records, values, length) = match context.available_volumes(&mut [], &mut [], &mut []) {
            Err(ReadError::BufferTooSmall { records, values, bytes })
                if records <= MAX_VOLUMES
                    && values <= MAX_VOLUME_TAGS
                    && bytes <= MAX_VOLUME_BYTES =>
            {
                (records, values, bytes)
            }
            Err(ReadError::Host(error)) => return Err(error),
            Err(_) => return Err(Error::LimitExceeded),
            Ok(_) => return Err(Error::NotFound),
        };
        let mut volumes = vec![data::Volume::default(); records];
        let mut tags = vec![data::Range::default(); values];
        let mut bytes = vec![0; length];
        let available =
            context.available_volumes(&mut volumes, &mut tags, &mut bytes).map_err(|error| {
                match error {
                    ReadError::Host(error) => error,
                    ReadError::BufferTooSmall { .. } => Error::LimitExceeded,
                }
            })?;
        #[cfg(feature = "fixture-root")]
        let root = available
            .volume(VolumeRole::Session)
            // Ordinary-session fixtures exercise the released Root flag contract.
            .or_else(|| available.iter().find(|volume| volume.is_root));
        #[cfg(not(feature = "fixture-root"))]
        let root = available.volume(VolumeRole::Session);
        let root = root.ok_or(Error::NotFound)?.handle;
        self.resolving = Some(context.begin_resolve_entry(root, "data")?);
        Err(Error::WouldBlock)
    }
    fn prepare_query(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        if !self.query_objects.is_empty() {
            return Ok(());
        }
        let records = (context.host_limit(HostLimit::QueryRecords)? as usize).min(MAX_ROWS);
        let value_bytes = context.host_limit(HostLimit::QueryValueBytes)? as usize;
        // Each task has one title and one Boolean. Only text uses byte storage;
        // the logical host budget also includes the Boolean's one byte.
        self.title_bytes_max = MAX_TITLE_BYTES.min(value_bytes.saturating_sub(1));
        self.query_bytes.resize(value_bytes.min(records * MAX_TITLE_BYTES), 0);
        self.query_fields.resize(records * 2, data::Field::default());
        self.query_values.resize(records * 2, data::Value::default());
        self.query_objects.resize(records, data::ObjectInfo::default());
        Ok(())
    }
    fn refresh(&mut self, context: &mut Context<'_>, completing_save: bool) -> Result<(), Error> {
        let span = context.trace_begin(trace_names::TODO_REFRESH, None, &[]).ok();
        let result = self.refresh_inner(context, completing_save);
        if let Some(span) = span {
            let _ = context.trace_end(span, &[]);
        }
        result
    }
    fn refresh_inner(
        &mut self,
        context: &mut Context<'_>,
        completing_save: bool,
    ) -> Result<(), Error> {
        if self.busy() {
            self.refresh_needed = true;
            return Ok(());
        }
        let root = match self.root(context) {
            Ok(root) => root,
            Err(Error::WouldBlock) => {
                self.refresh_needed = true;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        self.prepare_query(context)?;
        let handle = context.begin_query_objects(
            root,
            Query {
                type_id: TYPE,
                after: None,
                limit: self.query_objects.len() as u32,
                fields: Some(&[TITLE, DONE]),
            },
        )?;
        self.refresh_needed = false;
        self.query = Some(Querying {
            handle,
            rows: Vec::with_capacity(MAX_ROWS),
            next_control_id: self.next_control_id,
            completing_save,
        });
        Ok(())
    }
    fn complete(
        &mut self,
        context: &mut Context<'_>,
        handle: OperationHandle,
    ) -> Result<(), Error> {
        if self.resolving == Some(handle) {
            let completion = context.take_resolve_entry(handle).inspect_err(|_| {
                self.resolving = None;
            })?;
            match completion {
                Completion::Pending => return Ok(()),
                Completion::Ready(entry) => {
                    self.root = Some(entry);
                    self.resolving = None;
                    self.refresh(context, false)?;
                    self.error.clear();
                    return Ok(());
                }
            }
        }
        if let Some((_, draft)) = self.creation.take_if(|(pending, _)| *pending == handle) {
            match context.poll_create_object(handle) {
                Err(Error::LimitExceeded) => {
                    self.error = "This volume has reached its resource limit. \
                        Existing tasks can still be edited."
                        .into();
                    context.log(&self.error);
                    return Ok(());
                }
                Err(error) => return Err(error),
                Ok(Completion::Pending) => {
                    self.creation = Some((handle, draft));
                    return Ok(());
                }
                Ok(Completion::Ready(_)) => {
                    // Typing during save belongs to the next task.
                    if self.draft == draft {
                        self.draft.clear();
                    }
                    context.log("TODO added");
                    self.refresh(context, false)?;
                    self.error.clear();
                    return Ok(());
                }
            }
        }
        if let Some(query) = self.query.take_if(|query| query.handle == handle) {
            return self.complete_query(context, query);
        }
        let pending =
            self.pending.take_if(|pending| pending.handle() == handle).ok_or(Error::StaleHandle)?;
        match pending {
            Pending::Edit { id, .. } => match context.take_edit_object(handle)? {
                Completion::Pending => {
                    self.pending = Some(pending);
                    Ok(())
                }
                Completion::Ready(()) => {
                    if self.refresh_needed && self.query.is_none() {
                        // The fenced full query already reads the saved row.
                        // Starting another row read first adds a dependent I/O
                        // turn and an intermediate scene without newer data.
                        // Keep controls disabled until that saved snapshot is
                        // installed, rather than enabling their old values.
                        self.refresh(context, true)?;
                        context.log("TODO changed");
                        return Ok(());
                    }
                    let root = self.root(context)?;
                    let handle = context.begin_read_object(root, id, &[TITLE, DONE])?;
                    self.pending = Some(Pending::Read { handle, id });
                    Ok(())
                }
            },
            Pending::Read { id, .. } => {
                let record = match context.take_read_object(
                    handle,
                    &mut self.query_fields,
                    &mut self.query_values,
                    &mut self.query_bytes,
                ) {
                    Ok(Completion::Pending) => {
                        self.pending = Some(pending);
                        return Ok(());
                    }
                    Ok(Completion::Ready(record)) => record,
                    Err(ReadError::Host(error)) => return Err(error),
                    Err(ReadError::BufferTooSmall { .. }) => return Err(Error::LimitExceeded),
                };
                let index = self.rows.iter().position(|row| row.id == id).ok_or(Error::NotFound)?;
                self.rows[index] = Todo::read(
                    playbit_api::context::PageObject { id, record: Some(record) },
                    self.rows[index].control_id,
                )?;
                context.log("TODO changed");
                if self.refresh_needed {
                    self.refresh(context, false)?;
                }
                self.error.clear();
                Ok(())
            }
        }
    }
    fn complete_query(
        &mut self,
        context: &mut Context<'_>,
        mut query: Querying,
    ) -> Result<(), Error> {
        let page = match context.take_query_objects(
            query.handle,
            PageStorage {
                objects: &mut self.query_objects,
                fields: &mut self.query_fields,
                values: &mut self.query_values,
                bytes: &mut self.query_bytes,
            },
        ) {
            Ok(Completion::Pending) => {
                self.query = Some(query);
                return Ok(());
            }
            Ok(Completion::Ready(page)) => page,
            // An accepted save or coalesced change can invalidate this read's
            // captured revision. Discard its staged rows and let that existing
            // refresh intent issue a fresh read after the save finishes.
            Err(QueryError::Host(Error::Unavailable)) if self.refresh_needed => {
                self.refresh(context, query.completing_save)?;
                return Ok(());
            }
            Err(QueryError::Host(error)) => return Err(error),
            Err(QueryError::BufferTooSmall { .. }) => return Err(Error::LimitExceeded),
        };
        for object in page.iter() {
            let control_id = if let Some(row) = self.rows.iter().find(|row| row.id == object.id) {
                row.control_id
            } else {
                let id = query.next_control_id;
                query.next_control_id = id.checked_add(1).ok_or(Error::LimitExceeded)?;
                id
            };
            query.rows.push(Todo::read(object, control_id)?);
        }
        if let Some(next) = page.next
            && query.rows.len() < MAX_ROWS
        {
            let root = self.root(context)?;
            query.handle = context.begin_query_objects(
                root,
                Query {
                    type_id: TYPE,
                    after: Some(next),
                    limit: self.query_objects.len().min(MAX_ROWS - query.rows.len()) as u32,
                    fields: Some(&[TITLE, DONE]),
                },
            )?;
            self.query = Some(query);
            return Ok(());
        }
        // Commit the complete staged snapshot and allocator together.
        self.rows = query.rows;
        self.next_control_id = query.next_control_id;
        if self.refresh_needed {
            self.refresh(context, false)?;
        }
        Ok(())
    }
    fn busy(&self) -> bool {
        self.query.is_some() || self.resolving.is_some() || self.saving()
    }
    fn saving(&self) -> bool {
        self.pending.is_some()
            || self.creation.is_some()
            || self.query.as_ref().is_some_and(|query| query.completing_save)
    }
    fn event(
        &mut self,
        context: &mut Context<'_>,
        kind: EventKind,
        target: u32,
        data: &[u8],
    ) -> Result<(), Error> {
        match kind {
            EventKind::SurfaceChanged => self.surface = Some(Surface::decode(data)?),
            EventKind::Started => {
                self.refresh_needed = true;
            }
            EventKind::DataChanged => self.refresh_needed = true,
            EventKind::TextChanged | EventKind::Submit if target == INPUT => {
                self.draft = if kind == EventKind::TextChanged {
                    playbit_api::abi::text::TextInput::decode(data)?.text
                } else {
                    std::str::from_utf8(data).map_err(|_| Error::InvalidArgument)?
                }
                .into();
                if kind == EventKind::Submit {
                    self.add(context)?;
                }
            }
            EventKind::Click => match target {
                ADD => self.add(context)?,
                100.. => self.toggle(context, target)?,
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn add(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        if self.saving() {
            return Err(Error::Conflict);
        }
        if self.root.is_none() {
            self.error = "Tasks are loading. Try Add when ready.".into();
            return Ok(());
        }
        self.prepare_query(context)?;
        let title = self.draft.trim();
        if title.is_empty() {
            return Ok(());
        }
        let title_limit = self.title_bytes_max;
        if title.len() > title_limit {
            self.error = if title_limit == MAX_TITLE_BYTES {
                "Task is too long. Use at most 8,192 UTF-8 bytes.".into()
            } else {
                format!(
                    "Task is too long. This host supports at most {} UTF-8 bytes per task.",
                    title_limit
                )
            };
            context.log(&self.error);
            return Ok(());
        }
        if self.rows.len() >= MAX_ROWS {
            self.error = "This list already contains 64 tasks.".into();
            return Ok(());
        }
        let title = title.to_owned();
        let title_values = [Value::text(&title)?];
        let done_values = [Value::boolean(false)];
        let fields =
            [FieldChange::set(TITLE, &title_values)?, FieldChange::set(DONE, &done_values)?];
        // Initial fields and the object catalog become visible together.
        let root = self.root(context)?;
        let handle = context.begin_create_object(root, TYPE, &fields)?;
        // A refresh must not discard an accepted Submit. Start its mutation
        // immediately on the input root, retaining at most one concurrent
        // query and one creation. An uncertain mutation is never retried.
        self.creation = Some((handle, self.draft.clone()));
        self.refresh_needed |= self.query.is_some();
        Ok(())
    }

    fn toggle(&mut self, context: &mut Context<'_>, target: u32) -> Result<(), Error> {
        if self.saving() {
            return Err(Error::Conflict);
        }
        let row = self.rows.iter().find(|row| row.control_id == target).ok_or(Error::NotFound)?;
        let id = row.id;
        let values = [Value::boolean(!row.done)];
        let fields = [FieldChange::set(DONE, &values)?];
        let root = self.root(context)?;
        let handle = context.begin_edit_object(root, id, &fields)?;
        self.pending = Some(Pending::Edit { handle, id });
        // Keep one read refresh and one accepted save. A snapshot taken
        // before this edit must not overwrite its completed readback.
        self.refresh_needed |= self.query.is_some();
        Ok(())
    }
    fn render(&mut self, context: &mut Context<'_>, v: &mut ViewEncoder<'_>) -> Result<(), Error> {
        // Keep stored titles intact; the accepted scene grows with the rows.
        let length = 1024
            + self.error.len()
            + self.draft.len()
            + self.rows.iter().map(|row| row.title.len() + 11).sum::<usize>();
        if length > playbit_api::abi::syscall::MAX_CALL_BYTES {
            return Err(Error::LimitExceeded);
        }
        let saving = self.saving();
        v.column_start();
        v.text(TextStyle::Title, "TODO");
        let compact = self.surface.is_some_and(|surface| surface.width < 480);
        if compact {
            v.column_start()
        } else {
            v.row_start()
        }
        v.text_field(INPUT, &mut self.draft, "Add a task", false, false, false);
        v.button(
            ADD,
            "Add",
            ButtonStyle::Default,
            self.root.is_none()
                || saving
                || self.draft.trim().is_empty()
                || self.rows.len() >= MAX_ROWS,
        );
        if compact {
            v.column_end()
        } else {
            v.row_end()
        }
        if !self.error.is_empty() {
            v.text(TextStyle::Body, &self.error);
        }
        for row in &self.rows {
            v.checkbox(
                row.control_id,
                &row.title,
                &mut if row.done { CheckState::On } else { CheckState::Off },
                saving,
            );
        }
        v.column_end();
        v.submit(context)
    }
    pub fn turn(&mut self, context: &mut Context<'_>, view: &mut ViewEncoder<'_>) {
        let span = context.trace_begin(trace_names::TODO_TURN, None, &[]).ok();
        for event in context.raw_events() {
            let result = {
                let input = context.trace_begin(trace_names::TODO_INPUT, span, &[]).ok();
                let result = if matches!(
                    event.kind,
                    EventKind::DataReady | EventKind::ObjectCreationReady
                ) {
                    event.operation().and_then(|handle| self.complete(context, handle))
                } else {
                    self.event(context, event.kind, event.target, event.data)
                };
                if let Some(input) = input {
                    let _ = context.trace_end(input, &[]);
                }
                result
            };
            if let Err(error) = result {
                self.error = format!("Operation failed: {error:?}");
                context.log(&self.error);
            }
        }
        if !self.busy()
            && self.refresh_needed
            && let Err(error) = self.refresh(context, false)
        {
            self.refresh_needed = false;
            self.error = format!("Operation failed: {error:?}");
            context.log(&self.error);
        }
        let _ = context.trace_mark(trace_names::TODO_ROWS, self.rows.len() as u64);
        if let Err(error) = self.render(context, view) {
            context.log(&format!("Rendering failed: {error:?}"));
        }
        if let Some(span) = span {
            let _ = context.trace_end(span, &[]);
        }
    }
}

impl Todo {
    fn read(object: playbit_api::context::PageObject<'_>, control_id: u32) -> Result<Self, Error> {
        let record = object.record.ok_or(Error::Internal)?;
        if record.type_id != TYPE {
            return Err(Error::Internal);
        }
        let mut row = Self { control_id, id: object.id, title: String::new(), done: false };
        for field in record.fields() {
            let mut values = field.values().into_iter().flatten();
            let Some(value) = values.next() else { continue };
            match (field.id, value) {
                (TITLE, ValueRef::Text(text)) => {
                    if text.len() > MAX_TITLE_BYTES {
                        return Err(Error::LimitExceeded);
                    }
                    row.title = text.into();
                }
                (DONE, ValueRef::Boolean(done)) => row.done = done,
                _ => return Err(Error::InvalidArgument),
            }
            if values.next().is_some() {
                return Err(Error::InvalidArgument);
            }
        }
        Ok(row)
    }
}
