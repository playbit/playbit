//! Account replica storage, independent of guest startup. The trusted JavaScript
//! owner authenticates every envelope and its typed projection before applying a
//! delta. Guests cannot call these exports through the syscall interface.
use super::*;
use crate::store::account::{MAX_ENVELOPE_BYTES, hash_key, validate_delta, validate_envelope};
use kvdb::{Clock, CommittedDelta, DeltaReceiver};

const MAX_DELTA: usize = 64 * 1024;
const MAX_ENTRIES: usize = 256 * 1024;
const _: () = {
    assert!(MAX_DELTA <= SYNC_FRAGMENT);
    assert!(MAX_ENTRIES <= LIMIT);
    assert!(44 + MAX_ENVELOPE_BYTES <= MAX_ENTRIES);
};
thread_local! {
    // Like HOST, this tree lives exactly as long as its runtime worker. There is
    // only one initialization; callers cannot accumulate leaked databases.
    static ACCOUNT: RefCell<Option<Rc<Database>>> = const { RefCell::new(None) };
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    account: Id,
    replica_id: u64,
}

fn output(result: Result<Vec<u8>>) -> i32 {
    match result {
        Ok(bytes) => publish_output(bytes),
        Err(error) => error as i32,
    }
}
fn input(length: usize, limit: usize, run: impl FnOnce(&[u8]) -> Result<Vec<u8>>) -> i32 {
    let bytes = match admit_input(length, limit) {
        Ok(bytes) => bytes,
        Err(error) => return error as i32,
    };
    output(run(&bytes[..length]))
}
fn tree(run: impl FnOnce(&Database) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let database = database()?;
    run(&database)
}
pub(super) fn database() -> Result<Rc<Database>> {
    ACCOUNT.with(|cell| cell.borrow().clone().ok_or(Error::Unavailable))
}
// Output entry: key[32], originating clock[u64 LE], ciphertext length[u32 LE],
// ciphertext. Empty output means no entry; no plaintext is returned by Rust.
fn entry(output: &mut Vec<u8>, key: &[u8], clock: Clock, value: &[u8]) -> Result<()> {
    validate_envelope(key, clock, value).map_err(map_error)?;
    if output.len() + 44 + value.len() > MAX_ENTRIES {
        return Err(Error::LimitExceeded);
    }
    output.extend_from_slice(key);
    output.extend_from_slice(&clock.as_u64().to_le_bytes());
    output.extend_from_slice(&(value.len() as u32).to_le_bytes());
    output.extend_from_slice(value);
    assert!(output.len() <= MAX_ENTRIES, "framed account entries retain their byte budget");
    Ok(())
}

/// The owner supplies OPFS store slot0; app volume slots remain1..96. Slot0
/// consumes the same backend handle budget, not an additional allocation.
#[unsafe(no_mangle)]
pub extern "C" fn pb_account_open(length: usize) -> i32 {
    crate::platform::failure::guard();
    input(length, 256, |bytes| {
        ACCOUNT.with(|cell| {
            if cell.borrow().is_some() {
                return Err(Error::InvalidArgument);
            }
            let args: Open = serde_json::from_slice(bytes).map_err(map_error)?;
            let database = Db::open_or_create(
                OpfsStorage(0),
                &CreateOptions::syncable(hash_key(args.account), args.replica_id),
                &Default::default(),
            )
            .map_err(map_error)?;
            if !database.meta().syncable || database.meta().hash_key != hash_key(args.account) {
                return Err(map_error("account database identity does not match"));
            }
            *cell.borrow_mut() = Some(Rc::new(database));
            Ok(Vec::new())
        })
    })
}

/// Stage a previously authenticated durable outbox record. Input is key[32],
/// originating clock[u64 LE], ciphertext. A losing record returns no delta.
#[unsafe(no_mangle)]
pub extern "C" fn pb_account_stage(length: usize) -> i32 {
    crate::platform::failure::guard();
    input(length, 40 + MAX_ENVELOPE_BYTES, |bytes| {
        if bytes.len() < 40 {
            return Err(Error::InvalidArgument);
        }
        let (key, tail) = bytes.split_at(32);
        let clock = Clock::from_u64(u64::from_le_bytes(
            tail[..8].try_into().expect("checked fixed-width field has the destination width"),
        ));
        let value = &tail[8..];
        validate_envelope(key, clock, value).map_err(map_error)?;
        tree(|database| {
            let mut tx = database
                .begin_write_with_delta(unsafe { now() } as u64, MAX_DELTA)
                .map_err(map_error)?;
            let order = tx
                .get_with_clock(key)
                .map_err(map_error)?
                .map(|(previous, bytes)| (previous, bytes.as_ref()).cmp(&(clock, value)));
            let delta = match order {
                Some(std::cmp::Ordering::Greater) => return Ok(Vec::new()),
                Some(std::cmp::Ordering::Equal) => database
                    .begin_read()
                    .map_err(map_error)?
                    .delta_for_key(key, MAX_DELTA)
                    .map_err(map_error)?
                    .ok_or(Error::Internal)?,
                _ => {
                    tx.put_with_clock(key, value, clock).map_err(map_error)?;
                    tx.commit_with_delta().map_err(map_error)?
                }
            };
            match delta {
                CommittedDelta::Bytes(bytes) => Ok(bytes),
                CommittedDelta::PullRequired => Err(Error::LimitExceeded),
            }
        })
    })
}

/// Expose every entry, including LWW losers, for client authentication before
/// receipt. No prefix is committed, even if a malformed tail follows it.
#[unsafe(no_mangle)]
pub extern "C" fn pb_account_entries(length: usize) -> i32 {
    crate::platform::failure::guard();
    input(length, MAX_DELTA, |bytes| {
        validate_delta(bytes).map_err(map_error)?;
        let mut output = Vec::new();
        let mut failure = None;
        kvdb::sync::visit_delta(bytes, MAX_DELTA, |key, state| {
            if failure.is_some() {
                return;
            }
            let kvdb::sync::EntState::Present { clock, value } = state else { unreachable!() };
            failure = entry(&mut output, key, clock, value).err();
        })
        .map_err(map_error)?;
        failure.map_or(Ok(output), Err)
    })
}

/// Call only after authenticating all entries and validating their projections.
#[unsafe(no_mangle)]
pub extern "C" fn pb_account_apply(length: usize) -> i32 {
    crate::platform::failure::guard();
    input(length, MAX_DELTA, |bytes| {
        validate_delta(bytes).map_err(map_error)?;
        tree(|database| {
            let mut receiver =
                DeltaReceiver::new(database, Lww, unsafe { now() } as u64, MAX_DELTA)
                    .map_err(map_error)?;
            receiver.process(bytes).map_err(map_error)?;
            receiver.finish().map_err(map_error)?;
            Ok(Vec::new())
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_account_read(length: usize) -> i32 {
    crate::platform::failure::guard();
    input(length, 32, |key| {
        if key.len() != 32 {
            return Err(Error::InvalidArgument);
        }
        tree(|database| {
            let mut output = Vec::new();
            if let Some((clock, value)) =
                database.begin_read().map_err(map_error)?.get_with_clock(key).map_err(map_error)?
            {
                entry(&mut output, key, clock, &value)?;
            }
            Ok(output)
        })
    })
}

/// Startup/reconnect reconciliation only. An empty input starts enumeration;
/// otherwise input is the last returned key and that key is skipped.
#[unsafe(no_mangle)]
pub extern "C" fn pb_account_next(length: usize) -> i32 {
    crate::platform::failure::guard();
    input(length, 32, |after| {
        if !after.is_empty() && after.len() != 32 {
            return Err(Error::InvalidArgument);
        }
        tree(|database| {
            let snapshot = database.begin_read().map_err(map_error)?;
            let mut entries = snapshot.iter_from(after);
            let mut next = entries.next().transpose().map_err(map_error)?;
            if next.as_ref().is_some_and(|(key, _)| key.as_ref() == after) {
                next = entries.next().transpose().map_err(map_error)?;
            }
            let mut output = Vec::new();
            if let Some((key, _)) = next {
                let (clock, value) =
                    snapshot.get_with_clock(&key).map_err(map_error)?.ok_or(Error::Internal)?;
                entry(&mut output, &key, clock, &value)?;
            }
            Ok(output)
        })
    })
}

/// After receiving a complete bootstrap, release its snapshot and inspect its
/// journal before asynchronous authentication. The live database has no writer.
#[unsafe(no_mangle)]
pub extern "C" fn pb_account_sync_prepare() -> i32 {
    crate::platform::failure::guard();
    output(SYNC.with(|cell| {
        let mut cell = cell.borrow_mut();
        let sync = cell.as_ref().ok_or(Error::Unavailable)?;
        if !sync.account || !sync.done || !matches!(sync.transfer, Transfer::Pull(_)) {
            return Err(Error::InvalidArgument);
        }
        let sync =
            cell.take().expect("account synchronization state was checked before consumption");
        let Transfer::Pull(receiver) = sync.transfer else { unreachable!() };
        let inspection = receiver.finish().map_err(map_error)?.inspect();
        *cell = Some(Sync { transfer: Transfer::Inspect(inspection), ..sync });
        Ok(Vec::new())
    }))
}

/// One framed encrypted entry for trusted-client authentication and projection
/// validation. Missing remote keys have clock/length zero and retain local data;
/// tombstones are forbidden for account records. Empty output ends inspection.
/// Only after successful validation of every entry may the caller commit sync.
#[unsafe(no_mangle)]
pub extern "C" fn pb_account_sync_next() -> i32 {
    crate::platform::failure::guard();
    let mut bytes = OUTPUT.lock().expect("mutex is not poisoned");
    bytes.clear();
    let result = SYNC.with(|cell| {
        let mut cell = cell.borrow_mut();
        let sync = cell.as_mut().ok_or(Error::Unavailable)?;
        let Transfer::Inspect(inspection) = &mut sync.transfer else {
            return Err(Error::InvalidArgument);
        };
        inspection
            .next(|key, state| {
                if key.len() != 32 {
                    return Err(kvdb::Error::Corrupt);
                }
                match state {
                    kvdb::sync::SyncEntState::Missing => {
                        bytes.extend_from_slice(key);
                        bytes.extend_from_slice(&[0; 12]);
                    }
                    kvdb::sync::SyncEntState::Deleted { .. } => return Err(kvdb::Error::Corrupt),
                    kvdb::sync::SyncEntState::Present { clock, value } => {
                        if value.len() > MAX_ENVELOPE_BYTES {
                            return Err(kvdb::Error::Corrupt);
                        }
                        bytes.resize(44 + value.len(), 0);
                        bytes[..32].copy_from_slice(key);
                        bytes[32..40].copy_from_slice(&clock.as_u64().to_le_bytes());
                        bytes[40..44].copy_from_slice(&(value.len() as u32).to_le_bytes());
                        value.read_exact_at(0, &mut bytes[44..])?;
                        validate_envelope(key, clock, &bytes[44..])
                            .map_err(|_| kvdb::Error::Corrupt)?;
                    }
                }
                Ok(())
            })
            .map_err(map_error)?;
        Ok(bytes.len())
    });
    match result {
        Ok(length) => {
            assert!(length <= MAX_ENTRIES, "account inspection returns one bounded entry");
            length as i32
        }
        Err(error) => {
            bytes.clear();
            error as i32
        }
    }
}

/// Decode bounded client-only plaintext with the shared native schema. This
/// cannot authorize a receipt: the caller must authenticate the envelope,
/// logical key, invitation key and local projection before applying any delta.
#[unsafe(no_mangle)]
pub extern "C" fn pb_account_decode(length: usize) -> i32 {
    crate::platform::failure::guard();
    input(length, crate::store::account::MAX_PLAINTEXT_BYTES, |bytes| {
        crate::store::account::Record::browser_plaintext(bytes).map_err(map_error)
    })
}

/// Resolve public scope addresses before a session or account replica is open.
#[unsafe(no_mangle)]
pub extern "C" fn pb_scope_addresses(length: usize) -> i32 {
    use crate::package::{AppIdentity, AutomaticVolumeRole};
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ScopeRequest {
        app: AppIdentity,
        session: Option<String>,
        account: Option<Id>,
        installation: Option<Id>,
    }

    crate::platform::failure::guard();
    // Canonical IDs, a 64-byte app identifier and the bounded JSON envelope.
    input(length, 512, |bytes| {
        let request: ScopeRequest = serde_json::from_slice(bytes).map_err(map_error)?;
        let world = request
            .app
            .volume_id(AutomaticVolumeRole::Root, None, None, None)
            .map_err(map_error)?;
        let mut result = serde_json::json!({ "world": world });
        if let Some(session) = request.session {
            let session = crate::identifier::decode(&session).map_err(map_error)?;
            let shared = request
                .app
                .volume_id(AutomaticVolumeRole::Session, Some(session), None, None)
                .map_err(map_error)?;
            result["shared"] = serde_json::json!(shared);
            if let (Some(account), Some(installation)) = (request.account, request.installation) {
                for (name, role) in [
                    ("user", AutomaticVolumeRole::User),
                    ("user_session", AutomaticVolumeRole::UserSession),
                    ("local", AutomaticVolumeRole::Local),
                    ("local_session", AutomaticVolumeRole::LocalSession),
                ] {
                    let id = request
                        .app
                        .volume_id(
                            role,
                            role.is_session().then_some(session),
                            Some(account),
                            role.is_local().then_some(installation),
                        )
                        .map_err(map_error)?;
                    result[name] = serde_json::json!(id);
                }
            } else if request.account.is_some() || request.installation.is_some() {
                return Err(Error::InvalidArgument);
            }
        } else if request.account.is_some() || request.installation.is_some() {
            return Err(Error::InvalidArgument);
        }
        serde_json::to_vec(&result).map_err(map_error)
    })
}
