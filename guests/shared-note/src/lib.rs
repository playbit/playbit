#![cfg(target_arch = "wasm32")]
use playbit_api::abi::files::{FILE_PUBLISHED, FILE_READ, FILE_WRITE};
use playbit_api::abi::syscall::{Error, EventKind};
use playbit_api::context::{
    Channel, Completion, Context, File, FileOperation, OperationHandle, PollError, ReadError,
    VolumeRole,
};
use playbit_api::ui::{ButtonStyle, TextStyle, ViewEncoder};
const MAX_NOTE: usize = 8192;

enum Pending {
    OpenRead { operation: FileOperation, revision: u64, saved: bool },
    Read { operation: FileOperation, file: File, revision: u64, saved: bool, bytes: Vec<u8> },
    OpenWrite { operation: FileOperation, revision: u64, bytes: String },
    CloseWrite { operation: FileOperation, revision: u64 },
}
impl Pending {
    fn handle(&self) -> OperationHandle {
        match self {
            Self::OpenRead { operation, .. }
            | Self::Read { operation, .. }
            | Self::OpenWrite { operation, .. }
            | Self::CloseWrite { operation, .. } => operation.handle(),
        }
    }
}
#[derive(Default)]
struct App {
    root: Option<File>,
    note: String,
    draft: String,
    revision: u64,
    edited: bool,
    refresh: bool,
    pending: Option<Pending>,
    error: String,
    channels: Option<[Channel; 2]>,
    channel_operations: Vec<(OperationHandle, Option<usize>)>,
    subscribed: [bool; 2],
    preview: String,
    waves: usize,
    participants: usize,
    channel_status: String,
}
impl App {
    fn start_channels(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        let session = match context.session() {
            Ok(session) => session,
            Err(Error::Unsupported) => return Ok(()),
            Err(error) => return Err(error),
        };
        let channels = [session.channel(context, "drafts")?, session.channel(context, "waves")?];
        self.channels = Some(channels);
        self.subscribe_channels(context)?;
        self.channel_status = "Connecting live channels…".into();
        Ok(())
    }
    fn subscribe_channels(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        let Some(channels) = self.channels else { return Ok(()) };
        for (index, channel) in channels.into_iter().enumerate() {
            if !self.subscribed[index]
                && !self.channel_operations.iter().any(|(_, pending)| *pending == Some(index))
            {
                self.channel_operations.push((channel.subscribe(context)?, Some(index)));
            }
        }
        Ok(())
    }
    fn send_channel(&mut self, context: &mut Context<'_>, index: usize) -> Result<(), Error> {
        let channel = self.channels.ok_or(Error::Unsupported)?[index];
        let bytes = if index == 0 { self.draft.as_bytes() } else { b"wave" };
        self.channel_operations.push((channel.publish(context, bytes)?, None));
        Ok(())
    }

    fn session_volume(context: &mut Context<'_>) -> Result<File, Error> {
        let mut volumes = vec![playbit_api::abi::data::Volume::default(); 256];
        let mut tags = vec![playbit_api::abi::data::Range::default(); 16384];
        let mut bytes = vec![0; 65536];
        let mut available =
            context.available_volumes(&mut volumes, &mut tags, &mut bytes).map_err(read_error)?;
        available.take_volume_file(VolumeRole::Session).ok_or(Error::NotFound)
    }
    fn begin_refresh(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        if self.root.is_none() {
            self.root = Some(Self::session_volume(context)?);
        }
        self.begin_read(context, self.revision, false)
    }
    fn begin_read(
        &mut self,
        context: &mut Context<'_>,
        revision: u64,
        saved: bool,
    ) -> Result<(), Error> {
        let operation =
            self.root.as_ref().ok_or(Error::NotFound)?.open(context, b"note.txt", FILE_READ)?;
        self.pending = Some(Pending::OpenRead { operation, revision, saved });
        // This newly accepted open covers changes already observed. Changes
        // arriving during the read still request another fresh view.
        self.refresh = false;
        Ok(())
    }
    fn edit(&mut self, data: &[u8]) -> Result<(), Error> {
        let draft = std::str::from_utf8(data).map_err(|_| Error::InvalidArgument)?;
        if draft != self.draft {
            self.revision = self.revision.checked_add(1).ok_or(Error::LimitExceeded)?;
            draft.clone_into(&mut self.draft);
            self.edited = self.draft != self.note;
        }
        if self.draft.len() > MAX_NOTE {
            return Err(Error::LimitExceeded);
        }
        Ok(())
    }
    fn save(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        if self.draft.len() > MAX_NOTE {
            return Err(Error::LimitExceeded);
        }
        if self.pending.is_some() {
            return Err(Error::Conflict);
        }
        let operation =
            self.root.as_ref().ok_or(Error::NotFound)?.open(context, b"note.txt", FILE_WRITE)?;
        self.pending = Some(Pending::OpenWrite {
            operation,
            revision: self.revision,
            bytes: self.draft.clone(),
        });
        Ok(())
    }
    fn complete(
        &mut self,
        context: &mut Context<'_>,
        handle: OperationHandle,
    ) -> Result<bool, Error> {
        if let Some(index) =
            self.channel_operations.iter().position(|(operation, _)| *operation == handle)
        {
            match context.take_channel(handle) {
                Ok(Completion::Pending) => return Ok(false),
                Ok(Completion::Ready(())) => {
                    if let Some(channel) = self.channel_operations.remove(index).1 {
                        self.subscribed[channel] = true;
                    }
                    self.channel_status =
                        "Live messages are ephemeral. Save keeps the note in this session’s \
                         Shared data."
                            .into();
                }
                Err(error) => {
                    self.channel_operations.remove(index);
                    self.channel_status = format!("Live channel: {error:?}");
                }
            }
            return Ok(true);
        }
        if self.pending.as_ref().is_none_or(|pending| pending.handle() != handle) {
            return Ok(false);
        }
        let pending = self.pending.take().expect("matched pending operation");
        match pending {
            Pending::OpenRead { mut operation, revision, saved } => {
                match operation.take_open(context) {
                    Ok(Completion::Pending) => {
                        self.pending = Some(Pending::OpenRead { operation, revision, saved })
                    }
                    Ok(Completion::Ready(file)) => {
                        let operation = file.read(context, 0, (MAX_NOTE + 1) as u32)?;
                        self.pending = Some(Pending::Read {
                            operation,
                            file,
                            revision,
                            saved,
                            bytes: Vec::new(),
                        });
                    }
                    Err(Error::NotFound) if !saved => {
                        self.note.clear();
                        self.error.clear();
                        if self.revision == revision && !self.edited {
                            self.draft.clear();
                        }
                        return Ok(true);
                    }
                    Err(error) => return Err(error),
                }
            }
            Pending::Read { mut operation, file, revision, saved, mut bytes } => {
                let mut storage = [0; MAX_NOTE + 1];
                match operation.take_read(context, &mut storage).map_err(poll_error)? {
                    Completion::Pending => {
                        self.pending =
                            Some(Pending::Read { operation, file, revision, saved, bytes });
                    }
                    Completion::Ready(part) => {
                        if bytes.len() + part.len() > MAX_NOTE {
                            return Err(Error::LimitExceeded);
                        }
                        if part.is_empty() {
                            self.note =
                                String::from_utf8(bytes).map_err(|_| Error::InvalidArgument)?;
                            self.error.clear();
                            if self.revision == revision && (saved || !self.edited) {
                                self.draft = self.note.clone();
                                self.edited = false;
                            }
                            return Ok(true);
                        } else {
                            bytes.extend_from_slice(part);
                            // A short read is not EOF. Include one extra byte to
                            // distinguish a full-size note from an oversized file.
                            let operation = file.read(
                                context,
                                bytes.len() as u64,
                                (MAX_NOTE + 1 - bytes.len()) as u32,
                            )?;
                            self.pending =
                                Some(Pending::Read { operation, file, revision, saved, bytes });
                        }
                    }
                }
            }
            Pending::OpenWrite { mut operation, revision, bytes } => {
                match operation.take_open(context)? {
                    Completion::Pending => {
                        self.pending = Some(Pending::OpenWrite { operation, revision, bytes })
                    }
                    Completion::Ready(mut file) => {
                        // Close waits for accepted writes and reports any failed
                        // write without publication; its receipt is our barrier.
                        drop(file.write(context, 0, bytes.as_bytes())?);
                        let operation = file.close(context, 0)?;
                        self.pending = Some(Pending::CloseWrite { operation, revision });
                    }
                }
            }
            Pending::CloseWrite { mut operation, revision } => {
                match operation.take_close(context)? {
                    Completion::Pending => {
                        self.pending = Some(Pending::CloseWrite { operation, revision })
                    }
                    Completion::Ready(receipt) => {
                        if let Some(error) = receipt.error {
                            return Err(error);
                        }
                        if receipt.publication != FILE_PUBLISHED {
                            return Err(Error::Unavailable);
                        }
                        self.begin_read(context, revision, true)?;
                        // Publication and readback are one visible Saving phase.
                    }
                }
            }
        }
        Ok(false)
    }
    fn event(
        &mut self,
        context: &mut Context<'_>,
        kind: EventKind,
        target: u32,
        data: &[u8],
    ) -> Result<(), Error> {
        match kind {
            EventKind::Started => {
                self.refresh = true;
                self.start_channels(context)?;
            }
            EventKind::DataChanged => self.refresh = true,
            EventKind::Click if target == 4 => self.send_channel(context, 0)?,
            EventKind::Click if target == 5 => self.send_channel(context, 1)?,
            EventKind::TextChanged if target == 1 => {
                self.edit(playbit_api::abi::text::TextInput::decode(data)?.text.as_bytes())?
            }
            EventKind::Submit if target == 1 => {
                self.edit(data)?;
                self.save(context)?;
            }
            EventKind::Click if target == 2 => self.save(context)?,
            EventKind::Click if target == 3 => self.refresh = true,
            _ => {}
        }
        Ok(())
    }
    fn report(&mut self, context: &mut Context<'_>, result: Result<(), Error>) {
        if let Err(error) = result {
            self.error = match error {
                Error::Unavailable => {
                    "File unavailable. Reconnect and choose Reload to read current data.".into()
                }
                Error::AccessDenied => {
                    "This account cannot access the shared note. Ask the owner for access.".into()
                }
                Error::LimitExceeded => "This note can contain up to 8,192 UTF-8 bytes.".into(),
                Error::Conflict => {
                    "A note operation is still finishing. Your draft is retained.".into()
                }
                error => format!("Could not open or save the note: {error:?}"),
            };
            context.log(&self.error);
        }
    }
    fn render(
        &mut self,
        context: &mut Context<'_>,
        view: &mut ViewEncoder<'_>,
    ) -> Result<(), Error> {
        view.column_start();
        view.text(TextStyle::Title, "Shared Note");
        view.text_field(
            1,
            &mut self.draft,
            "Write a note and press Return to save",
            false,
            self.pending.is_some(),
            false,
        );
        view.button(2, "Save", ButtonStyle::Default, self.pending.is_some() || self.root.is_none());
        if let Some(pending) = &self.pending {
            view.text(
                TextStyle::Muted,
                match pending {
                    Pending::OpenWrite { .. }
                    | Pending::CloseWrite { .. }
                    | Pending::OpenRead { saved: true, .. }
                    | Pending::Read { saved: true, .. } => "Saving note…",
                    _ => "Loading note…",
                },
            );
        }
        view.text(TextStyle::Body, &self.note);
        view.text(TextStyle::Muted, &format!("{} bytes · Shared file", self.note.len()));
        if self.channels.is_some() {
            view.text(
                TextStyle::Muted,
                &format!("{} connected app instances · {} waves", self.participants, self.waves),
            );
            view.button(4, "Send live preview", ButtonStyle::Default, !self.subscribed[0]);
            view.button(5, "Wave", ButtonStyle::Default, !self.subscribed[1]);
            if !self.preview.is_empty() {
                view.text(TextStyle::Body, &format!("Live preview: {}", self.preview));
            }
            view.text(TextStyle::Muted, &self.channel_status);
        }
        if !self.error.is_empty() {
            view.text(TextStyle::Body, &self.error);
            view.button(3, "Reload", ButtonStyle::Default, self.pending.is_some());
        }
        view.column_end();
        view.submit(context)
    }
}
#[allow(unsafe_code, reason = "The SDK resolves this app registration by its ABI name.")]
#[unsafe(no_mangle)]
pub fn playbit_app() -> playbit_api::App {
    playbit_api::app(App::default(), turn, true)
}

fn turn(app: &mut App, context: &mut Context<'_>, view: &mut ViewEncoder<'_>) {
    let mut changed = false;
    for event in context.raw_events() {
        let result = if event.kind == EventKind::ChannelMessage {
            event.channel_message().map(|message| {
                if let Some(channels) = app.channels {
                    if message.channel == channels[0] {
                        app.preview = String::from_utf8_lossy(message.data).into();
                    } else if message.channel == channels[1] {
                        app.waves += 1;
                    }
                }
                true
            })
        } else if event.kind == EventKind::PresenceChanged {
            let mut participants = [playbit_api::context::Participant::default(); 64];
            match context.presence(&mut participants) {
                Ok(members) => {
                    app.participants = members.len();
                    app.subscribe_channels(context).map(|()| true)
                }
                Err(error) => Err(poll_error(error)),
            }
        } else if event.kind == EventKind::ChannelStatus {
            event.channel_status().map(|error| {
                app.channel_status = format!("Live channel: {error:?}");
                true
            })
        } else if event.kind == EventKind::DataReady {
            event.operation().and_then(|handle| app.complete(context, handle))
        } else {
            app.event(context, event.kind, event.target, event.data)
                .map(|()| !matches!(event.kind, EventKind::Started | EventKind::DataChanged))
        };
        // File progress that leaves the visible state unchanged needs no new view.
        changed |= result.as_ref().copied().unwrap_or(true);
        app.report(context, result.map(|_| ()));
    }
    if app.refresh && app.pending.is_none() {
        changed = true;
        let result = app.begin_refresh(context);
        if result.is_err() {
            app.refresh = false;
        }
        app.report(context, result);
    }
    if changed && let Err(error) = app.render(context, view) {
        context.log(&format!("Rendering failed: {error:?}"));
    }
}
fn read_error(error: ReadError) -> Error {
    match error {
        ReadError::Host(error) => error,
        ReadError::BufferTooSmall { .. } => Error::LimitExceeded,
    }
}
fn poll_error(error: PollError) -> Error {
    match error {
        PollError::Host(error) => error,
        PollError::BufferTooSmall(_) => Error::LimitExceeded,
    }
}
