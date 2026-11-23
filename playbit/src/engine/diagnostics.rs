//! Bounded `pb` diagnostic work on the existing background lane, never the UI owner.
//! Native IPC handles local volume copies on its bounded preparation worker.
//! Other requests and embedded management use this account background lane.
use super::reply::DiagnosticReply;
use super::*;

pub(crate) fn is_request(request: &Request) -> bool {
    matches!(
        request,
        Request::DuplicateVolume { .. }
            | Request::DuplicateEntry { .. }
            | Request::Usage { .. }
            | Request::PayloadSize { .. }
            | Request::FileRead { .. }
            | Request::FileWrite { .. }
            | Request::FileCreate { .. }
            | Request::FileCreateIn { .. }
            | Request::RecoverEntry { .. }
            | Request::RejectedEntries { .. }
    )
}

pub(super) fn execute(
    request: Request,
    remote: &mut remote::Remote,
    store: &Arc<LocalStore>,
) -> Result<DiagnosticReply> {
    let _timing = crate::trace::Span::new("diagnostics.execute");
    Ok(match request {
        request @ (Request::DuplicateVolume { .. } | Request::DuplicateEntry { .. }) => {
            copy(request, store)?
        }
        Request::Usage { volume } => DiagnosticReply::Usage(remote.usage(store, volume)?),
        request @ Request::PayloadSize { volume, entry, .. } => {
            let access = request.management_access(store)?;
            payload_size(store, &access, volume, entry, &std::sync::atomic::AtomicBool::new(false))?
        }
        _ => execute_file(request, store, &remote.blob_transport())?,
    })
}

pub(super) fn payload_size(
    store: &LocalStore,
    access: &Access,
    volume: Id,
    entry: Option<crate::store::EntryId>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<DiagnosticReply> {
    let bytes = store.payload_size(access, volume, entry, cancelled)?;
    Ok(DiagnosticReply::PayloadSize { volume, entry, bytes })
}

pub(super) fn copy(request: Request, store: &LocalStore) -> Result<DiagnosticReply> {
    Ok(match request {
        Request::DuplicateVolume { volume } => DiagnosticReply::VolumeDuplicated {
            source: volume,
            volume: store.duplicate_volume(volume)?,
        },
        Request::DuplicateEntry { address } => DiagnosticReply::EntryDuplicated {
            source: address,
            entry: store.duplicate_entry(&Access::owner(store.account().id), address)?,
        },
        _ => return Err("not a copy request".into()),
    })
}

fn execute_file(
    request: Request,
    store: &Arc<LocalStore>,
    blobs: &blob::BlobTransport,
) -> Result<DiagnosticReply> {
    let access = request.management_access(store)?;
    Ok(match request {
        Request::RejectedEntries { volume, after, limit } => DiagnosticReply::RejectedEntries {
            volume,
            after,
            page: store.rejected_entries(
                &Access::owner(store.account().id),
                volume,
                after,
                limit as usize,
            )?,
        },
        Request::RecoverEntry { address, name, revision, rejected_cut } => {
            if revision.is_some() && rejected_cut.is_some() {
                return Err("choose a retained revision or a rejected cut".into());
            }
            let access = Access::owner(store.account().id);
            let entry = if let Some(cut) = rejected_cut {
                store.recover_rejected_entry(&access, address, &name, cut)?
            } else {
                store.recover_entry(&access, address, &name, revision)?
            };
            DiagnosticReply::EntryRecovered { source: address, name, revision, rejected_cut, entry }
        }
        Request::FileCreate { volume, name, path, .. } => {
            let mut source = read_import(&path)?;
            let entry = store.create_blob_from_reader(&access, volume, &name, &mut source)?;
            DiagnosticReply::FileCreated {
                address: crate::store::volume_index::EntryAddress { volume, entry: entry.id },
                name,
                entry,
            }
        }
        Request::FileCreateIn { volume, parent, name, path, .. } => {
            let mut source = read_import(&path)?;
            let entry =
                store.create_blob_from_reader_in(&access, volume, parent, &name, &mut source)?;
            DiagnosticReply::FileCreatedIn {
                parent,
                address: crate::store::volume_index::EntryAddress { volume, entry: entry.id },
                name,
                entry,
            }
        }
        Request::FileWrite { address, path } => {
            let mut source = read_import(&path)?;
            let entry = store.write_blob_from_reader(
                &Access::owner(store.account().id),
                address.volume,
                address.entry,
                &mut source,
            )?;
            DiagnosticReply::FileWritten { address, entry }
        }
        Request::FileRead { session, address, path } => {
            let access = session.as_deref().map_or_else(
                || Access::owner(store.account().id),
                |id| Access::session(store.account().id, id),
            );
            let (version, file) =
                blobs.verified_file(store.clone(), access.clone(), session, address)?;
            export_blob(store, &access, &version, &file, &path)?;
            DiagnosticReply::FileRead { address, bytes: version.len_bytes }
        }
        _ => return Err("not a diagnostic request".into()),
    })
}

struct ExportStage(std::path::PathBuf);
impl Drop for ExportStage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn export_blob(
    store: &LocalStore,
    access: &Access,
    version: &crate::store::volume_index::BlobVersion,
    blob: &crate::store::VerifiedBlob,
    destination: &Path,
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent =
        destination.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let stage = ExportStage(parent.join(format!(".playbit-export-{}", Id::generate()?)));
    let mut file =
        fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&stage.0)?;
    let mut offset = 0;
    while offset < blob.len() {
        if store.blob_version(access, version.address, None)? != *version {
            return Err("file content changed during export".into());
        }
        let bytes = blob.read_range(offset, crate::store::BLOB_CHUNK_BYTES as u32)?;
        file.write_all(&bytes)?;
        offset += bytes.len() as u64;
    }
    file.sync_all()?;
    if store.blob_version(access, version.address, None)? != *version {
        return Err("file content changed during export".into());
    }
    fs::rename(&stage.0, destination)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Check the size of the open input, then let private staging enforce
/// the ceiling again in case another process changes the file while importing.
fn read_import(path: &Path) -> Result<fs::File> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > crate::engine::blob::MAX_FILE_BYTES {
        return Err("file exceeds development transfer limit".into());
    }
    Ok(file)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn oversized_file_write_is_refused_without_reading_the_whole_file() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let volume = engine.services.store.create_volume("Files", false).unwrap();
        let path = directory.path().join("oversized");
        // A sparse file costs no disk; only the bounded read is measured here.
        fs::File::create(&path).unwrap().set_len(crate::engine::blob::MAX_FILE_BYTES + 1).unwrap();
        let mut remote = engine.remote.take().unwrap();
        let error = execute(
            Request::FileWrite {
                address: crate::store::volume_index::EntryAddress {
                    volume: volume.id,
                    entry: crate::store::EntryId::generate().unwrap(),
                },
                path,
            },
            &mut remote,
            &engine.services.store,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "file exceeds development transfer limit");
        assert_eq!(engine.services.store.volume(volume.id).unwrap(), volume);
    }
}
