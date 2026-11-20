//! Persistent negative evidence; AP admission and receipt are trusted worker calls.
use super::*;
use crate::store::authority::{
    Barrier, EntryBarrier, barrier_key, binding_key, capture_generation, catalog_key,
    ceiling_entry_key, entry_barrier_key, entry_key,
};

fn marker(volume: &Volume, key: &[u8]) -> Result<Barrier> {
    let read = volume.index.begin_read().map_err(map_error)?;
    let value = read.local_get(key).map_err(map_error)?;
    Barrier::decode(value.as_deref()).map_err(store_error)
}
fn write(volume: &Volume, key: &[u8], barrier: Barrier) -> Result<()> {
    let mut tx = volume.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
    tx.local_indexes().put(key, &barrier.encode()).map_err(map_error)?;
    tx.commit().map_err(map_error)
}
pub(super) fn generation(host: &Host, volume: &Volume) -> Result<u64> {
    Ok(marker(volume, &barrier_key(host.account))?.generation)
}
pub(super) fn generations(
    host: &Host,
    volume: &Volume,
    entry: Option<EntryId>,
) -> Result<(u64, Option<u64>)> {
    let read = volume.index.begin_read().map_err(map_error)?;
    let bytes = read.local_get(&barrier_key(host.account)).map_err(map_error)?;
    let volume = Barrier::decode(bytes.as_deref()).map_err(store_error)?.generation;
    let entry = entry
        .map(|entry| {
            let bytes =
                read.local_get(&entry_barrier_key(host.account, entry)).map_err(map_error)?;
            Ok::<_, Error>(
                EntryBarrier::decode(bytes.as_deref())
                    .map_err(store_error)?
                    .current(volume)
                    .map_err(store_error)?
                    .generation,
            )
        })
        .transpose()?;
    Ok((volume, entry))
}
pub(super) fn required(host: &Host, volume: &Volume) -> Result<bool> {
    Ok(marker(volume, &barrier_key(host.account))?.fenced)
}
fn check_marker(host: &Host, volume: &Volume, entry: Option<EntryId>) -> Result<()> {
    let read = volume.index.begin_read().map_err(map_error)?;
    let bytes = read.local_get(&barrier_key(host.account)).map_err(map_error)?;
    let barrier = Barrier::decode(bytes.as_deref()).map_err(store_error)?;
    if let Some(entry) = entry {
        let selected =
            read.local_get(&entry_barrier_key(host.account, entry)).map_err(map_error)?;
        if EntryBarrier::decode(selected.as_deref())
            .map_err(store_error)?
            .current(barrier.generation)
            .map_err(store_error)?
            .fenced
        {
            return Err(map_error(
                "access denied: entry authority requires authenticated catch-up",
            ));
        }
    }
    let ceiling = read.local_get(&catalog_key(host.account)).map_err(map_error)?;
    match ceiling.as_deref() {
        None => {}
        Some([1]) => {
            let entry = entry.ok_or(Error::AccessDenied)?;
            let allowed =
                read.local_get(&ceiling_entry_key(host.account, entry)).map_err(map_error)?;
            match allowed.as_deref() {
                Some([1]) => {}
                None => return Err(Error::AccessDenied),
                _ => return Err(Error::Internal),
            }
        }
        _ => return Err(Error::Internal),
    }
    let capture = if barrier.fenced
        && let Some(entry) = entry
    {
        let bytes = read.local_get(&entry_key(host.account, entry)).map_err(map_error)?;
        capture_generation(bytes.as_deref()).map_err(store_error)?
    } else {
        None
    };
    if !barrier.permits(capture) {
        return Err(map_error("access denied: current authority requires authenticated catch-up"));
    }
    Ok(())
}
pub(super) fn check(host: &Host, target: &Volume, entry: Option<EntryId>) -> Result<()> {
    check_marker(host, target, entry)?;
    for volume in host.volumes.borrow().iter().filter(|volume| volume.id != target.id) {
        let read = volume.index.begin_read().map_err(map_error)?;
        let bytes = read.local_get(&binding_key(host.account)).map_err(map_error)?;
        let capture = capture_generation(bytes.as_deref()).map_err(store_error)?;
        if !marker(volume, &barrier_key(host.account))?.permits(capture) {
            return Err(map_error(
                "access denied: dependency authority requires authenticated catch-up",
            ));
        }
    }
    Ok(())
}
pub(super) fn publish_marker(
    host: &Host,
    original: &Volume,
    received: &Volume,
    captured: u64,
    entry: Option<(EntryId, u64)>,
    qualified: bool,
) -> Result<()> {
    let key = barrier_key(host.account);
    let mut current = marker(original, &key)?;
    if let Some((entry, captured_entry)) = entry {
        return publish_entry_marker(
            host,
            original,
            received,
            current,
            captured,
            (entry, captured_entry),
            qualified,
        );
    }
    let clears = qualified && current.accepts(captured);
    if clears {
        current = current.cleared_full();
    }
    let mut tx = received.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
    tx.local_indexes().put(&key, &current.encode()).map_err(map_error)?;
    if clears && !current.fenced {
        tx.local_indexes().del(&catalog_key(host.account)).map_err(map_error)?;
        tx.local_indexes().del(&binding_key(host.account)).map_err(map_error)?;
    }
    tx.commit().map_err(map_error)
}

fn publish_entry_marker(
    host: &Host,
    original: &Volume,
    received: &Volume,
    volume: Barrier,
    captured: u64,
    selected_capture: (EntryId, u64),
    qualified: bool,
) -> Result<()> {
    let (entry, captured_entry) = selected_capture;
    let key = entry_barrier_key(host.account, entry);
    let read = original.index.begin_read().map_err(map_error)?;
    let bytes = read.local_get(&key).map_err(map_error)?;
    let mut selected = EntryBarrier::decode(bytes.as_deref())
        .map_err(store_error)?
        .current(volume.generation)
        .map_err(store_error)?;
    let clears = qualified && volume.accepts(captured) && selected.accepts(captured_entry);
    if clears {
        selected.fenced = false;
    }
    let mut tx = received.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
    tx.local_indexes().put(&barrier_key(host.account), &volume.encode()).map_err(map_error)?;
    tx.local_indexes()
        .put(
            &key,
            &EntryBarrier { volume_generation: volume.generation, barrier: selected }.encode(),
        )
        .map_err(map_error)?;
    tx.local_indexes().put(&catalog_key(host.account), &[1]).map_err(map_error)?;
    tx.local_indexes().put(&ceiling_entry_key(host.account, entry), &[1]).map_err(map_error)?;
    if clears {
        tx.local_indexes()
            .put(&entry_key(host.account, entry), &volume.generation.to_le_bytes())
            .map_err(map_error)?;
        tx.local_indexes()
            .put(&binding_key(host.account), &volume.generation.to_le_bytes())
            .map_err(map_error)?;
    }
    tx.commit().map_err(map_error)
}

fn denial_key(account: Id, session: &Session) -> [u8; 49] {
    let mut key = [0; 49];
    key[0] = 14;
    key[1..17].copy_from_slice(account.as_bytes());
    key[17..].copy_from_slice(blake3::hash(session.id.as_bytes()).as_bytes());
    key
}
pub(super) fn check_session(host: &Host) -> Result<()> {
    let root = host.volumes.borrow()[0].clone();
    if marker(&root, &denial_key(host.account, &host.session.borrow()))?.fenced {
        return Err(map_error("access denied: session participant revoked"));
    }
    Ok(())
}
pub(super) fn terminal(
    host: &Host,
    source: Id,
    entry: Option<EntryId>,
    changed: Id,
    cause: u8,
) -> Result<()> {
    use crate::store::authority::TerminalCause;
    let source = host.replica_volume(source)?;
    let session = host.session.borrow();
    if changed != source.id && changed != session.replica_volume() {
        return Err(Error::AccessDenied);
    }
    match cause {
        value if value == TerminalCause::ServiceUnavailable as u8 => Ok(()),
        value if value == TerminalCause::ParticipantDenied as u8 => {
            let root = host.volumes.borrow()[0].clone();
            let key = denial_key(host.account, &session);
            write(&root, &key, marker(&root, &key)?.invalidate())
        }
        value if value == TerminalCause::AuthorityChanged as u8 => {
            let volume = host.replica_volume(changed)?;
            let key = barrier_key(host.account);
            write(&volume, &key, marker(&volume, &key)?.invalidate())
        }
        value if value == TerminalCause::EntryAuthorityChanged as u8 => {
            let entry = entry.ok_or(Error::AccessDenied)?;
            if changed != source.id {
                return Err(Error::AccessDenied);
            }
            let generation = generation(host, &source)?;
            let key = entry_barrier_key(host.account, entry);
            let mut tx = source.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
            let next = {
                let local = tx.local_indexes();
                let bytes = local.get(&key).map_err(map_error)?;
                EntryBarrier {
                    volume_generation: generation,
                    barrier: EntryBarrier::decode(bytes.as_deref())
                        .map_err(store_error)?
                        .current(generation)
                        .map_err(store_error)?
                        .invalidate(),
                }
            };
            tx.local_indexes().put(&key, &next.encode()).map_err(map_error)?;
            tx.commit().map_err(map_error)
        }
        _ => Err(Error::InvalidArgument),
    }
}

// Negative evidence is admitted even while a detached sync owns a read snapshot.
// It cannot modify source facts and invalidates any older publication epoch.
fn call(length: usize, run: impl FnOnce(&Host, &[u8]) -> Result<Vec<u8>>) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let input = admit_input(length, LIMIT)?;
        let host = HOST.with(|host| host.borrow().clone()).ok_or(Error::Unavailable)?;
        run(&host, &input[..length])
    })();
    match result {
        Ok(bytes) => publish_output(bytes),
        Err(error) => error as i32,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Admission {
    session: Session,
    current: Session,
    consent_revision: u32,
    generation: [u8; 8],
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisteredAdmission {
    admission: Admission,
    registered: Session,
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_session_authority(length: usize) -> i32 {
    call(length, |host, bytes| {
        let session: Session = serde_json::from_slice(bytes).map_err(map_error)?;
        let current = host.session.borrow().clone();
        if !session.same_identity(&current) {
            return Err(Error::AccessDenied);
        }
        let root = host.volumes.borrow()[0].clone();
        serde_json::to_vec(&Admission {
            generation: marker(&root, &denial_key(host.account, &session))?
                .generation
                .to_le_bytes(),
            session,
            current,
            consent_revision: host.consent_revision.get(),
        })
        .map_err(map_error)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_session_admit(length: usize) -> i32 {
    call(length, |host, bytes| {
        let registered: RegisteredAdmission = serde_json::from_slice(bytes).map_err(map_error)?;
        let admission = registered.admission;
        if !admission.session.same_identity(&registered.registered)
            || admission.session.resolved_build_id != registered.registered.resolved_build_id
            || admission.current != *host.session.borrow()
            || admission.consent_revision != host.consent_revision.get()
            || !admission.current.same_identity(&admission.session)
        {
            return Err(Error::AccessDenied);
        }
        let root = host.volumes.borrow()[0].clone();
        let key = denial_key(host.account, &admission.session);
        let current = marker(&root, &key)?;
        if current.accepts(u64::from_le_bytes(admission.generation)) {
            write(&root, &key, Barrier { fenced: false, ..current })?;
        }
        Ok(Vec::new())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_authority_refresh_required(length: usize) -> i32 {
    call(length, |host, bytes| {
        let id: Id = serde_json::from_slice(bytes).map_err(map_error)?;
        let volume = host.replica_volume(id)?;
        serde_json::to_vec(&required(host, &volume)?).map_err(map_error)
    })
}
/// Internal bootstrap planning only. AP must freshly admit every subscription;
/// this never clears negative evidence or grants guest catalog access.
#[unsafe(no_mangle)]
pub extern "C" fn pb_replica_catalog_allowed(length: usize) -> i32 {
    call(length, |host, bytes| {
        let id: Id = serde_json::from_slice(bytes).map_err(map_error)?;
        let volume = host.replica_volume(id)?;
        let empty =
            volume.index.begin_read().map_err(map_error)?.root_hash().map_err(map_error)?.is_none();
        let allowed = if empty {
            true
        } else {
            let policy = host.admit_volume(&volume, Permission::Read)?;
            match policy.authorize(host.account, Permission::Read) {
                Ok(()) => true,
                Err(crate::store::Error::AccessDenied(_)) => false,
                Err(error) => return Err(store_error(error)),
            }
        };
        serde_json::to_vec(&allowed).map_err(map_error)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_live_terminal(length: usize) -> i32 {
    call(length, |host, bytes| {
        if bytes.len() != 33 && bytes.len() != 48 {
            return Err(Error::InvalidArgument);
        }
        let source = Id::from_bytes(bytes[..16].try_into().expect("admitted identity width"));
        let (entry, offset) = if bytes.len() == 48 {
            (
                Some(
                    EntryId::from_bytes(bytes[16..31].try_into().expect("admitted entry width"))
                        .map_err(store_error)?,
                ),
                31,
            )
        } else {
            (None, 16)
        };
        let changed =
            Id::from_bytes(bytes[offset..offset + 16].try_into().expect("admitted identity width"));
        terminal(host, source, entry, changed, bytes[offset + 16])?;
        Ok(Vec::new())
    })
}
