// TODO Auto read

#[cfg(not(target_arch = "wasm32"))]
extern crate playbit;

use playbit_api::abi::data;
use playbit_api::abi::syscall::EventKind;
use playbit_api::context::{Completion, Context, EntryHandle, Event, OperationHandle};
use playbit_api::docdb::{Doc, RNG, SyncHelper};
use playbit_api::ui::{ButtonStyle, TextEvent, TextStyle, ViewEncoder};
use std::io::Cursor;
use std::slice::from_raw_parts_mut;

enum Control {
    Draft = 1,
    Read,
}

#[derive(Default)]
struct App {
    refresh: bool,
    appending: Option<OperationHandle>,
    pending_append: Option<([u8; 16], Vec<u8>)>,
    resolving: Option<OperationHandle>,
    reading: Option<OperationHandle>,
    read_cursor: Option<String>,
    log: Option<EntryHandle>,
    error: String,
    doc: Doc,
    sync_helper: SyncHelper,
}

#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub fn playbit_app() -> playbit_api::App {
    playbit_api::app(App::default(), turn, true)
}

fn describe(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
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
        self.resolving =
            Some(context.begin_resolve_entry(root, "logs/activity").map_err(describe)?);
        self.refresh = true;
        Ok(())
    }

    fn update(&mut self, context: &mut Context<'_>, event: Event<'_>) -> Result<(), String> {
        match event.kind {
            EventKind::Started => self.open(context)?,
            EventKind::DataChanged => self.refresh = true,
            EventKind::ObjectCreationReady => {
                // let result = context.poll_create_object(event.operation().map_err(describe)?);
            }
            EventKind::DataReady => {
                let operation = event.operation().map_err(describe)?;
                if self.resolving == Some(operation) {
                    match context.take_resolve_entry(operation).map_err(describe)? {
                        Completion::Pending => {}
                        Completion::Ready(entry) => {
                            context.log("got it!");
                            self.log = Some(entry);
                            self.resolving = None;
                        }
                    }
                } else if self.reading == Some(operation) {
                    let mut bytes = vec![0; 65_645];
                    let result = context.take_bloblog_read(operation, &mut bytes);
                    if result.is_err() {
                        self.reading = None;
                    }
                    match result.map_err(describe)? {
                        Completion::Pending => {}
                        Completion::Ready(page) => {
                            context.log("page..");
                            for entry in page.entries {
                                // Decode/apply entry.data.
                                // Use entry.id to deduplicate replayed entries.
                                context.log("entry!");
                                let mut cursor = Cursor::new(entry.data);
                                self.sync_helper.import(
                                    &mut self.doc,
                                    &mut cursor,
                                    &mut RNGWrapper { context },
                                );
                                // context.log(str::from_utf8(entry.data).unwrap());
                            }
                            // Copy the cursor before reusing the result buffer.
                            if let Some(log) = self.log
                                && page.more
                            {
                                context.log("more");
                                self.reading = Some(
                                    context
                                        .begin_bloblog_read(log, Some(page.cursor))
                                        .map_err(describe)?,
                                );
                            } else {
                                context.log("done");
                                self.read_cursor = Some(page.cursor.to_owned());
                                self.reading = None;
                            }
                        }
                    }
                } else if self.appending == Some(operation) {
                    let mut bytes = vec![0; 104];
                    context.log("got append");
                    let result = context.take_bloblog_append(operation, &mut bytes);
                    if result.is_err() {
                        self.sync_helper.confirm(false);
                        self.appending = None;
                    }
                    match result.map_err(describe)? {
                        Completion::Pending => {
                            context.log("pending");
                        }
                        Completion::Ready(_receipt) => {
                            context.log(&format!("ready '{}' ", _receipt));
                            self.appending = None;
                            self.refresh = true;
                            self.pending_append = None;
                            self.sync_helper.confirm(true);
                            self.try_write(context);
                        }
                    }
                } else if let Err(error) = context.take_edit_object(operation) {
                    self.refresh = true;
                    context.log("take_edit_object error");
                    return Err(describe(error));
                }
            }
            EventKind::Click if event.target == Control::Read as u32 => {
                if let Some(log) = self.log
                    && self.reading.is_none()
                {
                    context.log("start read");
                    if let Some(read_cursor) = &self.read_cursor {
                        self.reading = Some(
                            context.begin_bloblog_read(log, Some(read_cursor)).map_err(describe)?,
                        );
                    } else {
                        self.reading =
                            Some(context.begin_bloblog_read(log, None).map_err(describe)?);
                    }
                }
            }
            _ => {}
        }
        Ok(())
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

        view.button(Control::Read as u32, "Read", ButtonStyle::Default, self.reading.is_some());

        view.text(TextStyle::Title, "BlobTest");
        let field = view.text_field(
            Control::Draft as u32,
            &mut String::from_utf8(self.doc.get_data(b"obj2").to_vec()).unwrap(),
            "New item (press Return)",
            false,
            busy,
            false,
        );
        if handle_input {
            for event in field.events() {
                if let TextEvent::Submitted(_text) = event {
                    redraw = true;
                } else if let TextEvent::Changed { text, edits } = event {
                    if let Some(edits) = edits {
                        let mut list = Vec::new();
                        for edit in edits {
                            list.push(edit);
                        }
                        let mut ins_text = Vec::new();
                        let mut buf = Vec::new();
                        buf.extend_from_slice(text.as_bytes());
                        for i in 0..list.len() {
                            let edit = list[list.len() - i - 1];
                            ins_text
                                .push(Vec::from(&buf[edit.start..edit.start + edit.inserted_len]));
                            buf.splice(
                                edit.start..edit.start + edit.inserted_len,
                                vec![65u8; edit.end - edit.start],
                            );
                        }
                        for edit in list {
                            if edit.start != edit.end {
                                self.doc.mod_remove(
                                    b"obj2",
                                    edit.start as u32,
                                    (edit.end - edit.start) as u32,
                                );
                            }
                            if edit.inserted_len > 0 {
                                self.doc.mod_insert(
                                    b"obj2",
                                    edit.start as u32,
                                    ins_text.last().unwrap(),
                                );
                            }
                            ins_text.pop();
                            context
                                .log(&format!("{} {} {}", edit.start, edit.end, edit.inserted_len));
                        }
                        context.log(&format!(
                            "=> {}",
                            String::from_utf8(self.doc.get_data(b"obj2").to_vec()).unwrap()
                        ));
                        context.log(&format!("=> {}", text));
                    } else {
                        // Fallback for when the edits couldn't be captured (e.g. browser undo/redo).
                        self.doc.mod_remove(b"obj2", 0, self.doc.get_data(b"obj2").len() as u32);
                        self.doc.mod_insert(b"obj2", 0, text.as_bytes());
                    }
                    redraw = true;
                    self.try_write(context);
                }
            }
        }

        if !self.error.is_empty() {
            view.text(TextStyle::Body, &format!("Couldn't update the app: {}", self.error));
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
    fn secure_random_bytes(&mut self, buf: *mut u8, len: usize) {
        unsafe {
            self.context.random_fill(from_raw_parts_mut(buf, len)).unwrap();
            // self.context.log(&format!("{}", *buf));
        }
    }
}

fn turn(app: &mut App, context: &mut Context<'_>, view: &mut ViewEncoder<'_>) {
    for event in context.raw_events() {
        if let Err(error) = app.update(context, event) {
            app.error = error;
        }
    }
    let redraw = app.render(context, view, true);
    view.submit(context).expect("submit UI scene");
    if redraw {
        // The host presents the last scene; rebuild after handlers without repeating them.
        app.render(context, view, false);
        view.submit(context).expect("submit updated UI scene");
    }
}
