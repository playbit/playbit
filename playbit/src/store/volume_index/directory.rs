//! Native column queries retain one bounded page and canonical ancestry. Sorting
//! borrows the admitted projection's names; storage keys retain identity order.
use super::*;
use std::cmp::Ordering;

const RETAINED_BYTES: usize = namespace::MAX_ENTRIES * size_of::<&namespace::Entry>()
    + MAX_ENTRY_PAGE * (size_of::<EntrySummary>() + MAX_ENTRY_COMPONENT_BYTES)
    + namespace::MAX_DEPTH * (size_of::<DirectoryAncestor>() + MAX_ENTRY_COMPONENT_BYTES);
const SERIALIZED_BYTES: usize = MAX_ENTRY_PAGE * (2 * MAX_ENTRY_COMPONENT_BYTES + 192)
    + namespace::MAX_DEPTH * (2 * MAX_ENTRY_COMPONENT_BYTES + 100)
    + 256;
const _: () = assert!(RETAINED_BYTES <= summaries::MAX_RETAINED_BYTES);
const _: () = assert!(SERIALIZED_BYTES <= summaries::MAX_SERIALIZED_BYTES);

impl<S: Storage> IndexSnapshot<'_, S> {
    /// Capture one presentation-ordered child page without opening backing data
    /// or materializing additive schemas. Every continuation requires the exact
    /// snapshot clock; an edit invalidates it before any rows are returned.
    pub fn directory_entries(
        &self,
        target: DirectoryTarget,
        cut: Option<u64>,
        after: Option<EntryId>,
        limit: usize,
        compare: fn(&str, &str) -> Ordering,
    ) -> Result<DirectoryListing> {
        if limit == 0 || limit > MAX_ENTRY_PAGE || after.is_some() && cut.is_none() {
            return Err(Error::InvalidInput("invalid directory page request"));
        }
        let current = self.tx.clock()?.as_u64();
        if cut.is_some_and(|cut| cut != current) {
            return Ok(DirectoryListing::Changed { cut: current });
        }
        let view = &self.namespace_projection()?.admission.view;
        assert!(view.entries.len() <= namespace::MAX_ENTRIES, "admitted namespace entry bound");
        let parent = match target {
            DirectoryTarget::Root => None,
            DirectoryTarget::Directory(id) => projection::canonical_directory(view, Some(id))?,
            DirectoryTarget::Entry(id) => projection::visible_entry(view, id)?.parent,
        };
        let ancestors = ancestors(view, parent)?;
        let mut children = Vec::with_capacity(view.entries.len());
        children
            .extend(view.entries.values().filter(|entry| entry.visible && entry.parent == parent));
        children.sort_unstable_by(|a, b| compare(&a.name, &b.name).then_with(|| a.id.cmp(&b.id)));
        let start = match after {
            None => 0,
            Some(id) => {
                children
                    .iter()
                    .position(|entry| entry.id == id)
                    .ok_or(Error::InvalidInput("directory page cursor is not a child"))?
                    + 1
            }
        };
        let mut entries: Vec<EntrySummary> = Vec::with_capacity(limit);
        let mut source_bytes = 0usize;
        let mut next = None;
        for entry in &children[start..] {
            if entries.len() == limit {
                next = entries.last().map(|entry| entry.id);
                break;
            }
            source_bytes = source_bytes
                .checked_add(self.summary_source_bytes(entry.id)?)
                .ok_or(Error::LimitExceeded)?;
            if source_bytes > summaries::TARGET_SOURCE_BYTES && !entries.is_empty() {
                next = entries.last().map(|entry| entry.id);
                break;
            }
            assert!(
                source_bytes <= summaries::MAX_SOURCE_BYTES,
                "one bounded birth/name/parent fits the hard source ceiling"
            );
            entries.push(self.namespace_summary(entry)?);
        }
        Ok(DirectoryListing::Page { cut: current, parent, ancestors, entries, next })
    }
}

fn ancestors(
    view: &namespace::View,
    mut parent: Option<EntryId>,
) -> Result<Vec<DirectoryAncestor>> {
    let mut ancestors = Vec::with_capacity(namespace::MAX_DEPTH);
    for _ in 0..namespace::MAX_DEPTH {
        let Some(id) = parent else {
            ancestors.reverse();
            return Ok(ancestors);
        };
        let entry = projection::visible_entry(view, id)?;
        if entry.kind != namespace::Kind::Directory {
            return Err(Error::corrupt("namespace ancestor is not a directory"));
        }
        ancestors.push(DirectoryAncestor {
            id: entry.id,
            parent: entry.parent,
            name: entry.name.clone().into_boxed_str().into_string(),
        });
        parent = entry.parent;
    }
    if parent.is_some() {
        return Err(Error::LimitExceeded);
    }
    ancestors.reverse();
    Ok(ancestors)
}

#[cfg(test)]
#[path = "directory_tests.rs"]
mod tests;
