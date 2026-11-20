//! Active immutable views retain their original logical contents budget even
//! when the public File is disposed while an accepted read still owns the view.
use super::*;
const MAX_VIEWS: usize = 32;
const MAX_CONTENTS: u64 = 256 * 1024 * 1024;
pub(super) struct Budget {
    views: usize,
    bytes: u64,
    contents: BTreeMap<([u8; 32], u64), usize>,
    cache: Rc<RefCell<blob_cache::Cache>>,
}
impl Budget {
    pub(super) fn new(cache: &Rc<RefCell<blob_cache::Cache>>) -> Self {
        Self { views: 0, bytes: 0, contents: BTreeMap::new(), cache: cache.clone() }
    }
}
pub(super) struct Credit {
    budget: Rc<RefCell<Budget>>,
    content: ([u8; 32], u64),
    _pin: blob_cache::Pin,
}
impl Credit {
    pub(super) fn reserve(
        budget: &Rc<RefCell<Budget>>,
        digest: [u8; 32],
        length: u64,
    ) -> Result<Self> {
        let mut used = budget.borrow_mut();
        let content = (digest, length);
        let bytes = if used.contents.contains_key(&content) { 0 } else { length };
        if used.views == MAX_VIEWS || bytes > MAX_CONTENTS - used.bytes {
            return Err(Error::LimitExceeded);
        }
        let pin = blob_cache::Pin::reserve(&used.cache, digest)?;
        used.views += 1;
        used.bytes += bytes;
        *used.contents.entry(content).or_default() += 1;
        Ok(Self { budget: budget.clone(), content, _pin: pin })
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        let mut used = self.budget.borrow_mut();
        assert!(used.views > 0, "active view owns a reservation");
        used.views -= 1;
        let count = used.contents.get_mut(&self.content).expect("active content is charged");
        assert!(*count > 0, "active content has a view");
        *count -= 1;
        if *count == 0 {
            used.contents.remove(&self.content);
            used.bytes -= self.content.1;
        }
    }
}
