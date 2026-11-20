//! Replica delivery continuity, independent of transport and storage ownership.
//! Admission never advances progress. Hosts stop on terminal/error, skip
//! duplicates, and record only successfully durable nonterminal events.
use super::{ReplicaEvent, ReplicaEventKind, SubscriptionAck};
use std::fmt;

pub struct Cursor {
    epoch: [u8; 16],
    sequence: u64,
    policy_revision: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub enum CursorError {
    Invalid(&'static str),
    Gap,
}
impl fmt::Display for CursorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid(message) => message,
            Self::Gap => "replica delivery sequence gap",
        })
    }
}
impl std::error::Error for CursorError {}
impl Cursor {
    /// Begin at the service's validated subscription acknowledgment. A cursor
    /// never resumes from an arbitrary sequence without initialization/recovery.
    pub fn new(ack: &SubscriptionAck) -> Result<Self, CursorError> {
        ack.validate().map_err(CursorError::Invalid)?;
        Ok(Self { epoch: ack.epoch, sequence: ack.sequence, policy_revision: ack.policy_revision })
    }
    /// false means an already-applied duplicate. Advance only after durability.
    pub fn accept(&self, event: &ReplicaEvent<'_>) -> std::result::Result<bool, CursorError> {
        event.validate().map_err(CursorError::Invalid)?;
        if event.epoch != self.epoch {
            return Err(CursorError::Invalid("replica stream epoch changed"));
        }
        if event.sequence <= self.sequence {
            return Ok(false);
        }
        // A terminal may replace queued deliveries on overflow/revocation. Its
        // authority is the authenticated current epoch, not receipt of bytes
        // deliberately discarded when closing that stream. Terminal envelopes
        // may have revision zero; they never resume delivery.
        if matches!(event.kind, ReplicaEventKind::Gap | ReplicaEventKind::Terminal { .. }) {
            return Ok(true);
        }
        if self.sequence.checked_add(1) != Some(event.sequence) {
            return Err(CursorError::Gap);
        }
        if event.policy_revision < self.policy_revision {
            return Err(CursorError::Invalid("replica policy revision regressed"));
        }
        Ok(true)
    }
    /// Record a previously accepted, nonterminal event after durable apply (or
    /// ordered checkpoint handling). The caller must stop on Gap/Terminal/error.
    pub fn applied(&mut self, event: &ReplicaEvent<'_>) {
        self.record_applied(event.sequence, event.policy_revision);
    }
    /// Same boundary when the durable worker consumed the event's byte buffer.
    /// Sequence/revision must be those admitted before handing it to that worker.
    pub fn record_applied(&mut self, sequence: u64, policy_revision: u64) {
        assert_eq!(self.sequence.checked_add(1), Some(sequence), "applies are contiguous");
        assert!(policy_revision >= self.policy_revision, "applied policy cannot regress");
        self.sequence = sequence;
        self.policy_revision = policy_revision;
    }
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
