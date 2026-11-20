//! Fixed-size group rosters keep historical discovery on disk. A group contains
//! at most the global stream and the forty admitted app identities.
#![allow(unsafe_code, reason = "statvfs reads one checked POSIX filesystem statistics structure.")]
use super::*;
pub(super) const STREAMS: usize = 1 + crate::queue::MAX_SOURCES * crate::region::APPS;
pub(super) const INDEX_BYTES: usize = 64 + STREAMS * 17 + 4;
const INDEX_MAGIC: &[u8; 8] = b"PBTGRP01";
pub(super) type Instance = Option<[u8; 16]>;
#[derive(Clone)]
pub(super) struct Group {
    pub header: Segment,
    pub streams: Vec<Instance>,
}
pub(super) struct Active {
    pub group: Group,
    pub files: Vec<File>,
    pub lengths: Vec<u64>,
    pub bytes: u64,
}
pub(super) struct State {
    pub active: Option<Active>,
    pub oldest: u64,
    pub newest: u64,
    pub total: u64,
    pub revision: u64,
    #[cfg(test)]
    pub free: Option<u64>,
}
pub(super) fn directory(root: &Path, instance: Instance) -> PathBuf {
    root.join(instance.map_or_else(|| "global".to_owned(), |id| crate::identifier::encode(&id)))
}
pub(super) fn name(ordinal: u64, extension: &str) -> String {
    format!("group-{ordinal:020}.{extension}")
}
pub(super) fn path(root: &Path, instance: Instance, ordinal: u64) -> PathBuf {
    directory(root, instance).join(name(ordinal, "pbt"))
}
fn index_path(root: &Path, ordinal: u64) -> PathBuf {
    directory(root, None).join(name(ordinal, "index"))
}
pub(super) fn open(path: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    private_file(&file)?;
    Ok(file)
}
pub(super) fn create_header(root: &Path, instance: Instance, header: Segment) -> io::Result<File> {
    let path = path(root, instance, header.ordinal);
    let file = open(&path, true)?;
    let result = write_block(&mut At { file: &file, offset: 0 }, &header.encode())
        .and_then(|()| file.sync_data());
    if let Err(error) = result {
        // This exact file was created by us and no roster can reference it yet.
        // Failure cleanup never removes an admitted recording or its blocks.
        drop(file);
        fs::remove_file(&path).map_err(|cleanup| {
            io::Error::other(format!(
                "unpublished trace header failed: {error}; cleanup failed: {cleanup}"
            ))
        })?;
        File::open(directory(root, instance))?.sync_all()?;
        return Err(error);
    }
    Ok(file)
}
pub(super) fn allocated(file: &File) -> io::Result<u64> {
    file.metadata()?
        .blocks()
        .checked_mul(512)
        .ok_or_else(|| io::Error::other("trace disk accounting overflow"))
}
impl Group {
    pub fn read(root: &Path, ordinal: u64) -> io::Result<Self> {
        Self::read_at(&index_path(root, ordinal), ordinal)
    }
    fn read_at(path: &Path, ordinal: u64) -> io::Result<Self> {
        let file = open(path, false)?;
        if file.metadata()?.len() != INDEX_BYTES as u64 {
            return Err(io::Error::other("unsupported trace group index length"));
        }
        let mut bytes = [0; INDEX_BYTES];
        file.read_exact_at(&mut bytes, 0)?;
        if &bytes[..8] != INDEX_MAGIC
            || format::checksum(&bytes[..INDEX_BYTES - 4])
                != u32::from_le_bytes(bytes[INDEX_BYTES - 4..].try_into().expect("index checksum"))
        {
            return Err(io::Error::other("unsupported or malformed trace group index"));
        }
        let header = Segment::decode(&bytes[8..56])
            .map_err(|_| io::Error::other("unsupported trace group header"))?;
        let count = u64::from_le_bytes(bytes[56..64].try_into().expect("stream count"));
        if header.ordinal != ordinal || count == 0 || count > STREAMS as u64 {
            return Err(io::Error::other("invalid trace group roster"));
        }
        let mut streams = Vec::with_capacity(STREAMS);
        for entry in bytes[64..INDEX_BYTES - 4].as_chunks::<17>().0.iter().take(count as usize) {
            let stream = match entry[0] {
                0 if entry[1..] == [0; 16] => None,
                1 => Some(entry[1..].try_into().expect("instance identity")),
                _ => return Err(io::Error::other("invalid trace stream identity")),
            };
            if streams.contains(&stream) {
                return Err(io::Error::other("duplicate trace stream identity"));
            }
            streams.push(stream);
        }
        if bytes[64 + count as usize * 17..INDEX_BYTES - 4].iter().any(|byte| *byte != 0) {
            return Err(io::Error::other("invalid trace group padding"));
        }
        if streams.first() != Some(&None) {
            return Err(io::Error::other("trace group must start with the global stream"));
        }
        Ok(Self { header, streams })
    }
    pub fn write(&self, root: &Path) -> io::Result<()> {
        assert!(self.streams.len() <= STREAMS);
        let mut bytes = [0; INDEX_BYTES];
        bytes[..8].copy_from_slice(INDEX_MAGIC);
        bytes[8..56].copy_from_slice(&self.header.encode());
        bytes[56..64].copy_from_slice(&(self.streams.len() as u64).to_le_bytes());
        for (entry, stream) in
            bytes[64..INDEX_BYTES - 4].as_chunks_mut::<17>().0.iter_mut().zip(&self.streams)
        {
            if let Some(id) = stream {
                entry[0] = 1;
                entry[1..].copy_from_slice(id);
            }
        }
        let checksum = format::checksum(&bytes[..INDEX_BYTES - 4]);
        bytes[INDEX_BYTES - 4..].copy_from_slice(&checksum.to_le_bytes());
        // Publish the complete checksummed roster atomically. A crash leaves
        // the previous index or a recoverable complete pending index, never a
        // mixture of the two. Stream headers precede roster admission.
        let target = index_path(root, self.header.ordinal);
        let pending = target.with_extension("pending");
        let file = open(&pending, true)?;
        write_block(&mut At { file: &file, offset: 0 }, &bytes)?;
        file.sync_data()?;
        fs::rename(pending, target)?;
        File::open(directory(root, None))?.sync_all()
    }

    pub fn index_charge(&self, root: &Path) -> io::Result<u64> {
        allocated(&open(&index_path(root, self.header.ordinal), false)?)
    }
    pub fn charge(&self, root: &Path) -> io::Result<u64> {
        let mut bytes = self.index_charge(root)?;
        for stream in &self.streams {
            match open(&path(root, *stream, self.header.ordinal), false) {
                Ok(file) => {
                    bytes = bytes
                        .checked_add(allocated(&file)?)
                        .ok_or_else(|| io::Error::other("trace disk accounting overflow"))?
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        Ok(bytes)
    }
    pub fn remove(&self, root: &Path, selector: Instance) -> io::Result<(u64, u64)> {
        if selector.is_some() && !self.streams.contains(&selector) {
            return Ok((0, 0));
        }
        let before = self
            .charge(root)?
            .checked_add(directory_charge(root, &self.streams)?)
            .ok_or_else(|| io::Error::other("trace disk accounting overflow"))?;
        // Validate every file before deleting any part of the selected group.
        for stream in
            self.streams.iter().filter(|stream| selector.is_none() || **stream == selector)
        {
            match open(&path(root, *stream, self.header.ordinal), false) {
                Ok(file) => {
                    let mut bytes = [0; SEGMENT_HEADER_BYTES];
                    file.read_exact_at(&mut bytes, 0)?;
                    if Segment::decode(&bytes) != Ok(self.header) {
                        return Err(io::Error::other(
                            "unsupported retained trace segment preserved",
                        ));
                    }
                    validate_blocks(&file, self.header.epoch)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        for stream in
            self.streams.iter().filter(|stream| selector.is_none() || **stream == selector)
        {
            match fs::remove_file(path(root, *stream, self.header.ordinal)) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
            if stream.is_some() {
                remove_empty_directory(&directory(root, *stream))?;
            }
        }
        let files = if selector.is_none() {
            fs::remove_file(index_path(root, self.header.ordinal))?;
            0
        } else {
            let mut kept = self.clone();
            kept.streams.retain(|stream| *stream != selector);
            kept.write(root)?;
            kept.charge(root)?
        };
        let after = files
            .checked_add(directory_charge(root, &self.streams)?)
            .ok_or_else(|| io::Error::other("trace disk accounting overflow"))?;
        Ok((before, after))
    }
}
pub(super) fn inspect(root: &Path) -> io::Result<State> {
    recover_indexes(root)?;
    let mut oldest = u64::MAX;
    let mut newest = 0;
    let mut total = fs::symlink_metadata(root)?
        .blocks()
        .checked_mul(512)
        .ok_or_else(|| io::Error::other("trace directory accounting overflow"))?;
    let mut count = 0u64;
    // Streaming inventory retains one fixed group roster, never historical file
    // handles/catalogs. This runs only on the background recorder's first open.
    for entry in fs::read_dir(directory(root, None))? {
        let entry = entry?;
        let name = entry.file_name();
        let text = name.to_str().ok_or_else(|| io::Error::other("invalid trace filename"))?;
        if text.ends_with(".pbt") {
            continue;
        }
        let ordinal = ordinal(text, "index")?;
        let group = Group::read(root, ordinal)?;
        // A new epoch uses a fresh file; even an incompatible block payload is
        // never appended to. Only validate the independently versioned framing.
        for stream in &group.streams {
            match open(&path(root, *stream, ordinal), false) {
                Ok(file) => {
                    let mut header = [0; SEGMENT_HEADER_BYTES];
                    file.read_exact_at(&mut header, 0)?;
                    if Segment::decode(&header) != Ok(group.header) {
                        return Err(io::Error::other(
                            "unsupported retained trace segment preserved",
                        ));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        total = total
            .checked_add(group.charge(root)?)
            .ok_or_else(|| io::Error::other("trace disk accounting overflow"))?;
        oldest = oldest.min(ordinal);
        newest = newest.max(ordinal);
        count += 1;
    }
    if newest != 0 && newest - oldest + 1 != count {
        return Err(io::Error::other("trace group history has missing indexes"));
    }
    // Reject foreign files rather than allowing a familiar directory name to
    // authorize their deletion. In particular, the former ring is not migrated.
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "owner.lock" {
            total = total
                .checked_add(allocated(&open(&entry.path(), false)?)?)
                .ok_or_else(|| io::Error::other("trace disk accounting overflow"))?;
            continue;
        }
        let text = name.to_str().ok_or_else(|| io::Error::other("invalid trace directory"))?;
        let stream = if text == "global" {
            None
        } else {
            Some(crate::identifier::decode::<16>(text).map_err(io::Error::other)?)
        };
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other("trace stream directory must be private"));
        }
        total = total
            .checked_add(
                metadata
                    .blocks()
                    .checked_mul(512)
                    .ok_or_else(|| io::Error::other("trace directory accounting overflow"))?,
            )
            .ok_or_else(|| io::Error::other("trace disk accounting overflow"))?;
        for file in fs::read_dir(entry.path())? {
            let file = file?;
            let name = file.file_name();
            let text = name.to_str().ok_or_else(|| io::Error::other("invalid trace filename"))?;
            if stream.is_none() && text.ends_with(".index") {
                continue;
            }
            let ordinal = ordinal(text, "pbt")?;
            let group = match Group::read(root, ordinal) {
                Ok(group) => Some(group),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            if group.as_ref().is_none_or(|group| !group.streams.contains(&stream)) {
                // No published roster admits this file. A crash can leave any
                // prefix of create_header's single write, including an empty
                // file. Preserve incompatible prefixes and all block-bearing
                // files rather than allowing their names to authorize removal.
                let unpublished = open(&file.path(), false)?;
                unpublished_header(
                    &unpublished,
                    ordinal,
                    group.as_ref().map(|group| group.header),
                )?;
                let before = directory_charge(root, &[stream])?;
                drop(unpublished);
                fs::remove_file(file.path())?;
                // Root inventory has already charged this directory. Adjust its
                // allocation if removing an abandoned header changes entries.
                let after = directory_charge(root, &[stream])?;
                total = if after >= before {
                    total.checked_add(after - before)
                } else {
                    total.checked_sub(before - after)
                }
                .ok_or_else(|| io::Error::other("trace directory accounting overflow"))?;
            }
        }
        if stream.is_some() {
            let before = directory_charge(root, &[stream])?;
            remove_empty_directory(&entry.path())?;
            let after = directory_charge(root, &[stream])?;
            total = if after >= before {
                total.checked_add(after - before)
            } else {
                total.checked_sub(before - after)
            }
            .ok_or_else(|| io::Error::other("trace directory accounting overflow"))?;
        }
    }
    Ok(State {
        active: None,
        oldest: if newest == 0 { 0 } else { oldest },
        newest,
        total,
        revision: 1,
        #[cfg(test)]
        free: None,
    })
}
fn unpublished_header(file: &File, ordinal: u64, published: Option<Segment>) -> io::Result<()> {
    let length = file.metadata()?.len();
    if length > SEGMENT_HEADER_BYTES as u64 {
        return Err(io::Error::other("unrecognized retained trace file preserved"));
    }
    let mut bytes = [0; SEGMENT_HEADER_BYTES];
    file.read_exact_at(&mut bytes[..length as usize], 0)?;
    // With no published index the epoch is the only unknown header field.
    // Zero-fill its unwritten suffix, then use the actual encoder to check
    // every available magic, ordinal, extent, checksum and reserved byte.
    let epoch = [
        u64::from_le_bytes(bytes[8..16].try_into().expect("first epoch word")),
        u64::from_le_bytes(bytes[16..24].try_into().expect("second epoch word")),
    ];
    if published.is_none() && length >= 24 && epoch == [0, 0] {
        return Err(io::Error::other("unsupported unpublished trace header preserved"));
    }
    let expected = published.unwrap_or(Segment { epoch, ordinal }).encode();
    if bytes[..length as usize] != expected[..length as usize] {
        return Err(io::Error::other("unsupported unpublished trace header preserved"));
    }
    Ok(())
}
fn ordinal(name: &str, extension: &str) -> io::Result<u64> {
    let number = name
        .strip_prefix("group-")
        .and_then(|name| name.strip_suffix(&format!(".{extension}")))
        .ok_or_else(|| io::Error::other("unsupported retained trace filename"))?;
    let ordinal = number.parse::<u64>().map_err(io::Error::other)?;
    if ordinal == 0 || name != self::name(ordinal, extension) {
        return Err(io::Error::other("invalid trace group ordinal"));
    }
    Ok(ordinal)
}
pub(super) fn free_space(root: &Path) -> io::Result<(u64, u64)> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(root.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: valid terminated path and output structure, checked before read.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statvfs returned success and initialized the whole output.
    let stat = unsafe { stat.assume_init() };
    let block = stat.f_frsize;
    #[cfg(target_vendor = "apple")]
    let available = u64::from(stat.f_bavail);
    #[cfg(not(target_vendor = "apple"))]
    let available = stat.f_bavail;
    let free = available
        .checked_mul(block)
        .ok_or_else(|| io::Error::other("trace free-space accounting overflow"))?;
    Ok((free, block.max(512)))
}

fn validate_blocks(file: &File, epoch: [u64; 2]) -> io::Result<()> {
    let length = file.metadata()?.len();
    if length > DESKTOP_BYTES {
        return Err(io::Error::other("unsupported oversized trace segment preserved"));
    }
    let mut offset = SEGMENT_HEADER_BYTES as u64;
    let mut block = Vec::with_capacity(MAX_BLOCK_BYTES);
    // Every block has at least a 64-byte header and the file is <=16 MiB.
    for _ in 0..DESKTOP_BYTES / format::BLOCK_HEADER_BYTES as u64 {
        if offset == length {
            return Ok(());
        }
        if length.saturating_sub(offset) < format::BLOCK_HEADER_BYTES as u64 {
            return Err(io::Error::other("incomplete retained trace block preserved"));
        }
        block.resize(format::BLOCK_HEADER_BYTES, 0);
        file.read_exact_at(&mut block, offset)?;
        let bytes = Envelope::encoded_length(&block)
            .map_err(|_| io::Error::other("unsupported retained trace block preserved"))?;
        if bytes as u64 > length - offset {
            return Err(io::Error::other("incomplete retained trace block preserved"));
        }
        block.resize(bytes, 0);
        file.read_exact_at(
            &mut block[format::BLOCK_HEADER_BYTES..],
            offset + format::BLOCK_HEADER_BYTES as u64,
        )?;
        let (envelope, _) = Envelope::decode(&block)
            .map_err(|_| io::Error::other("unsupported retained trace block preserved"))?;
        if envelope.epoch != epoch {
            return Err(io::Error::other("retained trace epoch mismatch preserved"));
        }
        offset += bytes as u64;
    }
    Err(io::Error::other("retained trace validation work limit exceeded"))
}

fn recover_indexes(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(directory(root, None))? {
        let entry = entry?;
        let name = entry.file_name();
        let text = name.to_str().ok_or_else(|| io::Error::other("invalid trace filename"))?;
        if !text.ends_with(".pending") {
            continue;
        }
        let ordinal = ordinal(text, "pending")?;
        let group = match Group::read_at(&entry.path(), ordinal) {
            Ok(group) => group,
            Err(error) => {
                discard_partial_index(root, &entry.path(), ordinal)?;
                // The published roster remains authoritative; only a bounded,
                // recognizably current-format unpublished staging file is gone.
                let _ = error;
                continue;
            }
        };
        match Group::read(root, ordinal) {
            Ok(previous) if previous.header != group.header => {
                return Err(io::Error::other("pending trace index changes a retained identity"));
            }
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        for stream in &group.streams {
            match open(&path(root, *stream, ordinal), false) {
                Ok(file) => {
                    let mut header = [0; SEGMENT_HEADER_BYTES];
                    file.read_exact_at(&mut header, 0)?;
                    if Segment::decode(&header) != Ok(group.header) {
                        return Err(io::Error::other(
                            "pending trace index references unsupported bytes",
                        ));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        fs::rename(entry.path(), index_path(root, ordinal))?;
        File::open(directory(root, None))?.sync_all()?;
    }
    Ok(())
}
impl State {
    pub fn replace(&mut self, before: u64, after: u64) -> io::Result<()> {
        if after >= before {
            self.added(after - before)
        } else {
            self.removed(before - after);
            Ok(())
        }
    }
    pub fn added(&mut self, bytes: u64) -> io::Result<()> {
        self.total = self
            .total
            .checked_add(bytes)
            .ok_or_else(|| io::Error::other("trace disk accounting overflow"))?;
        #[cfg(test)]
        if let Some(free) = self.free.as_mut() {
            *free = free.saturating_sub(bytes);
        }
        Ok(())
    }
    pub fn removed(&mut self, bytes: u64) {
        self.total = self.total.saturating_sub(bytes);
        #[cfg(test)]
        if let Some(free) = self.free.as_mut() {
            *free = free.saturating_add(bytes);
        }
    }
}

pub(super) fn directory_charge(root: &Path, streams: &[Instance]) -> io::Result<u64> {
    let mut bytes = directory_bytes(root)?
        .checked_add(directory_bytes(&directory(root, None))?)
        .ok_or_else(|| io::Error::other("trace directory accounting overflow"))?;
    for stream in streams.iter().filter(|stream| stream.is_some()) {
        bytes = bytes
            .checked_add(directory_bytes(&directory(root, *stream))?)
            .ok_or_else(|| io::Error::other("trace directory accounting overflow"))?;
    }
    Ok(bytes)
}
fn directory_bytes(path: &Path) -> io::Result<u64> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0 => metadata
            .blocks()
            .checked_mul(512)
            .ok_or_else(|| io::Error::other("trace directory accounting overflow")),
        Ok(_) => Err(io::Error::other("trace stream directory must be private")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}
pub(super) fn remove_empty_directory(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn discard_partial_index(root: &Path, pending: &Path, ordinal: u64) -> io::Result<()> {
    let file = open(pending, false)?;
    let length = file.metadata()?.len();
    if length >= INDEX_BYTES as u64 {
        return Err(io::Error::other("unsupported unpublished trace index preserved"));
    }
    let mut bytes = vec![0; length as usize];
    file.read_exact_at(&mut bytes, 0)?;
    let prefix = bytes.len().min(INDEX_MAGIC.len());
    if bytes[..prefix] != INDEX_MAGIC[..prefix] {
        return Err(io::Error::other("unsupported unpublished trace index preserved"));
    }
    match Group::read(root, ordinal) {
        Ok(previous) => {
            let mut header = [0; 56];
            header[..8].copy_from_slice(INDEX_MAGIC);
            header[8..].copy_from_slice(&previous.header.encode());
            let prefix = bytes.len().min(header.len());
            if bytes[..prefix] != header[..prefix] {
                return Err(io::Error::other(
                    "unpublished index changes a retained trace identity",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if bytes.len() >= 56
                && !Segment::decode(&bytes[8..56]).is_ok_and(|header| header.ordinal == ordinal)
            {
                return Err(io::Error::other("unsupported unpublished trace index preserved"));
            }
        }
        Err(error) => return Err(error),
    }
    drop(file);
    fs::remove_file(pending)?;
    File::open(directory(root, None))?.sync_all()
}
