//! Local retained-cut discovery, separate from the replicated entry namespace.
use super::*;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct RecoveryCursor {
    pub cut: Id,
    pub offset: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct RejectedEntry {
    pub cursor: RecoveryCursor,
    pub entry: EntrySummary,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct RejectedEntryPage {
    pub entries: Vec<RejectedEntry>,
    pub next: Option<RecoveryCursor>,
}
