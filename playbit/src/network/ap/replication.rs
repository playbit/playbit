//! Bounded live events carry complete database identities in the volume stream.
//! The codec checks framing; admission and source validation are separate.
use crate::store::{DatabaseId, EntryId};
use serde::{Deserialize, Serialize};
mod cursor;
pub use cursor::{Cursor, CursorError};
pub const ALGORITHM: &str = "ap.replica.v1";
pub const MAX_DELTA_BYTES: usize = 64 * 1024;
pub const EVENT_HEADER_BYTES: usize = 52;
pub const MAX_EVENT_BYTES: usize = EVENT_HEADER_BYTES + MAX_DELTA_BYTES;
const MAX_BLOBLOG_BYTES: usize = 2048;
pub use crate::store::authority::TerminalCause;
const _: () = assert!(EVENT_HEADER_BYTES == 4 + 16 + 8 + 8 + DatabaseId::WIRE_BYTES);
const _: () = assert!(MAX_EVENT_BYTES <= super::MAX_FRAME_LEN);
pub const CHECKPOINT: [u8; 2] = [1, 0];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaEventKind<'a> {
    Changes {
        database: DatabaseId,
        delta: &'a [u8],
    },
    /// Selected Index sources and exact full-cut acceptance share one envelope.
    Namespace {
        entry: EntryId,
        delta: &'a [u8],
        evidence: &'a [u8],
    },
    /// Empty data requests catch-up for this exact named append log.
    Bloblog {
        log: EntryId,
        data: &'a [u8],
    },
    CaughtUp,
    Gap,
    Terminal {
        volume: [u8; 16],
        cause: TerminalCause,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaEvent<'a> {
    pub epoch: [u8; 16],
    pub sequence: u64,
    pub policy_revision: u64,
    pub kind: ReplicaEventKind<'a>,
}
pub fn validate_delta(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.is_empty() || bytes.len() > MAX_DELTA_BYTES {
        return Err("replica delta must contain 1 to 65536 bytes");
    }
    Ok(())
}
pub fn validate_checkpoint(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes != CHECKPOINT {
        return Err("invalid replica checkpoint control");
    }
    Ok(())
}
impl<'a> ReplicaEvent<'a> {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.epoch == [0; 16] || self.sequence == 0 {
            return Err("replica event requires nonzero epoch and sequence");
        }
        match self.kind {
            ReplicaEventKind::Changes { delta, .. } => {
                if self.policy_revision == 0 {
                    return Err("database event requires policy revision");
                }
                validate_delta(delta)?;
            }
            ReplicaEventKind::Namespace { delta, evidence, .. } => {
                if self.policy_revision == 0 || delta.len() + evidence.len() + 4 > MAX_DELTA_BYTES {
                    return Err("invalid namespace event bounds or policy revision");
                }
                validate_delta(delta)?;
                crate::store::volume_index::namespace::live::validate_evidence(evidence)
                    .map_err(|_| "invalid namespace event evidence")?;
            }
            ReplicaEventKind::Bloblog { data, .. }
                if data.len() > MAX_BLOBLOG_BYTES || self.policy_revision == 0 =>
            {
                return Err("invalid bloblog event");
            }
            ReplicaEventKind::CaughtUp if self.policy_revision == 0 => {
                return Err("caught-up event requires policy revision");
            }
            ReplicaEventKind::Terminal { volume, .. } if volume == [0; 16] => {
                return Err("terminal requires changed volume identity");
            }
            _ => {}
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        self.validate()?;
        let mut terminal = [0; 17];
        if let ReplicaEventKind::Terminal { volume, cause } = self.kind {
            terminal[..16].copy_from_slice(&volume);
            terminal[16] = cause as u8;
        }
        let (kind, database, payload) = match self.kind {
            ReplicaEventKind::Changes { database, delta } => (1u8, database, delta),
            ReplicaEventKind::Bloblog { log, data } => (6, DatabaseId::Entry(log), data),
            ReplicaEventKind::Namespace { entry, delta, .. } => {
                (7, DatabaseId::Entry(entry), delta)
            }
            ReplicaEventKind::CaughtUp => (2, DatabaseId::Index, &[][..]),
            ReplicaEventKind::Gap => (3, DatabaseId::Index, &[][..]),
            ReplicaEventKind::Terminal { .. } => (4, DatabaseId::Index, terminal.as_slice()),
        };
        let extra = match self.kind {
            ReplicaEventKind::Namespace { evidence, .. } => 4 + evidence.len(),
            _ => 0,
        };
        let mut out = Vec::with_capacity(EVENT_HEADER_BYTES + payload.len() + extra);
        out.extend_from_slice(&[1, kind, 0, 0]);
        out.extend_from_slice(&self.epoch);
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&self.policy_revision.to_le_bytes());
        out.extend_from_slice(&database.to_bytes());
        if let ReplicaEventKind::Namespace { delta, evidence, .. } = self.kind {
            out.extend_from_slice(&(delta.len() as u32).to_le_bytes());
            out.extend_from_slice(delta);
            out.extend_from_slice(evidence);
        } else {
            out.extend_from_slice(payload);
        }
        assert_eq!(out.len(), EVENT_HEADER_BYTES + payload.len() + extra);
        assert!(out.len() <= MAX_EVENT_BYTES);
        Ok(out)
    }
    // fuzz: ap_codec
    pub fn decode(bytes: &'a [u8]) -> Result<Self, &'static str> {
        if !(EVENT_HEADER_BYTES..=MAX_EVENT_BYTES).contains(&bytes.len())
            || bytes[0] != 1
            || bytes[2..4] != [0, 0]
        {
            return Err("invalid replica event header or length");
        }
        let database =
            DatabaseId::from_bytes(bytes[36..52].try_into().expect("fixed identity width"))
                .map_err(|_| "invalid replica database identity")?;
        let payload = &bytes[EVENT_HEADER_BYTES..];
        let kind = match (bytes[1], database) {
            (1, database) => ReplicaEventKind::Changes { database, delta: payload },
            (6, DatabaseId::Entry(log)) => ReplicaEventKind::Bloblog { log, data: payload },
            (7, DatabaseId::Entry(entry)) => {
                let (delta, evidence) =
                    crate::store::volume_index::namespace::live::decode_payload(payload)
                        .map_err(|_| "invalid namespace payload")?;
                ReplicaEventKind::Namespace { entry, delta, evidence }
            }
            (2..=3, DatabaseId::Index) if payload.is_empty() => match bytes[1] {
                2 => ReplicaEventKind::CaughtUp,
                _ => ReplicaEventKind::Gap,
            },
            (4, DatabaseId::Index) if payload.len() == 17 => ReplicaEventKind::Terminal {
                volume: payload[..16].try_into().expect("checked terminal width"),
                cause: match payload[16] {
                    1 => TerminalCause::AuthorityChanged,
                    2 => TerminalCause::ParticipantDenied,
                    3 => TerminalCause::ServiceUnavailable,
                    4 => TerminalCause::EntryAuthorityChanged,
                    _ => return Err("invalid terminal cause"),
                },
            },
            _ => return Err("invalid replica event kind or control identity"),
        };
        let event = Self {
            epoch: bytes[4..20].try_into().expect("fixed epoch width"),
            sequence: u64::from_le_bytes(bytes[20..28].try_into().expect("fixed sequence width")),
            policy_revision: u64::from_le_bytes(
                bytes[28..36].try_into().expect("fixed revision width"),
            ),
            kind,
        };
        event.validate()?;
        Ok(event)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionAck {
    #[serde(with = "crate::identifier::serde")]
    pub epoch: [u8; 16],
    pub sequence: u64,
    pub policy_revision: u64,
}
impl SubscriptionAck {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.epoch == [0; 16] || self.sequence != 0 {
            return Err("invalid replica subscription acknowledgment");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationAck {
    /// The submitted named or reserved database clock.
    pub database_clock: u64,
    pub policy_revision: u64,
}
impl PublicationAck {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.database_clock == 0 || self.policy_revision == 0 {
            return Err("invalid durable publication acknowledgment");
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert canonical transport fixtures.")]
mod tests {
    use super::*;
    fn event(kind: ReplicaEventKind<'_>) -> ReplicaEvent<'_> {
        ReplicaEvent { epoch: [7; 16], sequence: 1, policy_revision: 9, kind }
    }
    #[test]
    fn complete_identity_round_trips_and_payload_stays_borrowed() {
        for database in [
            DatabaseId::Index,
            DatabaseId::Revocations,
            DatabaseId::Entry(EntryId::from_bytes([255; 15]).unwrap()),
        ] {
            let source = event(ReplicaEventKind::Changes { database, delta: &[5, 6] });
            let encoded = source.encode().unwrap();
            assert_eq!(&encoded[36..52], &database.to_bytes());
            let decoded = ReplicaEvent::decode(&encoded).unwrap();
            assert_eq!(source, decoded);
            let ReplicaEventKind::Changes { delta, .. } = decoded.kind else { panic!() };
            assert_eq!(delta.as_ptr(), encoded[EVENT_HEADER_BYTES..].as_ptr());
        }
    }
    #[test]
    fn namespace_acceptance_binds_scope_and_bounded_borrowed_payloads() {
        // One authenticated envelope binds the selected identity, source bytes
        // and ordered acceptance masks without increasing the live byte cap.
        let entry = EntryId::from_bytes([8; 15]).unwrap();
        let mut evidence = vec![9; 16];
        evidence[15] = 1;
        let delta = vec![7; MAX_DELTA_BYTES - 4 - evidence.len()];
        let source =
            event(ReplicaEventKind::Namespace { entry, delta: &delta, evidence: &evidence });
        let encoded = source.encode().unwrap();
        assert_eq!(encoded.len(), MAX_EVENT_BYTES);
        let decoded = ReplicaEvent::decode(&encoded).unwrap();
        assert_eq!(decoded, source);
        let ReplicaEventKind::Namespace { delta, evidence, .. } = decoded.kind else { panic!() };
        assert_eq!(delta.as_ptr(), encoded[EVENT_HEADER_BYTES + 4..].as_ptr());
        assert_eq!(evidence.as_ptr(), encoded[encoded.len() - 16..].as_ptr());
        let mut invalid = encoded.clone();
        invalid[36..52].copy_from_slice(&DatabaseId::Index.to_bytes());
        assert!(ReplicaEvent::decode(&invalid).is_err());
        invalid = encoded.clone();
        invalid[52..56].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(ReplicaEvent::decode(&invalid).is_err());
        invalid = encoded;
        *invalid.last_mut().unwrap() = 4;
        assert!(ReplicaEvent::decode(&invalid).is_err());
        assert!(
            event(ReplicaEventKind::Namespace {
                entry,
                delta: &vec![7; MAX_DELTA_BYTES],
                evidence: &[]
            })
            .encode()
            .is_err()
        );
    }
    #[test]
    fn exact_log_scope_and_bounded_inline_hint() {
        let log = EntryId::from_bytes([3; 15]).unwrap();
        for data in [vec![], vec![1; 2048]] {
            let source = event(ReplicaEventKind::Bloblog { log, data: &data });
            assert_eq!(ReplicaEvent::decode(&source.encode().unwrap()).unwrap(), source);
        }
        assert!(event(ReplicaEventKind::Bloblog { log, data: &[1; 2049] }).encode().is_err());
    }
    #[test]
    fn terminal_requires_complete_dependency_and_explicit_cause() {
        for cause in [
            TerminalCause::AuthorityChanged,
            TerminalCause::ParticipantDenied,
            TerminalCause::ServiceUnavailable,
            TerminalCause::EntryAuthorityChanged,
        ] {
            let source = event(ReplicaEventKind::Terminal { volume: [9; 16], cause });
            let bytes = source.encode().unwrap();
            assert_eq!(bytes.len(), EVENT_HEADER_BYTES + 17);
            assert_eq!(ReplicaEvent::decode(&bytes).unwrap(), source);
            for end in EVENT_HEADER_BYTES..bytes.len() {
                assert!(ReplicaEvent::decode(&bytes[..end]).is_err());
            }
            let mut invalid = bytes.clone();
            invalid[EVENT_HEADER_BYTES..EVENT_HEADER_BYTES + 16].fill(0);
            assert!(ReplicaEvent::decode(&invalid).is_err());
            for cause in [0, 5, 255] {
                let mut invalid = bytes.clone();
                invalid[EVENT_HEADER_BYTES + 16] = cause;
                assert!(ReplicaEvent::decode(&invalid).is_err());
            }
            let mut extra = bytes;
            extra.push(0);
            assert!(ReplicaEvent::decode(&extra).is_err());
        }
    }
    #[test]
    fn unsupported_version_malformed_id_and_controls_are_rejected() {
        let good = event(ReplicaEventKind::Changes { database: DatabaseId::Index, delta: &[7] })
            .encode()
            .unwrap();
        for end in 0..=EVENT_HEADER_BYTES {
            assert!(ReplicaEvent::decode(&good[..end]).is_err());
        }
        for (offset, value) in [(0, 0), (0, 2), (1, 5), (2, 1), (3, 1), (36, 3), (51, 1)] {
            let mut bad = good.clone();
            bad[offset] = value;
            assert!(ReplicaEvent::decode(&bad).is_err());
        }
        for kind in [
            ReplicaEventKind::CaughtUp,
            ReplicaEventKind::Gap,
            ReplicaEventKind::Terminal {
                volume: [7; 16],
                cause: crate::network::ap::replication::TerminalCause::AuthorityChanged,
            },
        ] {
            let source = event(kind);
            let mut encoded = source.encode().unwrap();
            assert_eq!(ReplicaEvent::decode(&encoded).unwrap(), source);
            encoded[36] = 1;
            assert!(ReplicaEvent::decode(&encoded).is_err());
        }
        let maximum = vec![7; MAX_DELTA_BYTES];
        assert_eq!(
            event(ReplicaEventKind::Changes { database: DatabaseId::Index, delta: &maximum })
                .encode()
                .unwrap()
                .len(),
            MAX_EVENT_BYTES
        );
        assert!(
            event(ReplicaEventKind::Changes {
                database: DatabaseId::Index,
                delta: &vec![7; MAX_DELTA_BYTES + 1]
            })
            .encode()
            .is_err()
        );
    }
}
