//! Bounded, durable copies across filesystems. Only distribution-shaped bundles
//! (ordinary directories and single-link regular files) cross this boundary.
use super::archive::{MAX_ENTRIES, MAX_EXPANDED, MAX_TREE_ENTRIES};
use crate::engine::{Result, installation};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

pub(super) fn bundle(source: &Path, destination: &Path) -> Result<()> {
    if !fs::symlink_metadata(source)?.is_dir() {
        return Err("Update bundle is not an ordinary directory".into());
    }
    if destination.try_exists()? {
        return Err("Update copy destination already exists".into());
    }
    installation::private_directory(destination)?;
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf())];
    let mut directories = vec![destination.to_path_buf()];
    let mut entries = 1;
    let mut files = 0;
    let mut bytes = 0u64;
    while let Some((source, destination)) = pending.pop() {
        for entry in fs::read_dir(&source)? {
            let entry = entry?;
            entries += 1;
            if entries > MAX_TREE_ENTRIES {
                return Err("Update copy exceeds entry limit".into());
            }
            let target = destination.join(entry.file_name());
            let metadata = entry.path().symlink_metadata()?;
            if metadata.is_dir() {
                fs::DirBuilder::new().mode(0o700).create(&target)?;
                pending.push((entry.path(), target.clone()));
                directories.push(target);
            } else {
                files += 1;
                if files > MAX_ENTRIES {
                    return Err("Update copy exceeds file limit".into());
                }
                let copied = file(&entry.path(), &target, MAX_EXPANDED - bytes)?;
                bytes += copied;
            }
        }
    }
    for directory in directories.iter().rev() {
        File::open(directory)?.sync_all()?;
    }
    File::open(destination.parent().ok_or("Missing update copy parent")?)?.sync_all()?;
    Ok(())
}

pub(super) fn file(source: &Path, destination: &Path, limit: u64) -> Result<u64> {
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source)?;
    let metadata = input.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > limit {
        return Err("Unsafe or oversized update copy source".into());
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(destination)?;
    let mut bytes = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if bytes > limit || bytes > metadata.len() {
            return Err("Update copy source changed or exceeded its limit".into());
        }
        output.write_all(&buffer[..count])?;
    }
    if bytes != metadata.len() {
        return Err("Incomplete update copy".into());
    }
    output.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o777))?;
    output.sync_all()?;
    Ok(bytes)
}

#[cfg(test)]
#[path = "copy_tests.rs"]
mod copy_tests;
