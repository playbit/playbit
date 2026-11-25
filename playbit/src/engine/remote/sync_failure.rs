//! Scope diagnostics are built on the network worker, only after a failed operation.
//! An unavailable name never replaces the original cause or the exact replica identity.
use super::initialization::Pending;
use crate::engine::{live::LiveGap, publisher::PublishKey};
use crate::store::{Access, DatabaseId, Id, LocalStore};
use std::{error::Error, fmt};

pub(in crate::engine) type Cause = Box<dyn Error + Send + Sync>;
const PREFIX: &str = "Couldn’t sync ";

#[derive(Debug)]
struct Failure {
    key: PublishKey,
    database: Option<DatabaseId>,
    volume_label: Option<String>,
    database_label: Option<String>,
    cause: Cause,
}

impl fmt::Display for Failure {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(PREFIX)?;
        if let Some(database) = self.database {
            out.write_str("database ")?;
            match &self.database_label {
                Some(label) => out.write_str(label)?,
                None => write!(out, "{database}")?,
            }
            out.write_str(" in ")?;
        }
        out.write_str("volume ")?;
        match &self.volume_label {
            Some(label) => out.write_str(label)?,
            None => write!(out, "{}", self.key.volume)?,
        }
        write!(out, "\n{}", self.cause)
    }
}
impl Error for Failure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

#[cfg(feature = "app-host")]
pub(crate) fn sync_failure_parts(message: &str) -> Option<(&str, &str)> {
    let (scope, cause) = message.split_once('\n')?;
    scope.starts_with(PREFIX).then_some((scope, cause))
}

/// Apply textual service classifications to the original reason, never to a user-supplied label.
pub(super) fn reason<'a>(error: &'a (dyn Error + 'static)) -> &'a (dyn Error + 'static) {
    let error = error.downcast_ref::<Pending>().map_or(error, |pending| pending.0.as_ref());
    error.downcast_ref::<Failure>().map_or(error, |failure| failure.cause.as_ref())
}

pub(super) fn volume_id(error: &(dyn Error + 'static)) -> Option<Id> {
    let error = error.downcast_ref::<Pending>().map_or(error, |pending| pending.0.as_ref());
    error.downcast_ref::<Failure>().map(|failure| failure.key.volume)
}

/// Only an entry capture in this exact volume can defer World at room launch.
#[cfg(feature = "app-host")]
pub(super) fn missing_entry_backing(volume: Id, error: &(dyn Error + 'static)) -> bool {
    let error = error.downcast_ref::<Pending>().map_or(error, |pending| pending.0.as_ref());
    error.downcast_ref::<Failure>().is_some_and(|failure| {
        failure.key == volume.into()
            && matches!(failure.database, Some(DatabaseId::Entry(_)))
            && failure.cause.to_string() == "waiting for initial database synchronization"
    })
}

/// A current receive Gap permits recapture. Preserve the structured scope and
/// the failed apply underneath that disposition for both checkpoints and fences.
pub(in crate::engine) fn apply_gap(cause: Cause) -> Cause {
    match cause.downcast::<Failure>() {
        Ok(mut failure) => {
            failure.cause = LiveGap::Apply(failure.cause).into();
            failure
        }
        Err(cause) => LiveGap::Apply(cause).into(),
    }
}

/// A full-volume checkpoint can send a plain Gap when its final capture
/// already found an unready backing. Keep that observation for this capture only.
pub(super) fn captured_backing_gap(
    volume: Id,
    pending: Option<DatabaseId>,
    mut error: Cause,
) -> Cause {
    if let Some(database) = pending
        && let Some(failure) = error.downcast_mut::<Failure>()
        && failure.key == volume.into()
        && failure.database.is_none()
        && matches!(failure.cause.downcast_ref::<LiveGap>(), Some(LiveGap::Delivery))
    {
        failure.database = Some(database);
        failure.cause = LiveGap::PendingBacking.into();
    }
    error
}

/// Preserve the final capture's scope and observations when its retry budget ends.
pub(super) fn exhausted_gap(error: Cause) -> Cause {
    let exhaust = |cause: Cause| match cause.downcast::<LiveGap>() {
        Ok(gap) => gap.exhausted(),
        Err(cause) => cause,
    };
    match error.downcast::<Failure>() {
        Ok(mut failure) => {
            failure.cause = exhaust(failure.cause);
            failure
        }
        Err(cause) => exhaust(cause),
    }
}

fn label(id: impl fmt::Display, name: &str) -> String {
    // Bound the escaped text, including arbitrary Unicode/control characters,
    // so both IDs fit in the 512-byte status label.
    let mut quoted = String::from("\"");
    for ch in name.chars() {
        let escaped = ch.escape_debug();
        if quoted.len() + escaped.clone().map(char::len_utf8).sum::<usize>() > 80 {
            quoted.push('…');
            break;
        }
        quoted.extend(escaped);
    }
    format!("{quoted}\" ({id})")
}

fn scoped(key: PublishKey, database: Option<DatabaseId>, cause: Cause) -> Cause {
    if cause.is::<Failure>() || cause.is::<Pending>() || cause.is::<super::CanceledOperation>() {
        return cause;
    }
    Box::new(Failure { key, database, cause, volume_label: None, database_label: None })
}

/// Fence/receive tasks have an exact key but no caller's local name-read authority.
pub(in crate::engine) fn target(key: PublishKey, cause: impl Into<Cause>) -> Cause {
    scoped(key, key.entry.map(DatabaseId::Entry), cause.into())
}

pub(in crate::engine) fn target_database(
    key: impl Into<PublishKey>,
    database: DatabaseId,
    cause: impl Into<Cause>,
) -> Cause {
    scoped(key.into(), Some(database), cause.into())
}

/// Resolve a live error with the authority captured for its particular stream.
/// A catalog stream can fail on an entry database inside that volume.
pub(in crate::engine) fn resolve(
    store: &LocalStore,
    mut error: Cause,
    access: impl FnOnce(PublishKey) -> Option<Access>,
) -> Cause {
    if let Some(failure) = error.downcast_mut::<Failure>() {
        let volume = failure.key.volume;
        if failure.volume_label.is_none() {
            failure.volume_label = store.volume(volume).ok().map(|v| label(volume, &v.name));
        }
        if let Some(DatabaseId::Entry(entry)) = failure.database
            && failure.database_label.is_none()
            && failure.key.entry.is_none_or(|selected| selected == entry)
            && let Some(access) = access(failure.key)
            && let Ok(record) = store.entry(&access, volume, entry)
        {
            failure.database_label = Some(label(DatabaseId::Entry(entry), &record.info.name));
        }
    }
    error
}

pub(in crate::engine) fn stream_access(
    store: &LocalStore,
    scope: &crate::engine::live::ScopeKey,
) -> Access {
    let account = store.account().id;
    scope.session.as_ref().map_or_else(
        || Access::owner(account),
        |(id, build)| {
            Access::session_version(
                account,
                crate::identifier::encode(id),
                crate::identifier::encode(build),
            )
        },
    )
}

pub(super) fn recovery(
    store: &LocalStore,
    scope: &crate::engine::live::ScopeKey,
    error: Cause,
) -> Cause {
    let key = PublishKey { volume: scope.volume, entry: scope.entry };
    resolve(store, target(key, error), |source| {
        (source == key).then(|| stream_access(store, scope))
    })
}

pub(super) fn volume(store: &LocalStore, id: Id, error: impl Into<Cause>) -> Cause {
    resolve(store, target(id.into(), error), |_| None)
}

pub(super) fn database(
    store: &LocalStore,
    access: &Access,
    volume: Id,
    database: DatabaseId,
    error: impl Into<Cause>,
) -> Cause {
    resolve(store, target_database(volume, database, error), |source| {
        (source.volume == volume).then(|| access.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_backing_only_identifies_its_own_volume_delivery_gap() {
        let captured = Id::from_bytes([1; 16]);
        let other = Id::from_bytes([2; 16]);
        let entry = crate::store::EntryId::from_bytes([3; 15]).expect("valid fixture entry");
        let pending = Some(DatabaseId::Entry(entry));
        for error in [
            target(other.into(), LiveGap::Delivery),
            target(PublishKey { volume: captured, entry: Some(entry) }, LiveGap::Delivery),
            target(captured.into(), "access denied: volume permission"),
            target_database(captured, DatabaseId::Revocations, "failed durable write"),
            LiveGap::Delivery.into(),
            "connection lost".into(),
        ] {
            let original = error.to_string();
            assert_eq!(captured_backing_gap(captured, pending, error).to_string(), original);
        }
    }

    #[test]
    fn rendered_scope_preserves_both_ids_for_multibyte_or_escaped_names() {
        let volume = Id::from_bytes([255; 16]);
        let database =
            DatabaseId::Entry(crate::store::EntryId::from_bytes([254; 15]).expect("valid ID"));
        for name in ["\u{1b}".repeat(128), "🌊".repeat(64), "bad\nlabel".repeat(24)] {
            let scope = format!(
                "{PREFIX}database {} in volume {}",
                label(database, &name),
                label(volume, &name)
            );
            assert!(scope.len() < 512);
            assert_eq!(scope.lines().count(), 1);
            assert!(scope.contains(&database.to_string()));
            assert!(scope.ends_with(&format!("({volume})")));
        }
    }
}
