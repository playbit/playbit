//! Apple filesystem projection of the volume namespace. Names are presentation;
//! persistent entry identities and ordinary store authorization own every action.
#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
use super::volume_index::EntrySummary;
use super::{DatabaseReference, EntryId, EntryType, Error, Id, Result};
use serde::{Deserialize, Serialize};
#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
use std::collections::BTreeMap;

pub const PROVIDER_PAGE_ITEMS: usize = 128;
pub const PROVIDER_VOLUME_LIMIT: usize = 256;
// Includes sixfold JSON escaping of every maximum-length catalog name.
pub const PROVIDER_REPLY_BYTES: usize = 512 * 1024;
pub const PROVIDER_ROOT_ID: &str = "NSFileProviderRootContainerItemIdentifier";

pub enum ProviderChange<'a> {
    Move { parent: Option<EntryId>, name: &'a str },
    Delete { recursive: bool },
}
#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
const COMPONENT_BYTES: usize = 255;
#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
const SUFFIX_BYTES: usize = 1 + 21 * 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderItem {
    pub id: EntryId,
    pub parent: Option<EntryId>,
    pub filename: String,
    pub kind: EntryType,
    pub len_bytes: u64,
    pub readable: bool,
    pub writable: bool,
    /// Exact reference/namespace and, for blobs, immutable body are durable at AP.
    pub uploaded: bool,
    pub content_version: [u8; 32],
    pub metadata_version: [u8; 32],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderPage {
    pub items: Vec<ProviderItem>,
    pub next: Option<EntryId>,
}

/// Account-domain presentation. The raw store namespace keeps EntryId; only
/// the provider boundary combines its volume and entry identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderNamespaceItem {
    pub id: String,
    pub parent: String,
    pub role: String,
    pub filename: String,
    pub kind: EntryType,
    pub len_bytes: u64,
    pub readable: bool,
    pub writable: bool,
    pub uploaded: bool,
    pub unavailable: bool,
    pub content_version: [u8; 32],
    pub metadata_version: [u8; 32],
}

#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
#[derive(Default)]
pub(crate) struct ProviderVolumeStatus {
    pub available: bool,
    pub uploaded: bool,
}

pub fn provider_entry_identifier(volume: Id, entry: EntryId) -> String {
    let mut bytes = [0; 31];
    bytes[..16].copy_from_slice(volume.as_bytes());
    bytes[16..].copy_from_slice(entry.as_bytes());
    crate::identifier::encode(&bytes)
}

impl ProviderItem {
    pub fn in_account_domain(self, volume: Id) -> ProviderNamespaceItem {
        ProviderNamespaceItem {
            id: provider_entry_identifier(volume, self.id),
            parent: self.parent.map_or_else(
                || volume.to_string(),
                |parent| provider_entry_identifier(volume, parent),
            ),
            role: "entry".into(),
            filename: self.filename,
            kind: self.kind,
            len_bytes: self.len_bytes,
            readable: self.readable,
            writable: self.writable,
            uploaded: self.uploaded,
            unavailable: false,
            content_version: self.content_version,
            metadata_version: self.metadata_version,
        }
    }
}

/// Labels are not entry paths: escape separators, controls and the escape
/// alphabet before applying the same collision policy as ordinary entries.
#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
pub(crate) fn volume_filenames(volumes: &[super::Volume]) -> Result<BTreeMap<Id, String>> {
    if volumes.len() > PROVIDER_VOLUME_LIMIT {
        return Err(Error::LimitExceeded);
    }
    let mut counts = BTreeMap::<String, usize>::new();
    let mut candidates = Vec::with_capacity(volumes.len());
    for volume in volumes {
        let mut escaped = String::with_capacity(volume.name.len());
        for character in volume.name.chars() {
            match character {
                '%' => escaped.push_str("%25"),
                '~' => escaped.push_str("%7E"),
                '/' => escaped.push_str("%2F"),
                ':' => escaped.push_str("%3A"),
                c if c.is_control() => {
                    use std::fmt::Write;
                    for byte in c.encode_utf8(&mut [0; 4]).bytes() {
                        write!(&mut escaped, "%{byte:02X}").expect("write to String");
                    }
                }
                c => escaped.push(c),
            }
        }
        *counts.entry(escaped.to_ascii_lowercase()).or_default() += 1;
        let escaped_label = escaped != volume.name;
        candidates.push((volume.id, escaped, escaped_label));
    }
    let mut result = BTreeMap::new();
    for (id, mut name, escaped_label) in candidates {
        if escaped_label
            || !name.is_ascii()
            || name.len() > COMPONENT_BYTES
            || name == "."
            || name == ".."
            || counts[&name.to_ascii_lowercase()] > 1
        {
            let suffix = volume_filename_suffix(id);
            let mut end = name.len().min(COMPONENT_BYTES - 1 - suffix.len());
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            // A generated suffix must never leave an incomplete percent escape.
            if let Some(percent) = name[..end].rfind('%')
                && end - percent < 3
            {
                end = percent;
            }
            name.truncate(end);
            name.push('~');
            name.push_str(&suffix);
        }
        result.insert(id, name);
    }
    Ok(result)
}

#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
fn volume_filename_suffix(id: Id) -> String {
    let mut suffix = String::new();
    for byte in id.to_string().bytes() {
        if byte.is_ascii_uppercase() {
            use std::fmt::Write;
            write!(&mut suffix, "%{byte:02X}").expect("write to String");
        } else {
            suffix.push(char::from(byte));
        }
    }
    suffix
}

/// Only our exact identity suffix makes a generated name recognizable. A
/// newly supplied ordinary filename remains a literal label, including '%'.
#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
pub(crate) fn volume_label_from_filename(volume: Id, name: &str) -> Result<String> {
    let suffix = format!("~{}", volume_filename_suffix(volume));
    let Some(stem) = name.strip_suffix(&suffix) else { return Ok(name.to_owned()) };
    let mut decoded = Vec::with_capacity(stem.len());
    let bytes = stem.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let encoded = bytes
                .get(index + 1..index + 3)
                .ok_or(Error::InvalidInput("invalid provider volume filename escape"))?;
            let digit = |byte: u8| -> Option<u8> {
                match byte {
                    b'0'..=b'9' => Some(byte - b'0'),
                    b'A'..=b'F' => Some(byte - b'A' + 10),
                    _ => None,
                }
            };
            let high = digit(encoded[0])
                .ok_or(Error::InvalidInput("invalid provider volume filename escape"))?;
            let low = digit(encoded[1])
                .ok_or(Error::InvalidInput("invalid provider volume filename escape"))?;
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| Error::InvalidInput("invalid provider volume label UTF-8"))
}

#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
pub(crate) fn volume_namespace_item(
    volume: &super::Volume,
    filename: String,
    status: ProviderVolumeStatus,
) -> ProviderNamespaceItem {
    let mut metadata = blake3::Hasher::new();
    metadata.update(volume.id.as_bytes());
    metadata.update(volume.name.as_bytes());
    metadata.update(filename.as_bytes());
    metadata.update(&[status.available as u8]);
    ProviderNamespaceItem {
        id: volume.id.to_string(),
        parent: PROVIDER_ROOT_ID.into(),
        role: "volume".into(),
        filename,
        kind: EntryType::Directory,
        len_bytes: 0,
        readable: true,
        writable: true,
        uploaded: status.uploaded,
        unavailable: !status.available,
        content_version: *blake3::hash(volume.id.as_bytes()).as_bytes(),
        metadata_version: *metadata.finalize().as_bytes(),
    }
}

/// Allocate a deterministic, injective filename for each visible sibling. ASCII
/// collision groups share a suffix policy. Unicode names always carry identity,
/// making the result independent of filesystem case and normalization tables.
#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
pub(crate) fn filenames(entries: &[EntrySummary]) -> Result<BTreeMap<EntryId, String>> {
    if entries.len() > 16 * 1024 {
        return Err(Error::LimitExceeded);
    }
    let mut candidates = Vec::with_capacity(entries.len());
    let mut counts = BTreeMap::<String, usize>::new();
    for entry in entries {
        let escaped = entry.name.replace('~', "~~");
        let (stem, extension) = if database(entry.kind) {
            (escaped, ".pbdb".to_owned())
        } else if entry.kind == EntryType::Blob {
            match escaped.rfind('.') {
                Some(dot) if dot > 0 && escaped.len() - dot <= 32 => {
                    (escaped[..dot].to_owned(), escaped[dot..].to_owned())
                }
                _ => (escaped, String::new()),
            }
        } else {
            (escaped, String::new())
        };
        let candidate = format!("{stem}{extension}");
        *counts.entry(candidate.to_ascii_lowercase()).or_default() += 1;
        candidates.push((entry, stem, extension, candidate));
    }
    let mut result = BTreeMap::new();
    for (entry, mut stem, extension, candidate) in candidates {
        let needs_suffix = !candidate.is_ascii()
            || candidate.len() > COMPONENT_BYTES
            || candidate == "."
            || candidate == ".."
            || counts[&candidate.to_ascii_lowercase()] > 1;
        let filename = if needs_suffix {
            let budget = COMPONENT_BYTES - SUFFIX_BYTES - extension.len();
            let mut end = stem.len().min(budget);
            while !stem.is_char_boundary(end) {
                end -= 1;
            }
            stem.truncate(end);
            // Never leave half an escaped tilde at the suffix boundary.
            if stem.ends_with('~') && stem.bytes().rev().take_while(|b| *b == b'~').count() % 2 == 1
            {
                stem.pop();
            }
            // Percent-escape uppercase ID letters so distinct canonical base58
            // spellings remain distinct on case-insensitive Apple filesystems.
            let mut suffix = String::new();
            for byte in entry.id.to_string().bytes() {
                if byte.is_ascii_uppercase() {
                    use std::fmt::Write;
                    write!(&mut suffix, "%{byte:02X}").expect("write to String");
                } else {
                    suffix.push(char::from(byte));
                }
            }
            format!("{stem}~{suffix}{extension}")
        } else {
            candidate
        };
        result.insert(entry.id, filename);
    }
    Ok(result)
}

pub(crate) fn database(kind: EntryType) -> bool {
    matches!(kind, EntryType::KvDb | EntryType::ObjectDb)
}

impl ProviderItem {
    pub fn database_reference(&self, volume: super::Id) -> Result<DatabaseReference> {
        if !database(self.kind) {
            return Err(Error::InvalidInput("entry is not a database"));
        }
        Ok(DatabaseReference { volume, entry: self.id })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert filesystem projection outcomes.")]
mod tests {
    use super::*;

    #[test]
    fn account_projection_preserves_volume_entry_identity_and_safe_volume_labels() {
        // Project colliding and filesystem-invalid labels without merging
        // identities, and decode the same raw entry in two independent volumes.
        let owner = Id::from_bytes([1; 16]);
        let labels =
            ["Documents", "A/B", "A%2FB", "Nul\0label", "Name", "name", ".", "é", "e\u{301}"];
        let volumes = labels
            .iter()
            .enumerate()
            .map(|(index, name)| super::super::Volume {
                id: Id::from_bytes([index as u8 + 2; 16]),
                name: (*name).into(),
                owner,
                personal: true,
                share_key_with_service: true,
            })
            .collect::<Vec<_>>();
        let names = volume_filenames(&volumes).unwrap();
        let folded = names
            .values()
            .map(|name| name.to_lowercase())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(folded.len(), volumes.len());
        assert_eq!(names[&volumes[0].id], "Documents");
        for volume in &volumes {
            let filename = &names[&volume.id];
            assert!(
                filename.len() <= COMPONENT_BYTES
                    && !filename.contains('/')
                    && !filename.contains('\0')
            );
            assert_eq!(volume_label_from_filename(volume.id, filename).unwrap(), volume.name);
        }
        let entry = EntryId::from_bytes([4; 15]).unwrap();
        let first = provider_entry_identifier(volumes[0].id, entry);
        let second = provider_entry_identifier(volumes[1].id, entry);
        assert_ne!(first, second);
        let decoded = crate::identifier::decode::<31>(&first).unwrap();
        assert_eq!(&decoded[..16], volumes[0].id.as_bytes());
        assert_eq!(&decoded[16..], entry.as_bytes());
    }

    #[test]
    fn filenames_preserve_ordinary_names_and_disambiguate_apple_filesystem_collisions() {
        // Exercise db/blob, case, Unicode normalization, escape and length collisions together.
        let entries = [
            ("notes", EntryType::KvDb),
            ("notes.pbdb", EntryType::Blob),
            ("Photo.jpg", EntryType::Blob),
            ("photo.jpg", EntryType::Blob),
            ("report.txt", EntryType::Blob),
            ("é", EntryType::Blob),
            ("e\u{301}", EntryType::Blob),
            ("literal~name", EntryType::Blob),
            (&"界".repeat(85), EntryType::ObjectDb),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (name, kind))| EntrySummary {
            id: EntryId::from_bytes([index as u8 + 1; 15]).unwrap(),
            parent: None,
            name: name.to_owned(),
            kind,
            protocol: None,
            deleted: false,
            name_conflict: false,
        })
        .collect::<Vec<_>>();
        let mut entries = entries;
        for text in ["11111111111111A", "11111111111111a"] {
            let mut collision = entries[0].clone();
            collision.id = EntryId::parse(text).unwrap();
            entries.push(collision);
        }
        let names = filenames(&entries).unwrap();
        let folded = names
            .values()
            .map(|name| name.to_lowercase())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(folded.len(), entries.len());
        assert_eq!(names[&entries[4].id], "report.txt");
        assert_eq!(names[&entries[7].id], "literal~~name");
        assert!(names[&entries[0].id].ends_with(".pbdb"));
        assert!(names.values().all(|name| name.len() <= COMPONENT_BYTES));
        let mut reversed = entries.clone();
        reversed.reverse();
        assert_eq!(filenames(&reversed).unwrap(), names);
    }
}
