//! Current-writer admission for ordinary named KV source records.
use crate::store::{Error, Result};
use kvdb::sync::{Delta, EntState};

pub(super) fn validate_source(delta: &Delta<'_>, append: bool) -> Result<()> {
    if delta.key.is_empty() || delta.key.len() > kvdb::KEY_SIZE_MAX {
        return Err(Error::LimitExceeded);
    }
    match delta.remote {
        EntState::Present { value, clock }
            if value.len() <= 4 * 1024 * 1024 && clock != kvdb::Clock::ZERO => {}
        EntState::Deleted { clock } if clock != kvdb::Clock::ZERO => {}
        _ => return Err(Error::LimitExceeded),
    }
    if append {
        let allowed = match (delta.local, delta.remote) {
            (EntState::Missing, EntState::Present { .. }) => true,
            (
                EntState::Present { value: local, clock: a },
                EntState::Present { value: remote, clock: b },
            ) => a == b && local == remote,
            _ => false,
        };
        if !allowed {
            return Err(Error::AccessDenied("append cannot replace or delete"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_bounds_match_backing_and_replay_requires_the_same_clock() {
        let keys = [b'k'; kvdb::KEY_SIZE_MAX + 1];
        let bytes = vec![0; 4 * 1024 * 1024 + 1];
        let clock = kvdb::Clock::from_u64(1);
        let delta = |key, value, origin| Delta {
            key,
            local: EntState::Missing,
            remote: EntState::Present { value, clock: origin },
        };
        assert!(
            validate_source(
                &delta(&keys[..kvdb::KEY_SIZE_MAX], &bytes[..4 * 1024 * 1024], clock),
                true
            )
            .is_ok()
        );
        for invalid in [
            delta(&keys, b"value", clock),
            delta(b"", b"value", clock),
            delta(b"key", &bytes, clock),
            delta(b"key", b"value", kvdb::Clock::ZERO),
        ] {
            assert!(validate_source(&invalid, false).is_err());
        }
        for local in [
            EntState::Deleted { clock },
            EntState::Present { value: b"same", clock: kvdb::Clock::from_u64(2) },
            EntState::Present { value: b"different", clock },
        ] {
            let delta =
                Delta { key: b"key", local, remote: EntState::Present { value: b"same", clock } };
            assert!(validate_source(&delta, true).is_err());
        }
        let deleted =
            Delta { key: b"key", local: EntState::Missing, remote: EntState::Deleted { clock } };
        assert!(validate_source(&deleted, false).is_ok());
        assert!(validate_source(&deleted, true).is_err());
    }
}
