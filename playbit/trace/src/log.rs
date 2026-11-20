//! One profile's growing trace groups. The sole collector owns disk mutation;
//! readers copy one bounded block and never pin historical files across calls.
mod storage;
use crate::{
    format::{self, DecodeError, Envelope, MAX_BLOCK_BYTES, SEGMENT_HEADER_BYTES, Segment},
    notify, region,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use storage::{Active, Group, Instance, STREAMS, State};
const DESKTOP_BYTES: u64 = 16 * 1024 * 1024;
const DESKTOP_BURST: u64 = 8 * 1024 * 1024;
const DESKTOP_RATE: u64 = 1024 * 1024;
const IOS_BYTES: u64 = 4 * 1024 * 1024;
const IOS_BURST: u64 = 256 * 1024;
const IOS_RATE: u64 = 16 * 1024;
const _: () = assert!(2 * DESKTOP_BURST * 1_000_000_000 < (1u64 << 54));
const _: () = assert!(IOS_BURST <= DESKTOP_BURST && IOS_RATE > 0 && DESKTOP_RATE > 0);
const _: () = assert!(IOS_RATE < IOS_BURST && DESKTOP_RATE < DESKTOP_BURST);
const WRITE_ATTEMPTS: usize = 128;
pub const TURN_BYTES: u64 = 2 * 1024 * 1024;
const _: () = assert!(TURN_BYTES + STREAMS as u64 * (SEGMENT_HEADER_BYTES as u64) < IOS_BYTES);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub epoch: [u64; 2],
    pub sequence: u64,
}
pub struct Log {
    storage: Arc<Storage>,
    pub epoch: [u64; 2],
    pub previous: Option<Segment>,
    _ownership: Arc<Ownership>,
    credit: Credit,
    stopped: bool,
    notification: std::os::unix::net::UnixStream,
    receiver: Option<std::os::unix::net::UnixStream>,
}
pub(crate) struct Ownership(pub File);
pub(crate) fn private_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trace files require a private regular file with one link",
        ));
    }
    Ok(())
}
impl Drop for Ownership {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            crate::diagnostic!("Playbit trace ownership unlock failed: {error}");
        }
    }
}
#[derive(Clone)]
pub struct Reader {
    storage: Arc<Storage>,
}
struct Storage {
    copy: Mutex<State>,
    directory: PathBuf,
    group_bytes: u64,
    closed: AtomicU64,
}
#[derive(Clone, Copy)]
pub struct Position {
    pub cursor: Option<Cursor>,
    ordinal: u64,
    offsets: [u64; STREAMS],
    revision: u64,
    selector: Instance,
    reported_closed: bool,
}
#[derive(Clone, Copy)]
pub(crate) struct End {
    ordinal: u64,
    offsets: [u64; STREAMS],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadResult {
    Block(Envelope),
    Metadata(Envelope),
    Skipped,
    Pending,
    Expired,
    StreamGap,
    Epoch([u64; 2]),
    Incomplete,
    Stopped,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Append {
    Written,
    Rotate,
    Bandwidth,
    Deferred,
    Stopped,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Preparation {
    Ready,
    Rotated,
    Deferred,
}
pub struct Credit {
    available: u64,
    burst: u64,
    small: bool,
    last_ns: u64,
}
impl Credit {
    pub fn new(small: bool, now: u64) -> Self {
        let burst = if small { IOS_BURST } else { DESKTOP_BURST };
        Self { available: burst * 1_000_000_000, burst, small, last_ns: now }
    }
    pub fn take(&mut self, bytes: u64, now: u64, metadata: bool) -> bool {
        let elapsed = now.saturating_sub(self.last_ns);
        // Credit uses bytes × 1e9. Nanoseconds × bytes/second add credit
        // exactly, preserving fractional bytes without division or rounding.
        // Derive the refill window from the actual capacity and rate. Desktop
        // refills in 8 seconds and iOS in 16; products/sums fit below 2^54.
        let full = self.burst * 1_000_000_000;
        let rate = if self.small { IOS_RATE } else { DESKTOP_RATE };
        let refill = elapsed.min(full.div_ceil(rate)) * rate;
        self.available = (self.available + refill).min(full);
        self.last_ns = self.last_ns.max(now);
        // Events leave one second's allowance for definitions, loss and controls.
        // This reserve is inside the same burst/rate, never extra write credit.
        let reserve = if metadata { 0 } else { rate * 1_000_000_000 };
        if bytes > self.burst || self.available < reserve {
            return false;
        }
        let charge = bytes * 1_000_000_000;
        if charge > self.available - reserve {
            return false;
        }
        self.available -= charge;
        true
    }
}
impl Log {
    #[cfg(test)]
    pub fn open(directory: &Path, small: bool, now: u64) -> io::Result<Self> {
        let mut log = Self::create(directory, small, now)?;
        log.begin_epoch(now)?;
        Ok(log)
    }
    pub fn create(directory: &Path, small: bool, now: u64) -> io::Result<Self> {
        Self::load(directory, small, now, true)
    }
    pub fn retained(directory: &Path, small: bool, now: u64) -> io::Result<Option<Self>> {
        match fs::symlink_metadata(directory) {
            Ok(_) => Self::load(directory, small, now, false).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn load(directory: &Path, small: bool, now: u64, create: bool) -> io::Result<Self> {
        if create {
            crate::profile_paths::private_directory(directory)?;
        }
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "trace directory must be private (0700)",
            ));
        }
        let ownership = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("owner.lock"))?;
        private_file(&ownership)?;
        ownership.try_lock().map_err(io::Error::other)?;
        let ownership = Arc::new(Ownership(ownership));
        if create {
            crate::profile_paths::private_directory(&storage::directory(directory, None))?;
        }
        let state = storage::inspect(directory)?;
        let previous = if state.newest == 0 {
            None
        } else {
            Some(Group::read(directory, state.newest)?.header)
        };
        let storage = Arc::new(Storage {
            copy: Mutex::new(state),
            directory: directory.to_owned(),
            group_bytes: if small { IOS_BYTES } else { DESKTOP_BYTES },
            closed: AtomicU64::new(1),
        });
        let (notification, receiver) = notify::pair()?;
        Ok(Self {
            storage,
            epoch: previous.map_or([0, 0], |header| header.epoch),
            previous,
            _ownership: ownership,
            credit: Credit::new(small, now),
            stopped: true,
            notification,
            receiver: Some(receiver),
        })
    }
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }
    pub fn begin_epoch(&mut self, now: u64) -> io::Result<()> {
        assert!(self.stopped, "only a closed capture starts a new epoch");
        let epoch = region::incarnation()?;
        self.new_group(epoch, now)?;
        self.epoch = epoch;
        self.stopped = false;
        self.storage.closed.store(0, Ordering::Release);
        let _ = notify::send(self.notification.as_raw_fd());
        Ok(())
    }
    fn new_group(&mut self, epoch: [u64; 2], now: u64) -> io::Result<()> {
        if !self.credit.take((SEGMENT_HEADER_BYTES + storage::INDEX_BYTES) as u64, now, true) {
            return Err(io::Error::other("trace group write credit unavailable"));
        }
        let mut state =
            self.storage.copy.lock().map_err(|_| io::Error::other("trace copy lock poisoned"))?;
        // Explicit retry also recovers abandoned unpublished index/header
        // staging from an earlier failed capture. Normal rotation needs no scan.
        if self.stopped {
            state.active = None;
            let recovered = storage::inspect(&self.storage.directory)?;
            state.total = recovered.total;
            state.oldest = recovered.oldest;
            state.newest = state.newest.max(recovered.newest);
        }
        // Reserve before creating headers. Old active files may be evicted only
        // after leaving the active set, and readers retain no external handles.
        self.previous = if state.oldest == 0 {
            None
        } else {
            Some(Group::read(&self.storage.directory, state.newest)?.header)
        };
        state.active = None;
        self.storage.reserve(&mut state, TURN_BYTES)?;
        let ordinal = state
            .newest
            .checked_add(1)
            .ok_or_else(|| io::Error::other("trace ordinal exhausted"))?;
        let group = Group { header: Segment { epoch, ordinal }, streams: vec![None] };
        let before = storage::directory_charge(&self.storage.directory, &[])?;
        let file = storage::create_header(&self.storage.directory, None, group.header)?;
        group.write(&self.storage.directory)?;
        let after = group
            .charge(&self.storage.directory)?
            .checked_add(storage::directory_charge(&self.storage.directory, &[])?)
            .ok_or_else(|| io::Error::other("trace disk accounting overflow"))?;
        state.replace(before, after)?;
        state.newest = ordinal;
        if state.oldest == 0 {
            state.oldest = ordinal;
        }
        state.active = Some(Active {
            group,
            files: vec![file],
            lengths: vec![SEGMENT_HEADER_BYTES as u64],
            bytes: SEGMENT_HEADER_BYTES as u64 + storage::INDEX_BYTES as u64,
        });
        Ok(())
    }
    pub fn close(&mut self) {
        self.stopped = true;
        self.storage.closed.store(1, Ordering::Release);
        let _ = notify::send(self.notification.as_raw_fd());
    }
    pub fn reader(&self) -> Reader {
        Reader { storage: self.storage.clone() }
    }
    pub fn take_notification(&mut self) -> io::Result<std::os::unix::net::UnixStream> {
        if let Some(receiver) = self.receiver.take() {
            return Ok(receiver);
        }
        let (notification, receiver) = notify::pair()?;
        self.notification = notification;
        Ok(receiver)
    }
    pub fn prepare_turn(&mut self, now: u64) -> io::Result<Preparation> {
        let rotate = {
            let mut state = self
                .storage
                .copy
                .lock()
                .map_err(|_| io::Error::other("trace copy lock poisoned"))?;
            let (free, block) = storage::free_space(&self.storage.directory)?;
            #[cfg(test)]
            let free = state.free.unwrap_or(free);
            let reserve = Storage::reservation(TURN_BYTES, block)?;
            let pool = free
                .checked_add(state.total)
                .ok_or_else(|| io::Error::other("trace quota overflow"))?
                / 6;
            let rotate = state
                .active
                .as_ref()
                .is_some_and(|active| self.storage.group_bytes - active.bytes < TURN_BYTES)
                || state.total.saturating_add(reserve) > pool;
            if !rotate {
                self.storage.reserve(&mut state, TURN_BYTES)?;
            }
            rotate
        };
        if rotate {
            Ok(if self.rotate(now)? == Append::Written {
                Preparation::Rotated
            } else {
                Preparation::Deferred
            })
        } else {
            Ok(Preparation::Ready)
        }
    }
    pub fn prepare_streams(&mut self, instances: &[[u8; 16]], now: u64) -> io::Result<Preparation> {
        assert!(instances.len() < STREAMS);
        let rotate = {
            let state = self
                .storage
                .copy
                .lock()
                .map_err(|_| io::Error::other("trace copy lock poisoned"))?;
            state.active.as_ref().is_some_and(|active| {
                active.group.streams.len()
                    + instances
                        .iter()
                        .filter(|id| !active.group.streams.contains(&Some(**id)))
                        .count()
                    > STREAMS
            })
        };
        if rotate {
            Ok(if self.rotate(now)? == Append::Written {
                Preparation::Rotated
            } else {
                Preparation::Deferred
            })
        } else {
            self.prepare_turn(now)
        }
    }
    pub fn rotate(&mut self, now: u64) -> io::Result<Append> {
        if self.stopped {
            return Ok(Append::Stopped);
        }
        self.new_group(self.epoch, now)?;
        Ok(Append::Written)
    }
    #[cfg(test)]
    pub fn append(&mut self, bytes: &[u8], now: u64) -> io::Result<Append> {
        self.append_to(bytes, now, None)
    }
    pub fn append_to(&mut self, bytes: &[u8], now: u64, instance: Instance) -> io::Result<Append> {
        if self.stopped {
            return Ok(Append::Stopped);
        }
        let (envelope, _) = Envelope::decode(bytes)
            .map_err(|_| io::Error::other("invalid outgoing trace block"))?;
        if envelope.epoch != self.epoch {
            return Err(io::Error::other("trace epoch mismatch"));
        }
        let mut state =
            self.storage.copy.lock().map_err(|_| io::Error::other("trace copy lock poisoned"))?;
        let active =
            state.active.as_ref().ok_or_else(|| io::Error::other("trace active group missing"))?;
        let index = active.group.streams.iter().position(|stream| *stream == instance);
        let header =
            if index.is_none() { (SEGMENT_HEADER_BYTES + storage::INDEX_BYTES) as u64 } else { 0 };
        if index.is_none() && active.group.streams.len() == STREAMS {
            return Ok(Append::Deferred);
        }
        if bytes.len() as u64 + header > self.storage.group_bytes - active.bytes {
            return Ok(Append::Rotate);
        }
        if !self.credit.take(
            bytes.len() as u64 + header,
            now,
            !matches!(envelope.kind, format::BlockKind::Events),
        ) {
            return Ok(Append::Bandwidth);
        }
        let layout_before = if index.is_none() {
            active
                .group
                .index_charge(&self.storage.directory)?
                .checked_add(storage::directory_charge(&self.storage.directory, &[instance])?)
                .ok_or_else(|| io::Error::other("trace disk accounting overflow"))?
        } else {
            0
        };
        let active = state.active.as_mut().expect("active group");
        let index = match index {
            Some(index) => index,
            None => {
                crate::profile_paths::private_directory(&storage::directory(
                    &self.storage.directory,
                    instance,
                ))?;
                let file =
                    storage::create_header(&self.storage.directory, instance, active.group.header)?;
                active.group.streams.push(instance);
                active.group.write(&self.storage.directory)?;
                active.files.push(file);
                active.lengths.push(SEGMENT_HEADER_BYTES as u64);
                active.bytes += SEGMENT_HEADER_BYTES as u64;
                active.files.len() - 1
            }
        };
        let before = storage::allocated(&active.files[index])?;
        let mut writer = At { file: &active.files[index], offset: active.lengths[index] };
        write_block(&mut writer, bytes)?;
        active.lengths[index] = writer.offset;
        active.bytes += bytes.len() as u64;
        let after = storage::allocated(&active.files[index])?;
        // New streams also charge directory/index allocation changes. Ordinary
        // block appends cannot change directory entries and need only file deltas.
        let (before, after) = if header != 0 {
            let directories = storage::directory_charge(&self.storage.directory, &[instance])?;
            let after = after
                .checked_add(active.group.index_charge(&self.storage.directory)?)
                .and_then(|bytes| bytes.checked_add(directories))
                .ok_or_else(|| io::Error::other("trace disk accounting overflow"))?;
            (layout_before, after)
        } else {
            (before, after)
        };
        state.replace(before, after)?;
        let _ = notify::send(self.notification.as_raw_fd());
        Ok(Append::Written)
    }
    pub fn clean(&mut self, selector: Instance) -> io::Result<()> {
        let mut state =
            self.storage.copy.lock().map_err(|_| io::Error::other("trace copy lock poisoned"))?;
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("trace clear revision exhausted"))?;
        if selector.is_none() {
            state.active = None;
        }
        // Iterate ordinals on disk, retaining one bounded roster at a time.
        for ordinal in state.oldest..=state.newest {
            if ordinal == 0 {
                continue;
            }
            let group = match Group::read(&self.storage.directory, ordinal) {
                Ok(group) => group,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let (before, after) = group.remove(&self.storage.directory, selector)?;
            state.replace(before, after)?;
        }
        if let Some(active) = state.active.as_mut()
            && let Some(index) = active.group.streams.iter().position(|stream| *stream == selector)
        {
            active.group.streams.remove(index);
            active.files.remove(index);
            active.lengths.remove(index);
            active.bytes = active.lengths.iter().sum::<u64>() + storage::INDEX_BYTES as u64;
        }
        if selector.is_none() {
            state.oldest = 0;
            self.stopped = true;
            self.storage.closed.store(1, Ordering::Release);
        }
        let _ = notify::send(self.notification.as_raw_fd());
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn set_turn_budget(&mut self, slack: u64) {
        let (_, block) = storage::free_space(&self.storage.directory).expect("test filesystem");
        let reserve = Storage::reservation(TURN_BYTES, block).expect("test reservation");
        let total = self.storage.copy.lock().expect("test storage").total;
        self.set_free_bytes(6 * (reserve + slack) - total);
    }
    #[cfg(test)]
    pub(crate) fn set_free_bytes(&mut self, bytes: u64) {
        self.storage.copy.lock().expect("test storage").free = Some(bytes);
    }
}
impl Storage {
    fn reservation(bytes: u64, block: u64) -> io::Result<u64> {
        // Bound data-file rounding, one new directory per stream, parent
        // directory growth and the synced pending index's temporary allocation.
        bytes
            .checked_add((4 * STREAMS as u64 + 8) * block)
            .ok_or_else(|| io::Error::other("trace reservation overflow"))
    }
    fn reserve(&self, state: &mut State, bytes: u64) -> io::Result<()> {
        let (free, block) = storage::free_space(&self.directory)?;
        #[cfg(test)]
        let free = state.free.unwrap_or(free);
        let reservation = Self::reservation(bytes, block)?;
        // T <= F/5 after the reservation. Eviction transfers bytes from T to F,
        // so (F + T)/6 is the stable pool for this admission.
        let pool = free
            .checked_add(state.total)
            .ok_or_else(|| io::Error::other("trace quota overflow"))?
            / 6;
        while state.total.saturating_add(reservation) > pool {
            if state.oldest == 0
                || state
                    .active
                    .as_ref()
                    .is_some_and(|active| active.group.header.ordinal == state.oldest)
            {
                return Err(io::Error::other(
                    "insufficient free disk space for a bounded trace turn",
                ));
            }
            let ordinal = state.oldest;
            let group = Group::read(&self.directory, ordinal)?;
            let (before, after) = group.remove(&self.directory, None)?;
            state.replace(before, after)?;
            state.oldest = if ordinal == state.newest { 0 } else { ordinal + 1 };
        }
        Ok(())
    }
}
struct At<'a> {
    file: &'a File,
    offset: u64,
}
impl Write for At<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.file.write_at(bytes, self.offset)?;
        self.offset += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub fn write_block(writer: &mut impl Write, mut bytes: &[u8]) -> io::Result<()> {
    for _ in 0..WRITE_ATTEMPTS {
        match writer.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "trace write made no progress",
                ));
            }
            Ok(n) => {
                bytes = &bytes[n..];
                if bytes.is_empty() {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("trace write attempt budget exhausted"))
}
impl Position {
    pub fn new(cursor: Option<Cursor>) -> Self {
        Self {
            cursor,
            ordinal: 0,
            offsets: [SEGMENT_HEADER_BYTES as u64; STREAMS],
            revision: 0,
            selector: None,
            reported_closed: false,
        }
    }
    pub(crate) fn reached(&self, end: End) -> bool {
        self.ordinal > end.ordinal
            || (self.ordinal == end.ordinal
                && self.offsets.iter().zip(end.offsets).all(|(offset, end)| *offset >= end))
    }
    fn observe(&mut self, envelope: Envelope, index: usize, bytes: u64) -> ReadResult {
        if let Some(cursor) = self.cursor {
            if cursor.epoch != envelope.epoch {
                self.cursor = Some(Cursor { epoch: envelope.epoch, sequence: 0 });
                self.reported_closed = false;
                self.offsets[index] -= bytes;
                return ReadResult::Epoch(envelope.epoch);
            }
            if envelope.first + u64::from(envelope.count) - 1 <= cursor.sequence {
                return if matches!(
                    envelope.kind,
                    format::BlockKind::Source
                        | format::BlockKind::AppDefinition
                        | format::BlockKind::AppState
                ) {
                    ReadResult::Metadata(envelope)
                } else {
                    ReadResult::Skipped
                };
            }
            if cursor.sequence != 0 && envelope.first > cursor.sequence.saturating_add(1) {
                self.cursor = Some(Cursor { epoch: envelope.epoch, sequence: envelope.first - 1 });
                self.offsets[index] -= bytes;
                return ReadResult::StreamGap;
            }
        }
        self.cursor = Some(Cursor {
            epoch: envelope.epoch,
            sequence: envelope.first + u64::from(envelope.count) - 1,
        });
        ReadResult::Block(envelope)
    }
}
impl Reader {
    pub(crate) fn bounds(&self) -> io::Result<(Position, End)> {
        let state =
            self.storage.copy.lock().map_err(|_| io::Error::other("trace copy lock poisoned"))?;
        let (mut start, end) = self.group_bounds(&state, state.newest, None)?;
        start.ordinal = state.oldest;
        Ok((start, end))
    }
    pub(crate) fn recent_bounds(&self, selector: Instance) -> io::Result<(Position, End)> {
        let state =
            self.storage.copy.lock().map_err(|_| io::Error::other("trace copy lock poisoned"))?;
        self.group_bounds(&state, state.newest, selector)
    }
    pub(crate) fn previous_bounds(
        &self,
        start: Position,
        selector: Instance,
    ) -> io::Result<Option<(Position, End)>> {
        let state =
            self.storage.copy.lock().map_err(|_| io::Error::other("trace copy lock poisoned"))?;
        if state.oldest == 0 || start.ordinal <= state.oldest {
            return Ok(None);
        }
        // One bounded group per tail turn, including groups with no match.
        // Historical search never holds the collector lock across a scan.
        self.group_bounds(&state, start.ordinal - 1, selector).map(Some)
    }
    fn group_bounds(
        &self,
        state: &State,
        ordinal: u64,
        selector: Instance,
    ) -> io::Result<(Position, End)> {
        let mut start = Position::new(None);
        start.ordinal = if state.oldest == 0 { 0 } else { ordinal };
        start.revision = state.revision;
        start.selector = selector;
        let mut end =
            End { ordinal: start.ordinal, offsets: [SEGMENT_HEADER_BYTES as u64; STREAMS] };
        if state.oldest == 0 {
            return Ok((start, end));
        }
        if let Some(active) =
            state.active.as_ref().filter(|active| active.group.header.ordinal == ordinal)
        {
            end.offsets[..active.lengths.len()].copy_from_slice(&active.lengths);
            if selector.is_some() && !active.group.streams.contains(&selector) {
                start.offsets = end.offsets;
            }
        } else {
            let group = Group::read(&self.storage.directory, ordinal)?;
            for (index, stream) in group.streams.iter().enumerate() {
                match storage::open(
                    &storage::path(&self.storage.directory, *stream, ordinal),
                    false,
                ) {
                    Ok(file) => end.offsets[index] = file.metadata()?.len(),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error),
                }
            }
            if selector.is_some() && !group.streams.contains(&selector) {
                start.offsets = end.offsets;
            }
        }
        Ok((start, end))
    }
    pub fn next(&self, position: &mut Position, output: &mut Vec<u8>) -> io::Result<ReadResult> {
        if output.capacity() < MAX_BLOCK_BYTES {
            return Err(io::Error::other("trace copy capacity too small"));
        }
        let state =
            self.storage.copy.lock().map_err(|_| io::Error::other("trace copy lock poisoned"))?;
        if position.revision != 0 && position.revision != state.revision {
            let selector = position.selector;
            *position = Position::new(None);
            position.selector = selector;
            position.revision = state.revision;
            position.ordinal = state.oldest;
            return Ok(ReadResult::Expired);
        }
        position.revision = state.revision;
        if state.oldest == 0 {
            return Ok(
                if self.storage.closed.load(Ordering::Acquire) != 0 && !position.reported_closed {
                    position.reported_closed = true;
                    ReadResult::Stopped
                } else {
                    ReadResult::Pending
                },
            );
        }
        if position.ordinal == 0 {
            position.ordinal = state.oldest;
        }
        if position.ordinal < state.oldest {
            position.ordinal = state.oldest;
            position.offsets = [SEGMENT_HEADER_BYTES as u64; STREAMS];
            position.cursor = None;
            return Ok(ReadResult::Expired);
        }
        let group = if let Some(active) =
            state.active.as_ref().filter(|active| active.group.header.ordinal == position.ordinal)
        {
            active.group.clone()
        } else {
            Group::read(&self.storage.directory, position.ordinal)?
        };
        if position.selector.is_some() && !group.streams.contains(&position.selector) {
            // This group's sequence numbers belong to other instances. Skipping
            // them is intentional, while a later epoch must still be reported.
            if let Some(cursor) = &mut position.cursor {
                cursor.sequence = 0;
            }
            if position.ordinal < state.newest {
                position.ordinal += 1;
                position.offsets = [SEGMENT_HEADER_BYTES as u64; STREAMS];
                return Ok(ReadResult::Skipped);
            }
            return Ok(
                if self.storage.closed.load(Ordering::Acquire) != 0 && !position.reported_closed {
                    position.reported_closed = true;
                    ReadResult::Stopped
                } else {
                    ReadResult::Pending
                },
            );
        }
        let mut selected: Option<(usize, u64, usize)> = None;
        // Probe fixed headers only. At most 41 candidates share one complete
        // block buffer; no event-sized k-way-merge heap or historical fd pins.
        for (index, stream) in group.streams.iter().enumerate() {
            let file = match storage::open(
                &storage::path(&self.storage.directory, *stream, position.ordinal),
                false,
            ) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let length = file.metadata()?.len();
            let offset = position.offsets[index];
            if offset == length {
                continue;
            }
            if offset > length || length - offset < format::BLOCK_HEADER_BYTES as u64 {
                position.offsets[index] = length;
                return Ok(ReadResult::Incomplete);
            }
            let mut header = [0; format::BLOCK_HEADER_BYTES];
            file.read_exact_at(&mut header, offset)?;
            let bytes = Envelope::encoded_length(&header).map_err(|error| match error {
                DecodeError::Unsupported => io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported retained trace envelope",
                ),
                _ => io::Error::other("invalid retained trace envelope"),
            })?;
            if bytes as u64 > length - offset {
                position.offsets[index] = length;
                return Ok(ReadResult::Incomplete);
            }
            let sequence = u64::from_le_bytes(header[32..40].try_into().expect("sequence word"));
            if selected.is_none_or(|(_, first, _)| sequence < first) {
                selected = Some((index, sequence, bytes));
            }
        }
        if let Some((index, _, length)) = selected {
            let file = storage::open(
                &storage::path(&self.storage.directory, group.streams[index], position.ordinal),
                false,
            )?;
            output.clear();
            output.resize(length, 0);
            file.read_exact_at(output, position.offsets[index])?;
            let (envelope, _) = Envelope::decode(output).map_err(|error| match error {
                DecodeError::Unsupported => io::Error::new(
                    io::ErrorKind::Unsupported,
                    "this reader cannot open the retained trace event format",
                ),
                _ => io::Error::other("invalid retained trace block"),
            })?;
            position.offsets[index] += length as u64;
            return Ok(position.observe(envelope, index, length as u64));
        }
        if position.ordinal < state.newest {
            position.ordinal += 1;
            position.offsets = [SEGMENT_HEADER_BYTES as u64; STREAMS];
            return Ok(ReadResult::Skipped);
        }
        Ok(if self.storage.closed.load(Ordering::Acquire) != 0 && !position.reported_closed {
            position.reported_closed = true;
            ReadResult::Stopped
        } else {
            ReadResult::Pending
        })
    }
}
impl Drop for Log {
    fn drop(&mut self) {
        self.storage.closed.store(1, Ordering::Release);
        let _ = notify::send(self.notification.as_raw_fd());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use playbit_api::abi::trace::{self as wire, TraceEvent};
    fn fixture() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pt-groups-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("test directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("private test directory");
        path
    }
    fn event(log: &mut Log, sequence: u64, scope: u32, instance: Instance, now: u64) {
        let mut payload = [0; 56];
        payload[..4].copy_from_slice(&scope.to_le_bytes());
        payload[4..8].copy_from_slice(&1u32.to_le_bytes());
        payload[8..16].copy_from_slice(&1u64.to_le_bytes());
        wire::encode_header(
            TraceEvent { timestamp: sequence, kind: wire::INSTANT, ..Default::default() },
            &mut payload[16..],
        )
        .expect("event header");
        let envelope = Envelope {
            kind: format::BlockKind::Events,
            epoch: log.epoch,
            first: sequence,
            count: 1,
            source: [1, 2],
            time_base: now,
        };
        let mut bytes = Vec::with_capacity(MAX_BLOCK_BYTES);
        envelope.encode(&payload, &mut bytes).expect("event block");
        assert_eq!(log.append_to(&bytes, now, instance).expect("append event"), Append::Written);
    }
    fn events(reader: &Reader) -> Vec<(u64, u32)> {
        let mut position = Position::new(None);
        let mut bytes = Vec::with_capacity(MAX_BLOCK_BYTES);
        let mut events = Vec::new();
        for _ in 0..4096 {
            match reader.next(&mut position, &mut bytes).expect("read event") {
                ReadResult::Block(envelope) if envelope.kind == format::BlockKind::Events => {
                    let (_, payload) = Envelope::decode(&bytes).expect("retained block");
                    events.push((
                        envelope.first,
                        format::Events::decode(payload).expect("event payload").scope,
                    ));
                }
                ReadResult::Pending | ReadResult::Stopped => return events,
                _ => (),
            }
        }
        panic!("retained read exceeded bounded fixture");
    }
    #[test]
    fn starts_preserve_more_than_the_old_ring_and_reopen_without_mutation() {
        let path = fixture();
        let mut log = Log::open(&path, false, 0).expect("log");
        for capture in 0..12 {
            if capture != 0 {
                log.begin_epoch(capture * 1_000_000_000).expect("new capture");
            }
            event(&mut log, 1, 1, Some([1; 16]), capture * 1_000_000_000);
            log.close();
        }
        assert_eq!(events(&log.reader()).len(), 12);
        drop(log);
        let log = Log::retained(&path, false, 0).expect("retained log").expect("present log");
        assert_eq!(events(&log.reader()).len(), 12);
        drop(log);
        fs::remove_dir_all(path).expect("cleanup");
    }
    #[test]
    fn instance_files_merge_by_sequence_and_scoped_clean_preserves_siblings() {
        let path = fixture();
        let mut log = Log::open(&path, false, 0).expect("log");
        event(&mut log, 1, 1, Some([1; 16]), 0);
        event(&mut log, 2, 0, None, 0);
        event(&mut log, 3, 2, Some([2; 16]), 0);
        assert_eq!(events(&log.reader()), [(1, 1), (2, 0), (3, 2)]);
        let mut position = Position::new(None);
        let reader = log.reader();
        let mut bytes = Vec::with_capacity(MAX_BLOCK_BYTES);
        assert!(matches!(
            reader.next(&mut position, &mut bytes).expect("first"),
            ReadResult::Block(_)
        ));
        log.clean(Some([1; 16])).expect("clean A");
        assert_eq!(
            reader.next(&mut position, &mut bytes).expect("invalidation"),
            ReadResult::Expired
        );
        assert_eq!(events(&reader), [(2, 0), (3, 2)]);
        event(&mut log, 4, 2, Some([2; 16]), 0);
        assert_eq!(events(&reader), [(2, 0), (3, 2), (4, 2)]);
        log.clean(None).expect("profile clean");
        let (start, end) = reader.bounds().expect("empty snapshot");
        assert!(start.reached(end));
        assert!(events(&reader).is_empty());
        log.begin_epoch(1_000_000_000).expect("fresh capture after clean");
        event(&mut log, 1, 1, Some([1; 16]), 1_000_000_000);
        assert_eq!(events(&reader), [(1, 1)]);
        drop(reader);
        drop(log);
        fs::remove_dir_all(path).expect("cleanup");
    }
    #[test]
    fn quota_evicts_oldest_groups_across_instances_and_preserves_format() {
        let path = fixture();
        let mut log = Log::open(&path, false, 0).expect("log");
        log.set_turn_budget(512 * 1024);
        let payload = vec![0; format::MAX_PAYLOAD_BYTES];
        let mut bytes = Vec::with_capacity(MAX_BLOCK_BYTES);
        for group in 1..=40 {
            let instance = Some([if group % 2 == 0 { 1 } else { 2 }; 16]);
            let envelope = Envelope {
                kind: format::BlockKind::Source,
                epoch: log.epoch,
                first: group,
                count: 1,
                source: [1, 2],
                time_base: 0,
            };
            envelope.encode(&payload, &mut bytes).expect("large metadata block");
            assert_eq!(
                log.append_to(&bytes, group * 1_000_000_000, instance).expect("retained block"),
                Append::Written
            );
            log.rotate(group * 1_000_000_000).expect("next group");
        }
        let state = log.storage.copy.lock().expect("storage");
        assert!(state.oldest > 1, "oldest groups must be evicted");
        assert!(state.total <= state.free.expect("injected free") / 5);
        let oldest = state.oldest;
        drop(state);
        assert!(!storage::path(&path, Some([2; 16]), 1).exists());
        assert!(storage::path(&path, None, oldest).exists());
        drop(log);
        fs::remove_dir_all(path).expect("cleanup");
    }
    #[test]
    fn clean_preserves_unsupported_retained_blocks() {
        let path = fixture();
        let mut log = Log::open(&path, false, 0).expect("log");
        event(&mut log, 1, 1, Some([1; 16]), 0);
        let file_path = storage::path(&path, Some([1; 16]), 1);
        let mut bytes = fs::read(&file_path).expect("original block");
        bytes[SEGMENT_HEADER_BYTES + 4] = 255;
        fs::write(&file_path, &bytes).expect("future payload tag");
        assert!(log.clean(Some([1; 16])).is_err());
        assert_eq!(fs::read(&file_path).expect("preserved bytes"), bytes);
        drop(log);
        fs::remove_dir_all(path).expect("cleanup");
    }
    #[test]
    fn instance_reader_skips_unrelated_groups_without_reporting_loss() {
        let path = fixture();
        let mut log = Log::open(&path, false, 0).expect("log");
        event(&mut log, 1, 1, Some([1; 16]), 0);
        assert_eq!(log.rotate(0).expect("second group"), Append::Written);
        event(&mut log, 2, 2, Some([2; 16]), 0);
        assert_eq!(log.rotate(0).expect("third group"), Append::Written);
        event(&mut log, 3, 1, Some([1; 16]), 0);
        let reader = log.reader();
        let (mut position, _) = reader.bounds().expect("history bounds");
        position.selector = Some([1; 16]);
        let mut bytes = Vec::with_capacity(MAX_BLOCK_BYTES);
        let mut sequences = Vec::new();
        for _ in 0..16 {
            match reader.next(&mut position, &mut bytes).expect("selected history") {
                ReadResult::Block(envelope) => sequences.push(envelope.first),
                ReadResult::Skipped => (),
                ReadResult::Pending => break,
                other => panic!("unexpected selected-history result: {other:?}"),
            }
        }
        assert_eq!(sequences, [1, 3]);
        drop(reader);
        drop(log);
        fs::remove_dir_all(path).expect("cleanup");
    }
    #[test]
    fn group_roster_rotates_before_admitting_more_than_forty_instances() {
        let path = fixture();
        let mut log = Log::open(&path, false, 0).expect("log");
        for id in 1..=40 {
            event(&mut log, u64::from(id), u32::from(id), Some([id; 16]), 0);
        }
        assert_eq!(
            log.prepare_streams(&[[41; 16]], 0).expect("bounded new roster"),
            Preparation::Rotated
        );
        event(&mut log, 41, 41, Some([41; 16]), 0);
        assert_eq!(events(&log.reader()).len(), 41);
        drop(log);
        fs::remove_dir_all(path).expect("cleanup");
    }
    #[test]
    fn recovery_publishes_a_complete_pending_roster_without_losing_streams() {
        let path = fixture();
        let mut log = Log::open(&path, false, 0).expect("log");
        event(&mut log, 1, 1, Some([1; 16]), 0);
        let index = storage::directory(&path, None).join(storage::name(1, "index"));
        let previous = fs::read(&index).expect("old roster");
        event(&mut log, 2, 2, Some([2; 16]), 0);
        fs::copy(&index, index.with_extension("pending")).expect("complete pending roster");
        fs::write(&index, previous).expect("last published roster");
        drop(log);
        let log =
            Log::retained(&path, false, 0).expect("recover pending roster").expect("retained log");
        assert_eq!(events(&log.reader()), [(1, 1), (2, 2)]);
        assert!(!index.with_extension("pending").exists());
        drop(log);
        fs::remove_dir_all(path).expect("cleanup");
    }
    fn allocated_tree(path: &Path) -> u64 {
        let mut bytes = fs::symlink_metadata(path).expect("root metadata").blocks() * 512;
        for entry in fs::read_dir(path).expect("trace root") {
            let entry = entry.expect("trace entry");
            let metadata = fs::symlink_metadata(entry.path()).expect("entry metadata");
            bytes += metadata.blocks() * 512;
            if metadata.is_dir() {
                for file in fs::read_dir(entry.path()).expect("stream files") {
                    bytes += fs::symlink_metadata(file.expect("trace file").path())
                        .expect("file metadata")
                        .blocks()
                        * 512;
                }
            }
        }
        bytes
    }
    #[test]
    fn quota_ledger_includes_directories_and_clean_releases_empty_streams() {
        let path = fixture();
        let mut log = Log::open(&path, false, 0).expect("log");
        for id in 1..=16 {
            event(&mut log, u64::from(id), u32::from(id), Some([id; 16]), 0);
        }
        assert_eq!(log.storage.copy.lock().expect("storage").total, allocated_tree(&path));
        log.clean(Some([1; 16])).expect("clean one stream");
        assert!(!storage::directory(&path, Some([1; 16])).exists());
        assert_eq!(log.storage.copy.lock().expect("storage").total, allocated_tree(&path));
        log.clean(None).expect("clean all streams");
        for id in 1..=16 {
            assert!(!storage::directory(&path, Some([id; 16])).exists());
        }
        assert_eq!(log.storage.copy.lock().expect("storage").total, allocated_tree(&path));
        drop(log);
        fs::remove_dir_all(path).expect("cleanup");
    }
    #[test]
    fn evicting_old_history_does_not_rewind_a_current_tail() {
        let path = fixture();
        let mut log = Log::open(&path, false, 0).expect("log");
        let payload = vec![0; format::MAX_PAYLOAD_BYTES];
        let mut bytes = Vec::with_capacity(MAX_BLOCK_BYTES);
        for sequence in 1..=24 {
            Envelope {
                kind: format::BlockKind::Source,
                epoch: log.epoch,
                first: sequence,
                count: 1,
                source: [1, 2],
                time_base: 0,
            }
            .encode(&payload, &mut bytes)
            .expect("old metadata block");
            assert_eq!(log.append(&bytes, 0).expect("old history"), Append::Written);
        }
        log.rotate(1_000_000_000).expect("current group");
        event(&mut log, 25, 2, Some([2; 16]), 1_000_000_000);
        let reader = log.reader();
        let mut position = Position::new(None);
        let mut caught_up = false;
        for _ in 0..64 {
            if reader.next(&mut position, &mut bytes).expect("advance current tail")
                == ReadResult::Pending
            {
                caught_up = true;
                break;
            }
        }
        assert!(caught_up);
        let (_, block) = storage::free_space(&path).expect("filesystem block size");
        let slack = Group::read(&path, 2)
            .expect("current group")
            .charge(&path)
            .expect("current allocation")
            + 4 * block;
        assert!(
            Group::read(&path, 1).expect("old group").charge(&path).expect("old allocation")
                > slack
        );
        log.set_turn_budget(slack);
        log.rotate(2_000_000_000).expect("evict oldest group");
        assert_eq!(log.storage.copy.lock().expect("storage").oldest, 2);
        event(&mut log, 26, 2, Some([2; 16]), 2_000_000_000);
        for _ in 0..4 {
            match reader.next(&mut position, &mut bytes).expect("current tail") {
                ReadResult::Block(envelope) => {
                    assert_eq!(envelope.first, 26);
                    drop(reader);
                    drop(log);
                    fs::remove_dir_all(path).expect("cleanup");
                    return;
                }
                ReadResult::Expired => {
                    panic!("unrelated old-history eviction rewound a current tail")
                }
                _ => (),
            }
        }
        panic!("new event did not follow current cursor");
    }
    #[test]
    fn recovery_discards_partial_unpublished_roster_and_header_only_orphan() {
        let path = fixture();
        let mut log = Log::open(&path, false, 0).expect("log");
        event(&mut log, 1, 1, Some([1; 16]), 0);
        let index = storage::directory(&path, None).join(storage::name(1, "index"));
        let bytes = fs::read(&index).expect("published index");
        let pending =
            storage::open(&index.with_extension("pending"), true).expect("unpublished index");
        write_block(&mut At { file: &pending, offset: 0 }, &bytes[..100])
            .expect("partial unpublished write");
        drop(pending);
        let orphan_dir = storage::directory(&path, Some([2; 16]));
        crate::profile_paths::private_directory(&orphan_dir).expect("unpublished stream directory");
        let orphan = storage::open(&storage::path(&path, Some([2; 16]), 1), true)
            .expect("unpublished stream");
        write_block(
            &mut At { file: &orphan, offset: 0 },
            &Group::read(&path, 1).expect("published group").header.encode(),
        )
        .expect("unpublished header");
        drop(orphan);
        drop(log);
        let mut log =
            Log::retained(&path, false, 0).expect("recover partial staging").expect("retained log");
        assert_eq!(events(&log.reader()), [(1, 1)]);
        assert!(!index.with_extension("pending").exists());
        assert!(!orphan_dir.exists());
        assert_eq!(log.storage.copy.lock().expect("storage").total, allocated_tree(&path));
        log.begin_epoch(1_000_000_000).expect("recording retry remains usable");
        event(&mut log, 1, 2, Some([2; 16]), 1_000_000_000);
        assert_eq!(events(&log.reader()), [(1, 1), (1, 2)]);
        drop(log);
        fs::remove_dir_all(path).expect("cleanup");
    }
    #[test]
    fn recovery_removes_only_header_staging_before_a_first_roster_publication() {
        let header = Segment { epoch: [7, 9], ordinal: 1 }.encode();
        for length in 0..=SEGMENT_HEADER_BYTES {
            let path = fixture();
            let log = Log::create(&path, false, 0).expect("unstarted log");
            let orphan_path = storage::path(&path, None, 1);
            let orphan = storage::open(&orphan_path, true).expect("unpublished header file");
            if length != 0 {
                write_block(&mut At { file: &orphan, offset: 0 }, &header[..length])
                    .expect("interrupted header write");
            }
            drop(orphan);
            drop(log);
            let mut log = Log::retained(&path, false, 0)
                .expect("recover every interrupted header prefix")
                .expect("retained destination");
            assert!(events(&log.reader()).is_empty());
            assert!(!orphan_path.exists());
            log.begin_epoch(0).expect("first supported capture remains usable");
            event(&mut log, 1, 1, Some([1; 16]), 0);
            assert_eq!(events(&log.reader()), [(1, 1)]);
            drop(log);
            fs::remove_dir_all(path).expect("cleanup");
        }
        let mut incompatible = vec![b"other format".to_vec()];
        let mut zero_epoch = header[..24].to_vec();
        zero_epoch[8..24].fill(0);
        incompatible.push(zero_epoch);
        for offset in [0, 24, 32, 40, 44] {
            let mut bytes = header[..offset + 1].to_vec();
            bytes[offset] ^= 1;
            incompatible.push(bytes);
        }
        let mut block_bearing = header.to_vec();
        block_bearing.push(0);
        incompatible.push(block_bearing);
        for bytes in incompatible {
            let path = fixture();
            let log = Log::create(&path, false, 0).expect("unstarted log");
            let orphan_path = storage::path(&path, None, 1);
            let orphan = storage::open(&orphan_path, true).expect("foreign header file");
            write_block(&mut At { file: &orphan, offset: 0 }, &bytes).expect("foreign bytes");
            drop(orphan);
            drop(log);
            assert!(Log::retained(&path, false, 0).is_err());
            assert_eq!(fs::read(&orphan_path).expect("preserved bytes"), bytes);
            fs::remove_dir_all(path).expect("cleanup");
        }
        // A published index protects even an empty or incomplete segment;
        // recognizable prefix cleanup applies exclusively to unpublished files.
        for length in [0, 8, 47] {
            let path = fixture();
            let log = Log::open(&path, false, 0).expect("published group");
            let published_path = storage::path(&path, None, 1);
            let bytes = fs::read(&published_path).expect("published header");
            fs::write(&published_path, &bytes[..length]).expect("interrupted published file");
            drop(log);
            assert!(Log::retained(&path, false, 0).is_err());
            assert_eq!(
                fs::read(&published_path).expect("preserved published bytes"),
                bytes[..length]
            );
            fs::remove_dir_all(path).expect("cleanup");
        }
        let path = fixture();
        let log = Log::open(&path, false, 0).expect("published group");
        let mut wrong_epoch =
            Group::read(&path, 1).expect("published index").header.encode()[..9].to_vec();
        wrong_epoch[8] ^= 1;
        let orphan_dir = storage::directory(&path, Some([2; 16]));
        crate::profile_paths::private_directory(&orphan_dir).expect("unpublished instance");
        let orphan_path = storage::path(&path, Some([2; 16]), 1);
        let orphan = storage::open(&orphan_path, true).expect("unpublished header");
        write_block(&mut At { file: &orphan, offset: 0 }, &wrong_epoch)
            .expect("wrong epoch prefix");
        drop(orphan);
        drop(log);
        assert!(Log::retained(&path, false, 0).is_err());
        assert_eq!(fs::read(&orphan_path).expect("preserved incompatible epoch"), wrong_epoch);
        fs::remove_dir_all(path).expect("cleanup");
    }
}
