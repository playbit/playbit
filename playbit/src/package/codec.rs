//! Canonical bundle index and retained immutable backing. Parsing and hashing
//! use bounded forward reads; file payloads are never retained by the index.
use super::{AppIdentity, MAX_BUNDLE_BYTES, MAX_NAME_BYTES, Result, signature, trace_catalog};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
};

pub const MAGIC: &[u8; 8] = b"PBAPP\0\0\0";
pub const HEADER_BYTES: usize = 32;
pub const MAX_INDEX_BYTES: usize = 1024 * 1024;
pub const MAX_ENTRIES: usize = 1024;
pub const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_WASM_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_ICON_BYTES: usize = 1024 * 1024;
pub const MANIFEST: &str = "app/manifest.json";
pub const EXECUTABLE: &str = "app/main.wasm";
pub const INITIALIZERS: &str = "app/volumes.json";
pub const ICON: &str = "app/icon";
pub const SIGNATURE: &str = "app/signature";
pub const UPGRADE_REQUIRED: &str =
    "This app needs to be rebuilt for this version of Playbit. Its saved data has been kept.";
pub const MAX_DISPLAY_MANIFEST_BYTES: usize = 16 * 1024;
pub const MAX_REQUIRED_HOST_FEATURES: usize = 64;
const CHUNK: usize = 64 * 1024;
const _: () = assert!(MAX_ENTRIES * (24 + 512 + 7) < MAX_INDEX_BYTES);
const _: () = assert!(MAX_FILE_BYTES as u64 <= playbit_api::abi::files::MAX_FILE_READ_EXTENT);

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub offset: u64,
    pub length: u64,
}
#[derive(Clone, Debug)]
pub struct Index {
    pub entries: Vec<Entry>,
    pub length: u64,
    pub payload: u64,
}
#[derive(Serialize, Deserialize)]
struct DisplayManifest {
    app_id: String,
    name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required_host_features: Vec<String>,
}

/// Backings outlive every pending File read. Callers publish files atomically
/// and never mutate a published package in place.
pub trait Backing: Send + Sync {
    fn read_at(&self, offset: u64, bytes: &mut [u8]) -> std::io::Result<()>;
}
impl Backing for Arc<[u8]> {
    fn read_at(&self, offset: u64, bytes: &mut [u8]) -> std::io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        let end = start.checked_add(bytes.len()).ok_or(std::io::ErrorKind::InvalidInput)?;
        bytes.copy_from_slice(self.get(start..end).ok_or(std::io::ErrorKind::UnexpectedEof)?);
        Ok(())
    }
}
#[cfg(unix)]
impl Backing for std::fs::File {
    fn read_at(&self, offset: u64, bytes: &mut [u8]) -> std::io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, bytes, offset)
    }
}
#[derive(Clone)]
pub struct Package {
    app_id: String,
    name: String,
    required_host_features: Vec<String>,
    index: Arc<Index>,
    backing: Arc<dyn Backing>,
    digest: [u8; 32],
    trace: Option<Arc<[u8]>>,
    signature: Option<signature::PackageSignature>,
    content_digest: [u8; 32],
    identity: Option<AppIdentity>,
    initializers: super::VolumeInitializers,
}
impl fmt::Debug for Package {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Package").field("name", &self.name).field("index", &self.index).finish()
    }
}
fn align(n: u64) -> Result<u64> {
    Ok(n.checked_add(7).ok_or("bundle offset overflow")? & !7)
}
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.contains(['\0', '\\'])
        && path.split('/').count() <= 64
        && path.split('/').all(|p| !matches!(p, "" | "." | "..") && p.len() <= 255)
}
pub(super) fn optional_extension(path: &str) -> bool {
    path.strip_prefix("app/extensions/").and_then(|tail| tail.split_once('/')).is_some_and(
        |(name, resource)| {
            !name.is_empty()
                && name.len() <= 64
                && !resource.is_empty()
                && name.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
                })
        },
    )
}
fn payload_limit(path: &str) -> usize {
    match path {
        MANIFEST => MAX_DISPLAY_MANIFEST_BYTES,
        INITIALIZERS => super::initialization::MAX_INITIALIZERS_BYTES,
        trace_catalog::FILE => trace_catalog::MAX_BYTES,
        ICON => MAX_ICON_BYTES,
        SIGNATURE => signature::SIGNATURE_BYTES,
        EXECUTABLE => MAX_WASM_BYTES,
        _ => MAX_FILE_BYTES,
    }
}
impl Index {
    // fuzz: package_contract
    pub fn read(reader: &mut impl Read) -> Result<Self> {
        let mut header = [0; HEADER_BYTES];
        reader.read_exact(&mut header)?;
        if &header[..8] != MAGIC || u32::from_le_bytes(header[8..12].try_into()?) != 1 {
            return Err("unsupported app bundle".into());
        }
        let count = u32::from_le_bytes(header[12..16].try_into()?) as usize;
        let size = u64::from_le_bytes(header[16..24].try_into()?);
        let length = u64::from_le_bytes(header[24..32].try_into()?);
        if count > MAX_ENTRIES || size > MAX_INDEX_BYTES as u64 || length > MAX_BUNDLE_BYTES as u64
        {
            return Err("bundle index or file exceeds size limit".into());
        }
        let mut bytes = vec![0; size as usize];
        reader.read_exact(&mut bytes)?;
        let mut entries: Vec<Entry> = Vec::with_capacity(count);
        let mut cursor = 0usize;
        let mut offset = HEADER_BYTES as u64 + size;
        for _ in 0..count {
            let record = bytes.get(cursor..cursor + 24).ok_or("truncated bundle index")?;
            let path_len = u32::from_le_bytes(record[..4].try_into()?) as usize;
            let reserved = u32::from_le_bytes(record[4..8].try_into()?);
            let position = u64::from_le_bytes(record[8..16].try_into()?);
            let length = u64::from_le_bytes(record[16..24].try_into()?);
            cursor += 24;
            let end = cursor.checked_add(path_len).ok_or("path length overflow")?;
            let path = std::str::from_utf8(bytes.get(cursor..end).ok_or("truncated bundle path")?)?;
            let padded = align(end as u64)? as usize;
            if reserved != 0
                || !valid_path(path)
                || length > payload_limit(path) as u64
                || position != align(offset)?
                || bytes.get(end..padded).ok_or("truncated index padding")?.iter().any(|b| *b != 0)
                || entries.last().is_some_and(|e| e.path.as_str() >= path)
            {
                return Err("noncanonical bundle index".into());
            }
            offset = position
                .checked_add(length)
                .filter(|n| *n <= MAX_BUNDLE_BYTES as u64)
                .ok_or("bundle payload exceeds size limit")?;
            entries.push(Entry { path: path.into(), offset: position, length });
            cursor = padded;
        }
        if cursor != bytes.len() || offset != length {
            return Err("invalid bundle length".into());
        }
        let index = Self { entries, length, payload: HEADER_BYTES as u64 + size };
        index.validate_names()?;
        Ok(index)
    }
    fn validate_names(&self) -> Result<()> {
        for entry in &self.entries {
            if entry.path == "app/root-volume.json" {
                return Err(UPGRADE_REQUIRED.into());
            }
            for (position, _) in entry.path.match_indices('/') {
                if self.entry(&entry.path[..position]).is_some() {
                    return Err("bundle file is also a directory".into());
                }
            }
            if entry.path == "app"
                || entry.path.starts_with("app/")
                    && !matches!(
                        entry.path.as_str(),
                        MANIFEST
                            | EXECUTABLE
                            | INITIALIZERS
                            | ICON
                            | SIGNATURE
                            | trace_catalog::FILE
                    )
                    && !optional_extension(&entry.path)
            {
                return Err("unknown reserved bundle entry".into());
            }
        }
        for path in [MANIFEST, EXECUTABLE, INITIALIZERS] {
            if self.entry(path).is_none() {
                return Err("required bundle entry is missing".into());
            }
        }
        Ok(())
    }
    pub fn entry(&self, path: &str) -> Option<&Entry> {
        self.entries.binary_search_by(|e| e.path.as_str().cmp(path)).ok().map(|i| &self.entries[i])
    }
    pub fn is_directory(&self, path: &str) -> bool {
        path.is_empty()
            || self
                .entries
                .iter()
                .any(|e| e.path.strip_prefix(path).is_some_and(|p| p.starts_with('/')))
    }
    pub fn child(&self, path: &str, entry: usize) -> Option<(&str, bool)> {
        let entry = self.entries.get(entry)?;
        let tail = if path.is_empty() {
            entry.path.as_str()
        } else {
            entry.path.strip_prefix(path)?.strip_prefix('/')?
        };
        Some(tail.split_once('/').map_or((tail, false), |(p, _)| (p, true)))
    }
    pub fn children(&self, path: &str) -> Vec<usize> {
        // Retain only index positions, never another copy of the names. Eight
        // concurrent File lists together cost at most 64 KiB on 64-bit hosts.
        let name = |index| self.child(path, index).map(|child| child.0);
        let mut entries: Vec<_> = (0..self.entries.len()).filter(|i| name(*i).is_some()).collect();
        entries.sort_unstable_by(|a, b| name(*a).cmp(&name(*b)));
        entries.dedup_by(|a, b| name(*a) == name(*b));
        entries
    }
}
struct HashReader<R> {
    input: R,
    hash: blake3::Hasher,
    content: blake3::Hasher,
    position: u64,
    zero: Option<std::ops::Range<u64>>,
}
impl<R: Read> Read for HashReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.input.read(out)?;
        self.hash.update(&out[..n]);
        hash_content(&mut self.content, self.position, &out[..n], self.zero.as_ref());
        self.position += n as u64;
        Ok(n)
    }
}
fn hash_content(
    hash: &mut blake3::Hasher,
    position: u64,
    bytes: &[u8],
    zero: Option<&std::ops::Range<u64>>,
) {
    let Some(zero) =
        zero.filter(|zero| zero.start < position + bytes.len() as u64 && zero.end > position)
    else {
        hash.update(bytes);
        return;
    };
    let start = zero.start.saturating_sub(position) as usize;
    let end = (zero.end - position).min(bytes.len() as u64) as usize;
    hash.update(&bytes[..start]);
    assert!(end - start <= 64, "signature exclusion is exactly one Ed25519 signature");
    hash.update(&[0; 64][..end - start]);
    hash.update(&bytes[end..]);
}
pub struct Verified {
    pub index: Index,
    pub app_id: String,
    pub name: String,
    pub required_host_features: Vec<String>,
    pub digest: [u8; 32],
    pub trace: Option<Arc<[u8]>>,
    pub(crate) signature: Option<signature::PackageSignature>,
    pub content_digest: [u8; 32],
    pub initializers: super::VolumeInitializers,
}
/// Resumable admission lets the browser yield between bounded payload reads.
/// Native readers drive the same state machine to completion on a File worker.
pub(crate) struct Verifier<R> {
    input: HashReader<R>,
    index: Index,
    position: u64,
    entry: usize,
    started: bool,
    complete: bool,
    chunk: Vec<u8>,
    app_id: Option<String>,
    name: Option<String>,
    required_host_features: Vec<String>,
    trace: Option<Arc<[u8]>>,
    signature: Option<signature::PackageSignature>,
    initializers: Option<super::VolumeInitializers>,
}
impl<R: Read> Verifier<R> {
    pub(crate) fn new(reader: R) -> Result<Self> {
        let mut content = blake3::Hasher::new();
        content.update(signature::CONTENT_DOMAIN);
        let mut input = HashReader {
            input: reader,
            hash: blake3::Hasher::new(),
            content,
            position: 0,
            zero: None,
        };
        let index = Index::read(&mut input)?;
        if let Some(entry) = index.entry(SIGNATURE) {
            if entry.length != signature::SIGNATURE_BYTES as u64 {
                return Err("invalid package signature length".into());
            }
            input.zero = Some(entry.offset + entry.length - 64..entry.offset + entry.length);
        }
        Ok(Self {
            position: index.payload,
            index,
            input,
            entry: 0,
            started: false,
            complete: false,
            chunk: vec![0; CHUNK],
            app_id: None,
            name: None,
            required_host_features: Vec::new(),
            trace: None,
            signature: None,
            initializers: None,
        })
    }
    pub(crate) fn step(&mut self) -> Result<bool> {
        let Some(entry) = self.index.entries.get(self.entry) else {
            if self.input.read(&mut [0])? != 0 {
                return Err("trailing app content".into());
            }
            self.complete = true;
            return Ok(true);
        };
        if !self.started {
            let padding = (entry.offset - self.position) as usize;
            self.input.read_exact(&mut self.chunk[..padding])?;
            if self.chunk[..padding].iter().any(|b| *b != 0) {
                return Err("invalid payload padding".into());
            }
            self.position = entry.offset;
            let metadata = match entry.path.as_str() {
                MANIFEST => Some(MANIFEST),
                INITIALIZERS => Some(INITIALIZERS),
                SIGNATURE => Some(SIGNATURE),
                trace_catalog::FILE => Some(trace_catalog::FILE),
                _ => None,
            };
            if let Some(path) = metadata {
                let length = entry.length;
                self.read_metadata(path, length as usize)?;
                self.position += length;
                self.entry += 1;
                return Ok(false);
            }
            self.started = true;
        }
        let remaining = entry.offset + entry.length - self.position;
        let n = remaining.min(CHUNK as u64) as usize;
        self.input.read_exact(&mut self.chunk[..n])?;
        if self.position == entry.offset
            && entry.path == EXECUTABLE
            && (n < 8 || &self.chunk[..8] != b"\0asm\x01\0\0\0")
        {
            return Err("app content is not WebAssembly".into());
        }
        self.position += n as u64;
        if self.position == entry.offset + entry.length {
            self.entry += 1;
            self.started = false;
        }
        Ok(false)
    }
    fn read_metadata(&mut self, path: &str, length: usize) -> Result<()> {
        let mut bytes = vec![0; length];
        self.input.read_exact(&mut bytes)?;
        match path {
            MANIFEST => {
                let value = display_manifest(&bytes)?;
                self.app_id = Some(value.app_id);
                self.name = Some(value.name);
                self.required_host_features = value.required_host_features;
            }
            INITIALIZERS => {
                let value = super::VolumeInitializers::from_json(&bytes)?;
                if value.to_json()? != bytes {
                    return Err("noncanonical volume initializers".into());
                }
                self.initializers = Some(value);
            }
            SIGNATURE => self.signature = Some(signature::PackageSignature::from_bytes(&bytes)?),
            trace_catalog::FILE => {
                super::json::decode(&bytes, trace_catalog::MAX_BYTES)?;
                trace_catalog::TraceCatalog::decode(&bytes)?;
                self.trace = Some(Arc::from(bytes));
            }
            _ => unreachable!("metadata paths are selected before calling the metadata reader"),
        }
        Ok(())
    }
    pub(crate) fn finish(self) -> Result<Verified> {
        if !self.complete {
            return Err("incomplete app verification".into());
        }
        Ok(Verified {
            index: self.index,
            app_id: self.app_id.ok_or("missing app identity")?,
            name: self.name.ok_or("missing app manifest")?,
            required_host_features: self.required_host_features,
            digest: *self.input.hash.finalize().as_bytes(),
            trace: self.trace,
            signature: self.signature,
            content_digest: *self.input.content.finalize().as_bytes(),
            initializers: self.initializers.ok_or("missing volume initialization declarations")?,
        })
    }
}
/// The streaming admission path is also used by exact-build trace observers.
// fuzz: package_contract
pub fn verify(reader: &mut impl Read) -> Result<Verified> {
    let mut verifier = Verifier::new(reader)?;
    while !verifier.step()? {}
    verifier.finish()
}
fn validate_app_id(value: &str) -> Result<()> {
    if !super::valid_app_id(value) {
        return Err("invalid app_id".into());
    }
    Ok(())
}
fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > MAX_NAME_BYTES {
        return Err("invalid app name".into());
    }
    Ok(())
}
fn display_manifest(bytes: &[u8]) -> Result<DisplayManifest> {
    let value: DisplayManifest =
        serde_json::from_value(super::json::decode(bytes, MAX_DISPLAY_MANIFEST_BYTES)?)?;
    validate_name(&value.name)?;
    validate_app_id(&value.app_id)?;
    validate_required_features(&value.required_host_features)?;
    Ok(value)
}
pub(super) fn validate_required_features(features: &[String]) -> Result<()> {
    if features.len() > MAX_REQUIRED_HOST_FEATURES
        || features.iter().any(|name| !playbit_api::abi::syscall::valid_capability_name(name))
        || features.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err("required host features must be bounded, sorted and distinct".into());
    }
    Ok(())
}
pub(super) fn manifest_bytes(app_id: &str, name: &str, features: &[String]) -> Result<Vec<u8>> {
    validate_app_id(app_id)?;
    validate_name(name)?;
    validate_required_features(features)?;
    super::json::encode(
        &DisplayManifest {
            app_id: app_id.into(),
            name: name.into(),
            required_host_features: features.to_vec(),
        },
        MAX_DISPLAY_MANIFEST_BYTES,
    )
}
impl Package {
    /// A publisher claim becomes authority only through a trusted issuer and
    /// the host's admitted, non-revoked signer status.
    pub fn authenticate(mut self, trust: &signature::PublisherTrust<'_>, now: u64) -> Result<Self> {
        self.identity =
            Some(self.signature.as_ref().ok_or("package signature is missing")?.authenticate(
                &self.app_id,
                &self.content_digest,
                trust,
                now,
            )?);
        Ok(self)
    }
    /// Capture the first successful verification at the host's current time.
    /// Retain this record only in protected host metadata for these exact bytes.
    #[cfg(any(target_arch = "wasm32", feature = "local-store"))]
    pub(crate) fn authenticate_first(
        self,
        trust: &signature::PublisherTrust<'_>,
        now: u64,
    ) -> Result<(Self, signature::PackageAdmission)> {
        let package = self.authenticate(trust, now)?;
        let claims =
            &package.publisher_certificate().ok_or("package certificate is missing")?.claims;
        let admission = signature::PackageAdmission {
            issuer: claims.issuer,
            build: package.digest(),
            publisher: claims.publisher,
            signer: claims.signer,
            admitted_at: now,
        };
        Ok((package, admission))
    }
    /// Recheck exact bytes against host-admitted evidence and current key status.
    pub fn authenticate_admitted(
        mut self,
        trust: &signature::PublisherTrust<'_>,
        admission: &signature::PackageAdmission,
        now: u64,
    ) -> Result<Self> {
        self.identity = Some(
            self.signature.as_ref().ok_or("package signature is missing")?.authenticate_admitted(
                &self.app_id,
                &self.content_digest,
                &self.digest,
                trust,
                admission,
                now,
            )?,
        );
        Ok(self)
    }
    pub fn identity(&self) -> Result<&AppIdentity> {
        self.identity.as_ref().ok_or_else(|| "package publisher has not been authenticated".into())
    }
    /// Untrusted certificate material for issuer/status discovery, never an access grant.
    pub fn publisher_certificate(&self) -> Option<&signature::PublisherCertificate> {
        self.signature.as_ref().map(|signature| &signature.certificate)
    }
    pub fn app_id(&self) -> &str {
        &self.app_id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn required_host_features(&self) -> &[String] {
        &self.required_host_features
    }
    /// Check the actual host before running code. Structural admission and
    /// display inspection intentionally do not require execution support.
    pub fn admit_execution(&self, supports: impl Fn(&str) -> bool) -> Result<()> {
        if let Some(feature) = self.required_host_features.iter().find(|name| !supports(name)) {
            return Err(format!("unsupported package host feature: {feature}").into());
        }
        self.identity()?;
        Ok(())
    }
    pub fn index(&self) -> &Index {
        &self.index
    }
    pub fn trace(&self) -> Option<&Arc<[u8]>> {
        self.trace.as_ref()
    }
    pub(crate) fn from_verified(backing: Arc<dyn Backing>, verified: Verified) -> Self {
        Self {
            app_id: verified.app_id,
            name: verified.name,
            required_host_features: verified.required_host_features,
            index: Arc::new(verified.index),
            backing,
            digest: verified.digest,
            trace: verified.trace,
            signature: verified.signature,
            content_digest: verified.content_digest,
            identity: None,
            initializers: verified.initializers,
        }
    }
    #[cfg(unix)]
    pub fn open(path: &std::path::Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err("bundle must be a regular file".into());
        }
        let verified = verify(&mut file)?;
        Ok(Self::from_verified(Arc::new(file), verified))
    }
    // fuzz: package_contract
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let verified = verify(&mut &bytes[..])?;
        let bytes: Arc<[u8]> = Arc::from(bytes);
        Ok(Self::from_verified(Arc::new(bytes), verified))
    }
    pub fn id(&self) -> Result<String> {
        Ok(crate::identifier::encode(&self.digest))
    }
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }
    pub fn read(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        let entry = self.index.entry(path).ok_or("bundle file not found")?;
        if offset > entry.length {
            return Err("bundle read beyond end".into());
        }
        let length = length.min((entry.length - offset) as usize);
        let mut bytes = vec![0; length];
        self.backing.read_at(entry.offset + offset, &mut bytes)?;
        Ok(bytes)
    }
    pub fn wasm(&self) -> Result<Vec<u8>> {
        self.read(EXECUTABLE, 0, MAX_WASM_BYTES)
    }
    pub fn initializer(
        &self,
        role: super::AutomaticVolumeRole,
    ) -> Result<crate::store::RootInitialization> {
        Ok(self.initializers.for_role(role))
    }
    pub fn initializers(&self) -> &super::VolumeInitializers {
        &self.initializers
    }
    pub fn initializer_fingerprint(&self, role: super::AutomaticVolumeRole) -> Result<[u8; 32]> {
        self.initializers.for_role(role).fingerprint().map_err(Into::into)
    }
    pub fn write_to(&self, output: &mut impl Write) -> Result<()> {
        let mut chunk = vec![0; CHUNK];
        let mut offset = 0u64;
        while offset < self.index.length {
            let n = chunk.len().min((self.index.length - offset) as usize);
            self.backing.read_at(offset, &mut chunk[..n])?;
            output.write_all(&chunk[..n])?;
            offset += n as u64;
        }
        Ok(())
    }
    /// Signing is a source-build operation. Imported signed packages keep their
    /// original bytes and publisher, even when copied by another account.
    pub fn write_signed(
        &self,
        signer: &signature::PackageSigner,
        signed_at: u64,
        output: &mut impl Write,
    ) -> Result<()> {
        if self.signature.is_some() {
            return Err("an existing signed package cannot be re-signed".into());
        }
        write_signed(self.sources(), signer, signed_at, output)
    }

    /// Copy an installed macOS template after the caller validates its app bundle seal.
    #[cfg(all(target_os = "macos", feature = "desktop"))]
    pub(crate) fn write_unsigned_template(&self, output: &mut impl Write) -> Result<()> {
        let mut sources = self.sources();
        sources.remove(SIGNATURE);
        write(&sources, output)
    }

    fn sources(&self) -> BTreeMap<String, Source> {
        self.index
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.path.clone(),
                    Source::Backing {
                        backing: self.backing.clone(),
                        offset: entry.offset,
                        length: entry.length,
                    },
                )
            })
            .collect()
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = vec![0; self.index.length as usize];
        self.backing.read_at(0, &mut bytes)?;
        Ok(bytes)
    }
    pub fn new(
        app_id: String,
        name: String,
        wasm: Vec<u8>,
        files: BTreeMap<String, Vec<u8>>,
    ) -> Result<Self> {
        Self::create(app_id, name, wasm, files, &super::VolumeInitializers::default())
    }
    pub fn create(
        app_id: String,
        name: String,
        wasm: Vec<u8>,
        mut files: BTreeMap<String, Vec<u8>>,
        initializers: &super::VolumeInitializers,
    ) -> Result<Self> {
        validate_name(&name)?;
        validate_app_id(&app_id)?;
        files.insert(MANIFEST.into(), manifest_bytes(&app_id, &name, &[])?);
        files.insert(EXECUTABLE.into(), wasm);
        files.insert(INITIALIZERS.into(), initializers.to_json()?);
        let sources = files.into_iter().map(|(path, bytes)| (path, Source::Bytes(bytes))).collect();
        let mut bytes = Vec::new();
        write(&sources, &mut bytes)?;
        Self::from_bytes(&bytes)
    }
    pub fn read_app_id(reader: &mut (impl Read + Seek)) -> Result<String> {
        let bytes =
            display(reader, MANIFEST, MAX_DISPLAY_MANIFEST_BYTES)?.ok_or("missing app manifest")?;
        let value = display_manifest(&bytes)?;
        Ok(value.app_id)
    }
    /// Display inspection reads only the index and selected metadata.
    // fuzz: package_contract
    pub fn read_display_name(reader: &mut (impl Read + Seek)) -> Result<String> {
        let bytes =
            display(reader, MANIFEST, MAX_DISPLAY_MANIFEST_BYTES)?.ok_or("missing app manifest")?;
        let value = display_manifest(&bytes)?;
        Ok(value.name)
    }
    // fuzz: package_contract
    pub fn read_display_icon(reader: &mut (impl Read + Seek)) -> Result<Option<Vec<u8>>> {
        display(reader, ICON, MAX_ICON_BYTES)
    }
    // fuzz: package_contract
    pub fn read_trace_catalog(reader: &mut impl Read, build: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let value = verify(reader)?;
        if &value.digest != build {
            return Err("app content does not match captured build".into());
        }
        Ok(value.trace.map(|bytes| bytes.to_vec()))
    }
}
fn display(reader: &mut (impl Read + Seek), path: &str, limit: usize) -> Result<Option<Vec<u8>>> {
    reader.seek(SeekFrom::Start(0))?;
    let index = Index::read(reader)?;
    if reader.seek(SeekFrom::End(0))? != index.length {
        return Err("invalid bundle length".into());
    }
    let Some(entry) = index.entry(path) else {
        return Ok(None);
    };
    if entry.length > limit as u64 {
        return Err("bundle display metadata exceeds limit".into());
    }
    reader.seek(SeekFrom::Start(entry.offset))?;
    let mut bytes = vec![0; entry.length as usize];
    reader.read_exact(&mut bytes)?;
    Ok(Some(bytes))
}
pub enum Source {
    Bytes(Vec<u8>),
    File(std::fs::File),
    Backing { backing: Arc<dyn Backing>, offset: u64, length: u64 },
}
impl Source {
    fn length(&self) -> Result<u64> {
        Ok(match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::File(file) => file.metadata()?.len(),
            Self::Backing { length, .. } => *length,
        })
    }
}
/// Source descriptors are captured before publication; writes never enumerate
/// the source tree or buffer an entire resource file.
pub fn write(sources: &BTreeMap<String, Source>, output: &mut impl Write) -> Result<()> {
    let index = source_index(sources)?;
    write_indexed(&index, sources, output)
}
fn source_index(sources: &BTreeMap<String, Source>) -> Result<Index> {
    if sources.len() > MAX_ENTRIES {
        return Err("too many bundle files".into());
    }
    let size = sources
        .keys()
        .try_fold(0u64, |n, path| -> Result<_> { Ok(n + align(24 + path.len() as u64)?) })?;
    if size > MAX_INDEX_BYTES as u64 {
        return Err("bundle index exceeds size limit".into());
    }
    let mut offset = HEADER_BYTES as u64 + size;
    let mut entries = Vec::with_capacity(sources.len());
    for (path, source) in sources {
        let length = source.length()?;
        if !valid_path(path) || length > payload_limit(path) as u64 {
            return Err("invalid bundle file".into());
        }
        offset = align(offset)?;
        entries.push(Entry { path: path.clone(), offset, length });
        offset = offset
            .checked_add(length)
            .filter(|n| *n <= MAX_BUNDLE_BYTES as u64)
            .ok_or("bundle exceeds size limit")?;
    }
    let index = Index { entries, length: offset, payload: HEADER_BYTES as u64 + size };
    index.validate_names()?;
    Ok(index)
}
fn write_indexed(
    index: &Index,
    sources: &BTreeMap<String, Source>,
    output: &mut impl Write,
) -> Result<()> {
    output.write_all(MAGIC)?;
    output.write_all(&1u32.to_le_bytes())?;
    output.write_all(&(sources.len() as u32).to_le_bytes())?;
    output.write_all(&(index.payload - HEADER_BYTES as u64).to_le_bytes())?;
    output.write_all(&index.length.to_le_bytes())?;
    for entry in &index.entries {
        write_entry(entry, output)?;
    }
    write_payloads(index, sources, output)
}
fn write_entry(entry: &Entry, output: &mut impl Write) -> Result<()> {
    output.write_all(&(entry.path.len() as u32).to_le_bytes())?;
    output.write_all(&0u32.to_le_bytes())?;
    output.write_all(&entry.offset.to_le_bytes())?;
    output.write_all(&entry.length.to_le_bytes())?;
    output.write_all(entry.path.as_bytes())?;
    let padding = (align(entry.path.len() as u64)? - entry.path.len() as u64) as usize;
    output.write_all(&[0; 8][..padding])?;
    Ok(())
}
fn write_payloads(
    index: &Index,
    sources: &BTreeMap<String, Source>,
    output: &mut impl Write,
) -> Result<()> {
    let mut position = index.payload;
    let mut chunk = vec![0; CHUNK];
    for entry in &index.entries {
        output.write_all(&[0; 8][..(entry.offset - position) as usize])?;
        match &sources[&entry.path] {
            Source::Bytes(bytes) => output.write_all(bytes)?,
            Source::File(file) => {
                let mut file = file.try_clone()?;
                file.seek(SeekFrom::Start(0))?;
                let mut remaining = entry.length as usize;
                while remaining > 0 {
                    let n = remaining.min(chunk.len());
                    file.read_exact(&mut chunk[..n])?;
                    output.write_all(&chunk[..n])?;
                    remaining -= n;
                }
                if file.read(&mut [0])? != 0 {
                    return Err("source changed during packaging".into());
                }
            }
            Source::Backing { backing, offset, .. } => {
                let mut position = 0;
                while position < entry.length {
                    let n = (entry.length - position).min(chunk.len() as u64) as usize;
                    backing.read_at(offset + position, &mut chunk[..n])?;
                    output.write_all(&chunk[..n])?;
                    position += n as u64;
                }
            }
        }
        position = entry.offset + entry.length;
    }
    Ok(())
}

pub(super) fn write_signed(
    mut sources: BTreeMap<String, Source>,
    signer: &signature::PackageSigner,
    signed_at: u64,
    output: &mut impl Write,
) -> Result<()> {
    if sources.contains_key(SIGNATURE) {
        return Err("an existing signed package cannot be re-signed".into());
    }
    let mut declaration = signer.declaration(signed_at)?;
    sources.insert(SIGNATURE.into(), Source::Bytes(declaration.to_bytes()?.to_vec()));
    let index = source_index(&sources)?;
    let entry = index.entry(SIGNATURE).expect("the signature source was inserted before indexing");
    let zero = entry.offset + entry.length - 64..entry.offset + entry.length;
    let mut sink = std::io::sink();
    let digest = write_content(&index, &sources, &zero, &mut sink)?;
    declaration.signature = signer.sign(&digest);
    sources.insert(SIGNATURE.into(), Source::Bytes(declaration.to_bytes()?.to_vec()));
    if write_content(&index, &sources, &zero, output)? != digest {
        return Err("source changed during package signing".into());
    }
    Ok(())
}

fn write_content(
    index: &Index,
    sources: &BTreeMap<String, Source>,
    zero: &std::ops::Range<u64>,
    output: &mut impl Write,
) -> Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    hash.update(signature::CONTENT_DOMAIN);
    let mut writer = ContentWriter { output, hash, position: 0, zero };
    write_indexed(index, sources, &mut writer)?;
    Ok(*writer.hash.finalize().as_bytes())
}
struct ContentWriter<'a, W> {
    output: &'a mut W,
    hash: blake3::Hasher,
    position: u64,
    zero: &'a std::ops::Range<u64>,
}
impl<W: Write> Write for ContentWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.output.write(bytes)?;
        hash_content(&mut self.hash, self.position, &bytes[..n], Some(self.zero));
        self.position += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

#[cfg(test)]
impl Package {
    pub(crate) fn renamed(&self, name: &str) -> Result<Self> {
        validate_name(name)?;
        let mut sources = BTreeMap::new();
        for entry in &self.index.entries {
            let mut bytes = self.read(&entry.path, 0, entry.length as usize)?;
            if entry.path == MANIFEST {
                let mut value = super::json::decode(&bytes, MAX_DISPLAY_MANIFEST_BYTES)?;
                value["name"] = name.into();
                bytes = super::json::encode(&value, MAX_DISPLAY_MANIFEST_BYTES)?;
            }
            sources.insert(entry.path.clone(), Source::Bytes(bytes));
        }
        let mut bytes = Vec::new();
        write(&sources, &mut bytes)?;
        Self::from_bytes(&bytes)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "Tests construct bounded retained bundle fixtures.")]
    use super::*;

    #[test]
    fn retained_root_volume_bundle_requires_rebuild_without_admitting_execution() {
        // Encode the retained shape directly: today's writer rejects it too.
        for (path, expected) in [
            ("app/root-volume.json", UPGRADE_REQUIRED),
            ("app/unknown.json", "unknown reserved bundle entry"),
        ] {
            let sources = BTreeMap::from([
                (MANIFEST.into(), Source::Bytes(br#"{"app_id":"test","name":"Example"}"#.to_vec())),
                (EXECUTABLE.into(), Source::Bytes(b"\0asm\x01\0\0\0".to_vec())),
                (path.into(), Source::Bytes(b"{}".to_vec())),
            ]);
            let payload = HEADER_BYTES as u64
                + sources
                    .keys()
                    .map(|path: &String| align(24 + path.len() as u64).unwrap())
                    .sum::<u64>();
            let mut offset = payload;
            let mut entries = Vec::new();
            for (path, source) in &sources {
                offset = align(offset).unwrap();
                let length = source.length().unwrap();
                entries.push(Entry { path: path.clone(), offset, length });
                offset += length;
            }
            let index = Index { entries, length: offset, payload };
            let mut bytes = Vec::new();
            write_indexed(&index, &sources, &mut bytes).unwrap();
            assert_eq!(Package::from_bytes(&bytes).unwrap_err().to_string(), expected);
            assert_eq!(write(&sources, &mut Vec::new()).unwrap_err().to_string(), expected);
        }
    }
}
