//! Operation identities, shared limits, and packed scene/event data codecs.
//!
//! App calls use the named C operations in sibling modules. Reader and Writer
//! remain for packed UI data and event payloads; they are not a syscall transport.
//! Their cursors stay within bounded borrowed slices, and malformed lengths
//! return Error. Operation arguments and results use typed C records instead.

pub const IMPORT_MODULE: &str = "playbit";
pub const MAIN_EXPORT: &str = "pb_main";
pub const MEMORY_EXPORT: &str = "memory";
pub const MAX_CALL_BYTES: usize = 64 * 1024 * 1024;
/// Logical data values and query output retain their independent 1 MiB budget.
pub const MAX_DATA_BYTES: usize = 1024 * 1024;
/// Timestamp-independent envelope charge, including the payload length.
pub const EVENT_QUEUE_OVERHEAD: usize = 24;
pub const MAX_EVENT_BYTES: usize =
    /* super::text::MAX_TEXT_BYTES + super::text::TEXT_EVENT_OVERHEAD */
    265220;
/// Bound synchronous platform entropy work to one WebCrypto-sized request.
pub const MAX_RANDOM_BYTES: usize = 64 * 1024;
pub const MAX_QUEUED_EVENTS: usize = 256;
pub const MAX_QUEUED_EVENT_BYTES: usize = 1024 * 1024;
pub const MAX_PAGE_OBJECTS: u32 = 64;
pub const MAX_FIELDS: u32 = 256;
pub const MAX_VALUES: u32 = 4096;
pub const MAX_QUERY_SCRATCH_BYTES: usize = 16 * 1024 * 1024;
/// All fixed output arrays and variable bytes fit one 16 MiB query budget.
/// Value records include a complete 15-byte database/blob identity.
pub const MAX_PAGE_VALUES: u32 = ((MAX_QUERY_SCRATCH_BYTES
    - MAX_PAGE_OBJECTS as usize * MAX_FIELDS as usize * core::mem::size_of::<super::data::Field>()
    - MAX_DATA_BYTES
    - MAX_PAGE_OBJECTS as usize * core::mem::size_of::<super::data::ObjectInfo>())
    / core::mem::size_of::<super::data::Value>()) as u32;
pub const MAX_VOLUMES: usize = 256;
pub const MAX_PENDING_OPERATIONS: usize = 64;
pub const MAX_STRING_BYTES: usize = 4096;
pub const MAX_STARTUP_ARGUMENTS: usize = 32;
pub const MAX_STARTUP_ARGUMENT_NAME_BYTES: usize = 128;
pub const MAX_STARTUP_ARGUMENT_JSON_BYTES: usize = 32 * 1024;

// Queue accounting includes generation, kind, target and payload length.
const _: () = assert!(EVENT_QUEUE_OVERHEAD + MAX_EVENT_BYTES <= MAX_CALL_BYTES);
const _: () = assert!(EVENT_QUEUE_OVERHEAD + MAX_EVENT_BYTES <= MAX_QUEUED_EVENT_BYTES);
const _: () = assert!(MAX_STRING_BYTES + 4 <= MAX_EVENT_BYTES);
const _: () = assert!(MAX_STARTUP_ARGUMENT_NAME_BYTES <= MAX_STRING_BYTES);
const _: () = assert!(MAX_STARTUP_ARGUMENT_JSON_BYTES <= MAX_CALL_BYTES);
const _: () = assert!(MAX_PENDING_OPERATIONS * 8 <= MAX_EVENT_BYTES);
const _: () = assert!(MAX_CALL_BYTES <= i32::MAX as usize);
const _: () = assert!(MAX_DATA_BYTES <= MAX_CALL_BYTES);
const _: () = assert!(MAX_RANDOM_BYTES <= MAX_CALL_BYTES);
const _: () = assert!(MAX_QUERY_SCRATCH_BYTES >= MAX_DATA_BYTES);

/// Startup argument names and capability names share one grammar: 1 to 128
/// bytes of lowercase ASCII letters, digits, '.', '_' or '-'.
pub fn valid_startup_argument_name(name: &str) -> bool {
    valid_name(name)
}

/// See [`valid_startup_argument_name`].
pub fn valid_capability_name(name: &str) -> bool {
    valid_name(name)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_STARTUP_ARGUMENT_NAME_BYTES
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

/// Name and tag bytes in the caller-owned volume text storage. Record arrays
/// have separate count limits. An admitted context must fit the text limit.
pub fn available_volumes_text_bytes<'a, T: IntoIterator<Item = &'a str>>(
    volumes: impl IntoIterator<Item = (&'a str, T)>,
) -> Option<usize> {
    volumes.into_iter().try_fold(0usize, |size, (name, tags)| {
        let size = size.checked_add(name.len())?;
        tags.into_iter()
            .try_fold(size, |size, tag| size.checked_add(tag.len()))
            .filter(|size| *size <= MAX_DATA_BYTES)
    })
}

/// Exact compact JSON object size used by protected launch requests. Count
/// JSON escaping in values, not only their unencoded UTF-8 bytes.
pub fn startup_arguments_json_bytes<'a>(
    arguments: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<usize> {
    fn string_bytes(value: &str) -> Option<usize> {
        value.bytes().try_fold(2usize, |size, byte| {
            size.checked_add(match byte {
                b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
                0..=31 => 6,
                _ => 1,
            })
        })
    }
    let mut size = 2usize;
    let mut separator = 0usize;
    for (name, value) in arguments {
        size = size
            .checked_add(separator)?
            .checked_add(string_bytes(name)?)?
            .checked_add(1)?
            .checked_add(string_bytes(value)?)?;
        if size > MAX_STARTUP_ARGUMENT_JSON_BYTES {
            return None;
        }
        separator = 1;
    }
    Some(size)
}

pub const MIN_WAKE_DELAY_MS: u32 = 1000;
pub const MAX_WAKE_DELAY_MS: u32 = 86_400_000;

pub type Id = [u8; 16];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u32)]
pub enum Opcode {
    Capabilities = 1,
    Log = 2,
    ViewSubmit = 3,
    EventPoll = 4,
    Identity = 5,
    StartupArguments = 6,
    AvailableVolumes = 7,
    UpdateDefer = 9,
    Presence = 10,
    WakeAfter = 11,
    HostPlatform = 12,
    HostLimit = 13,
    QuitOnLastWindow = 14,
    TraceRecord = 15,
    ClockRead = 16,
    VolumesObserve = 18,
    Session = 19,
    RandomFill = 20,
    EventPollDeadline = 21,
    ChannelOpen = 0x160,
    ChannelBegin = 0x161,
    ChannelTake = 0x162,
    SessionInfo = 0x163,
    SchemaReadBegin = 0x102,
    SchemaReadTake = 0x103,
    SchemaEstablishBegin = 0x104,
    SchemaEstablishTake = 0x105,
    EntryResolveBegin = 0x106,
    EntryResolveTake = 0x107,
    ObjectCreateBegin = 0x114,
    ObjectCreatePoll = 0x115,
    FileCopyBegin = 0x116,
    FileCopyPoll = 0x117,
    FileReadBegin = 0x124,
    FileReadTake = 0x125,
    FileWriteBegin = 0x126,
    FileWriteTake = 0x127,
    FileSelectBegin = 0x122,
    FileSelectPoll = 0x123,
    ObjectReadBegin = 0x130,
    ObjectReadTake = 0x131,
    ObjectEditBegin = 0x132,
    ObjectEditTake = 0x133,
    ObjectQueryBegin = 0x134,
    ObjectQueryTake = 0x135,
    ObjectFieldReadAuthenticatedBegin = 0x136,
    ObjectFieldReadAuthenticatedTake = 0x137,
    KvKeysBegin = 0x140,
    KvKeysTake = 0x141,
    KvReadBegin = 0x142,
    KvReadTake = 0x143,
    KvPutBegin = 0x144,
    KvPutTake = 0x145,
    KvDeleteBegin = 0x146,
    KvDeleteTake = 0x147,
    BloblogAppendBegin = 0x170,
    BloblogAppendTake = 0x171,
    BloblogReadBegin = 0x172,
    BloblogReadTake = 0x173,
    FileOpenBegin = 0x150,
    FileOpenTake = 0x151,
    FileStatusBegin = 0x152,
    FileStatusTake = 0x153,
    FileCloseBegin = 0x154,
    FileCloseTake = 0x155,
    FileListOpenBegin = 0x156,
    FileListOpenTake = 0x157,
    FileListNextBegin = 0x158,
    FileListNextTake = 0x159,
    FileListClose = 0x15a,
    OperationDispose = 0x15b,
    FileDispose = 0x15c,
    AudioOpenBegin = 0x1a0,
    AudioOpenTake = 0x1a1,
    AudioClose = 0x1a2,
    AudioSampleRate = 0x1a3,
    AudioVolume = 0x1a4,
    AudioSetVolume = 0x1a5,
    SoundOpenPcm = 0x1a6,
    SoundClose = 0x1a7,
    SoundPlay = 0x1a8,
    SoundTell = 0x1a9,
    SoundSeek = 0x1aa,
    SoundSetGain = 0x1ab,
    SoundSetBalance = 0x1ac,
    SoundStop = 0x1ad,
}

/// Stable ordering for operation identity and capability discovery.
pub const REGISTRY: &[(Opcode, &str)] = &[
    (Opcode::Capabilities, "capabilities"),
    (Opcode::Log, "log"),
    (Opcode::ViewSubmit, "ui.submit"),
    (Opcode::EventPoll, "event.poll"),
    (Opcode::Identity, "identity.read"),
    (Opcode::StartupArguments, "startup.read"),
    (Opcode::AvailableVolumes, "volume.list"),
    (Opcode::UpdateDefer, "update.defer"),
    (Opcode::Presence, "presence.read"),
    (Opcode::WakeAfter, "wake.after"),
    (Opcode::HostPlatform, "host.platform"),
    (Opcode::HostLimit, "host.limit"),
    (Opcode::QuitOnLastWindow, "lifecycle.quit-on-last-window"),
    (Opcode::TraceRecord, "trace.record"),
    (Opcode::ClockRead, "clock.read"),
    (Opcode::VolumesObserve, "volume.observe"),
    (Opcode::Session, "session.read"),
    (Opcode::RandomFill, "random.fill"),
    (Opcode::EventPollDeadline, "event.poll.deadline"),
    (Opcode::ChannelOpen, "session.channel"),
    (Opcode::ChannelBegin, "session.channel.begin"),
    (Opcode::ChannelTake, "session.channel.take"),
    (Opcode::SchemaReadBegin, "object.schema.read.begin"),
    (Opcode::SchemaReadTake, "object.schema.read.take"),
    (Opcode::SchemaEstablishBegin, "object.schema.establish.begin"),
    (Opcode::SchemaEstablishTake, "object.schema.establish.take"),
    (Opcode::EntryResolveBegin, "entry.resolve.begin"),
    (Opcode::EntryResolveTake, "entry.resolve.take"),
    (Opcode::ObjectCreateBegin, "object.create.begin"),
    (Opcode::ObjectCreatePoll, "object.create.poll"),
    (Opcode::FileCopyBegin, "file.copy.begin"),
    (Opcode::FileCopyPoll, "file.copy.poll"),
    (Opcode::FileReadBegin, "file.read.begin"),
    (Opcode::FileReadTake, "file.read.take"),
    (Opcode::FileWriteBegin, "file.write.begin"),
    (Opcode::FileWriteTake, "file.write.take"),
    (Opcode::FileSelectBegin, "file.select.begin"),
    (Opcode::FileSelectPoll, "file.select.poll"),
    (Opcode::ObjectReadBegin, "object.read.begin"),
    (Opcode::ObjectReadTake, "object.read.take"),
    (Opcode::ObjectEditBegin, "object.edit.begin"),
    (Opcode::ObjectEditTake, "object.edit.take"),
    (Opcode::ObjectQueryBegin, "object.query.begin"),
    (Opcode::ObjectQueryTake, "object.query.take"),
    (Opcode::ObjectFieldReadAuthenticatedBegin, "object.field.read-authenticated.begin"),
    (Opcode::ObjectFieldReadAuthenticatedTake, "object.field.read-authenticated.take"),
    (Opcode::KvKeysBegin, "kv.keys.begin"),
    (Opcode::KvKeysTake, "kv.keys.take"),
    (Opcode::KvReadBegin, "kv.read.begin"),
    (Opcode::KvReadTake, "kv.read.take"),
    (Opcode::KvPutBegin, "kv.put.begin"),
    (Opcode::KvPutTake, "kv.put.take"),
    (Opcode::KvDeleteBegin, "kv.delete.begin"),
    (Opcode::KvDeleteTake, "kv.delete.take"),
    (Opcode::BloblogAppendBegin, "bloblog.append.begin"),
    (Opcode::BloblogAppendTake, "bloblog.append.take"),
    (Opcode::BloblogReadBegin, "bloblog.read.begin"),
    (Opcode::BloblogReadTake, "bloblog.read.take"),
    (Opcode::FileOpenBegin, "file.open.begin"),
    (Opcode::FileOpenTake, "file.open.take"),
    (Opcode::FileStatusBegin, "file.status.begin"),
    (Opcode::FileStatusTake, "file.status.take"),
    (Opcode::FileCloseBegin, "file.close.begin"),
    (Opcode::FileCloseTake, "file.close.take"),
    (Opcode::FileListOpenBegin, "file_list.open.begin"),
    (Opcode::FileListOpenTake, "file_list.open.take"),
    (Opcode::FileListNextBegin, "file_list.next.begin"),
    (Opcode::FileListNextTake, "file_list.next.take"),
    (Opcode::FileListClose, "file_list.close"),
    (Opcode::OperationDispose, "operation.dispose"),
    (Opcode::FileDispose, "file.dispose"),
    (Opcode::SessionInfo, "session.info"),
    (Opcode::AudioOpenBegin, "audio.open.begin"),
    (Opcode::AudioOpenTake, "audio.open.take"),
    (Opcode::AudioClose, "audio.close"),
    (Opcode::AudioSampleRate, "audio.sample-rate"),
    (Opcode::AudioVolume, "audio.volume"),
    (Opcode::AudioSetVolume, "audio.set-volume"),
    (Opcode::SoundOpenPcm, "sound.open.pcm"),
    (Opcode::SoundClose, "sound.close"),
    (Opcode::SoundPlay, "sound.play"),
    (Opcode::SoundTell, "sound.tell"),
    (Opcode::SoundSeek, "sound.seek"),
    (Opcode::SoundSetGain, "sound.set-gain"),
    (Opcode::SoundSetBalance, "sound.set-balance"),
    (Opcode::SoundStop, "sound.stop"),
];

impl Opcode {
    #[inline]
    pub fn from_raw(value: u32) -> Option<Self> {
        if value < 0x100 {
            return Self::from_core_raw(value);
        }
        match value {
            0x160 => Some(Self::ChannelOpen),
            0x161 => Some(Self::ChannelBegin),
            0x162 => Some(Self::ChannelTake),
            0x163 => Some(Self::SessionInfo),
            0x102 => Some(Self::SchemaReadBegin),
            0x103 => Some(Self::SchemaReadTake),
            0x104 => Some(Self::SchemaEstablishBegin),
            0x105 => Some(Self::SchemaEstablishTake),
            0x106 => Some(Self::EntryResolveBegin),
            0x107 => Some(Self::EntryResolveTake),
            0x114 => Some(Self::ObjectCreateBegin),
            0x115 => Some(Self::ObjectCreatePoll),
            0x116 => Some(Self::FileCopyBegin),
            0x117 => Some(Self::FileCopyPoll),
            0x124 => Some(Self::FileReadBegin),
            0x125 => Some(Self::FileReadTake),
            0x126 => Some(Self::FileWriteBegin),
            0x127 => Some(Self::FileWriteTake),
            0x122 => Some(Self::FileSelectBegin),
            0x123 => Some(Self::FileSelectPoll),
            0x130 => Some(Self::ObjectReadBegin),
            0x131 => Some(Self::ObjectReadTake),
            0x132 => Some(Self::ObjectEditBegin),
            0x133 => Some(Self::ObjectEditTake),
            0x134 => Some(Self::ObjectQueryBegin),
            0x135 => Some(Self::ObjectQueryTake),
            0x136 => Some(Self::ObjectFieldReadAuthenticatedBegin),
            0x137 => Some(Self::ObjectFieldReadAuthenticatedTake),
            0x140 => Some(Self::KvKeysBegin),
            0x141 => Some(Self::KvKeysTake),
            0x142 => Some(Self::KvReadBegin),
            0x143 => Some(Self::KvReadTake),
            0x144 => Some(Self::KvPutBegin),
            0x145 => Some(Self::KvPutTake),
            0x146 => Some(Self::KvDeleteBegin),
            0x147 => Some(Self::KvDeleteTake),
            0x170 => Some(Self::BloblogAppendBegin),
            0x171 => Some(Self::BloblogAppendTake),
            0x172 => Some(Self::BloblogReadBegin),
            0x173 => Some(Self::BloblogReadTake),
            0x150 => Some(Self::FileOpenBegin),
            0x151 => Some(Self::FileOpenTake),
            0x152 => Some(Self::FileStatusBegin),
            0x153 => Some(Self::FileStatusTake),
            0x154 => Some(Self::FileCloseBegin),
            0x155 => Some(Self::FileCloseTake),
            0x156 => Some(Self::FileListOpenBegin),
            0x157 => Some(Self::FileListOpenTake),
            0x158 => Some(Self::FileListNextBegin),
            0x159 => Some(Self::FileListNextTake),
            0x15a => Some(Self::FileListClose),
            0x15b => Some(Self::OperationDispose),
            0x15c => Some(Self::FileDispose),
            _ => Self::from_audio_raw(value),
        }
    }

    fn from_audio_raw(value: u32) -> Option<Self> {
        match value {
            0x1a0 => Some(Self::AudioOpenBegin),
            0x1a1 => Some(Self::AudioOpenTake),
            0x1a2 => Some(Self::AudioClose),
            0x1a3 => Some(Self::AudioSampleRate),
            0x1a4 => Some(Self::AudioVolume),
            0x1a5 => Some(Self::AudioSetVolume),
            0x1a6 => Some(Self::SoundOpenPcm),
            0x1a7 => Some(Self::SoundClose),
            0x1a8 => Some(Self::SoundPlay),
            0x1a9 => Some(Self::SoundTell),
            0x1aa => Some(Self::SoundSeek),
            0x1ab => Some(Self::SoundSetGain),
            0x1ac => Some(Self::SoundSetBalance),
            0x1ad => Some(Self::SoundStop),
            _ => None,
        }
    }

    fn from_core_raw(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Capabilities),
            2 => Some(Self::Log),
            3 => Some(Self::ViewSubmit),
            4 => Some(Self::EventPoll),
            5 => Some(Self::Identity),
            6 => Some(Self::StartupArguments),
            7 => Some(Self::AvailableVolumes),
            9 => Some(Self::UpdateDefer),
            10 => Some(Self::Presence),
            11 => Some(Self::WakeAfter),
            12 => Some(Self::HostPlatform),
            13 => Some(Self::HostLimit),
            14 => Some(Self::QuitOnLastWindow),
            15 => Some(Self::TraceRecord),
            16 => Some(Self::ClockRead),
            18 => Some(Self::VolumesObserve),
            19 => Some(Self::Session),
            20 => Some(Self::RandomFill),
            21 => Some(Self::EventPollDeadline),
            _ => None,
        }
    }

    pub fn capability(self) -> &'static str {
        REGISTRY
            .iter()
            .find(|(opcode, _)| *opcode == self)
            .expect("every Opcode has a registry entry")
            .1
    }

    pub const fn is_core(self) -> bool {
        (self as u32) < 0x100 || matches!(self, Self::FileDispose | Self::SessionInfo)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum Error {
    NotFound = -1,
    AccessDenied = -2,
    InvalidArgument = -3,
    Internal = -4,
    Unavailable = -5,
    WouldBlock = -6,
    Unsupported = -7,
    StaleHandle = -8,
    LimitExceeded = -9,
    Conflict = -10,
    BufferTooSmall = -11,
    Busy = -12,
    Closed = -13,
    DestinationDeleted = -14,
    Uncertain = -15,
}

impl TryFrom<i32> for Error {
    type Error = i32;

    fn try_from(code: i32) -> Result<Self, i32> {
        Ok(match code {
            -1 => Self::NotFound,
            -2 => Self::AccessDenied,
            -3 => Self::InvalidArgument,
            -4 => Self::Internal,
            -5 => Self::Unavailable,
            -6 => Self::WouldBlock,
            -7 => Self::Unsupported,
            -8 => Self::StaleHandle,
            -9 => Self::LimitExceeded,
            -10 => Self::Conflict,
            -11 => Self::BufferTooSmall,
            -12 => Self::Busy,
            -13 => Self::Closed,
            -14 => Self::DestinationDeleted,
            -15 => Self::Uncertain,
            _ => return Err(code),
        })
    }
}

impl TryFrom<i64> for Error {
    type Error = i64;

    fn try_from(code: i64) -> Result<Self, i64> {
        let narrow = i32::try_from(code).map_err(|_| code)?;
        Self::try_from(narrow).map_err(|_| code)
    }
}

/// Future event IDs with this bit set are optional advisory notifications only.
/// Existing IDs must never carry it; lifecycle, authority and completion are mandatory.
pub const OPTIONAL_EVENT_KIND_BIT: u32 = 1 << 31;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum EventKind {
    Started = 1,
    Click = 2,
    Submit = 3,
    DataChanged = 4,
    PresenceChanged = 5,
    UpdateAvailable = 6,
    ObjectCreationReady = 7,
    FileCopyReady = 8,
    FileSelectionReady = 9,
    Wake = 10,
    /// Original named-canvas pointer input.
    Pointer = 11,
    /// Original named-canvas keyboard input.
    Key = 12,
    Frame = 13,
    /// Native byte edits and the full committed UTF-8 value; marked text stays native.
    TextChanged = 14,
    /// Current viewport/density/input facilities; target zero and a 16-byte Surface payload.
    SurfaceChanged = 15,
    DataReady = 16,
    /// One-shot root-set change; target zero, little-endian u64 revision payload.
    VolumesChanged = 17,
    ChannelMessage = 18,
    ChannelStatus = 19,
    /// Capture generation snapshot at callback entry; not a wake source.
    TraceConfig = 20,
    WindowPointer = 22,
    WindowKeyboard = 23,
    WindowScroll = 24,
    WindowGesture = 25,
    WindowGamepad = 26,
    /// Advisory, target zero, empty payload. Reread the identity display name.
    IdentityChanged = 0x8000_0015,
}

/// Canvas pointer coordinates are local integer coordinates, including edges.
/// Phase: move=0, down=1, up=2, cancel=3. Buttons: primary pressed=1, none=0.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerInput {
    pub x: u32,
    pub y: u32,
    pub phase: u32,
    pub buttons: u32,
}

impl PointerInput {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes)?;
        let input = Self {
            x: reader.u32()?,
            y: reader.u32()?,
            phase: reader.u32()?,
            buttons: reader.u32()?,
        };
        reader.finish()?;
        if input.phase > 3 || input.buttons > 1 {
            return Err(Error::InvalidArgument);
        }
        Ok(input)
    }

    pub fn encode(self) -> [u8; 16] {
        let mut bytes = [0; 16];
        for (part, value) in
            bytes.as_chunks_mut::<4>().0.iter_mut().zip([self.x, self.y, self.phase, self.buttons])
        {
            *part = value.to_le_bytes();
        }
        bytes
    }
}

/// Physical controls: left=1, right=2, up=3, down=4, space=5.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyInput {
    pub key: u32,
    pub pressed: bool,
}

impl KeyInput {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes)?;
        let key = reader.u32()?;
        let pressed = reader.u32()?;
        reader.finish()?;
        if !(1..=5).contains(&key) || pressed > 1 {
            return Err(Error::InvalidArgument);
        }
        Ok(Self { key, pressed: pressed == 1 })
    }

    pub fn encode(self) -> [u8; 8] {
        let mut bytes = [0; 8];
        bytes[..4].copy_from_slice(&self.key.to_le_bytes());
        bytes[4..].copy_from_slice(&u32::from(self.pressed).to_le_bytes());
        bytes
    }
}

/// Elapsed display time; hosts clamp suspension gaps to 100 milliseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameInput {
    pub elapsed_us: u32,
}

impl FrameInput {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let bytes: [u8; 4] = bytes.try_into().map_err(|_| Error::InvalidArgument)?;
        let elapsed_us = u32::from_le_bytes(bytes);
        if !(1..=100_000).contains(&elapsed_us) {
            return Err(Error::InvalidArgument);
        }
        Ok(Self { elapsed_us })
    }

    pub fn encode(self) -> [u8; 4] {
        self.elapsed_us.to_le_bytes()
    }
}

/// A volume capability in one guest generation. Never persist or transfer it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct VolumeHandle {
    pub generation: u32,
    pub slot: u32,
}

/// One pending operation in its owning guest thread. Slot generations reject
/// stale references after reuse. Never persist or transfer this handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct OperationHandle {
    pub generation: u32,
    pub slot: u32,
}

pub struct Reader<'a> {
    bytes: &'a [u8],
    length: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_CALL_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(Self { bytes, length: bytes.len() })
    }
    pub fn position(&self) -> usize {
        assert!(self.bytes.len() <= self.length, "reader cursor must stay inside its request");
        self.length - self.bytes.len()
    }
    pub fn remaining(&self) -> &'a [u8] {
        assert!(self.bytes.len() <= self.length, "reader cursor must stay inside its request");
        self.bytes
    }
    pub fn is_empty(&self) -> bool {
        self.position() == self.length
    }
    pub fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let before = self.position();
        if length > self.bytes.len() {
            return Err(Error::InvalidArgument);
        }
        let (value, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        assert_eq!(self.position() - before, length);
        Ok(value)
    }
    pub fn u8(&mut self) -> Result<u8, Error> {
        let before = self.position();
        let value = self.take(1)?[0];
        assert_eq!(self.position() - before, 1);
        Ok(value)
    }
    pub fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("take returned two bytes")))
    }
    pub fn u32(&mut self) -> Result<u32, Error> {
        let before = self.position();
        let value = u32::from_le_bytes(self.take(4)?.try_into().expect("take returned four bytes"));
        assert_eq!(self.position() - before, 4);
        Ok(value)
    }
    pub fn u64(&mut self) -> Result<u64, Error> {
        let before = self.position();
        let value =
            u64::from_le_bytes(self.take(8)?.try_into().expect("take returned eight bytes"));
        assert_eq!(self.position() - before, 8);
        Ok(value)
    }
    pub fn id(&mut self) -> Result<Id, Error> {
        let before = self.position();
        let value = self.take(16)?.try_into().expect("take returned sixteen bytes");
        assert_eq!(self.position() - before, 16);
        Ok(value)
    }
    pub fn bool(&mut self) -> Result<bool, Error> {
        let before = self.position();
        let value = match self.u8()? {
            0 => false,
            1 => true,
            _ => return Err(Error::InvalidArgument),
        };
        assert_eq!(self.position() - before, 1);
        Ok(value)
    }
    pub fn handle(&mut self) -> Result<VolumeHandle, Error> {
        let before = self.position();
        let value = VolumeHandle { slot: self.u32()?, generation: self.u32()? };
        assert_eq!(self.position() - before, 8);
        Ok(value)
    }
    pub fn operation_handle(&mut self) -> Result<OperationHandle, Error> {
        let before = self.position();
        let value = OperationHandle { slot: self.u32()?, generation: self.u32()? };
        assert_eq!(self.position() - before, 8);
        Ok(value)
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], Error> {
        let before = self.position();
        let length = self.u32()? as usize;
        let value = self.take(length)?;
        assert_eq!(self.position() - before, 4 + length);
        Ok(value)
    }
    pub fn string(&mut self) -> Result<&'a str, Error> {
        let before = self.position();
        let bytes = self.bytes()?;
        if bytes.len() > MAX_STRING_BYTES {
            return Err(Error::LimitExceeded);
        }
        let value = core::str::from_utf8(bytes).map_err(|_| Error::InvalidArgument)?;
        assert_eq!(self.position() - before, 4 + value.len());
        Ok(value)
    }
    pub fn count(&mut self, maximum: u32) -> Result<u32, Error> {
        let before = self.position();
        let value = self.u32()?;
        if value > maximum {
            return Err(Error::LimitExceeded);
        }
        assert_eq!(self.position() - before, 4);
        Ok(value)
    }
    pub fn finish(self) -> Result<(), Error> {
        if !self.bytes.is_empty() {
            return Err(Error::InvalidArgument);
        }
        assert_eq!(self.position(), self.length);
        Ok(())
    }
}

pub struct Writer<'a> {
    bytes: &'a mut [u8],
    length: usize,
}

impl<'a> Writer<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, length: 0 }
    }
    pub fn position(&self) -> usize {
        assert!(self.length <= self.capacity(), "writer cursor must stay inside its response");
        self.length
    }
    pub fn capacity(&self) -> usize {
        self.bytes.len().min(MAX_CALL_BYTES)
    }
    pub fn put(&mut self, value: &[u8]) -> Result<(), Error> {
        let before = self.position();
        let end = self.length.checked_add(value.len()).ok_or(Error::LimitExceeded)?;
        if end > MAX_CALL_BYTES || end > self.bytes.len() {
            return Err(Error::LimitExceeded);
        }
        self.bytes[self.length..end].copy_from_slice(value);
        self.length = end;
        assert_eq!(self.length - before, value.len());
        Ok(())
    }
    pub fn u8(&mut self, value: u8) -> Result<(), Error> {
        let before = self.position();
        self.put(&[value])?;
        assert_eq!(self.length - before, 1);
        Ok(())
    }
    pub fn u32(&mut self, value: u32) -> Result<(), Error> {
        let before = self.position();
        self.put(&value.to_le_bytes())?;
        assert_eq!(self.length - before, 4);
        Ok(())
    }
    pub fn u64(&mut self, value: u64) -> Result<(), Error> {
        let before = self.position();
        self.put(&value.to_le_bytes())?;
        assert_eq!(self.length - before, 8);
        Ok(())
    }
    pub fn id(&mut self, value: Id) -> Result<(), Error> {
        let before = self.position();
        self.put(&value)?;
        assert_eq!(self.length - before, 16);
        Ok(())
    }
    pub fn bool(&mut self, value: bool) -> Result<(), Error> {
        let before = self.position();
        self.u8(u8::from(value))?;
        assert_eq!(self.length - before, 1);
        Ok(())
    }
    pub fn handle(&mut self, value: VolumeHandle) -> Result<(), Error> {
        let before = self.position();
        self.u32(value.slot)?;
        self.u32(value.generation)?;
        assert_eq!(self.length - before, 8);
        Ok(())
    }
    pub fn operation_handle(&mut self, value: OperationHandle) -> Result<(), Error> {
        let before = self.position();
        self.u32(value.slot)?;
        self.u32(value.generation)?;
        assert_eq!(self.length - before, 8);
        Ok(())
    }
    pub fn bytes(&mut self, value: &[u8]) -> Result<(), Error> {
        let before = self.position();
        self.u32(u32::try_from(value.len()).map_err(|_| Error::LimitExceeded)?)?;
        self.put(value)?;
        assert_eq!(self.length - before, 4 + value.len());
        Ok(())
    }
    pub fn string(&mut self, value: &str) -> Result<(), Error> {
        let before = self.position();
        if value.len() > MAX_STRING_BYTES {
            return Err(Error::LimitExceeded);
        }
        self.bytes(value.as_bytes())?;
        assert_eq!(self.length - before, 4 + value.len());
        Ok(())
    }
    pub fn finish(self) -> &'a [u8] {
        assert!(self.length <= self.capacity(), "finished response must fit its buffer");
        &self.bytes[..self.length]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    extern crate std;
    use self::std::vec;
    #[test]
    fn host_error_codes_preserve_known_errors_and_reject_unknown_values() {
        for code in -15..=-1 {
            assert_eq!(super::Error::try_from(code).unwrap() as i32, code);
        }
        for code in [i32::MIN, -16, 0, 1, i32::MAX] {
            assert_eq!(super::Error::try_from(code), Err(code));
        }
    }
    use super::*;

    #[test]
    fn available_volume_text_sizes_count_names_and_every_tag() {
        let none: [&str; 0] = [];
        assert_eq!(available_volumes_text_bytes([("", none)]), Some(0));
        assert_eq!(available_volumes_text_bytes([("ab", none)]), Some(2));
        assert_eq!(available_volumes_text_bytes([("", ["xy"])]), Some(2));
        let chunk = "v".repeat(256);
        let exact = core::iter::repeat_n((chunk.as_str(), none), MAX_DATA_BYTES / chunk.len());
        assert_eq!(available_volumes_text_bytes(exact.clone()), Some(MAX_DATA_BYTES));
        assert_eq!(available_volumes_text_bytes(exact.chain([("x", none)])), None);
        // A volume with the largest name and tags a context may carry, at the
        // largest count it may carry, does not fit in one response.
        let name_bytes = [b'v'; 256];
        let tag_bytes = [b't'; 128];
        let name = core::str::from_utf8(&name_bytes).unwrap();
        let tags = [core::str::from_utf8(&tag_bytes).unwrap(); 64];
        assert_eq!(available_volumes_text_bytes([(name, tags)]), Some(8448));
        assert_eq!(
            available_volumes_text_bytes(core::iter::repeat_n((name, tags), MAX_VOLUMES)),
            None
        );
    }

    #[test]
    fn capability_and_startup_argument_names_share_one_grammar() {
        for (name, valid) in [
            ("mode-1.sync", true),
            ("a_b", true),
            ("", false),
            ("Mode", false),
            ("mode name", false),
            ("naïve", false),
            (&"x".repeat(MAX_STARTUP_ARGUMENT_NAME_BYTES), true),
            (&"x".repeat(MAX_STARTUP_ARGUMENT_NAME_BYTES + 1), false),
        ] {
            assert_eq!(valid_capability_name(name), valid, "capability {name:?}");
            assert_eq!(valid_startup_argument_name(name), valid, "startup argument {name:?}");
        }
    }

    #[test]
    fn startup_argument_sizes_count_json_escaping() {
        assert!(valid_startup_argument_name("mode-1.sync"));
        assert!(!valid_startup_argument_name("Mode"));
        assert!(!valid_startup_argument_name("mode name"));
        assert_eq!(startup_arguments_json_bytes([]), Some(2));
        assert_eq!(startup_arguments_json_bytes([("k", "\n")]), Some(10));
        assert_eq!(startup_arguments_json_bytes([("k", "é")]), Some(10));
        assert!(startup_arguments_json_bytes(core::iter::repeat_n(("k", "\0"), 3000)).is_none());
    }

    #[test]
    fn operation_registry_has_unique_identities_and_capabilities() {
        let expected = [
            (1, "capabilities"),
            (2, "log"),
            (3, "ui.submit"),
            (4, "event.poll"),
            (5, "identity.read"),
            (6, "startup.read"),
            (7, "volume.list"),
            (9, "update.defer"),
            (10, "presence.read"),
            (11, "wake.after"),
            (21, "event.poll.deadline"),
            (0x163, "session.info"),
            (0x102, "object.schema.read.begin"),
            (0x103, "object.schema.read.take"),
            (0x104, "object.schema.establish.begin"),
            (0x105, "object.schema.establish.take"),
            (0x114, "object.create.begin"),
            (0x115, "object.create.poll"),
            (0x116, "file.copy.begin"),
            (0x117, "file.copy.poll"),
            (0x136, "object.field.read-authenticated.begin"),
            (0x137, "object.field.read-authenticated.take"),
            (0x124, "file.read.begin"),
            (0x125, "file.read.take"),
            (0x126, "file.write.begin"),
            (0x127, "file.write.take"),
            (0x122, "file.select.begin"),
            (0x123, "file.select.poll"),
        ];
        for (number, capability) in expected {
            assert_eq!(Opcode::from_raw(number).unwrap().capability(), capability);
        }
        for (index, (opcode, name)) in REGISTRY.iter().enumerate() {
            assert!(
                !REGISTRY[..index]
                    .iter()
                    .any(|(previous, label)| previous == opcode || label == name)
            );
        }
        assert_eq!(Opcode::from_raw(u32::MAX), None);
    }

    #[test]
    fn wire_fixture_and_bounded_reading() {
        let mut buffer = [0u8; 64];
        let mut writer = Writer::new(&mut buffer);
        writer.handle(VolumeHandle { generation: 0x1234, slot: 2 }).unwrap();
        writer.string("root").unwrap();
        let bytes = writer.finish();
        assert_eq!(bytes, &[2, 0, 0, 0, 0x34, 0x12, 0, 0, 4, 0, 0, 0, b'r', b'o', b'o', b't']);
        let mut reader = Reader::new(bytes).unwrap();
        assert_eq!(reader.handle().unwrap(), VolumeHandle { generation: 0x1234, slot: 2 });
        assert_eq!(reader.string().unwrap(), "root");
        reader.finish().unwrap();
        assert_eq!(Reader::new(&[0xff; 4]).unwrap().bytes(), Err(Error::InvalidArgument));
        assert_eq!(Reader::new(&[2]).unwrap().bool(), Err(Error::InvalidArgument));
        assert_eq!(Reader::new(&[0]).unwrap().finish(), Err(Error::InvalidArgument));
    }

    #[test]
    fn operation_handle_is_exactly_eight_little_endian_bytes() {
        let handle = OperationHandle { generation: 0x04030201, slot: 0x08070605 };
        let mut bytes = [0; 8];
        let mut writer = Writer::new(&mut bytes);
        writer.operation_handle(handle).unwrap();
        let bytes = writer.finish();
        assert_eq!(bytes, &[5, 6, 7, 8, 1, 2, 3, 4]);
        let mut reader = Reader::new(bytes).unwrap();
        assert_eq!(reader.operation_handle().unwrap(), handle);
        reader.finish().unwrap();
        assert_eq!(
            Reader::new(&bytes[..7]).unwrap().operation_handle(),
            Err(Error::InvalidArgument)
        );
        assert_eq!(EventKind::ObjectCreationReady as u32, 7);
        assert_eq!(EventKind::FileCopyReady as u32, 8);
        assert_eq!(EventKind::FileSelectionReady as u32, 9);
        assert_eq!(Opcode::FileCopyBegin as u32, 0x116);
        assert_eq!(Opcode::FileCopyPoll as u32, 0x117);
        assert_eq!(Opcode::FileSelectBegin as u32, 0x122);
        assert_eq!(Opcode::FileSelectPoll as u32, 0x123);
        assert_eq!(MAX_PENDING_OPERATIONS, 64);
    }
    #[test]
    fn packed_fields_work_at_unaligned_offsets_and_report_exact_cursors() {
        let mut buffer = [0u8; 64];
        let mut writer = Writer::new(&mut buffer);
        writer.u8(7).unwrap();
        writer.u32(0x12345678).unwrap();
        writer.u64(0x1122334455667788).unwrap();
        writer.id([9; 16]).unwrap();
        writer.string("abc").unwrap();
        assert_eq!(writer.position(), 1 + 4 + 8 + 16 + 4 + 3);
        let encoded = writer.finish();
        let mut reader = Reader::new(encoded).unwrap();
        assert_eq!(reader.u8().unwrap(), 7);
        assert_eq!(reader.position(), 1);
        assert_eq!(reader.u32().unwrap(), 0x12345678);
        assert_eq!(reader.u64().unwrap(), 0x1122334455667788);
        assert_eq!(reader.id().unwrap(), [9; 16]);
        assert_eq!(reader.string().unwrap(), "abc");
        assert_eq!(reader.position(), encoded.len());
        reader.finish().unwrap();
    }

    #[test]
    fn failed_atomic_fields_preserve_cursor_and_composites_keep_consumed_prefix() {
        for length in 0..8 {
            let input = vec![0; length];
            let mut reader = Reader::new(&input).unwrap();
            assert_eq!(reader.u64(), Err(Error::InvalidArgument));
            assert_eq!(reader.position(), 0);
            let mut output = vec![0xcc; length];
            let mut writer = Writer::new(&mut output);
            assert_eq!(writer.u64(1), Err(Error::LimitExceeded));
            assert_eq!(writer.position(), 0);
            assert!(output.iter().all(|byte| *byte == 0xcc));
        }
        let mut reader = Reader::new(&[3, 0, 0, 0, 1]).unwrap();
        assert_eq!(reader.bytes(), Err(Error::InvalidArgument));
        assert_eq!(reader.position(), 4);
        assert_eq!(reader.remaining(), &[1]);
        let mut output = [0xcc; 5];
        let mut writer = Writer::new(&mut output);
        assert_eq!(writer.bytes(&[1, 2, 3]), Err(Error::LimitExceeded));
        assert_eq!(writer.position(), 4);
        assert_eq!(writer.finish(), &[3, 0, 0, 0]);
        assert_eq!(output[4], 0xcc);
    }

    #[test]
    fn exact_maximum_and_oversized_codec_buffers_are_fallible() {
        let input = vec![0; MAX_CALL_BYTES + 1];
        assert!(matches!(Reader::new(&input), Err(Error::LimitExceeded)));
        let mut reader = Reader::new(&input[..MAX_CALL_BYTES]).unwrap();
        reader.take(MAX_CALL_BYTES).unwrap();
        assert_eq!(reader.take(1), Err(Error::InvalidArgument));
        reader.finish().unwrap();
        let mut output = vec![0; MAX_CALL_BYTES + 1];
        let mut writer = Writer::new(&mut output);
        writer.put(&input[..MAX_CALL_BYTES]).unwrap();
        assert_eq!(writer.u8(1), Err(Error::LimitExceeded));
        assert_eq!(writer.position(), MAX_CALL_BYTES);
    }
}
