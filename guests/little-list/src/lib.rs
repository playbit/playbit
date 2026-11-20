#[cfg(not(target_arch = "wasm32"))]
extern crate playbit;

mod schema;

use playbit_api::abi::data;
use playbit_api::abi::syscall::{Error, EventKind};
use playbit_api::context::{Completion, Context, EntryHandle, Event, OperationHandle};
use playbit_api::docdb::{Doc, RNG, SyncHelper, UUID};
use playbit_api::ui::{CheckState, TextEvent, TextStyle, ViewEncoder};
use std::io::Cursor;
use std::slice::from_raw_parts_mut;

enum Control {
    Draft = 1,
    List,
}

#[derive(Default)]
struct App {
    refresh: bool,
    // Remember changes that arrive while a bloblog read is in flight.
    read_requested: bool,
    appending: Option<OperationHandle>,
    pending_append: Option<([u8; 16], Vec<u8>)>,
    resolving: Option<OperationHandle>,
    reading: Option<OperationHandle>,
    read_cursor: Option<String>,
    log: Option<EntryHandle>,
    error: String,
    read_error: String,
    doc: Doc,
    sync_helper: SyncHelper,
    draft: String,
}

#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub fn playbit_app() -> playbit_api::App {
    playbit_api::app(App::default(), turn, true)
}

fn describe(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

fn describe_read(error: Error) -> String {
    match error {
        Error::AccessDenied => {
            "This list can't be read right now. Check access and synchronization, then reopen it if needed.".into()
        }
        _ => describe(error),
    }
}

impl App {
    fn open(&mut self, context: &mut Context<'_>) -> Result<(), String> {
        let mut bytes = vec![0; 16_384];
        let mut volumes = [data::Volume::default(); 256];
        let mut tags = [data::Range::default(); 1024];
        let available =
            context.available_volumes(&mut volumes, &mut tags, &mut bytes).map_err(describe)?;
        let root = available
            .iter()
            .find(|volume| volume.is_root)
            .ok_or("The task volume is unavailable.")?
            .handle;
        self.resolving = Some(context.begin_resolve_entry(root, "docdb").map_err(describe_read)?);
        self.read_requested = true;
        Ok(())
    }

    fn update(&mut self, context: &mut Context<'_>, event: Event<'_>) -> Result<(), String> {
        match event.kind {
            EventKind::Started => {
                if let Err(error) = self.open(context) {
                    self.read_error = error;
                }
            }
            EventKind::DataChanged => self.read_requested = true,
            EventKind::ObjectCreationReady => {
                // let result = context.poll_create_object(event.operation().map_err(describe)?);
            }
            EventKind::DataReady => {
                let operation = event.operation().map_err(describe)?;
                if self.resolving == Some(operation) {
                    match context.take_resolve_entry(operation) {
                        Ok(Completion::Pending) => {}
                        Ok(Completion::Ready(entry)) => {
                            self.log = Some(entry);
                            self.resolving = None;
                        }
                        Err(error) => {
                            self.resolving = None;
                            self.read_error = describe_read(error);
                        }
                    }
                } else if self.reading == Some(operation) {
                    let mut bytes = vec![0; 65_645];
                    let result = context.take_bloblog_read(operation, &mut bytes);
                    if result.is_err() {
                        self.reading = None;
                        self.read_requested = false;
                    }
                    match result {
                        Ok(Completion::Pending) => {}
                        Ok(Completion::Ready(page)) => {
                            for entry in page.entries {
                                // Docdb deduplicates transaction groups when a cursor replays.
                                let mut cursor = Cursor::new(entry.data);
                                self.sync_helper.import(
                                    &mut self.doc,
                                    &mut cursor,
                                    &mut RNGWrapper { context },
                                );
                            }
                            // Copy the cursor before reusing the result buffer.
                            self.read_cursor = Some(page.cursor.to_owned());
                            if let Some(log) = self.log
                                && page.more
                            {
                                self.reading =
                                    match context.begin_bloblog_read(log, Some(page.cursor)) {
                                        Ok(operation) => Some(operation),
                                        Err(error) => {
                                            self.read_requested = false;
                                            self.read_error = describe_read(error);
                                            None
                                        }
                                    };
                            } else {
                                self.reading = None;
                                self.read_error.clear();
                            }
                        }
                        Err(error) => self.read_error = describe_read(error),
                    }
                } else if self.appending == Some(operation) {
                    let mut bytes = vec![0; 104];
                    let result = context.take_bloblog_append(operation, &mut bytes);
                    if result.is_err() {
                        self.sync_helper.confirm(false);
                        self.appending = None;
                    }
                    match result.map_err(describe)? {
                        Completion::Pending => {}
                        Completion::Ready(_receipt) => {
                            self.appending = None;
                            self.refresh = true;
                            self.read_requested = true;
                            self.pending_append = None;
                            self.sync_helper.confirm(true);
                            self.try_write(context);
                        }
                    }
                } else if let Err(error) = context.take_edit_object(operation) {
                    self.refresh = true;
                    return Err(describe(error));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn start_pending_bloblog_read(&mut self, context: &mut Context<'_>) {
        if !self.read_requested || self.reading.is_some() {
            return;
        }
        let Some(log) = self.log else { return };
        self.read_requested = false;
        match context.begin_bloblog_read(log, self.read_cursor.as_deref()) {
            Ok(operation) => self.reading = Some(operation),
            Err(error) => self.read_error = describe_read(error),
        }
    }

    fn try_write(&mut self, context: &mut Context<'_>) {
        if self.appending.is_some() {
            return;
        }
        let Some(log) = self.log else { return };
        if self.pending_append.is_none() {
            let Some(packet) = self.sync_helper.query(&mut self.doc, &mut RNGWrapper { context })
            else {
                return;
            };
            let mut append_id = [0; 16];
            if let Err(error) = context.random_fill(&mut append_id) {
                self.sync_helper.confirm(false);
                self.error = describe(error);
                return;
            }
            self.pending_append = Some((append_id, packet));
        }
        if let Some((id, bytes)) = &self.pending_append {
            match context.begin_bloblog_append(log, id, &[bytes]) {
                Ok(handle) => self.appending = Some(handle),
                Err(error) => {
                    self.sync_helper.confirm(false);
                    self.error = describe(error)
                }
            }
        }
    }

    fn render(
        &mut self,
        context: &mut Context<'_>,
        view: &mut ViewEncoder<'_>,
        handle_input: bool,
    ) -> bool {
        let busy = false;
        let mut redraw = false;

        view.column_start();

        let state_root = schema::StateRoot::root();
        let items = state_root.items();

        view.text(TextStyle::Title, "Little List");
        let field = view.text_field(
            Control::Draft as u32,
            &mut self.draft,
            "New item (press Return)",
            false,
            busy,
            false,
        );
        if handle_input {
            for event in field.events() {
                if let TextEvent::Submitted(_text) = event {
                    items.append(&mut self.doc, UUID::generate(&mut RNGWrapper { context }));
                    let task =
                        schema::Item::from_link(items.get(&self.doc, items.len(&self.doc) - 1));
                    task.text().set(&mut self.doc, &self.draft);
                    self.try_write(context);
                }
            }
        }

        view.scroll_start(Control::List as u32, false);
        view.column_start();
        for task_index in 0..items.len(&self.doc) {
            let task = schema::Item::from_link(items.get(&self.doc, task_index));
            let mut state =
                if task.get_done(&self.doc) == 1 { CheckState::On } else { CheckState::Off };
            let checkbox = view.checkbox(
                100 + task_index as u32,
                task.text().get(&self.doc),
                &mut state,
                busy,
            );
            if checkbox.was_clicked() && handle_input {
                task.set_done(&mut self.doc, if state == CheckState::On { 1 } else { 0 });
                self.try_write(context);
            }
        }
        view.column_end();
        view.scroll_end();

        if !self.error.is_empty() {
            view.text(TextStyle::Body, &format!("Couldn't update the app: {}", self.error));
        }
        if !self.read_error.is_empty() {
            view.text(TextStyle::Body, &format!("Couldn't read your list: {}", self.read_error));
        }
        view.column_end();

        if handle_input && self.refresh {
            // TODO.
            self.refresh = false;
            redraw = true;
        }
        redraw
    }
}

struct RNGWrapper<'a, 'b> {
    context: &'b mut Context<'a>,
}

impl RNG for RNGWrapper<'_, '_> {
    #[allow(unsafe_code)]
    fn secure_random_bytes(&mut self, buf: *mut u8, len: usize) {
        // The docdb RNG callback supplies a writable buffer of exactly `len` bytes.
        #[allow(unsafe_code)]
        unsafe {
            self.context.random_fill(from_raw_parts_mut(buf, len)).unwrap();
        }
    }
}

fn turn(app: &mut App, context: &mut Context<'_>, view: &mut ViewEncoder<'_>) {
    for event in context.raw_events() {
        if let Err(error) = app.update(context, event) {
            app.error = error;
        }
    }
    app.start_pending_bloblog_read(context);
    let redraw = app.render(context, view, true);
    view.submit(context).expect("submit UI scene");
    if redraw {
        // The host presents the last scene; rebuild after handlers without repeating them.
        app.render(context, view, false);
        view.submit(context).expect("submit updated UI scene");
    }
}
