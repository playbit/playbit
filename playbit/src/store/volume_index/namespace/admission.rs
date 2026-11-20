//! Causal admission precedes namespace projection. Unvalidated source records
//! never become namespace edits merely because their bytes reached storage.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventState {
    Accepted,
    Pending,
    Rejected,
}
/// Effective location fields contributed after observed-context normalization.
/// Exact source bytes remain unchanged, including omitted redundant fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignments {
    pub name: bool,
    pub parent: bool,
}
impl Assignments {
    pub fn bits(self) -> u8 {
        u8::from(self.name) | (u8::from(self.parent) << 1)
    }
    pub fn from_bits(bits: u8) -> Result<Self> {
        if bits > 3 {
            return Err(Error::InvalidInput("invalid namespace assignment bits"));
        }
        Ok(Self { name: bits & 1 != 0, parent: bits & 2 != 0 })
    }
    fn normalized(change: &Change) -> Self {
        match change {
            Change::Edit { name, parent, .. } => {
                Self { name: name.is_some(), parent: parent.is_some() }
            }
            Change::Birth { .. } | Change::Delete { .. } => Self::default(),
        }
    }
    pub(crate) fn validate_source(self, change: &Change) -> Result<()> {
        let available = Self::normalized(change);
        let combined = matches!(change, Change::Edit { name: Some(_), parent: Some(_), .. });
        if self.name && !available.name
            || self.parent && !available.parent
            || !combined && self != available
        {
            return Err(Error::AccessDenied("namespace assignment absent from source"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admission {
    pub view: View,
    pub events: BTreeMap<EntryId, EventState>,
    pub assignments: BTreeMap<EntryId, Assignments>,
}

/// Bind an event to its entry, immutable revision and exact canonical source.
/// `source_digest` is calculated by the source decoder, never accepted from an
/// observed-ID vector. This distinguishes a denied local payload from a different
/// authoritative payload that used the same entry/revision key.
pub fn event_id(entry: EntryId, revision: EntryId, source_digest: [u8; 32]) -> Result<EntryId> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"app.playbit.namespace.event.v1\0");
    hash.update(entry.as_bytes());
    hash.update(revision.as_bytes());
    hash.update(&source_digest);
    let mut bytes = [0; 15];
    bytes.copy_from_slice(&hash.finalize().as_bytes()[..15]);
    EntryId::from_bytes(bytes)
}

/// Resolve a bounded source cut after entry-level authorization and immutable
/// record admission. Births MUST come only from each entry's single immutable
/// declaration. IDs MUST be calculated with event_id, never supplied by peers.
///
/// Pending records retain their bytes and do not replace an accepted edit.
/// Rejected records and their causal descendants never change the namespace.
/// Exhaustion fails the entire projection; it never accepts a truncated cut.
pub fn admit(facts: &[Fact]) -> Result<Admission> {
    Ok(resolve(facts, false)?.0)
}

pub(super) fn derive(facts: &[Fact]) -> Result<View> {
    Ok(resolve(facts, true)?.0.view)
}

/// Conservative fresh-admission costs, carried only with an exact source cut.
#[derive(Clone, Copy)]
pub(super) struct Budget {
    pub work: usize,
    pub projection: usize,
    pub source_bytes: usize,
}

pub(super) fn admit_budgeted(facts: &[Fact]) -> Result<(Admission, Budget)> {
    resolve(facts, false)
}

fn resolve(facts: &[Fact], strict: bool) -> Result<(Admission, Budget)> {
    let Classified { facts, events, mut work, source_bytes } = classify(facts, strict)?;
    let accepted = facts.iter().map(|fact| (fact.id, fact)).collect();
    let all: Vec<_> = facts.iter().collect();
    let before = work.0;
    let mut view = project(&all, &accepted, &mut work)?;
    let projection = work.0 - before;
    allocate_names(&mut view, &accepted, &mut work)?;
    let assignments =
        facts.iter().map(|fact| (fact.id, Assignments::normalized(&fact.change))).collect();
    Ok((Admission { view, events, assignments }, Budget { work: work.0, projection, source_bytes }))
}

/// A complete-context content append cannot change a deletion-free namespace.
/// The caller owns the exact committed predecessor; no source may have changed.
/// Preserve a bound on *fresh* full admission, not merely this shortcut's work.
pub(super) fn content_append_budget(
    prior: &Admission,
    budget: Budget,
    facts: &[Fact],
    next: &Fact,
) -> Result<Option<Budget>> {
    if !matches!(next.change, Change::Edit { parent: None, name: None, content: true, .. })
        || facts.len() >= MAX_FACTS
        || prior.events.contains_key(&next.id)
        || facts.iter().any(|fact| {
            fact.clock >= next.clock
                || fact.observed.contains(&next.id)
                || matches!(fact.change, Change::Delete { .. })
        })
        || next.observed.len() != prior.assignments.len()
        || !next.observed.iter().eq(prior.assignments.keys())
    {
        return Ok(None);
    }
    let source_bytes =
        budget.source_bytes.checked_add(next.validate_shape()?).ok_or(Error::LimitExceeded)?;
    // Old classification is unchanged. A full run adds one source, the exact
    // prefix comparison, at most one prior projection, and one scope update.
    // Content adds no structural edges, births, names or deletion preservation.
    let work = budget
        .work
        .checked_add(next.observed.len())
        .and_then(|value| value.checked_add(budget.projection))
        .and_then(|value| value.checked_add(2))
        .ok_or(Error::LimitExceeded)?;
    if source_bytes > MAX_SOURCE_BYTES || work > MAX_WORK {
        return Ok(None);
    }
    match validate_assignment(next, &prior.view, &mut Work(0)) {
        Ok(()) => {}
        Err(Error::InvalidInput(_) | Error::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    }
    Ok(Some(Budget { source_bytes, work, ..budget }))
}

pub(super) struct Classified {
    pub facts: Vec<Fact>,
    pub events: BTreeMap<EntryId, EventState>,
    pub work: Work,
    pub source_bytes: usize,
}

/// Normalized facts are resolver-owned copies. Their identity, clock and causal
/// edges still describe the exact unchanged source; only effective assignments
/// omit redundant fields from a combined intent.
pub(super) fn classify(facts: &[Fact], strict: bool) -> Result<Classified> {
    let mut work = Work(0);
    let (by_id, source_bytes) = sources(facts, strict, &mut work)?;
    let mut ordered: Vec<_> = by_id.values().copied().cloned().collect();
    // Equal-clock dependencies are valid only between births. A predecessor
    // has fewer closed observed IDs; winner rank remains independently clock/ID.
    ordered.sort_by_key(|fact| (fact.clock, fact.observed.len(), fact.id));
    let mut events = BTreeMap::new();
    let mut accepted = BTreeMap::new();
    let mut accepted_clock = 0;
    let mut scope = None;
    let mut has_deletion = false;
    for fact in &mut ordered {
        let state = if complete_prefix(fact, &accepted, accepted_clock, &mut work)? {
            EventState::Accepted
        } else {
            dependencies(fact, &by_id, &events, &mut work)?
        };
        if strict {
            match state {
                EventState::Pending => return Err(Error::NotFound("namespace dependency")),
                EventState::Rejected => {
                    return Err(Error::InvalidInput("namespace context is not causally closed"));
                }
                EventState::Accepted => {}
            }
        }
        let state = if state == EventState::Accepted {
            match validate_next(fact, &accepted, &mut scope, has_deletion, &mut work) {
                Ok(()) => EventState::Accepted,
                Err(
                    error @ (Error::InvalidInput(_) | Error::NotFound(_) | Error::AlreadyExists(_)),
                ) => {
                    if strict {
                        return Err(error);
                    }
                    EventState::Rejected
                }
                Err(error) => return Err(error),
            }
        } else {
            state
        };
        events.insert(fact.id, state);
        if state == EventState::Accepted {
            has_deletion |= matches!(fact.change, Change::Delete { .. });
            accepted_clock = fact.clock;
            accepted.insert(fact.id, &*fact);
        } else {
            scope = None;
        }
    }
    ordered.retain(|fact| events[&fact.id] == EventState::Accepted);
    Ok(Classified { facts: ordered, events, work, source_bytes })
}

/// Every accepted predecessor has a closed accepted context. Observing exactly
/// that entire set therefore proves closure without looking up each edge again.
/// The strict clock comparison leaves equal-clock birth rules to dependencies().
fn complete_prefix(
    fact: &Fact,
    accepted: &BTreeMap<EntryId, &Fact>,
    accepted_clock: u64,
    work: &mut Work,
) -> Result<bool> {
    if fact.clock <= accepted_clock || fact.observed.len() != accepted.len() {
        return Ok(false);
    }
    work.charge(fact.observed.len())?;
    Ok(fact.observed.iter().eq(accepted.keys()))
}

fn sources<'a>(
    facts: &'a [Fact],
    strict: bool,
    work: &mut Work,
) -> Result<(BTreeMap<EntryId, &'a Fact>, usize)> {
    if facts.len() > MAX_FACTS {
        return Err(Error::LimitExceeded);
    }
    let mut by_id = BTreeMap::new();
    let mut births = BTreeSet::new();
    let mut source_bytes = 0usize;
    for fact in facts {
        work.charge(1)?;
        source_bytes =
            source_bytes.checked_add(fact.validate_shape()?).ok_or(Error::LimitExceeded)?;
        if source_bytes > MAX_SOURCE_BYTES {
            return Err(Error::LimitExceeded);
        }
        if let Some(prior) = by_id.insert(fact.id, fact) {
            if strict {
                return Err(Error::InvalidInput("duplicate namespace event"));
            }
            if prior != fact {
                return Err(Error::corrupt("namespace source event identity reused"));
            }
            continue;
        }
        if let Change::Birth { entry, .. } = fact.change {
            if !births.insert(entry) {
                return Err(if strict {
                    Error::InvalidInput("duplicate namespace birth")
                } else {
                    Error::corrupt("multiple immutable namespace declarations")
                });
            }
            if births.len() > MAX_ENTRIES {
                return Err(Error::LimitExceeded);
            }
        }
    }
    Ok((by_id, source_bytes))
}

/// Reuse only a complete accepted prefix after an edit that cannot change
/// ancestry or visibility. A partial causal cut, birth, move, or deletion drops
/// the cached scope. No payload or accepted-event evidence is inferred from it.
fn validate_next(
    fact: &mut Fact,
    accepted: &BTreeMap<EntryId, &Fact>,
    cached: &mut Option<View>,
    has_deletion: bool,
    work: &mut Work,
) -> Result<()> {
    let mut prior = cached.take();
    if !matches!(fact.change, Change::Edit { .. }) {
        drop(prior);
        return validate_edits(&[fact], accepted, work);
    }
    // Dependencies already proved every observed ID Accepted. Equal cardinality
    // therefore establishes exact equality with this complete accepted prefix.
    let complete = fact.observed.len() == accepted.len();
    if !complete {
        prior = None;
    }
    let mut scope = match prior {
        Some(scope) => scope,
        None => project(&context(fact, accepted), accepted, work)?,
    };
    normalize_combined_assignment(fact, &scope)?;
    validate_assignment(fact, &scope, work)?;
    if complete
        && !has_deletion
        && let Change::Edit { entry, parent: None, name, .. } = &fact.change
    {
        work.charge(1)?;
        let id = canonical(&scope, *entry)?;
        let entry = scope.entries.get_mut(&id).expect("validated edit target exists");
        if let Some(name) = name {
            // An edit observing the entire prefix has a strictly later clock
            // than every earlier assignment, so this name is its new winner.
            entry.name.clone_from(name);
            entry.requested_name.clone_from(name);
            entry.name_event = fact.id;
        }
        *cached = Some(scope);
    }
    Ok(())
}

fn normalize_combined_assignment(fact: &mut Fact, scope: &View) -> Result<()> {
    let Change::Edit { entry, parent: Some(parent), name: Some(name), .. } = &fact.change else {
        return Ok(());
    };
    let before = &scope.entries[&canonical(scope, *entry)?];
    let redundant_parent = parent.map(|id| canonical(scope, id)).transpose()? == before.parent;
    let redundant_name = *name == before.requested_name;
    let Change::Edit { parent, name, .. } = &mut fact.change else { unreachable!() };
    if redundant_parent {
        *parent = None;
    }
    if redundant_name {
        *name = None;
    }
    // Both fields may disappear. This neutral event remains in causal history
    // but contributes no edge, name winner or deletion-preservation evidence.
    Ok(())
}

pub(super) fn dependencies(
    fact: &Fact,
    by_id: &BTreeMap<EntryId, &Fact>,
    states: &BTreeMap<EntryId, EventState>,
    work: &mut Work,
) -> Result<EventState> {
    let mut pending = false;
    let mut largest: Option<&Fact> = None;
    for id in &fact.observed {
        work.charge(1)?;
        let Some(dependency) = by_id.get(id) else {
            pending = true;
            continue;
        };
        if dependency.observed.len() >= fact.observed.len()
            || dependency.clock > fact.clock
            || (dependency.clock == fact.clock
                && !matches!(
                    (&dependency.change, &fact.change),
                    (Change::Birth { .. }, Change::Birth { .. })
                ))
        {
            return Ok(EventState::Rejected);
        }
        match states.get(id) {
            Some(EventState::Accepted) => {
                if largest.is_none_or(|prior| dependency.observed.len() > prior.observed.len()) {
                    largest = Some(dependency);
                }
            }
            Some(EventState::Rejected) => return Ok(EventState::Rejected),
            Some(EventState::Pending) => pending = true,
            None => return Err(Error::corrupt("causal predecessor was not classified")),
        }
    }
    if let Some(prior) = largest
        && prior.observed.len() + 1 == fact.observed.len()
    {
        // The prior's context was already proved closed. Exact equality to
        // that context plus its event proves this sequential extension closed
        // without rescanning every ancestor's entire closed context.
        work.charge(fact.observed.len())?;
        if !fact.observed.iter().filter(|id| **id != prior.id).eq(prior.observed.iter()) {
            return Ok(EventState::Rejected);
        }
    } else {
        for dependency in fact.observed.iter().filter_map(|id| by_id.get(id)) {
            work.charge(dependency.observed.len())?;
            if dependency.observed.iter().any(|id| !fact.observed.contains(id)) {
                return Ok(EventState::Rejected);
            }
        }
    }
    Ok(if pending { EventState::Pending } else { EventState::Accepted })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_prefix_agrees_with_dependency_validation() {
        let id = |number| EntryId::from_bytes([number; 15]).expect("fixture identity");
        let make = |number, observed: Vec<EntryId>, change| Fact {
            id: id(number),
            clock: u64::from(number),
            observed: observed.into(),
            origin: Origin { installation: Id::from_bytes([1; 16]), label: None },
            change,
        };
        let edit = || Change::Edit { entry: id(1), parent: None, name: None, content: true };
        let facts = [
            make(
                1,
                vec![],
                Change::Birth {
                    creation_context: None,
                    entry: id(1),
                    parent: None,
                    name: "note".into(),
                    kind: Kind::File,
                    automatic: false,
                    deletions: vec![],
                    departures: vec![],
                },
            ),
            make(2, vec![id(1)], edit()),
            make(3, vec![id(99)], edit()),
            make(4, vec![], edit()),
        ];
        let classified = classify(&facts, false).expect("mixed accepted/pending/rejected history");
        assert_eq!(classified.events[&id(3)], EventState::Pending);
        assert_eq!(classified.events[&id(4)], EventState::Rejected);
        let accepted: BTreeMap<_, _> =
            classified.facts.iter().map(|fact| (fact.id, fact)).collect();
        let by_id = facts.iter().map(|fact| (fact.id, fact)).collect();
        let choices = [id(1), id(2), id(3), id(4), id(99)];
        let mut shortcuts = 0;
        for mask in 0..32 {
            for clock in [1, 2, 5] {
                let observed = choices
                    .iter()
                    .enumerate()
                    .filter_map(|(bit, id)| (mask & (1 << bit) != 0).then_some(*id))
                    .collect();
                let mut next = make(5, observed, edit());
                next.clock = clock;
                let shortcut = complete_prefix(&next, &accepted, 2, &mut Work(0))
                    .expect("bounded prefix check");
                assert_eq!(shortcut, mask == 3 && clock == 5);
                if shortcut {
                    shortcuts += 1;
                    assert_eq!(
                        dependencies(&next, &by_id, &classified.events, &mut Work(0))
                            .expect("bounded dependency check"),
                        EventState::Accepted
                    );
                }
            }
        }
        assert_eq!(shortcuts, 1);
    }
}
