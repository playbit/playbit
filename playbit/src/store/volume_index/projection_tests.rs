//! Source-backed namespace navigation, including reordered/offline publications.
use super::*;

fn observe_received(database: &Db<MemStorage>) {
    let accepted = namespace::source::cut::accepted(&database.begin_read().unwrap()).unwrap();
    namespace::receipt::retain_fixture(database, None, &accepted).unwrap();
    let observations = namespace::receipt::capture(&database.begin_read().unwrap(), None).unwrap();
    let mut tx = database.begin_write(WALL + 100).unwrap();
    let before = namespace::observations::Ledger::load(&tx.local_indexes()).unwrap();
    let mut after = before.clone();
    after.apply_receipt(&observations).unwrap();
    after.write_changes(&before, &mut tx.local_indexes()).unwrap();
    tx.commit().unwrap();
}

#[test]
fn automatic_aliases_route_existing_handles_and_late_children_without_source_moves() {
    // Independent automatic parents coalesce; children published through a
    // still-offline losing identity remain reachable after all sources arrive.
    let a = db();
    let b = db();
    let late = db();
    let mut tx = a.begin_write(WALL).unwrap();
    create_at_path(&mut tx, id(10), "folder/a", &record("a", 1)).unwrap();
    tx.commit().unwrap();
    replicate(&a, &late, false);
    let original = snapshot(&a).lookup("folder").unwrap().id;
    let mut tx = b.begin_write(WALL + 1).unwrap();
    create_at_path(&mut tx, id(20), "folder/b", &record("b", 2)).unwrap();
    tx.commit().unwrap();
    let winner = snapshot(&b).lookup("folder").unwrap().id;
    assert_ne!(original, winner);
    replicate(&b, &a, true);
    let before_late = snapshot(&a);
    assert_eq!(before_late.lookup("folder").unwrap().id, winner);
    assert_eq!(before_late.lookup_from(Some(original), "b").unwrap().id, id(20));
    let mut tx = late.begin_write(WALL + 2).unwrap();
    let mut child = record("c", 3);
    child.namespace.parent = Some(original);
    create(&mut tx, id(30), &child).unwrap();
    tx.commit().unwrap();
    replicate(&late, &a, false);
    let current = snapshot(&a);
    assert_eq!(current.lookup("folder/c").unwrap().id, id(30));
    assert_eq!(current.entry(id(30)).unwrap().info.namespace.parent, Some(original));
    assert_eq!(current.lookup("folder/c").unwrap().info.namespace.parent, Some(winner));
    assert_eq!(current.children(None).unwrap().len(), 1);
    let page = current.page(None, 64).unwrap();
    assert_eq!(page.entries.len(), 4);
    assert!(!page.entries.iter().any(|row| row.id == original));
    assert!(
        page.entries.iter().filter(|row| row.id != winner).all(|row| row.parent == Some(winner))
    );
    let rows = current.children(Some(original)).unwrap();
    assert_eq!(rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
    assert_eq!(rows, current.children(Some(winner)).unwrap());
    let DirectoryListing::Page { parent, ancestors, entries, .. } = current
        .directory_entries(DirectoryTarget::Directory(original), None, None, 64, str::cmp)
        .unwrap()
    else {
        panic!("canonical directory page")
    };
    assert_eq!(parent, Some(winner));
    assert_eq!(ancestors.iter().map(|entry| entry.id).collect::<Vec<_>>(), [winner]);
    assert_eq!(entries, rows);
    assert_eq!(before_late.children(Some(original)).unwrap().len(), 2);
    observe_received(&a);
    let mut tx = a.begin_write(WALL + 101).unwrap();
    rename(&mut tx, original, "archive", id(4), &record("origin", 1).origin).unwrap();
    tx.commit().unwrap();
    assert_eq!(snapshot(&a).lookup("archive/c").unwrap().id, id(30));
    assert_eq!(snapshot(&a).revision(original, id(4)).unwrap().revision, id(4));
    assert!(snapshot(&a).revision(winner, id(4)).is_err());
}

#[test]
fn concurrent_child_restores_retained_ancestry_without_restoring_removed_siblings() {
    // A deletion hides the old child; a concurrent birth restores its same
    // parent identity. A deliberately recreated directory remains independent.
    let a = db();
    let b = db();
    let mut tx = a.begin_write(WALL).unwrap();
    create_at_path(&mut tx, id(10), "folder/old", &record("old", 1)).unwrap();
    tx.commit().unwrap();
    let parent = snapshot(&a).lookup("folder").unwrap().id;
    replicate(&a, &b, false);
    let mut tx = a.begin_write(WALL + 1).unwrap();
    retire(&mut tx, parent, id(2), &record("origin", 1).origin).unwrap();
    tx.commit().unwrap();
    assert!(snapshot(&a).directory(Some(parent)).is_err());
    let mut tx = a.begin_write(WALL + 2).unwrap();
    create_at_path(&mut tx, id(11), "folder/recreated", &record("recreated", 3)).unwrap();
    tx.commit().unwrap();
    let recreated = snapshot(&a).lookup("folder").unwrap().id;
    assert_ne!(parent, recreated);
    assert!(snapshot(&a).directory(Some(parent)).is_err());
    let mut tx = b.begin_write(WALL + 3).unwrap();
    let mut child = record("new", 4);
    child.namespace.parent = Some(parent);
    create(&mut tx, id(12), &child).unwrap();
    tx.commit().unwrap();
    replicate(&b, &a, true);
    let current = snapshot(&a);
    current.directory(Some(parent)).unwrap();
    assert_eq!(current.lookup_from(Some(parent), "new").unwrap().id, id(12));
    assert!(current.lookup_from(Some(parent), "old").is_err());
    assert!(current.lookup_from(Some(parent), "recreated").is_err());
    assert_eq!(current.lookup_from(Some(recreated), "recreated").unwrap().id, id(11));
    assert_eq!(current.children(Some(parent)).unwrap().len(), 1);
    let page = current.page(None, 64).unwrap();
    assert!(!page.entries.iter().find(|row| row.id == parent).unwrap().deleted);
    assert!(page.entries.iter().find(|row| row.id == id(10)).unwrap().deleted);
}

#[test]
fn pending_name_head_does_not_replace_an_admitted_path() {
    // A well-shaped, anchored head with an absent dependency remains Pending.
    // Navigation retains the prior assignment rather than using the LWW head.
    let database = db();
    put(&database, 10, record("original", 1), WALL);
    let birth_snapshot = database.begin_read().unwrap();
    let (clock, bytes) = birth_snapshot.get_with_clock(&entry_key(id(10))).unwrap().unwrap();
    let declaration = namespace::source::Declaration::decode(id(10), clock, &bytes).unwrap();
    let mut observed = vec![declaration.witness().id().unwrap(), id(99)];
    observed.sort();
    let mut tx = database.begin_write(WALL + 1).unwrap();
    let revision = EntryRevision {
        observed,
        revision: id(2),
        creation: creation_digest(id(10), clock, &bytes),
        origin_clock: tx.clock().as_u64(),
        origin: record("origin", 1).origin,
        change: EntryChange::Name { name: "pending".into() },
    };
    let bytes = revision.canonical().unwrap();
    tx.put(&meta_key(NAME, id(10)), &bytes).unwrap();
    tx.put(&revision_key(id(10), id(2)), &bytes).unwrap();
    repair(&mut tx.local_indexes(), &[id(10)]).unwrap();
    tx.commit().unwrap();
    let current = IndexSnapshot::new(database.begin_read().unwrap());
    assert_eq!(current.lookup("original").unwrap().name_revision, id(1));
    assert!(current.lookup("pending").is_err());
    assert_eq!(current.children(None).unwrap()[0].name, "original");
    assert_eq!(current.page(None, 64).unwrap().entries[0].name, "original");
    let mut tx = database.begin_write(WALL + 2).unwrap();
    assert!(
        assign_entry_root(&mut tx, id(10), "pending", id(3), &record("origin", 1).origin).is_err()
    );
    assert!(tx.get(&revision_key(id(10), id(3))).unwrap().is_none());
    drop(tx);
    let mut tx = database.begin_write(WALL + 2).unwrap();
    rename(&mut tx, id(10), "renamed", id(3), &record("origin", 1).origin).unwrap();
    tx.commit().unwrap();
    assert_eq!(snapshot(&database).lookup("renamed").unwrap().id, id(10));
    assert!(snapshot(&database).lookup("pending").is_err());
}

#[test]
fn missing_parents_follow_a_generated_directory_name() {
    // Independent explicit directories retain distinct identities. Creating
    // beneath the loser's exposed name must traverse that exact directory.
    let a = db();
    let b = db();
    let mut directory = record("folder", 1);
    directory.kind = EntryKind::Directory {};
    directory.protocol = None;
    put(&a, 10, directory.clone(), WALL);
    directory.created_revision = id(2);
    put(&b, 20, directory, WALL + 1);
    replicate(&b, &a, false);
    let index = snapshot(&a);
    let exposed =
        index.children(None).unwrap().into_iter().find(|entry| entry.id == id(10)).unwrap().name;
    assert_ne!(exposed, "folder");
    let mut tx = a.begin_write(WALL + 2).unwrap();
    create_at_path(&mut tx, id(30), &format!("{exposed}/missing/file"), &record("file", 3))
        .unwrap();
    tx.commit().unwrap();
    let current = snapshot(&a);
    assert_eq!(current.lookup_from(Some(id(10)), "missing/file").unwrap().id, id(30));
    assert_eq!(current.children(None).unwrap().len(), 2);
    assert!(current.children(Some(id(20))).unwrap().is_empty());
    let mut tx = a.begin_write(WALL + 3).unwrap();
    assert!(matches!(
        move_entry(&mut tx, id(30), None, Some(&exposed), id(4), &record("origin", 1).origin),
        Err(Error::AlreadyExists(_))
    ));
}

#[test]
fn child_publication_uses_effective_ancestry_after_concurrent_move_cycle() {
    // Locally valid concurrent moves form a raw cycle. The resolver's fallback
    // edge keeps both directories usable without rewriting either source move.
    let a = db();
    let b = db();
    let mut directory = record("A", 1);
    directory.kind = EntryKind::Directory {};
    directory.protocol = None;
    put(&a, 10, directory.clone(), WALL);
    directory.name = "B".into();
    directory.created_revision = id(2);
    put(&a, 20, directory.clone(), WALL + 1);
    replicate(&a, &b, false);
    let mut tx = a.begin_write(WALL + 2).unwrap();
    move_entry(&mut tx, id(10), Some(id(20)), None, id(3), &directory.origin).unwrap();
    tx.commit().unwrap();
    let mut tx = b.begin_write(WALL + 3).unwrap();
    move_entry(&mut tx, id(20), Some(id(10)), None, id(4), &directory.origin).unwrap();
    tx.commit().unwrap();
    replicate(&b, &a, true);
    let index = snapshot(&a);
    assert_eq!(index.entry(id(10)).unwrap().info.namespace.parent, Some(id(20)));
    assert_eq!(index.entry(id(20)).unwrap().info.namespace.parent, Some(id(10)));
    index.directory(Some(id(10))).unwrap();
    let mut tx = a.begin_write(WALL + 4).unwrap();
    let mut child = record("child", 5);
    child.namespace.parent = Some(id(10));
    create(&mut tx, id(30), &child).unwrap();
    tx.commit().unwrap();
    assert_eq!(snapshot(&a).lookup_from(Some(id(10)), "child").unwrap().id, id(30));
    observe_received(&a);
    let mut tx = a.begin_write(WALL + 101).unwrap();
    rename(&mut tx, id(10), "renamed", id(6), &directory.origin).unwrap();
    tx.commit().unwrap();
    assert_eq!(snapshot(&a).lookup_from(Some(id(10)), "child").unwrap().id, id(30));
    assert_eq!(snapshot(&a).namespace_entry(id(10)).unwrap().name, "renamed");
}

#[test]
fn creation_cannot_commit_a_cut_that_exceeds_projection_capacity() {
    // Find the admitted boundary using independent remote births. A local
    // creation beyond it must fail before commit, preserving the usable root.
    fn fixture(count: usize) -> Db<MemStorage> {
        let database = db();
        let mut tx = database.begin_write(WALL).unwrap();
        for number in 1..=count {
            let mut bytes = [7; 15];
            bytes[13..].copy_from_slice(&(number as u16).to_be_bytes());
            let entry = EntryId::from_bytes(bytes).unwrap();
            let info = record(&format!("{number:04}{}", "n".repeat(251)), 1);
            tx.put(&entry_key(entry), &info.canonical().unwrap()).unwrap();
        }
        tx.commit().unwrap();
        database
    }
    let (mut low, mut high) = (0, namespace::MAX_ENTRIES + 1);
    while low + 1 < high {
        let count = (low + high) / 2;
        let database = fixture(count);
        match namespace::source::cut::project(&database.begin_read().unwrap()) {
            Ok(_) => low = count,
            Err(Error::LimitExceeded) => high = count,
            Err(error) => panic!("valid independent birth fixture: {error}"),
        }
    }
    assert!(low > 0);
    let database = fixture(low);
    let before = database.begin_read().unwrap().root_hash().unwrap();
    let mut tx = database.begin_write(WALL + 1).unwrap();
    let name = format!("{high:04}{}", "n".repeat(251));
    assert!(matches!(create(&mut tx, id(250), &record(&name, 2)), Err(Error::LimitExceeded)));
    drop(tx);
    assert_eq!(database.begin_read().unwrap().root_hash().unwrap(), before);
    assert!(!database.begin_read().unwrap().has(&entry_key(id(250))).unwrap());
    assert!(
        namespace::observations::Publication::capture(&database.begin_read().unwrap())
            .unwrap()
            .observed
            .is_empty()
    );
    assert_eq!(snapshot(&database).children(None).unwrap().len(), low);
}

#[test]
fn file_lookup_keeps_accepted_content_and_visibility_when_new_heads_are_pending() {
    let database = db();
    let mut initial = record("notes", 1);
    let content = |byte| EntryKind::Blob {
        len_bytes: 1,
        digest: [byte; 32],
        encrypted_key: [byte; 32],
        published_at_micros: i64::from(byte),
    };
    initial.kind = content(1);
    initial.protocol = None;
    put(&database, 10, initial, WALL);
    content_db(&database, 10, content(2), 2, WALL + 1);
    let pinned = snapshot(&database);
    let mut observed =
        namespace::observations::Publication::capture(&database.begin_read().unwrap())
            .unwrap()
            .observed;
    observed.push(id(99));
    observed.sort_unstable();
    for (number, change, register) in [
        (3, EntryChange::Content { kind: content(3) }, CONTENT),
        (4, EntryChange::Delete {}, RETIRED),
    ] {
        let mut tx = database.begin_write(WALL + u64::from(number)).unwrap();
        let (mut event, _) =
            change_event(&tx, id(10), id(number), change, &record("origin", 1).origin).unwrap();
        event.observed = observed.clone();
        let bytes = event.canonical().unwrap();
        tx.put(&meta_key(register, id(10)), &bytes).unwrap();
        tx.put(&revision_key(id(10), id(number)), &bytes).unwrap();
        repair(&mut tx.local_indexes(), &[id(10)]).unwrap();
        tx.commit().unwrap();
        let current = snapshot(&database);
        let file = current.lookup("notes").unwrap();
        assert_eq!(file.content_revision, id(2));
        assert_eq!(file.info.kind, content(2));
        assert!(!file.deleted);
        assert!(
            current
                .namespace_projection()
                .unwrap()
                .admission
                .events
                .values()
                .any(|state| *state == namespace::EventState::Pending)
        );
    }
    assert_eq!(pinned.lookup("notes").unwrap().info.kind, content(2));
}

#[test]
fn accepted_file_content_preserves_the_register_tie_break() {
    let a = db();
    let b = db();
    let mut initial = record("notes", 1);
    let content = |byte| EntryKind::Blob {
        len_bytes: 1,
        digest: [byte; 32],
        encrypted_key: [byte; 32],
        published_at_micros: i64::from(byte),
    };
    initial.kind = content(1);
    initial.protocol = None;
    put(&a, 10, initial, WALL);
    replicate(&a, &b, false);
    content_db(&a, 10, content(2), 2, WALL + 200);
    content_db(&b, 10, content(3), 3, WALL + 200);
    let left_read = a.begin_read().unwrap();
    let right_read = b.begin_read().unwrap();
    let left = left_read.get_with_clock(&meta_key(CONTENT, id(10))).unwrap().unwrap();
    let right = right_read.get_with_clock(&meta_key(CONTENT, id(10))).unwrap().unwrap();
    assert_eq!(left.0, right.0);
    let expected =
        revision(if left.1.as_ref() > right.1.as_ref() { &left.1 } else { &right.1 }).unwrap();
    replicate(&a, &b, true);
    replicate(&b, &a, false);
    for database in [&a, &b] {
        let file = snapshot(database).lookup("notes").unwrap();
        assert_eq!(file.content_revision, expected.revision);
        let EntryChange::Content { kind } = &expected.change else { panic!() };
        assert_eq!(&file.info.kind, kind);
    }
}

#[test]
#[allow(
    clippy::arc_with_non_send_sync,
    reason = "The single-threaded memory fixture uses the production cache's Arc database identity."
)]
fn namespace_cache_reuses_only_the_exact_database_and_pinned_commit() {
    use std::sync::Arc;
    let database = Arc::new(db());
    let cache = NamespaceCache::default();
    let empty = IndexSnapshot::cached(&database, &cache).unwrap();
    assert!(empty.children(None).unwrap().is_empty());
    let mut tx = database.begin_write(WALL).unwrap();
    create(&mut tx, id(10), &record("before", 1)).unwrap();
    tx.commit().unwrap();
    let before = IndexSnapshot::cached(&database, &cache).unwrap();
    assert_eq!(before.lookup("before").unwrap().id, id(10));
    let same = IndexSnapshot::cached(&database, &cache).unwrap();
    same.namespace_projection().unwrap();
    assert!(Arc::ptr_eq(before.namespace.get().unwrap(), same.namespace.get().unwrap()));

    let root = database.begin_read().unwrap().root_hash().unwrap();
    let mut tx = database.begin_write(WALL).unwrap();
    // Existing snapshots retain this predecessor, so a writer cannot consume it.
    assert!(cache.predecessor(&database, &tx).take().unwrap().is_none());
    tx.local_indexes().put(b"cache-local-change", b"receipt").unwrap();
    tx.commit().unwrap();
    assert_eq!(database.begin_read().unwrap().root_hash().unwrap(), root);
    let next = database.begin_write(WALL).unwrap();
    assert!(cache.predecessor(&database, &next).take().unwrap().is_none());
    drop(next);
    let local = IndexSnapshot::cached(&database, &cache).unwrap();
    local.namespace_projection().unwrap();
    assert!(!Arc::ptr_eq(before.namespace.get().unwrap(), local.namespace.get().unwrap()));

    let delayed = IndexSnapshot::cached(&database, &cache).unwrap();
    let mut tx = database.begin_write(WALL + 1).unwrap();
    rename(&mut tx, id(10), "after", id(2), &record("origin", 1).origin).unwrap();
    tx.commit().unwrap();
    let after = IndexSnapshot::cached(&database, &cache).unwrap();
    assert!(after.lookup("before").is_err());
    assert_eq!(after.lookup("after").unwrap().id, id(10));
    assert_eq!(before.lookup("before").unwrap().id, id(10));
    // An older reader may publish its projection after a newer reader. The
    // next current read must still reject that cache entry's old clock.
    assert_eq!(delayed.lookup("before").unwrap().id, id(10));
    let current = IndexSnapshot::cached(&database, &cache).unwrap();
    assert_eq!(current.lookup("after").unwrap().id, id(10));
    assert!(current.lookup("before").is_err());
    assert_eq!(Arc::strong_count(&database), 1);

    // Equal clocks in another database must never alias this store-wide slot.
    let other = Arc::new(db());
    let mut tx = other.begin_write(WALL).unwrap();
    create(&mut tx, id(20), &record("other", 1)).unwrap();
    tx.commit().unwrap();
    assert_eq!(other.begin_read().unwrap().clock().unwrap(), before.tx.clock().unwrap());
    let next = other.begin_write(WALL).unwrap();
    assert!(cache.predecessor(&other, &next).take().unwrap().is_none());
    drop(next);
    let other_snapshot = IndexSnapshot::cached(&other, &cache).unwrap();
    assert_eq!(other_snapshot.lookup("other").unwrap().id, id(20));
    assert!(other_snapshot.lookup("before").is_err());
    assert!(!Arc::ptr_eq(before.namespace.get().unwrap(), other_snapshot.namespace.get().unwrap()));

    let exclusive = NamespaceCache::default();
    {
        let snapshot = IndexSnapshot::cached(&database, &exclusive).unwrap();
        snapshot.namespace_projection().unwrap();
    }
    // An exclusive cache is still stale after another source transaction.
    let mut changed = database.begin_write(WALL + 2).unwrap();
    rename(&mut changed, id(10), "newer", id(3), &record("origin", 1).origin).unwrap();
    changed.commit().unwrap();
    let next = database.begin_write(WALL + 3).unwrap();
    assert!(exclusive.predecessor(&database, &next).take().unwrap().is_none());
    drop(next);
    {
        let snapshot = IndexSnapshot::cached(&database, &exclusive).unwrap();
        snapshot.namespace_projection().unwrap();
    }
    let next = database.begin_write(WALL + 3).unwrap();
    let token = exclusive.predecessor(&database, &next);
    let taken = token.take().unwrap().unwrap();
    assert_eq!(taken.admission.view.entries[&id(10)].name, "newer");
    assert!(exclusive.predecessor(&database, &next).take().unwrap().is_none());
}
