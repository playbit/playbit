//! Admit the small, symlink-free distribution ZIP before creating any files.
use crate::engine::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

pub const MAX_ZIP: u64 = 256 * 1024 * 1024;
pub const MAX_EXPANDED: u64 = 512 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 256;
// ZIP entries may omit their parent directories. Each admitted path has at
// most 16 components, so its extracted tree can contain this many nodes.
pub const MAX_TREE_ENTRIES: usize = MAX_ENTRIES * 16;
const MAX_DIRECTORY: u64 = 1024 * 1024;

// ZipArchive loads its directory before callers can inspect len(). Bound that
// allocation first. The distribution format uses ordinary ZIP, never ZIP64,
// split archives, prepended executables or encrypted entries.
fn admit(file: &mut File) -> Result<usize> {
    let length = file.metadata()?.len();
    if !(22..=MAX_ZIP).contains(&length) {
        return Err("Invalid update ZIP size".into());
    }
    let tail_length = length.min(65557);
    file.seek(SeekFrom::End(-(tail_length as i64)))?;
    let mut tail = vec![0; tail_length as usize];
    file.read_exact(&mut tail)?;
    let offset = (0..=tail.len() - 22)
        .rev()
        .find(|&i| {
            tail[i..].starts_with(b"PK\x05\x06")
                && i + 22 + u16::from_le_bytes([tail[i + 20], tail[i + 21]]) as usize == tail.len()
        })
        .ok_or("Missing update ZIP directory")?;
    let footer = &tail[offset..];
    let word = |i| u16::from_le_bytes([footer[i], footer[i + 1]]) as u64;
    let dword =
        |i| u32::from_le_bytes(footer[i..i + 4].try_into().expect("ZIP footer field")) as u64;
    let count = word(10);
    let directory_size = dword(12);
    let directory_start = dword(16);
    if word(4) != 0
        || word(6) != 0
        || word(8) != count
        || count == 0
        || count > MAX_ENTRIES as u64
        || directory_size > MAX_DIRECTORY
        || directory_start + directory_size != length - tail_length + offset as u64
    {
        return Err("Unsupported or oversized update ZIP directory".into());
    }
    // Reject a ZIP64 locator even if a forged classic footer advertises small
    // counts. Otherwise ZipArchive could allocate from the ZIP64 count first.
    if offset >= 20 && tail[offset - 20..].starts_with(b"PK\x06\x07") {
        return Err("ZIP64 updates are unsupported".into());
    }
    admit_directory(file, directory_start, directory_size, count)?;
    file.seek(SeekFrom::Start(0))?;
    let mut signature = [0; 4];
    file.read_exact(&mut signature)?;
    if signature != *b"PK\x03\x04" {
        return Err("Prepended update ZIP content".into());
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(count as usize)
}

pub fn extract(archive: &Path, directory: &Path) -> Result<()> {
    let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(archive)?;
    let count = admit(&mut file)?;
    let mut zip = zip::ZipArchive::new(file)?;
    if zip.len() != count {
        return Err("Duplicate or inconsistent update ZIP entries".into());
    }
    admit_entries(&mut zip)?;
    unpack(&mut zip, directory)
}

fn admit_entries(zip: &mut zip::ZipArchive<File>) -> Result<()> {
    let mut paths = BTreeSet::new();
    let mut spelling = BTreeMap::new();
    let mut files = BTreeSet::new();
    let mut expanded = 0u64;
    // No extraction takes place until every path and declared size is checked.
    for index in 0..zip.len() {
        let entry = zip.by_index(index)?;
        let name = entry.name();
        let components: Vec<_> = name.trim_end_matches('/').split('/').collect();
        let mode = entry.unix_mode().unwrap_or(0);
        let kind = mode & libc::S_IFMT as u32;
        if name.len() > 1024
            || !name.is_ascii()
            || name.contains(['\\', '\0'])
            || components.len() > 16
            || components.first() != Some(&"Playbit.app")
            || components.iter().any(|v| v.is_empty() || *v == "." || *v == "..")
            || entry.enclosed_name().is_none()
            || entry.encrypted()
            || (kind != 0 && kind != libc::S_IFDIR as u32 && kind != libc::S_IFREG as u32)
            || (kind == libc::S_IFDIR as u32 && !entry.is_dir())
            || (kind == libc::S_IFREG as u32 && entry.is_dir())
            || !matches!(
                entry.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            )
            || !paths.insert(name.trim_end_matches('/').to_ascii_lowercase())
        {
            return Err("Unsafe or duplicate update ZIP entry".into());
        }
        let mut prefix = String::new();
        for component in components {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let folded = prefix.to_ascii_lowercase();
            if spelling.get(&folded).is_some_and(|existing| existing != &prefix) {
                return Err("Colliding update ZIP path components".into());
            }
            spelling.insert(folded, prefix.clone());
        }
        if !entry.is_dir() {
            files.insert(name.to_ascii_lowercase());
        }
        expanded = expanded.checked_add(entry.size()).ok_or("Update ZIP size overflow")?;
        if expanded > MAX_EXPANDED {
            return Err("Expanded update exceeds limit".into());
        }
    }
    for file in files {
        let prefix = format!("{file}/");
        if paths.iter().any(|path| path.starts_with(&prefix)) {
            return Err("Update ZIP file collides with a directory".into());
        }
    }
    Ok(())
}

fn unpack(zip: &mut zip::ZipArchive<File>, directory: &Path) -> Result<()> {
    crate::engine::installation::private_directory(directory)?;
    let mut directories = BTreeSet::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let target = directory.join(entry.enclosed_name().ok_or("Unsafe update path")?);
        for parent in target.ancestors().skip(1).take_while(|p| p.starts_with(directory)) {
            directories.insert(parent.to_path_buf());
        }
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            directories.insert(target);
            continue;
        }
        fs::create_dir_all(target.parent().ok_or("Missing ZIP parent")?)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&target)?;
        let declared = entry.size();
        let executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
        let mut count = 0u64;
        loop {
            let n = entry.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            count += n as u64;
            total += n as u64;
            if count > declared || total > MAX_EXPANDED {
                return Err("Update ZIP expanded beyond its declared size".into());
            }
            output.write_all(&buffer[..n])?;
        }
        if count != declared {
            return Err("Incomplete update ZIP entry".into());
        }
        output.set_permissions(fs::Permissions::from_mode(if executable {
            0o755
        } else {
            0o644
        }))?;
        output.sync_all()?;
    }
    for directory in directories.iter().rev() {
        File::open(directory)?.sync_all()?;
    }
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn admit_directory(
    file: &mut File,
    directory_start: u64,
    directory_size: u64,
    count: u64,
) -> Result<()> {
    file.seek(SeekFrom::Start(directory_start))?;
    let mut directory = vec![0; directory_size as usize];
    file.read_exact(&mut directory)?;
    let mut remaining = directory.as_slice();
    for _ in 0..count {
        if remaining.len() < 46 || !remaining.starts_with(b"PK\x01\x02") {
            return Err("Invalid update ZIP directory entry".into());
        }
        let word = |i| u16::from_le_bytes([remaining[i], remaining[i + 1]]) as usize;
        let record = 46 + word(28) + word(30) + word(32);
        if record > remaining.len() {
            return Err("Truncated update ZIP directory".into());
        }
        remaining = &remaining[record..];
    }
    if !remaining.is_empty() {
        return Err("Update ZIP directory count mismatch".into());
    }
    Ok(())
}
