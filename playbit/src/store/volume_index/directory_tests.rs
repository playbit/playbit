#![allow(clippy::unwrap_used, reason = "Tests assert bounded persistent directory fixtures.")]
use super::*;
use kvdb::{CreateOptions, Db, storage::mem::MemStorage};

const WALL: u64 = kvdb::clock::EPOCH_UNIX_SECS + 1000;
fn id(number: u64) -> EntryId {
    let mut bytes = [0; 15];
    bytes[7..].copy_from_slice(&number.to_be_bytes());
    EntryId::from_bytes(bytes).unwrap()
}
fn database() -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap()
}
fn birth(number: u64, name: &str) -> EntryInfo {
    EntryInfo {
        namespace: Default::default(),
        origin: namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
        initial_permissions: EntryAcl::Inherit,
        created_revision: id(2000 + number),
        name: name.into(),
        kind: EntryKind::KvDb {},
        protocol: Some(DatabaseProtocol::Kvdb),
    }
}

#[test]
fn direct_child_pages_keep_one_cut_and_effective_ancestry_after_moves() {
    let database = database();
    let mut tx = database.begin_write(WALL).unwrap();
    create_at_path(&mut tx, id(1), "foo/bar/first", &birth(1, "first")).unwrap();
    create_at_path(&mut tx, id(2), "foo/bar/second", &birth(2, "second")).unwrap();
    tx.commit().unwrap();
    let pinned = IndexSnapshot::new(database.begin_read().unwrap());
    let foo = pinned.lookup("foo").unwrap().id;
    let bar = pinned.lookup("foo/bar").unwrap().id;
    let DirectoryListing::Page { cut, parent, ancestors, entries, next } =
        pinned.directory_entries(DirectoryTarget::Entry(id(1)), None, None, 1, str::cmp).unwrap()
    else {
        panic!("initial page")
    };
    assert_eq!(parent, Some(bar));
    assert_eq!(
        ancestors.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(),
        ["foo", "bar"]
    );
    assert_eq!(ancestors.iter().map(|entry| entry.id).collect::<Vec<_>>(), [foo, bar]);
    assert_eq!(entries.iter().map(|entry| entry.id).collect::<Vec<_>>(), [id(1)]);
    assert_eq!(next, Some(id(1)));
    let mut tx = database.begin_write(WALL + 1).unwrap();
    move_entry(&mut tx, bar, None, Some("bar2"), id(3001), &birth(1, "").origin).unwrap();
    tx.commit().unwrap();
    let current = IndexSnapshot::new(database.begin_read().unwrap());
    assert!(matches!(
        current.directory_entries(DirectoryTarget::Directory(bar), Some(cut), next, 1, str::cmp).unwrap(),
        DirectoryListing::Changed { cut: changed } if changed != cut
    ));
    let DirectoryListing::Page { ancestors, entries, next, .. } = pinned
        .directory_entries(DirectoryTarget::Directory(bar), Some(cut), next, 1, str::cmp)
        .unwrap()
    else {
        panic!("pinned continuation")
    };
    assert_eq!(ancestors.iter().map(|entry| entry.id).collect::<Vec<_>>(), [foo, bar]);
    assert_eq!(entries[0].id, id(2));
    assert_eq!(next, None);
    let DirectoryListing::Page { parent, ancestors, entries, .. } =
        current.directory_entries(DirectoryTarget::Entry(id(1)), None, None, 64, str::cmp).unwrap()
    else {
        panic!("moved entry parent")
    };
    assert_eq!(parent, Some(bar));
    assert_eq!(ancestors.len(), 1);
    assert_eq!(
        (ancestors[0].id, ancestors[0].parent, ancestors[0].name.as_str()),
        (bar, None, "bar2")
    );
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| entry.parent == Some(bar)));
    assert!(
        current.directory_entries(DirectoryTarget::Root, None, Some(id(1)), 32, str::cmp).is_err()
    );
    assert!(
        current
            .directory_entries(DirectoryTarget::Root, Some(cut + 1), Some(id(1)), 0, str::cmp)
            .is_err()
    );
    let current_cut = current.tx.clock().unwrap().as_u64();
    assert!(
        current
            .directory_entries(DirectoryTarget::Root, Some(current_cut), Some(id(1)), 32, str::cmp)
            .is_err()
    );
    assert!(
        current
            .directory_entries(DirectoryTarget::Directory(id(1)), None, None, 32, str::cmp)
            .is_err()
    );
}

#[cfg(any(feature = "native-ui", feature = "app-host"))]
#[test]
fn unicode_sibling_order_is_global_across_identity_and_page_boundaries() {
    let database = database();
    let mut tx = database.begin_write(WALL).unwrap();
    for number in 1..=130 {
        let name = match number {
            1 => "Café".into(),
            2 => "Cafe\u{301}".into(),
            _ if number % 2 == 0 => format!("Family-{:03}", 130 - number),
            _ => format!("éclair-{:03}", 130 - number),
        };
        create(&mut tx, id(number), &birth(number, &name)).unwrap();
    }
    tx.commit().unwrap();
    let snapshot = IndexSnapshot::new(database.begin_read().unwrap());
    let compare = crate::ui::collation::compare;
    for limit in [1, 32, 64] {
        let (mut after, mut cut) = (None, None);
        let mut observed = Vec::new();
        loop {
            let DirectoryListing::Page { cut: got, ancestors, entries, next, .. } = snapshot
                .directory_entries(DirectoryTarget::Root, cut, after, limit, compare)
                .unwrap()
            else {
                panic!("unchanged pinned page")
            };
            assert!(ancestors.is_empty());
            assert!(!entries.is_empty() && entries.len() <= limit);
            cut = Some(got);
            observed.extend(entries);
            after = next;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(observed.len(), 130);
        assert!(observed.windows(2).all(|rows| {
            compare(&rows[0].name, &rows[1].name).then_with(|| rows[0].id.cmp(&rows[1].id)).is_lt()
        }));
        assert_eq!(compare(&observed[0].name, &observed[1].name), Ordering::Equal);
        assert_eq!((observed[0].id, observed[1].id), (id(1), id(2)));
        assert!(observed[2..66].iter().all(|entry| entry.name.starts_with("éclair")));
        assert!(observed[66..].iter().all(|entry| entry.name.starts_with("Family")));
    }
}

#[test]
fn maximum_page_and_ancestry_fit_the_existing_serialized_credit() {
    let name = "\\".repeat(MAX_ENTRY_COMPONENT_BYTES);
    let listing = DirectoryListing::Page {
        cut: u64::MAX,
        parent: Some(id(1)),
        ancestors: (1..=namespace::MAX_DEPTH)
            .map(|number| DirectoryAncestor {
                id: id(number as u64),
                parent: Some(id(number as u64 + 1)),
                name: name.clone(),
            })
            .collect(),
        entries: (1..=MAX_ENTRY_PAGE)
            .map(|number| EntrySummary {
                id: id(number as u64),
                parent: Some(id(1)),
                name: name.clone(),
                kind: crate::store::EntryType::ObjectDb,
                protocol: Some(DatabaseProtocol::BlobLog),
                deleted: false,
                name_conflict: false,
            })
            .collect(),
        next: Some(id(1)),
    };
    assert!(serde_json::to_vec(&listing).unwrap().len() <= summaries::MAX_SERIALIZED_BYTES);
}
