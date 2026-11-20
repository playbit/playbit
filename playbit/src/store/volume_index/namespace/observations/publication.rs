//! Local authoring reads the causal head and affected rows, not retained history.
//! Newly observed opaque dependencies require bounded individual status checks.
//! Receipt reconciliation rebuilds the head from the fully validated ledger.
//! Both paths write head and rows in one transaction.
use super::*;

const HEAD: &[u8] = b"namespace-context";
const MAX_PUBLICATION_EVENTS: usize = 3;

#[derive(Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Head {
    #[serde(with = "super::super::causal_context")]
    context: Vec<EntryId>,
    // Includes excluded history and opaque dependencies, so incremental local
    // publication enforces the same reopen capacity as receipt reconciliation.
    #[serde(with = "super::super::causal_context")]
    known: Vec<EntryId>,
    events: usize,
    bytes: usize,
    clock: u64,
    births_only: bool,
}
impl Head {
    pub(super) fn from_ledger(ledger: &Ledger) -> Result<Self> {
        let mut bytes = 0usize;
        let mut known = BTreeSet::new();
        for (id, event) in &ledger.events {
            bytes = bytes.checked_add(event.validate()?).ok_or(Error::LimitExceeded)?;
            known.insert(*id);
            known.extend(event.source.observed.iter().copied());
        }
        check_materialization(ledger.events.len(), known.len())?;
        let context = ledger.context()?;
        let mut head = Self {
            context,
            known: known.into_iter().collect(),
            events: ledger.events.len(),
            bytes,
            ..Self::default()
        };
        for (id, event) in &ledger.events {
            if head.context.binary_search(id).is_ok() {
                head.observe_clock(&event.source);
            }
        }
        Ok(head)
    }
    fn observe_clock(&mut self, source: &Witness) {
        if source.clock > self.clock {
            self.clock = source.clock;
            self.births_only = source.birth;
        } else if source.clock == self.clock {
            self.births_only &= source.birth;
        }
    }
    pub(super) fn load(local: &LocalIndexes<'_, '_, impl Storage>) -> Result<Self> {
        let Some(bytes) = local.get(HEAD)? else {
            if let Some(item) = local.iter_from(PREFIX)?.next() {
                let (key, _) = item?;
                if key.starts_with(PREFIX) {
                    return Err(Error::corrupt("namespace observation head is missing"));
                }
            }
            return Ok(Self::default());
        };
        Self::decode(&bytes)
    }
    pub(super) fn load_read(tx: &kvdb::ReadTxn<'_, impl Storage>) -> Result<Self> {
        let head = match tx.local_get(HEAD)? {
            Some(bytes) => Head::decode(&bytes)?,
            None => {
                if let Some(item) = tx.local_iter_from(PREFIX)?.next() {
                    let (key, _) = item?;
                    if key.starts_with(PREFIX) {
                        return Err(Error::corrupt("namespace observation head is missing"));
                    }
                }
                Head::default()
            }
        };
        Ok(head)
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > super::super::super::MAX_ENTRY_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        let head: Self = decode(bytes)?;
        super::super::validate_context(&head.context)?;
        super::super::validate_context(&head.known)?;
        check_materialization(head.events, head.known.len())?;
        if head.events > MAX_FACTS
            || head.bytes > MAX_SOURCE_BYTES
            || head.bytes < head.events * 512
            || head.events == 0
                && (!head.known.is_empty() || !head.context.is_empty() || head.bytes != 0)
            || head.context.iter().any(|id| head.known.binary_search(id).is_err())
            || (head.clock == 0) != head.context.is_empty()
            || head.clock == 0 && head.births_only
        {
            return Err(Error::corrupt("invalid namespace observation head"));
        }
        Ok(head)
    }
    pub(super) fn encoded(&self) -> Result<Vec<u8>> {
        let bytes = encode(self)?;
        if bytes.len() > super::super::super::MAX_ENTRY_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(bytes)
    }
    pub(super) fn write(&self, local: &mut LocalIndexes<'_, '_, impl Storage>) -> Result<()> {
        local.put(HEAD, &self.encoded()?)?;
        Ok(())
    }
}

pub struct Publication {
    head: Head,
    additions: Vec<(EntryId, Observation)>,
}
impl Publication {
    pub fn capture(tx: &kvdb::ReadTxn<'_, impl Storage>) -> Result<super::super::CreationContext> {
        Ok(super::super::CreationContext { observed: Head::load_read(tx)?.context })
    }
    /// Load and persist within the same source write transaction. The caller
    /// must abort that transaction on any error, including a write failure.
    pub fn load(local: &LocalIndexes<'_, '_, impl Storage>) -> Result<Self> {
        Ok(Self { head: Head::load(local)?, additions: Vec::new() })
    }
    pub fn context(&self) -> Vec<EntryId> {
        self.head.context.clone()
    }
    pub(crate) fn heap_bytes(&self) -> usize {
        size_of::<EntryId>() * (self.head.context.capacity() + self.head.known.capacity())
            + self.additions.capacity() * size_of::<(EntryId, Observation)>()
            + self
                .additions
                .iter()
                .map(|(_, row)| {
                    // Charge each context independently, even if its Arc is shared.
                    row.source.observed.heap_bytes()
                        + row.source.observed.len() * size_of::<EntryId>()
                })
                .sum::<usize>()
    }
    /// Equal-clock parent/child births are the only coalesced transaction form
    /// admitted by the namespace protocol. Edits need a later clock.
    pub fn publish(
        &mut self,
        source: Witness,
        local: &LocalIndexes<'_, '_, impl Storage>,
    ) -> Result<()> {
        if source.clock < self.head.clock
            || source.clock == self.head.clock && !(source.birth && self.head.births_only)
        {
            return Err(Error::InvalidInput("namespace edit requires a later transaction clock"));
        }
        self.observe_with_status(source, local, Status::Authored)
    }
    pub fn observe(
        &mut self,
        source: Witness,
        local: &LocalIndexes<'_, '_, impl Storage>,
    ) -> Result<()> {
        self.observe_with_status(source, local, Status::Provisional)
    }
    fn observe_with_status(
        &mut self,
        source: Witness,
        local: &LocalIndexes<'_, '_, impl Storage>,
        status: Status,
    ) -> Result<()> {
        assert!(matches!(status, Status::Provisional | Status::Authored));
        let charge = source.validate()?;
        let id = source.id()?;
        if let Some((_, prior)) = self.additions.iter().find(|(key, _)| *key == id) {
            return Self::check_existing(prior, &source, &self.head.context);
        }
        if let Some(bytes) = local.get(&[PREFIX, id.as_bytes()].concat())? {
            if bytes.len() > super::super::super::MAX_ENTRY_RECORD_BYTES {
                return Err(Error::LimitExceeded);
            }
            let prior: Observation = decode(&bytes)?;
            return Self::check_existing(&prior, &source, &self.head.context);
        }
        if self.additions.len() == MAX_PUBLICATION_EVENTS || self.head.events == MAX_FACTS {
            return Err(Error::LimitExceeded);
        }
        let bytes = self.head.bytes.checked_add(charge).ok_or(Error::LimitExceeded)?;
        if bytes > MAX_SOURCE_BYTES {
            return Err(Error::LimitExceeded);
        }
        self.check_dependencies(&source, local)?;
        let mut context: BTreeSet<_> = self.head.context.iter().copied().collect();
        context.extend(source.observed.iter().copied());
        context.insert(id);
        if context.len() >= MAX_FACTS {
            return Err(Error::LimitExceeded);
        }
        let mut known: BTreeSet<_> = self.head.known.iter().copied().collect();
        known.extend(source.observed.iter().copied());
        known.insert(id);
        check_materialization(self.head.events + 1, known.len())?;
        if known.len() >= MAX_FACTS {
            return Err(Error::LimitExceeded);
        }
        self.head.context = context.into_iter().collect();
        self.head.known = known.into_iter().collect();
        self.head.events += 1;
        self.head.bytes = bytes;
        self.head.observe_clock(&source);
        self.additions.push((id, Observation { source, status, assignment: None }));
        Ok(())
    }
    fn check_existing(prior: &Observation, source: &Witness, context: &[EntryId]) -> Result<()> {
        prior.validate()?;
        if prior.source != *source {
            return Err(Error::corrupt("namespace observation identity reused"));
        }
        if matches!(prior.status, Status::Rejected | Status::Excluded)
            || context.binary_search(&source.id()?).is_err()
        {
            return Err(Error::AccessDenied(
                "namespace source or its causal dependency was rejected",
            ));
        }
        Ok(())
    }
    fn check_dependencies(
        &self,
        source: &Witness,
        local: &LocalIndexes<'_, '_, impl Storage>,
    ) -> Result<()> {
        let mut source_bytes = 0usize;
        for dependency in &source.observed {
            // The maintained head already excludes rejected provisional chains.
            // Only newly observed opaque IDs need individual status checks.
            if self.head.context.binary_search(dependency).is_ok() {
                continue;
            }
            if let Some(bytes) = local.get(&[PREFIX, dependency.as_bytes()].concat())? {
                if bytes.len() > super::super::super::MAX_ENTRY_RECORD_BYTES {
                    return Err(Error::LimitExceeded);
                }
                source_bytes = source_bytes.checked_add(bytes.len()).ok_or(Error::LimitExceeded)?;
                if source_bytes > MAX_SOURCE_BYTES {
                    return Err(Error::LimitExceeded);
                }
                let prior: Observation = decode(&bytes)?;
                if prior.source.id()? != *dependency {
                    return Err(Error::corrupt("namespace dependency identity changed"));
                }
                return Err(if prior.status == Status::Confirmed {
                    Error::corrupt("confirmed namespace dependency is absent from its head")
                } else {
                    Error::AccessDenied("namespace dependency is not usable")
                });
            }
        }
        Ok(())
    }
    pub(crate) fn witness(
        &self,
        id: EntryId,
        local: &LocalIndexes<'_, '_, impl Storage>,
    ) -> Result<Witness> {
        if self.head.context.binary_search(&id).is_err() {
            return Err(Error::AccessDenied("namespace event is not observed"));
        }
        if let Some((_, event)) = self.additions.iter().find(|(key, _)| *key == id) {
            return Ok(event.source.clone());
        }
        let bytes = local
            .get(&[PREFIX, id.as_bytes()].concat())?
            .ok_or(Error::NotFound("namespace observation source"))?;
        if bytes.len() > super::super::super::MAX_ENTRY_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        let event: Observation = decode(&bytes)?;
        if event.source.id()? != id || matches!(event.status, Status::Rejected | Status::Excluded) {
            return Err(Error::corrupt("invalid observed namespace source"));
        }
        event.validate()?;
        Ok(event.source)
    }
    pub fn write(self, local: &mut LocalIndexes<'_, '_, impl Storage>) -> Result<()> {
        // Encode all candidates before the first write; storage failures still
        // require aborting the caller's complete source transaction.
        let mut encoded = Vec::with_capacity(self.additions.len());
        for (id, event) in self.additions {
            let bytes = encode(&event)?;
            if bytes.len() > super::super::super::MAX_ENTRY_RECORD_BYTES {
                return Err(Error::LimitExceeded);
            }
            encoded.push(([PREFIX, id.as_bytes()].concat(), bytes));
        }
        self.head.write(local)?;
        for (key, bytes) in encoded {
            local.put(&key, &bytes)?;
        }
        Ok(())
    }
}
