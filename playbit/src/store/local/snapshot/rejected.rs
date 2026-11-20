//! Recovery archives belong to the immutable cut, with current Read filtering.
use super::*;
use crate::store::EntryId;
use crate::store::local::received::rejected::{RejectedCut, cuts};
use crate::store::volume_index::EntryChange;
use std::collections::BTreeSet;

impl VolumeSnapshot {
    /// Only entries admitted during capture can access the retained cut.
    pub fn with_rejected_database<T>(
        &self,
        operation: Id,
        entry: EntryId,
        f: impl FnOnce(kvdb::ReadTxn<'_, RedbStorage>) -> Result<T>,
    ) -> Result<T> {
        let (cut, directory) = self.rejected_cut(operation, entry)?;
        let file = DatabaseId::Entry(entry).file_name();
        if !cut.files.contains_key(&file) {
            return Err(Error::NotFound("snapshot rejected backing"));
        }
        let database = open_replica(&directory.join("rejected").join(file), CACHE_BYTES)?;
        f(database.begin_read()?)
    }

    pub fn open_rejected_blob(&self, operation: Id, version: &BlobVersion) -> Result<fs::File> {
        if version.address.volume != self.volume {
            return Err(Error::AccessDenied("snapshot blob volume"));
        }
        let (cut, directory) = self.rejected_cut(operation, version.address.entry)?;
        let index = open_replica(&directory.join("rejected/index.kvdb"), CACHE_BYTES)?;
        let retained = IndexSnapshot::new(index.begin_read()?);
        let (_, kind) =
            retained.retained_blob_kind(version.address.entry, Some(version.revision))?;
        if !matches!(kind, EntryKind::Blob { len_bytes, digest, encrypted_key, .. }
            if len_bytes == version.len_bytes && digest == version.digest
                && encrypted_key == version.encrypted_key)
        {
            return Err(Error::AccessDenied("snapshot rejected blob version"));
        }
        let name = format!("{}.blob", hex(&version.digest));
        if !cut.files.contains_key(&name) {
            return Err(Error::NotFound("snapshot rejected blob"));
        }
        fs::File::open(directory.join("rejected").join(name)).map_err(io)
    }

    fn rejected_cut(&self, operation: Id, entry: EntryId) -> Result<(RejectedCut, PathBuf)> {
        let manifest = self.manifest()?;
        let cut: RejectedCut = get(&manifest, format!("rejected/{operation}").as_bytes())?
            .ok_or(Error::AccessDenied("snapshot rejected cut"))?;
        if cut.volume != self.volume || cut.operation != operation {
            return Err(Error::corrupt(
                "snapshot rejected cut does not match its volume or operation",
            ));
        }
        if !cut.entries.contains(&entry) {
            return Err(Error::AccessDenied("snapshot rejected entry"));
        }
        let directory = self.directory.0.join(crate::paths::RECEIVED).join(operation.to_string());
        cut.verify(&directory)?;
        Ok((cut, directory))
    }
}

impl LocalStore {
    pub(super) fn capture_rejected(&self, capture: &Capture, access: &Access) -> Result<()> {
        let index = open_replica(
            &capture.snapshot.directory.0.join(DatabaseId::Index.file_name()),
            CACHE_BYTES,
        )?;
        let current = IndexSnapshot::new(index.begin_read()?);
        let parent = capture.snapshot.directory.0.join(crate::paths::RECEIVED);
        fs::DirBuilder::new().mode(0o700).create(&parent).map_err(io)?;
        for mut cut in cuts(&index)? {
            capture.cut.check()?;
            if cut.volume != capture.snapshot.volume {
                return Err(Error::corrupt("rejected cut belongs to a different snapshot volume"));
            }
            let source = self.path.join(crate::paths::RECEIVED).join(cut.operation.to_string());
            let original = open_replica(&source.join("rejected/index.kvdb"), CACHE_BYTES)?;
            cut.verify_index(&original)?;
            let retained = IndexSnapshot::new(original.begin_read()?);
            let mut entries = Vec::new();
            for entry in &cut.entries {
                let address = EntryAddress { volume: cut.volume, entry: *entry };
                match self.authorize_rejected_read(access, address, &current, &retained) {
                    Ok(()) => entries.push(*entry),
                    Err(Error::AccessDenied(_)) => {}
                    Err(error) => return Err(error),
                }
                capture.cut.check()?;
            }
            if entries.is_empty() {
                continue;
            }
            cut.entries = entries;
            let files = retained_files(&cut, &retained)?;
            cut.files.retain(|name, _| files.contains(name));
            copy_cut(capture, &source, &parent, &cut)?;
            put(&capture.manifest, format!("rejected/{}", cut.operation).as_bytes(), &cut)?;
        }
        initialization::sync_directory(&parent)?;
        capture.cut.check()
    }
}

fn retained_files(
    cut: &RejectedCut,
    index: &IndexSnapshot<'_, RedbStorage>,
) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::from(["index.kvdb".to_owned()]);
    for entry in &cut.entries {
        let backing = DatabaseId::Entry(*entry).file_name();
        if cut.files.contains_key(&backing) {
            files.insert(backing);
        }
        let mut after = None;
        loop {
            let page = index.revisions(*entry, after, 64)?;
            for revision in page.revisions {
                let kind = match revision.change {
                    EntryChange::Created { info } => Some(info.kind),
                    EntryChange::Content { kind } => Some(kind),
                    _ => None,
                };
                if let Some(EntryKind::Blob { digest, .. }) = kind {
                    let name = format!("{}.blob", hex(&digest));
                    if cut.files.contains_key(&name) {
                        files.insert(name);
                    }
                }
            }
            let Some(next) = page.next else { break };
            after = Some(next);
        }
    }
    Ok(files)
}

fn copy_cut(capture: &Capture, source: &Path, parent: &Path, cut: &RejectedCut) -> Result<()> {
    let directory = parent.join(cut.operation.to_string());
    fs::DirBuilder::new().mode(0o700).create(&directory).map_err(io)?;
    let archive = directory.join("rejected");
    fs::DirBuilder::new().mode(0o700).create(&archive).map_err(io)?;
    let mut remaining = cut.bytes;
    for name in cut.files.keys() {
        let mut file = fs::File::open(source.join("rejected").join(name)).map_err(io)?;
        remaining = remaining
            .checked_sub(file.metadata().map_err(io)?.len())
            .ok_or(Error::LimitExceeded)?;
        copy_file(&mut file, &archive.join(name), &capture.cut)?;
    }
    cut.verify(&directory)?;
    let mut manifest = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("rejected.json"))
        .map_err(io)?;
    manifest.write_all(&encode(cut)?).map_err(io)?;
    manifest.sync_all().map_err(io)?;
    initialization::sync_directory(&archive)?;
    initialization::sync_directory(&directory)?;
    capture.cut.check()
}
