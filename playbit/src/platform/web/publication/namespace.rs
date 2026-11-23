//! Exact received namespace evidence is applied only to the detached working
//! index. Its ledger, accepted payloads and current controls commit together;
//! the durable generation pointer remains the visibility boundary.
use super::*;
use crate::store::volume_index::namespace::{
    self as ns,
    observations::{self, Ledger, Observation, Status},
    source::cut,
};

const PLAN_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn qualify(
    stage: &Preparation,
    actor: DataActor,
    confirmed: bool,
) -> crate::store::Result<()> {
    let accepted = stage.accepted_index.begin_read()?;
    let confirmed = if confirmed {
        ns::receipt::capture(&accepted, stage.entry_scope.map(|scope| scope.0))?
    } else {
        // Authority-only cuts can exclude an exactly denied local source. They
        // cannot confirm a remote event or certify its normalized assignments.
        Vec::new()
    };
    let mut rows = BTreeMap::new();
    let mut retained = 0;
    for row in confirmed {
        insert(&mut rows, &mut retained, row)?;
    }
    exclude(stage, actor, &mut rows, &mut retained)?;
    let rows = rows.into_values().collect::<Vec<_>>();
    let current = stage.volume.index.begin_read()?;
    let before = Ledger::load_read(&current)?;
    let mut after = before.clone();
    after.apply_receipt(&rows)?;
    after.validate_persistence()?;
    drop(current);
    let mut tx = stage.volume.index.begin_write(unsafe { now() } as u64)?;
    ns::payloads::Usage::validate_local(&tx.local_indexes())?;
    after.write_changes(&before, &mut tx.local_indexes())?;
    drop(after);
    drop(before);
    let mut scan = cut::Scan::materializing(retained)?;
    for row in &rows {
        if row.status == Status::Confirmed {
            let bytes = ns::payloads::capture(&accepted, &row.source, &mut scan, retained)?;
            ns::payloads::retain(&mut tx.local_indexes(), row, &bytes, retained)?;
        }
    }
    drop(rows);
    ns::payloads::Usage::validate_local(&tx.local_indexes())?;
    cut::project(&tx)?.write_current(&mut tx.local_indexes())?;
    tx.commit()?;
    Ok(())
}

fn insert(
    rows: &mut BTreeMap<EntryId, Observation>,
    retained: &mut usize,
    row: Observation,
) -> crate::store::Result<()> {
    let id = row.source.id()?;
    if let Some(prior) = rows.get(&id) {
        // A selected head and its history copy identify the same source. Exact
        // accepted evidence takes precedence over an omitted history copy.
        if prior.source != row.source {
            return Err(crate::store::Error::corrupt("namespace receipt source changed"));
        }
        return Ok(());
    }
    let bytes = 512 + row.source.observed.heap_bytes();
    *retained = retained.checked_add(bytes).ok_or(crate::store::Error::LimitExceeded)?;
    if *retained > PLAN_BYTES || rows.len() == ns::MAX_FACTS {
        return Err(crate::store::Error::LimitExceeded);
    }
    rows.insert(id, row);
    Ok(())
}

fn exclude(
    stage: &Preparation,
    actor: DataActor,
    rows: &mut BTreeMap<EntryId, Observation>,
    retained: &mut usize,
) -> crate::store::Result<()> {
    let local = stage.original.index.begin_read()?;
    let policy = MetadataPolicy::read(&stage.accepted_index)?;
    let mut cursor = vec![volume_index::ENTRY_ID_BYTE];
    let mut exclusive = false;
    let mut entries = 0;
    let mut scan = cut::Scan::materializing(*retained)?;
    while let Some(key) = local.next_source_key(&cursor, exclusive)? {
        if key.first() != Some(&volume_index::ENTRY_ID_BYTE) {
            break;
        }
        let entry = volume_index::source_entry_id(&key)?;
        cursor = key.clone();
        exclusive = true;
        entries += 1;
        if entries > ns::MAX_ENTRIES {
            return Err(crate::store::Error::LimitExceeded);
        }
        if stage.entry_scope.is_some_and(|scope| scope.0 != entry) {
            continue;
        }
        exclude_entry(stage, actor, &local, &policy, entry, &mut scan, |row| {
            insert(rows, retained, row)?;
            Ok(*retained)
        })?;
    }
    Ok(())
}

fn exclude_entry(
    stage: &Preparation,
    actor: DataActor,
    local: &kvdb::ReadTxn<'_, OpfsStorage>,
    policy: &MetadataPolicy,
    entry: EntryId,
    scan: &mut cut::Scan,
    mut emit: impl FnMut(Observation) -> crate::store::Result<usize>,
) -> crate::store::Result<()> {
    let (clock, bytes) = scan
        .read(local, &volume_index::entry_key(entry))?
        .ok_or(crate::store::Error::corrupt("namespace declaration disappeared"))?;
    let declaration = ns::source::Declaration::decode(entry, clock, &bytes)?;
    drop(bytes);
    scan.retained = declaration.heap_bytes();
    volume_index::visit_rejected_sources(
        local,
        &stage.accepted_index,
        policy,
        actor,
        entry,
        scan,
        |key, clock, bytes, scan| {
            if let Some(source) = declaration.source(key, clock, bytes)?
                && observations::is_provisional(
                    local,
                    &source.witness,
                    declaration.heap_bytes() + bytes.len() + 4096,
                )?
            {
                scan.retain_materialized(emit(Observation {
                    source: source.witness,
                    status: Status::Excluded,
                    assignment: None,
                })?)?;
            }
            Ok(true)
        },
    )?;
    scan.retained = 0;
    Ok(())
}
