//! Closed causal contexts keep their exact wire IDs. A materialized source cut
//! shares one sorted dictionary and stores each context as ordinal bits.
use super::*;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Context(Repr);
#[derive(Clone, Debug)]
enum Repr {
    Ids(Arc<[EntryId]>),
    Bits { ids: Arc<[EntryId]>, words: Arc<[u64]>, count: usize },
}
impl From<Vec<EntryId>> for Context {
    fn from(ids: Vec<EntryId>) -> Self {
        Self(Repr::Ids(ids.into()))
    }
}
impl PartialEq for Context {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}
impl Eq for Context {}
impl Serialize for Context {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match &self.0 {
            Repr::Ids(ids) => causal_context::serialize(ids, serializer),
            Repr::Bits { .. } => {
                let ids: Vec<_> = self.iter().copied().collect();
                causal_context::serialize(&ids, serializer)
            }
        }
    }
}
impl<'de> Deserialize<'de> for Context {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        causal_context::deserialize(deserializer).map(Self::from)
    }
}
impl Context {
    pub fn len(&self) -> usize {
        match &self.0 {
            Repr::Ids(ids) => ids.len(),
            Repr::Bits { count, .. } => *count,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn contains(&self, id: &EntryId) -> bool {
        match &self.0 {
            Repr::Ids(ids) => ids.binary_search(id).is_ok(),
            Repr::Bits { ids, words, .. } => ids.binary_search(id).is_ok_and(|index| {
                words.get(index / 64).is_some_and(|word| word & (1 << (index % 64)) != 0)
            }),
        }
    }
    pub fn iter(&self) -> Iter<'_> {
        match &self.0 {
            Repr::Ids(ids) => Iter { ids, words: None, position: 0 },
            Repr::Bits { ids, words, .. } => Iter { ids, words: Some(words), position: 0 },
        }
    }
    pub(super) fn ordered(&self) -> bool {
        match &self.0 {
            Repr::Ids(ids) => ordered(ids),
            Repr::Bits { ids, words, count } => {
                ids.len() <= MAX_FACTS
                    && ordered(ids)
                    && words.len() == ids.len().div_ceil(64)
                    && words.iter().map(|word| word.count_ones() as usize).sum::<usize>() == *count
                    && (ids.len().is_multiple_of(64)
                        || words.last().is_none_or(|word| word >> (ids.len() % 64) == 0))
            }
        }
    }
    /// The caller owns and charges the shared dictionary separately. Allocation
    /// happens only after all IDs are present; unknown dependencies are retained.
    pub(crate) fn compact(&mut self, dictionary: &Arc<[EntryId]>) -> Result<()> {
        if dictionary.len() > MAX_FACTS || !ordered(dictionary) || !self.ordered() {
            return Err(Error::InvalidInput("invalid namespace context dictionary"));
        }
        // Both sequences are ordered. Walk the dictionary once per pass;
        // repeated binary searches multiply the cost of dense causal contexts.
        let mut available = dictionary.iter();
        if self.iter().any(|id| available.find(|candidate| *candidate >= id) != Some(id)) {
            return Err(Error::InvalidInput("namespace context dictionary omits a dependency"));
        }
        let mut words = vec![0u64; dictionary.len().div_ceil(64)];
        let mut index = 0;
        for id in self.iter() {
            while dictionary[index] < *id {
                index += 1;
            }
            words[index / 64] |= 1 << (index % 64);
        }
        *self =
            Self(Repr::Bits { ids: dictionary.clone(), words: words.into(), count: self.len() });
        Ok(())
    }
    pub(crate) fn heap_bytes(&self) -> usize {
        match &self.0 {
            Repr::Ids(ids) => 32 + size_of_val(ids.as_ref()),
            Repr::Bits { words, .. } => 32 + size_of_val(words.as_ref()),
        }
    }
}
#[derive(Clone)]
pub struct Iter<'a> {
    ids: &'a [EntryId],
    words: Option<&'a [u64]>,
    position: usize,
}
impl<'a> Iterator for Iter<'a> {
    type Item = &'a EntryId;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(words) = self.words {
            while self.position < self.ids.len() {
                let word = words.get(self.position / 64)? & (u64::MAX << (self.position % 64));
                if word != 0 {
                    self.position = self.position / 64 * 64 + word.trailing_zeros() as usize;
                    break;
                }
                self.position = (self.position / 64 + 1) * 64;
            }
        }
        let id = self.ids.get(self.position)?;
        self.position += 1;
        Some(id)
    }
}
impl<'a> IntoIterator for &'a Context {
    type Item = &'a EntryId;
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests compare exact compact contexts and their wire bytes.")]
mod tests {
    use super::*;
    #[test]
    fn compact_context_preserves_word_boundaries_and_opaque_dependencies() {
        // Shared ordinals must preserve canonical order across bitset words;
        // an omitted opaque dependency is an error, never silently discarded.
        let dictionary: Arc<[EntryId]> = (1..=130u16)
            .map(|number| {
                let mut bytes = [0; 15];
                bytes[13..].copy_from_slice(&number.to_be_bytes());
                EntryId::from_bytes(bytes).unwrap()
            })
            .collect::<Vec<_>>()
            .into();
        for indices in [vec![], vec![0, 63, 64, 65, 129], (0..130).collect()] {
            let ids: Vec<_> = indices.iter().map(|index| dictionary[*index]).collect();
            let mut context = Context::from(ids.clone());
            let wire = serde_json::to_vec(&context).unwrap();
            context.compact(&dictionary).unwrap();
            context.compact(&dictionary).unwrap(); // A bitset is also an ordered input.
            assert!(context.ordered());
            assert_eq!(context.len(), ids.len());
            assert_eq!(context.iter().copied().collect::<Vec<_>>(), ids);
            assert_eq!(serde_json::to_vec(&context).unwrap(), wire);
            assert_eq!(serde_json::from_slice::<Context>(&wire).unwrap(), context);
            for id in dictionary.iter() {
                assert_eq!(context.contains(id), ids.contains(id));
            }
        }
        for excluded in [0, 64, 129] {
            let mut missing = Context::from(dictionary.to_vec());
            let incomplete: Arc<[EntryId]> = dictionary
                .iter()
                .enumerate()
                .filter_map(|(index, id)| (index != excluded).then_some(*id))
                .collect::<Vec<_>>()
                .into();
            assert!(missing.compact(&incomplete).is_err());
            assert_eq!(missing.iter().copied().collect::<Vec<_>>(), dictionary.as_ref());
        }
    }
}
