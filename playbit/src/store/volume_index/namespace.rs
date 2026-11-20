//! Deterministic namespace projection from immutable, causally closed facts.
//! Source facts carry authority; this module never creates replicated edits.
use super::{EntryId, Error, Id, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_FACTS: usize = 4096;
pub const MAX_ENTRIES: usize = 1024;
pub const MAX_COMPONENT_BYTES: usize = 255;
pub const MAX_PATH_BYTES: usize = playbit_api::abi::files::MAX_FILE_PATH_BYTES;
pub const MAX_DEPTH: usize = 64;
mod admission;
mod context;
pub use context::Context;
pub(crate) mod causal_context;
pub mod live;
pub mod observations;
pub mod payloads;
pub mod receipt;
pub mod source;
pub use admission::{Admission, Assignments, EventState, admit, event_id};
pub(super) const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_WORK: usize = 4 * 1024 * 1024;
const MAX_ORIGIN_LABEL_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Origin {
    pub installation: Id,
    pub label: Option<String>,
}
impl Origin {
    pub fn validate(&self) -> Result<()> {
        if self.installation == Id::from_bytes([0; 16])
            || self.label.as_ref().is_some_and(|label| label.len() > MAX_ORIGIN_LABEL_BYTES)
        {
            return Err(Error::InvalidInput("invalid namespace origin"));
        }
        Ok(())
    }
}
/// Immutable location and closed causal context of an entry birth. Root-level
/// callers construct this explicitly; missing persisted fields are not accepted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Birth {
    pub creation_context: Option<CreationContext>,
    pub parent: Option<EntryId>,
    pub automatic: bool,
    pub deletions: Vec<EntryId>,
    pub departures: Vec<EntryId>,
    #[serde(with = "causal_context")]
    pub observed: Vec<EntryId>,
}
/// Absence observed when a private writer was opened. Publication still carries
/// the current full observed context, including subsequent deletions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct CreationContext {
    #[serde(with = "causal_context")]
    pub observed: Vec<EntryId>,
}
impl CreationContext {
    fn validate(&self, observed: &[EntryId]) -> Result<()> {
        validate_context(&self.observed)?;
        if self.observed.iter().any(|id| observed.binary_search(id).is_err()) {
            return Err(Error::InvalidInput("creation context is not observed"));
        }
        Ok(())
    }
}
impl Birth {
    pub fn validate(&self, directory: bool) -> Result<()> {
        validate_context(&self.observed)?;
        if let Some(context) = &self.creation_context {
            context.validate(&self.observed)?;
            if directory || self.automatic {
                return Err(Error::InvalidInput(
                    "directory cannot retain a writer creation context",
                ));
            }
        }
        if self.automatic && !directory
            || self.deletions.len() > MAX_FACTS
            || self.departures.len() > MAX_ENTRIES
            || !ordered(&self.deletions)
            || !ordered(&self.departures)
            || self.deletions.iter().any(|id| self.observed.binary_search(id).is_err())
            || self.departures.iter().any(|id| self.observed.binary_search(id).is_err())
        {
            return Err(Error::InvalidInput("invalid entry birth context"));
        }
        Ok(())
    }
}
pub fn validate_context(observed: &[EntryId]) -> Result<()> {
    if observed.len() >= MAX_FACTS || !ordered(observed) {
        return Err(Error::InvalidInput("invalid entry causal context"));
    }
    Ok(())
}
/// Resolver-owned form of a writer's open-time context. Its serialized shape
/// matches the immutable declaration; compaction never changes source bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactCreationContext {
    pub observed: Context,
}
impl From<CreationContext> for FactCreationContext {
    fn from(context: CreationContext) -> Self {
        Self { observed: context.observed.into() }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    File,
    Directory,
    Other,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", deny_unknown_fields)]
pub enum Change {
    Birth {
        creation_context: Option<FactCreationContext>,
        entry: EntryId,
        parent: Option<EntryId>,
        name: String,
        kind: Kind,
        automatic: bool,
        deletions: Vec<EntryId>,
        departures: Vec<EntryId>,
    },
    Edit {
        entry: EntryId,
        #[serde(with = "parent_assignment")]
        parent: Option<Option<EntryId>>,
        name: Option<String>,
        content: bool,
    },
    Delete {
        entry: EntryId,
    },
}
// Option<Option<T>> alone loses Some(None) in JSON. Parent assignments have
// three distinct wire states: unchanged, root, and a particular directory.
mod parent_assignment {
    use super::*;
    #[derive(Serialize, Deserialize)]
    #[serde(tag = "type", content = "entry", rename_all = "snake_case", deny_unknown_fields)]
    enum Assignment {
        Unchanged,
        Root,
        Entry(EntryId),
    }
    pub fn serialize<S: serde::Serializer>(
        value: &Option<Option<EntryId>>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let value = match value {
            None => Assignment::Unchanged,
            Some(None) => Assignment::Root,
            Some(Some(id)) => Assignment::Entry(*id),
        };
        value.serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<Option<EntryId>>, D::Error> {
        Ok(match Assignment::deserialize(deserializer)? {
            Assignment::Unchanged => None,
            Assignment::Root => Some(None),
            Assignment::Entry(id) => Some(Some(id)),
        })
    }
}

impl Change {
    pub(super) fn entry(&self) -> EntryId {
        match self {
            Self::Birth { entry, .. } | Self::Edit { entry, .. } | Self::Delete { entry } => *entry,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub id: EntryId,
    pub clock: u64,
    /// Exact immutable event IDs, sorted and unique, including transitive dependencies.
    pub observed: Context,
    pub origin: Origin,
    pub change: Change,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: EntryId,
    pub parent: Option<EntryId>,
    pub name: String,
    pub requested_name: String,
    pub kind: Kind,
    pub visible: bool,
    pub parent_event: EntryId,
    pub name_event: EntryId,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub entries: BTreeMap<EntryId, Entry>,
    /// Includes every birth identity, including invisible coalesced identities.
    pub canonical: BTreeMap<EntryId, EntryId>,
}
struct Work(usize);
impl Work {
    fn charge(&mut self, count: usize) -> Result<()> {
        self.0 = self.0.checked_add(count).ok_or(Error::LimitExceeded)?;
        if self.0 > MAX_WORK {
            return Err(Error::LimitExceeded);
        }
        Ok(())
    }
}
fn ordered(ids: &[EntryId]) -> bool {
    ids.windows(2).all(|pair| pair[0] < pair[1])
}
pub fn component(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_COMPONENT_BYTES
        || matches!(name, "." | "..")
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput("invalid volume path component"));
    }
    Ok(())
}
fn rank(fact: &Fact) -> (u64, EntryId) {
    (fact.clock, fact.id)
}

impl Fact {
    /// Validate bounded wire shape without requiring delivery of causal dependencies.
    pub fn validate_shape(&self) -> Result<usize> {
        let fact = self;
        let mut bytes = 0usize;
        fact.origin.validate()?;
        let label = fact.origin.label.as_deref().unwrap_or("");
        if fact.clock == 0
            || label.len() > MAX_ORIGIN_LABEL_BYTES
            || !fact.observed.ordered()
            || fact.observed.len() > MAX_FACTS
            || fact.observed.contains(&fact.id)
        {
            return Err(Error::InvalidInput("invalid namespace event context"));
        }
        bytes = bytes
            .checked_add(512 + label.len() + fact.observed.len() * 15)
            .ok_or(Error::LimitExceeded)?;
        match &fact.change {
            Change::Birth {
                name,
                kind,
                automatic,
                deletions,
                departures,
                creation_context,
                ..
            } => {
                component(name)?;
                if let Some(context) = creation_context {
                    if !context.observed.ordered()
                        || context.observed.len() >= MAX_FACTS
                        || context.observed.iter().any(|id| !fact.observed.contains(id))
                    {
                        return Err(Error::InvalidInput("creation context is not observed"));
                    }
                    if *kind != Kind::File {
                        return Err(Error::InvalidInput(
                            "only files retain a writer creation context",
                        ));
                    }
                    bytes += context.observed.len() * 15;
                }
                if *automatic && *kind != Kind::Directory
                    || !ordered(deletions)
                    || !ordered(departures)
                    || deletions.len() > MAX_FACTS
                    || departures.len() > MAX_ENTRIES
                {
                    return Err(Error::InvalidInput("invalid namespace birth"));
                }
                bytes = bytes
                    .checked_add(name.len() + 15 * (deletions.len() + departures.len()))
                    .ok_or(Error::LimitExceeded)?;
            }
            Change::Edit { name, parent, content, .. } => {
                if let Some(name) = name {
                    component(name)?;
                    bytes = bytes.checked_add(name.len()).ok_or(Error::LimitExceeded)?;
                }
                if name.is_none() && parent.is_none() && !content {
                    return Err(Error::InvalidInput("empty namespace edit"));
                }
            }
            Change::Delete { .. } => {}
        }
        Ok(bytes)
    }
}

/// All successful projections depend only on the immutable fact set. Exhaustion
/// and missing dependencies are explicit errors, never partial namespaces.
pub fn derive(facts: &[Fact]) -> Result<View> {
    admission::derive(facts)
}

fn project(all: &[&Fact], by_id: &BTreeMap<EntryId, &Fact>, work: &mut Work) -> Result<View> {
    let mut view = structure(all, work)?;
    for deletion in all.iter().filter(|fact| matches!(fact.change, Change::Delete { .. })) {
        let context = context(deletion, by_id);
        let scope = structure(&context, work)?;
        let target = canonical(&view, deletion.change.entry())?;
        let observed_target = canonical(&scope, deletion.change.entry())?;
        for node in scope.entries.values() {
            let Some(chain) = branch(&scope, node.id, observed_target, work)? else {
                continue;
            };
            let current = canonical(&view, node.id)?;
            let chain: BTreeSet<_> = chain.iter().map(|id| view.canonical[id]).collect();
            work.charge(all.len())?;
            let preserved = current != target
                && all.iter().any(|edit| {
                    if deletion.observed.contains(&edit.id) || edit.observed.contains(&deletion.id)
                    {
                        return false;
                    }
                    match &edit.change {
                        Change::Edit { entry, parent, name, content } => {
                            let affected = view.canonical.get(entry).copied();
                            (*content && affected == Some(current))
                                || ((parent.is_some() || name.is_some())
                                    && affected
                                        .is_some_and(|id| id != target && chain.contains(&id)))
                        }
                        _ => false,
                    }
                });
            if !preserved {
                view.entries.get_mut(&current).expect("canonical entry exists").visible = false;
            }
        }
        // An unseen automatic duplicate inherits deletion of its incarnation.
        view.entries.get_mut(&target).ok_or(Error::NotFound("deletion target"))?.visible = false;
    }
    let survivors: Vec<_> =
        view.entries.values().filter(|entry| entry.visible).map(|entry| entry.id).collect();
    for id in survivors {
        let mut parent = view.entries[&id].parent;
        for _ in 0..MAX_DEPTH {
            let Some(id) = parent else { break };
            work.charge(1)?;
            let ancestor = view.entries.get_mut(&id).expect("resolved ancestry exists");
            ancestor.visible = true;
            parent = ancestor.parent;
        }
        if parent.is_some() {
            return Err(Error::LimitExceeded);
        }
    }
    Ok(view)
}
fn context<'a>(fact: &Fact, by_id: &BTreeMap<EntryId, &'a Fact>) -> Vec<&'a Fact> {
    fact.observed.iter().map(|id| by_id[id]).collect()
}
fn canonical(view: &View, entry: EntryId) -> Result<EntryId> {
    view.canonical.get(&entry).copied().ok_or(Error::NotFound("namespace entry dependency"))
}
fn branch(
    view: &View,
    start: EntryId,
    target: EntryId,
    work: &mut Work,
) -> Result<Option<Vec<EntryId>>> {
    let mut chain = Vec::new();
    let mut current = Some(start);
    for _ in 0..MAX_DEPTH {
        let Some(id) = current else { return Ok(None) };
        work.charge(1)?;
        chain.push(id);
        if id == target {
            return Ok(Some(chain));
        }
        current = view.entries.get(&id).ok_or(Error::NotFound("namespace parent"))?.parent;
    }
    if current.is_none() { Ok(None) } else { Err(Error::LimitExceeded) }
}

#[derive(Clone, Copy)]
struct Edge<'a> {
    owner: EntryId,
    parent: Option<EntryId>,
    event: &'a Fact,
}
fn structure(facts: &[&Fact], work: &mut Work) -> Result<View> {
    let canonical = canonical_births(facts, work)?;
    let mut entries = BTreeMap::new();
    let mut edges = Vec::new();
    let mut names: BTreeMap<EntryId, &Fact> = BTreeMap::new();
    for fact in facts {
        let id = *canonical.get(&fact.change.entry()).ok_or(Error::NotFound("namespace birth"))?;
        match &fact.change {
            Change::Birth { parent, name, kind, .. } => {
                entries.entry(id).or_insert(Entry {
                    id,
                    parent: None,
                    name: name.clone(),
                    requested_name: name.clone(),
                    kind: *kind,
                    visible: true,
                    parent_event: fact.id,
                    name_event: fact.id,
                });
                edges.push(Edge { owner: id, parent: *parent, event: fact });
                if names.get(&id).is_none_or(|prior| rank(fact) > rank(prior)) {
                    names.insert(id, fact);
                }
            }
            Change::Edit { parent, name, .. } => {
                if let Some(parent) = parent {
                    edges.push(Edge { owner: id, parent: *parent, event: fact });
                }
                if name.is_some() && names.get(&id).is_none_or(|prior| rank(fact) > rank(prior)) {
                    names.insert(id, fact);
                }
            }
            Change::Delete { .. } => {}
        }
    }
    let mut view = root_structure(entries, canonical, edges, work)?;
    for (id, event) in names {
        let name = match &event.change {
            Change::Birth { name, .. } => name,
            Change::Edit { name: Some(name), .. } => name,
            _ => unreachable!(),
        };
        let entry = view.entries.get_mut(&id).expect("name birth exists");
        entry.name = name.clone();
        entry.requested_name = name.clone();
        entry.name_event = event.id;
    }
    Ok(view)
}
fn validate_edits(
    facts: &[&Fact],
    by_id: &BTreeMap<EntryId, &Fact>,
    work: &mut Work,
) -> Result<()> {
    for fact in facts {
        match &fact.change {
            Change::Birth { .. } => validate_birth(fact, by_id, work)?,
            Change::Edit { .. } => {
                let scope = project(&context(fact, by_id), by_id, work)?;
                validate_assignment(fact, &scope, work)?;
            }
            Change::Delete { entry } => {
                canonical(&structure(&context(fact, by_id), work)?, *entry)?;
            }
        }
    }
    Ok(())
}

fn validate_assignment(fact: &Fact, scope: &View, work: &mut Work) -> Result<()> {
    let Change::Edit { entry, parent, name, content } = &fact.change else { unreachable!() };
    let id = canonical(scope, *entry)?;
    let before = &scope.entries[&id];
    if !before.visible {
        return Err(Error::NotFound("deleted edit target"));
    }
    if *content && before.kind != Kind::File {
        return Err(Error::InvalidInput("content edit requires a file"));
    }
    if name.as_ref().is_some_and(|name| *name == before.requested_name) {
        return Err(Error::InvalidInput("no-op name assignment"));
    }
    if let Some(parent) = parent {
        let parent = parent.map(|id| canonical(scope, id)).transpose()?;
        if parent == before.parent {
            return Err(Error::InvalidInput("no-op parent assignment"));
        }
        if let Some(parent) = parent
            && (!scope.entries[&parent].visible
                || scope.entries[&parent].kind != Kind::Directory
                || branch(scope, parent, id, work)?.is_some())
        {
            return Err(Error::InvalidInput("invalid local parent assignment"));
        }
    }
    Ok(())
}

/// A birth records the exact observed departure and deletion events affecting
/// identities occupying its logical slot at the event's own causal cut. A
/// deletion after departure belongs to the new location, not the vacated slot.
/// Learning later facts never rewrites a birth's recorded context. The full
/// observed projection provides canonical IDs; each event's own context gives
/// its historical ancestry without treating a wall clock as causal evidence.
pub(super) fn automatic_context(
    birth: &Fact,
    facts: &[Fact],
) -> Result<(Vec<EntryId>, Vec<EntryId>)> {
    let admission::Classified { facts, mut work, .. } = admission::classify(facts, true)?;
    let by_id: BTreeMap<_, _> = facts.iter().map(|fact| (fact.id, fact)).collect();
    if by_id.len() != birth.observed.len()
        || birth.observed.iter().any(|id| !by_id.contains_key(id))
    {
        return Err(Error::NotFound("automatic directory observation source"));
    }
    birth_context(birth, &by_id, &mut work)
}

fn birth_context(
    birth: &Fact,
    by_id: &BTreeMap<EntryId, &Fact>,
    work: &mut Work,
) -> Result<(Vec<EntryId>, Vec<EntryId>)> {
    let Change::Birth { parent, name, .. } = &birth.change else { unreachable!() };
    let observed = context(birth, by_id);
    let scope = structure(&observed, work)?;
    let parent = parent.map(|id| canonical(&scope, id)).transpose()?;
    let at_slot = |entry: &Entry| -> Result<bool> {
        Ok(entry.kind == Kind::Directory
            && entry.requested_name == *name
            && entry.parent.map(|id| canonical(&scope, id)).transpose()? == parent)
    };
    let mut departures = BTreeSet::new();
    for event in &observed {
        if let Change::Edit { entry, parent: changed_parent, name: changed_name, .. } =
            &event.change
            && (changed_parent.is_some() || changed_name.is_some())
        {
            let before = structure(&context(event, by_id), work)?;
            let old = &before.entries[&canonical(&before, *entry)?];
            if at_slot(old)? {
                departures.insert(event.id);
            }
        }
    }
    let mut deletions = Vec::new();
    for deletion in observed.iter().filter(|event| matches!(event.change, Change::Delete { .. })) {
        let before = structure(&context(deletion, by_id), work)?;
        let target = canonical(&before, deletion.change.entry())?;
        for entry in before.entries.values() {
            work.charge(1)?;
            if at_slot(entry)? && branch(&before, entry.id, target, work)?.is_some() {
                deletions.push(deletion.id);
                break;
            }
        }
    }
    Ok((deletions, departures.into_iter().collect()))
}

fn bidi(c: char) -> bool {
    matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}
fn space_or_control(c: char) -> bool {
    matches!(c, '\u{0000}'..='\u{0020}' | '\u{007f}'..='\u{009f}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}
fn fit(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    assert!(budget >= 3);
    let mut end = budget - 3;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
fn label(origin: &Origin) -> String {
    let mut label = String::new();
    let mut pending_space = false;
    for c in origin.label.as_deref().unwrap_or("").chars() {
        if bidi(c) {
            continue;
        }
        if space_or_control(c) {
            pending_space = !label.is_empty();
            continue;
        }
        if pending_space {
            label.push(' ');
            pending_space = false;
        }
        label.push(if matches!(c, '/' | '\\') { '-' } else { c });
    }
    if label.is_empty() { origin.installation.to_string() } else { fit(&label, 64) }
}
fn candidate(entry: &Entry, label: &str, number: usize) -> String {
    let name = &entry.requested_name;
    let extension = if entry.kind == Kind::File {
        name.rfind('.')
            .filter(|&index| index > 0 && index + 1 < name.len() && name.len() - index <= 32)
    } else {
        None
    };
    let (stem, extension) = extension.map_or((name.as_str(), ""), |index| name.split_at(index));
    let suffix = if number == 1 {
        format!(" (conflict {label}){extension}")
    } else {
        format!(" (conflict {label}, {number}){extension}")
    };
    format!("{}{}", fit(stem, MAX_COMPONENT_BYTES - suffix.len()), suffix)
}
fn allocate_names(
    view: &mut View,
    facts: &BTreeMap<EntryId, &Fact>,
    work: &mut Work,
) -> Result<()> {
    let mut directories: BTreeMap<Option<EntryId>, Vec<EntryId>> = BTreeMap::new();
    for entry in view.entries.values().filter(|entry| entry.visible) {
        directories.entry(entry.parent).or_default().push(entry.id);
    }
    for children in directories.values() {
        let mut occupied = BTreeSet::new();
        let mut winners = BTreeSet::new();
        for id in children.iter().rev() {
            work.charge(1)?;
            if occupied.insert(view.entries[id].requested_name.clone()) {
                winners.insert(*id);
            }
        }
        let mut next = BTreeMap::new();
        for id in children.iter().rev().filter(|id| !winners.contains(id)) {
            let entry = &view.entries[id];
            let event = [facts[&entry.parent_event], facts[&entry.name_event]]
                .into_iter()
                .max_by_key(|fact| rank(fact))
                .expect("two selected assignments");
            let label = label(&event.origin);
            let key = (entry.requested_name.clone(), label.clone(), entry.kind == Kind::File);
            let mut number = *next.get(&key).unwrap_or(&1);
            let chosen = loop {
                work.charge(1)?;
                if number > children.len() {
                    return Err(Error::LimitExceeded);
                }
                let name = candidate(entry, &label, number);
                number += 1;
                if occupied.insert(name.clone()) {
                    break name;
                }
            };
            next.insert(key, number);
            view.entries.get_mut(id).expect("allocated entry exists").name = chosen;
        }
    }
    for entry in view.entries.values().filter(|entry| entry.visible) {
        let mut length = entry.name.len();
        let mut parent = entry.parent;
        for _ in 1..MAX_DEPTH {
            let Some(id) = parent else { break };
            work.charge(1)?;
            length =
                length.checked_add(view.entries[&id].name.len() + 1).ok_or(Error::LimitExceeded)?;
            parent = view.entries[&id].parent;
        }
        if parent.is_some() || length > MAX_PATH_BYTES {
            return Err(Error::LimitExceeded);
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "namespace_tests.rs"]
mod tests;

fn canonical_births(facts: &[&Fact], work: &mut Work) -> Result<BTreeMap<EntryId, EntryId>> {
    let births: BTreeMap<_, _> = facts
        .iter()
        .filter_map(|fact| match fact.change {
            Change::Birth { entry, .. } => Some((entry, *fact)),
            _ => None,
        })
        .collect();
    let mut canonical: BTreeMap<_, _> = births.keys().map(|id| (*id, *id)).collect();
    for _ in 0..=births.len() {
        let mut groups = BTreeMap::new();
        let mut changed = false;
        for (id, fact) in &births {
            work.charge(1)?;
            let Change::Birth { parent, name, automatic, deletions, departures, .. } = &fact.change
            else {
                unreachable!()
            };
            let parent = parent
                .map(|id| canonical.get(&id).copied().ok_or(Error::NotFound("birth parent")))
                .transpose()?;
            if !automatic {
                continue;
            }
            let key = (parent, name, deletions, departures);
            let candidate = (fact.clock, *id);
            let winner = groups.entry(key).or_insert(candidate);
            if candidate > *winner {
                *winner = candidate;
            }
        }
        let mut next = canonical.clone();
        for (id, fact) in &births {
            work.charge(1)?;
            let Change::Birth { parent, name, automatic, deletions, departures, .. } = &fact.change
            else {
                unreachable!()
            };
            if !automatic {
                continue;
            }
            let parent = parent.map(|id| canonical[&id]);
            let winner = groups[&(parent, name, deletions, departures)].1;
            changed |= canonical[id] != winner;
            next.insert(*id, winner);
        }
        canonical = next;
        if !changed {
            break;
        }
    }
    Ok(canonical)
}

fn root_structure(
    mut entries: BTreeMap<EntryId, Entry>,
    canonical: BTreeMap<EntryId, EntryId>,
    mut edges: Vec<Edge<'_>>,
    work: &mut Work,
) -> Result<View> {
    edges.sort_by_key(|edge| (rank(edge.event), edge.owner));
    for edge in &mut edges {
        work.charge(1)?;
        edge.parent = edge
            .parent
            .map(|id| canonical.get(&id).copied().ok_or(Error::NotFound("parent dependency")))
            .transpose()?;
        if edge.parent.is_some_and(|id| entries[&id].kind != Kind::Directory) {
            return Err(Error::InvalidInput("parent is not a directory"));
        }
        let entry = entries.get_mut(&edge.owner).expect("edge birth exists");
        entry.parent = edge.parent;
        entry.parent_event = edge.event.id;
    }
    let mut view = View { entries, canonical };
    let mut rooted = BTreeSet::new();
    for _ in 0..=view.entries.len() {
        for _ in 0..MAX_DEPTH {
            let mut added = false;
            for entry in view.entries.values() {
                work.charge(1)?;
                if entry.parent.is_none_or(|id| rooted.contains(&id)) {
                    added |= rooted.insert(entry.id);
                }
            }
            if !added {
                break;
            }
        }
        if rooted.len() == view.entries.len() {
            break;
        }
        let fallback = edges
            .iter()
            .rev()
            .find(|edge| {
                !rooted.contains(&edge.owner) && edge.parent.is_none_or(|id| rooted.contains(&id))
            })
            .ok_or(Error::InvalidInput("namespace has no rooted parent history"))?;
        work.charge(edges.len())?;
        let entry = view.entries.get_mut(&fallback.owner).expect("fallback birth exists");
        entry.parent = fallback.parent;
        entry.parent_event = fallback.event.id;
    }
    Ok(view)
}

fn creation_scope(
    opened: &FactCreationContext,
    by_id: &BTreeMap<EntryId, &Fact>,
    work: &mut Work,
) -> Result<View> {
    work.charge(opened.observed.len())?;
    let largest =
        opened.observed.iter().map(|id| by_id[id]).max_by_key(|prior| prior.observed.len());
    if let Some(prior) = largest.filter(|prior| prior.observed.len() + 1 == opened.observed.len()) {
        work.charge(opened.observed.len())?;
        if !opened.observed.iter().filter(|id| **id != prior.id).eq(prior.observed.iter()) {
            return Err(Error::InvalidInput("creation context is not closed"));
        }
    } else {
        for id in &opened.observed {
            let prior = by_id[id];
            work.charge(prior.observed.len())?;
            if prior.observed.iter().any(|id| !opened.observed.contains(id)) {
                return Err(Error::InvalidInput("creation context is not closed"));
            }
        }
    }
    let opened: Vec<_> = opened.observed.iter().map(|id| by_id[id]).collect();
    let mut opened_scope = project(&opened, by_id, work)?;
    allocate_names(&mut opened_scope, by_id, work)?;
    Ok(opened_scope)
}

fn validate_birth_destination(
    fact: &Fact,
    by_id: &BTreeMap<EntryId, &Fact>,
    work: &mut Work,
) -> Result<View> {
    let Change::Birth { parent, name, creation_context, .. } = &fact.change else { unreachable!() };
    let mut scope = project(&context(fact, by_id), by_id, work)?;
    allocate_names(&mut scope, by_id, work)?;
    let destination = parent.map(|id| canonical(&scope, id)).transpose()?;
    work.charge(scope.entries.len())?;
    let opened_scope;
    let (absence, opened_destination) = if let Some(opened) =
        creation_context.as_ref().filter(|opened| opened.observed != fact.observed)
    {
        opened_scope = creation_scope(opened, by_id, work)?;
        let destination = parent.map(|id| canonical(&opened_scope, id)).transpose()?;
        if destination.is_some_and(|id| !opened_scope.entries[&id].visible) {
            return Err(Error::NotFound("deleted creation parent"));
        }
        (&opened_scope, destination)
    } else {
        (&scope, destination)
    };
    if absence
        .entries
        .values()
        .any(|entry| entry.visible && entry.parent == opened_destination && entry.name == *name)
    {
        return Err(Error::AlreadyExists("observed birth destination"));
    }
    if let Some(parent) = parent {
        if !scope.entries[&canonical(&scope, *parent)?].visible {
            return Err(Error::NotFound("deleted birth parent"));
        }
        if scope.entries[&canonical(&scope, *parent)?].kind != Kind::Directory {
            return Err(Error::InvalidInput("birth parent is not a directory"));
        }
    }
    Ok(scope)
}

fn validate_birth(fact: &Fact, by_id: &BTreeMap<EntryId, &Fact>, work: &mut Work) -> Result<()> {
    let Change::Birth { parent, name, automatic, deletions, departures, .. } = &fact.change else {
        unreachable!()
    };
    if *automatic {
        let expected = birth_context(fact, by_id, work)?;
        if expected != (deletions.clone(), departures.clone()) {
            return Err(Error::InvalidInput("inexact directory birth context"));
        }
    }
    for deletion in deletions {
        if !fact.observed.contains(deletion)
            || !matches!(by_id[deletion].change, Change::Delete { .. })
        {
            return Err(Error::InvalidInput("birth deletion context is not observed"));
        }
    }
    let scope = validate_birth_destination(fact, by_id, work)?;
    for departure in departures {
        if !fact.observed.contains(departure) {
            return Err(Error::InvalidInput("unobserved directory departure"));
        }
        let prior = by_id[departure];
        let Change::Edit { entry, parent: changed_parent, name: changed_name, .. } = &prior.change
        else {
            return Err(Error::InvalidInput("departure is not a namespace edit"));
        };
        let before = structure(&context(prior, by_id), work)?;
        let old = &before.entries[&canonical(&before, *entry)?];
        let old_parent = old.parent.map(|id| canonical(&scope, id)).transpose()?;
        let new_parent = parent.map(|id| canonical(&scope, id)).transpose()?;
        if old.kind != Kind::Directory
            || old_parent != new_parent
            || old.requested_name != *name
            || (changed_parent.is_none() && changed_name.is_none())
        {
            return Err(Error::InvalidInput("departure does not vacate birth location"));
        }
    }
    Ok(())
}
