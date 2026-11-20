//! Physical browser cache accounting and exclusive deletion ownership. Missing
//! retention records are unknown, never evidence that content is recoverable.
//! One worker calls exports synchronously; a deletion fence spans async OPFS I/O.
mod retention;
use super::*;
use std::collections::BTreeMap;

const CACHE_BYTES: u64 = 256 * 1024 * 1024;
const CONTENT_LIMIT: usize = 4096;
const SCAN_LIMIT: usize = 64;
const PIN_LIMIT: u32 = 128;
type Digest = [u8; 32];
#[derive(Default)]
struct Content {
    physical: Option<u64>,
    pins: u32,
}
#[derive(Default)]
pub(super) struct Cache {
    contents: BTreeMap<Digest, Content>,
    bytes: u64,
    after: Option<Digest>,
    deleting: Option<Digest>,
}
impl Cache {
    fn content(&mut self, digest: Digest) -> Result<&mut Content> {
        if self.deleting == Some(digest) {
            return Err(Error::Busy);
        }
        if !self.contents.contains_key(&digest) && self.contents.len() == CONTENT_LIMIT {
            return Err(Error::LimitExceeded);
        }
        Ok(self.contents.entry(digest).or_default())
    }
    pub(super) fn pin(&mut self, digest: Digest) -> Result<()> {
        let content = self.content(digest)?;
        if content.pins == PIN_LIMIT {
            return Err(Error::LimitExceeded);
        }
        content.pins += 1;
        Ok(())
    }
    pub(super) fn unpin(&mut self, digest: Digest) -> Result<()> {
        let content = self.contents.get_mut(&digest).ok_or(Error::InvalidArgument)?;
        if content.pins == 0 {
            return Err(Error::InvalidArgument);
        }
        content.pins -= 1;
        if content.pins == 0 && content.physical.is_none() {
            self.contents.remove(&digest);
        }
        self.after = None;
        Ok(())
    }
    fn observe(&mut self, digest: Digest, length: u64) -> Result<()> {
        if length > playbit_api::abi::files::MAX_FILE_EXTENT {
            return Err(Error::LimitExceeded);
        }
        let content = self.content(digest)?;
        let previous = content.physical.replace(length).unwrap_or(0);
        self.bytes = self
            .bytes
            .checked_sub(previous)
            .and_then(|n| n.checked_add(length))
            .ok_or(Error::Internal)?;
        self.after = None;
        Ok(())
    }
    fn deleted(&mut self, digest: Digest, success: bool) -> Result<()> {
        if self.deleting != Some(digest) {
            return Err(Error::StaleHandle);
        }
        if success {
            let content = self.contents.remove(&digest).expect("deletion owns its content");
            assert_eq!(content.pins, 0, "deletion excludes all owners");
            self.bytes -= content.physical.expect("deletion owns physical bytes");
        }
        self.deleting = None;
        if !success {
            self.after = None;
        }
        Ok(())
    }
    fn next(&mut self, host: &Host) -> Result<serde_json::Value> {
        if self.deleting.is_some() {
            return Err(Error::Busy);
        }
        if self.bytes <= CACHE_BYTES && self.contents.len() < CONTENT_LIMIT {
            self.after = None;
            return Ok(serde_json::json!({ "kind": "done", "bytes": self.bytes }));
        }
        use std::ops::Bound::{Excluded, Unbounded};
        let start = self.after.map_or(Unbounded, Excluded);
        for (scanned, (digest, content)) in self.contents.range((start, Unbounded)).enumerate() {
            if scanned == SCAN_LIMIT {
                return Ok(serde_json::json!({ "kind": "more" }));
            }
            self.after = Some(*digest);
            if content.physical.is_some()
                && content.pins == 0
                && retention::evictable(host, digest)?
            {
                self.deleting = Some(*digest);
                return Ok(serde_json::json!({ "kind": "remove", "digest": blob::hex(digest) }));
            }
        }
        self.after = None;
        Ok(serde_json::json!({ "kind": "done", "bytes": self.bytes }))
    }
}

pub(super) struct Pin {
    cache: Rc<RefCell<Cache>>,
    digest: Digest,
}
impl Pin {
    pub(super) fn reserve(cache: &Rc<RefCell<Cache>>, digest: Digest) -> Result<Self> {
        cache.borrow_mut().pin(digest)?;
        Ok(Self { cache: cache.clone(), digest })
    }
}
impl Drop for Pin {
    fn drop(&mut self) {
        self.cache.borrow_mut().unpin(self.digest).expect("retained reader owns a cache pin");
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Observe { digest: String, length: u64 },
    Pin { digest: String },
    Unpin { digest: String },
    Saved { digest: String, length: u64, confirmed: bool },
    Next,
    Deleted { digest: String, success: bool },
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_cache(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, 512) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = HOST.with(|cell| {
        let host = cell.borrow();
        let host = host.as_ref().ok_or(Error::Unavailable)?;
        let request: Request = serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let mut cache = host.blob_cache.borrow_mut();
        match request {
            Request::Observe { digest, length } => cache.observe(blob::digest(&digest)?, length)?,
            Request::Pin { digest } => cache.pin(blob::digest(&digest)?)?,
            Request::Unpin { digest } => cache.unpin(blob::digest(&digest)?)?,
            Request::Saved { digest, length, confirmed } => {
                let digest = blob::digest(&digest)?;
                let fresh = cache.content(digest)?.physical.is_none();
                cache.observe(digest, length)?;
                retention::saved(host, &digest, fresh, confirmed)?;
            }
            Request::Next => return serde_json::to_vec(&cache.next(host)?).map_err(map_error),
            Request::Deleted { digest, success } => {
                let digest = blob::digest(&digest)?;
                if cache.deleting != Some(digest) {
                    return Err(Error::StaleHandle);
                }
                let forgotten = if success { retention::forget(host, &digest) } else { Ok(()) };
                cache.deleted(digest, success && forgotten.is_ok())?;
                forgotten?;
            }
        }
        Ok(Vec::new())
    });
    result.map_or_else(|error| error as i32, publish_output)
}

pub(super) use retention::{acknowledge, protect};
