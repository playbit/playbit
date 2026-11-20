//! Compare local winning facts against a detached accepted index.
use super::*;

/// The caller keeps `remote` and `policy` on the same detached source cut.
/// This only classifies facts; retaining or replacing them requires the caller's
/// qualified-source and durable-publication checks.
pub fn rejected_sources<S: kvdb::Storage, T: kvdb::Storage>(
    local: &kvdb::ReadTxn<'_, S>,
    remote: &kvdb::Db<T>,
    policy: &MetadataPolicy,
    account: Id,
    entry: EntryId,
) -> Result<bool> {
    let mut rejected = false;
    visit_rejected_sources(
        local,
        remote,
        policy,
        account,
        entry,
        &mut namespace::source::cut::Scan::materializing(0)?,
        |_, _, _, _| {
            rejected = true;
            Ok(false)
        },
    )?;
    Ok(rejected)
}

/// Visit exact denied winning sources on the same canonical cut. Returning false
/// stops the scan. Backing differences and entry-wide flags supply no witnesses.
pub(crate) fn visit_rejected_sources<S: kvdb::Storage, T: kvdb::Storage>(
    local: &kvdb::ReadTxn<'_, S>,
    remote: &kvdb::Db<T>,
    policy: &MetadataPolicy,
    account: Id,
    entry: EntryId,
    scan: &mut namespace::source::cut::Scan,
    mut visit: impl FnMut(&[u8], kvdb::Clock, &[u8], &mut namespace::source::cut::Scan) -> Result<bool>,
) -> Result<()> {
    use kvdb::sync::{Delta, EntState};
    let read = remote.begin_read()?;
    let snapshot = IndexSnapshot::new(remote.begin_read()?);
    for prefix in entry_source_prefixes(entry, true) {
        let mut cursor = prefix.clone();
        let mut exclusive = false;
        while let Some(key) = local.next_source_key(&cursor, exclusive)? {
            if !key.starts_with(&prefix) {
                break;
            }
            cursor = key.clone();
            exclusive = true;
            let Some((clock, value)) = scan.read(local, &key)? else { continue };
            let accepted = accepted_source(&read, &key)?;
            if accepted
                .as_ref()
                .is_some_and(|(other_clock, other)| *other_clock == clock && other == &value)
            {
                continue;
            }
            let before = accepted.as_ref().map_or(EntState::Missing, |(clock, value)| {
                EntState::Present { clock: *clock, value }
            });
            let delta = Delta {
                key: &key,
                local: before,
                remote: EntState::Present { clock, value: &value },
            };
            if kvdb::Resolver::resolve(&mut kvdb::Lww, &delta) == kvdb::sync::Resolution::KeepLocal
            {
                continue;
            }
            admit_source_decoder(
                &read,
                entry,
                scan,
                &value,
                accepted.as_ref().map(|(_, bytes)| bytes.as_slice()),
            )?;
            match snapshot.authorize_source(
                policy.volume(),
                policy.default_permissions(),
                account,
                &delta,
            ) {
                Ok(()) => {}
                Err(Error::AccessDenied(_)) => {
                    drop(accepted);
                    if !visit(&key, clock, &value, scan)? {
                        return Ok(());
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn admit_source_decoder(
    read: &kvdb::ReadTxn<'_, impl kvdb::Storage>,
    entry: EntryId,
    scan: &mut namespace::source::cut::Scan,
    value: &[u8],
    accepted: Option<&[u8]>,
) -> Result<()> {
    let mut decoder_bytes =
        scan.retained + value.len() + accepted.map_or(0, <[u8]>::len) + 16 * 1024;
    // Dependency validation holds the remote birth while decoding the
    // proposed revision; ACL admission can decode birth or override ACL.
    // Reserve both complete inputs before any authorization decoder runs.
    decoder_bytes += crate::store::catalog::index_record_charge(value)?;
    if let Some(accepted) = accepted {
        decoder_bytes += crate::store::catalog::index_record_charge(accepted)?;
    }
    for dependency in [entry_key(entry).to_vec(), meta_key(PERMISSIONS, entry).to_vec()] {
        if let Some(reader) = read.value_reader(&dependency)? {
            let bytes = read_sync_value(&reader)?;
            decoder_bytes += bytes.len() + crate::store::catalog::index_record_charge(&bytes)?;
        }
    }
    scan.admit_decoder(decoder_bytes)?;
    Ok(())
}

fn accepted_source(
    read: &kvdb::ReadTxn<'_, impl kvdb::Storage>,
    key: &[u8],
) -> Result<Option<(kvdb::Clock, Vec<u8>)>> {
    let accepted = read
        .value_reader(key)?
        .map(|value| {
            Ok::<_, Error>((
                value.clock().ok_or(Error::corrupt("reconciliation source clock"))?,
                read_sync_value(&value)?,
            ))
        })
        .transpose()?;
    Ok(accepted)
}
