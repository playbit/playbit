use super::*;

fn id(value: u16) -> EntryId {
    let mut bytes = [1; 15];
    bytes[13..].copy_from_slice(&value.to_be_bytes());
    EntryId::from_bytes(bytes).expect("nonzero fixture identity")
}
fn event(number: u16, observed: &[u16], change: Change) -> Fact {
    let mut observed: Vec<_> = observed.iter().copied().map(id).collect();
    observed.sort();
    Fact {
        id: id(number),
        clock: u64::from(number),
        observed: observed.into(),
        origin: Origin { installation: Id::from_bytes([1; 16]), label: Some("Work".into()) },
        change,
    }
}
fn birth(number: u16, parent: Option<u16>, name: &str, kind: Kind, observed: &[u16]) -> Fact {
    event(
        number,
        observed,
        Change::Birth {
            creation_context: None,
            entry: id(number),
            parent: parent.map(id),
            name: name.into(),
            kind,
            automatic: kind == Kind::Directory,
            deletions: vec![],
            departures: vec![],
        },
    )
}
fn edit(
    number: u16,
    entry: u16,
    parent: Option<Option<u16>>,
    name: Option<&str>,
    content: bool,
    observed: &[u16],
) -> Fact {
    event(
        number,
        observed,
        Change::Edit {
            entry: id(entry),
            parent: parent.map(|parent| parent.map(id)),
            name: name.map(str::to_owned),
            content,
        },
    )
}
fn delete(number: u16, entry: u16, observed: &[u16]) -> Fact {
    event(number, observed, Change::Delete { entry: id(entry) })
}
fn converges(facts: &[Fact]) -> View {
    let expected = derive(facts).expect("valid namespace fixture");
    let mut shuffled = facts.to_vec();
    let mut rng = 0x81728123u64;
    for _ in 0..128 {
        for i in (1..shuffled.len()).rev() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            shuffled.swap(i, rng as usize % (i + 1));
        }
        assert_eq!(derive(&shuffled).expect("reordering preserves validity"), expected);
    }
    expected
}
#[test]
fn nested_coalescence_retains_late_children_and_all_histories() {
    let facts = [
        birth(1, None, "D", Kind::Directory, &[]),
        birth(2, None, "D", Kind::Directory, &[]),
        birth(3, Some(1), "S", Kind::Directory, &[1]),
        birth(4, Some(2), "S", Kind::Directory, &[2]),
        birth(5, Some(3), "a", Kind::File, &[1, 3]),
        birth(6, Some(4), "b", Kind::File, &[2, 4]),
        birth(7, None, "D", Kind::Directory, &[]),
        edit(8, 3, None, Some("T"), false, &[1, 3]),
        birth(9, Some(3), "late", Kind::File, &[1, 3]),
    ];
    let view = converges(&facts);
    assert_eq!(view.canonical[&id(1)], id(7));
    assert_eq!(view.canonical[&id(2)], id(7));
    assert_eq!(view.canonical[&id(3)], id(4));
    assert_eq!(view.entries[&id(4)].name, "T");
    for number in [5, 6, 9] {
        assert_eq!(view.entries[&id(number)].parent, Some(id(4)));
    }
}
#[test]
fn losing_rename_preserves_only_its_branch() {
    let facts = [
        birth(1, None, "D", Kind::Directory, &[]),
        birth(2, Some(1), "S", Kind::Directory, &[1]),
        birth(3, Some(2), "a", Kind::File, &[1, 2]),
        birth(4, Some(1), "sibling", Kind::File, &[1]),
        edit(5, 2, None, Some("pictures"), false, &[1, 2, 3, 4]),
        edit(6, 2, None, Some("images"), false, &[1, 2, 3, 4]),
        delete(7, 1, &[1, 2, 3, 4, 6]),
    ];
    let view = converges(&facts);
    assert!(view.entries[&id(1)].visible);
    assert!(view.entries[&id(2)].visible);
    assert!(view.entries[&id(3)].visible);
    assert!(!view.entries[&id(4)].visible);
    assert_eq!(view.entries[&id(2)].name, "images");
    let mut later = facts.to_vec();
    later.push(delete(8, 1, &[1, 2, 3, 4, 5, 6, 7]));
    assert!(converges(&later).entries.values().all(|entry| !entry.visible));
}
#[test]
fn unseen_duplicate_edit_preserves_descendants_of_canonical_branch() {
    let facts = [
        birth(1, None, "D", Kind::Directory, &[]),
        birth(2, Some(1), "S", Kind::Directory, &[1]),
        birth(3, Some(2), "a", Kind::File, &[1, 2]),
        birth(4, Some(1), "S", Kind::Directory, &[1]),
        edit(5, 4, None, Some("renamed"), false, &[1, 4]),
        delete(6, 1, &[1, 2, 3]),
    ];
    let view = converges(&facts);
    assert!(view.entries.values().all(|entry| entry.visible));
    assert_eq!(view.entries[&id(4)].name, "renamed");
}
#[test]
fn concurrent_content_preserves_file_not_sibling_and_direct_delete_wins() {
    let facts = [
        birth(1, None, "D", Kind::Directory, &[]),
        birth(2, Some(1), "a", Kind::File, &[1]),
        birth(3, Some(1), "b", Kind::File, &[1]),
        edit(4, 2, None, None, true, &[1, 2]),
        delete(5, 1, &[1, 2, 3]),
    ];
    let view = converges(&facts);
    assert!(view.entries[&id(1)].visible);
    assert!(view.entries[&id(2)].visible);
    assert!(!view.entries[&id(3)].visible);
    let mut direct = facts.to_vec();
    direct.push(delete(6, 2, &[1, 2]));
    assert!(converges(&direct).entries.values().all(|entry| !entry.visible));
}
#[test]
fn concurrent_child_resurrects_ancestors_without_removed_siblings() {
    let facts = [
        birth(1, None, "D", Kind::Directory, &[]),
        birth(2, Some(1), "S", Kind::Directory, &[1]),
        birth(3, Some(2), "old", Kind::File, &[1, 2]),
        birth(4, Some(2), "new", Kind::File, &[1, 2]),
        delete(5, 1, &[1, 2, 3]),
        edit(6, 2, None, Some("renamed"), false, &[1, 2, 3, 4, 5]),
    ];
    let view = converges(&facts);
    assert!(view.entries[&id(1)].visible);
    assert!(view.entries[&id(2)].visible);
    assert!(!view.entries[&id(3)].visible);
    assert!(view.entries[&id(4)].visible);
}
#[test]
fn cycle_fallback_keeps_independent_name_and_deleted_parent_location() {
    let facts = [
        birth(1, None, "A", Kind::Directory, &[]),
        birth(2, None, "B", Kind::Directory, &[]),
        edit(3, 1, Some(Some(2)), None, false, &[1, 2]),
        edit(4, 2, Some(Some(1)), None, false, &[1, 2]),
        edit(5, 1, None, Some("renamed"), false, &[1]),
    ];
    let view = converges(&facts);
    assert_eq!(view.entries[&id(2)].parent, None);
    assert_eq!(view.entries[&id(1)].parent, Some(id(2)));
    assert_eq!(view.entries[&id(1)].name, "renamed");
    let mut later = facts.to_vec();
    later.push(delete(6, 2, &[1, 2, 3, 4, 5]));
    let view = converges(&later);
    assert!(!view.entries[&id(1)].visible);
    assert_eq!(view.entries[&id(1)].parent, Some(id(2)));
}
#[test]
fn move_outside_deletion_scope_preserves_subtree() {
    let facts = [
        birth(1, None, "D", Kind::Directory, &[]),
        birth(2, Some(1), "S", Kind::Directory, &[1]),
        birth(3, Some(2), "a", Kind::File, &[1, 2]),
        birth(4, None, "E", Kind::Directory, &[]),
        edit(5, 2, Some(Some(4)), None, false, &[1, 2, 3, 4]),
        delete(6, 1, &[1, 2, 3, 4]),
    ];
    let view = converges(&facts);
    assert!(!view.entries[&id(1)].visible);
    assert!(view.entries[&id(2)].visible);
    assert!(view.entries[&id(3)].visible);
    assert_eq!(view.entries[&id(2)].parent, Some(id(4)));
}
#[test]
fn exact_deletion_context_distinguishes_recreation() {
    let mut facts =
        vec![birth(1, None, "D", Kind::Directory, &[]), delete(2, 1, &[1]), delete(3, 1, &[1])];
    for (number, context) in
        [(4, vec![1, 2]), (5, vec![1, 3]), (6, vec![1, 2, 3]), (7, vec![1, 2, 3])]
    {
        let mut fact = birth(number, None, "D", Kind::Directory, &context);
        if let Change::Birth { deletions, .. } = &mut fact.change {
            *deletions = context.iter().copied().filter(|&n| n != 1).map(id).collect();
        }
        facts.push(fact);
    }
    let view = converges(&facts);
    assert!(!view.entries[&id(1)].visible);
    assert_eq!(view.canonical[&id(6)], id(7));
    assert_eq!(view.entries.values().filter(|entry| entry.visible).count(), 3);
}
#[test]
fn conflict_allocator_reserves_literal_names_and_reclaims_after_delete() {
    let facts = [
        birth(1, None, "notes.txt", Kind::File, &[]),
        birth(2, None, "notes.txt", Kind::File, &[]),
        birth(3, None, "notes.txt", Kind::File, &[]),
        birth(4, None, "notes (conflict Work).txt", Kind::File, &[]),
    ];
    let view = converges(&facts);
    assert_eq!(view.entries[&id(3)].name, "notes.txt");
    assert_eq!(view.entries[&id(2)].name, "notes (conflict Work, 2).txt");
    assert_eq!(view.entries[&id(1)].name, "notes (conflict Work, 3).txt");
    let mut later = facts.to_vec();
    later.push(delete(5, 3, &[1, 2, 3, 4]));
    assert_eq!(converges(&later).entries[&id(2)].name, "notes.txt");
}
#[test]
fn provenance_comes_from_effective_namespace_assignment_not_content() {
    let mut facts = vec![
        birth(1, None, "D", Kind::Directory, &[]),
        birth(2, None, "a", Kind::File, &[]),
        birth(3, Some(1), "b", Kind::File, &[1]),
        edit(4, 2, Some(Some(1)), None, false, &[1, 2]),
        edit(5, 2, None, Some("b"), false, &[2]),
        edit(6, 2, None, None, true, &[2]),
    ];
    facts[3].origin.label = Some("Move".into());
    facts[4].origin.label = Some("Rename".into());
    facts[5].origin.label = Some("Content".into());
    assert_eq!(converges(&facts).entries[&id(2)].name, "b (conflict Rename)");
}
#[test]
fn label_tables_and_utf8_budgets_are_exact() {
    let origin = Origin {
        installation: Id::from_bytes([1; 16]),
        label: Some(" \u{202e}Jane/Work\\Mac\n👩\u{200d}💻 \u{2069}".into()),
    };
    assert_eq!(label(&origin), "Jane-Work-Mac 👩\u{200d}💻");
    let mut facts = vec![
        birth(1, None, &format!("{}.txt", "é".repeat(125)), Kind::File, &[]),
        birth(2, None, &format!("{}.txt", "é".repeat(125)), Kind::File, &[]),
    ];
    facts[0].origin.label = Some("é".repeat(100));
    let view = converges(&facts);
    assert!(view.entries[&id(1)].name.len() <= 255);
    assert!(view.entries[&id(1)].name.ends_with("…).txt"));
    component(&view.entries[&id(1)].name).expect("generated name is a valid component");
}
#[test]
fn malformed_dependencies_and_nonintentional_edits_fail() {
    let missing = [birth(2, Some(1), "x", Kind::File, &[1])];
    assert!(matches!(derive(&missing), Err(Error::NotFound(_))));
    let mut facts =
        vec![birth(1, None, "a", Kind::File, &[]), edit(2, 1, None, Some("a"), false, &[1])];
    assert!(matches!(derive(&facts), Err(Error::InvalidInput(_))));
    facts[1] = delete(2, 1, &[1]);
    facts.push(delete(3, 1, &[2]));
    assert!(matches!(derive(&facts), Err(Error::InvalidInput(_))));
}

#[test]
fn observed_deletion_rejects_new_relative_work_without_resurrection() {
    let mut facts = vec![birth(1, None, "D", Kind::Directory, &[]), delete(2, 1, &[1])];
    facts.push(birth(3, Some(1), "new", Kind::File, &[1, 2]));
    assert!(matches!(derive(&facts), Err(Error::NotFound("deleted birth parent"))));
    facts[2] = edit(3, 1, None, Some("renamed"), false, &[1, 2]);
    assert!(matches!(derive(&facts), Err(Error::NotFound("deleted edit target"))));
}

#[test]
fn directory_context_cannot_omit_relevant_or_add_unrelated_deletions() {
    let missing = vec![
        birth(1, None, "D", Kind::Directory, &[]),
        delete(2, 1, &[1]),
        birth(3, None, "D", Kind::Directory, &[1, 2]),
    ];
    assert!(matches!(
        derive(&missing),
        Err(Error::InvalidInput("inexact directory birth context"))
    ));
    let mut unrelated = birth(4, None, "D", Kind::Directory, &[2, 3]);
    if let Change::Birth { deletions, .. } = &mut unrelated.change {
        deletions.push(id(3));
    }
    let facts = vec![
        birth(1, None, "D", Kind::Directory, &[]),
        birth(2, None, "X", Kind::Directory, &[]),
        delete(3, 2, &[2]),
        unrelated,
    ];
    assert!(matches!(derive(&facts), Err(Error::InvalidInput("inexact directory birth context"))));
}

#[test]
fn automatic_birth_after_departure_does_not_merge_with_vacated_identity() {
    let mut replacement = birth(3, None, "D", Kind::Directory, &[1, 2]);
    if let Change::Birth { departures, .. } = &mut replacement.change {
        departures.push(id(2));
    }
    let facts = vec![
        birth(1, None, "D", Kind::Directory, &[]),
        edit(2, 1, None, Some("E"), false, &[1]),
        replacement,
    ];
    let view = converges(&facts);
    assert_eq!(view.entries[&id(1)].name, "E");
    assert_eq!(view.entries[&id(3)].name, "D");
    assert_eq!(view.canonical[&id(1)], id(1));
}

#[test]
fn deletion_after_departure_does_not_split_replacement_directory() {
    let mut first = birth(4, None, "D", Kind::Directory, &[1, 2]);
    let mut second = birth(5, None, "D", Kind::Directory, &[1, 2, 3]);
    for fact in [&mut first, &mut second] {
        if let Change::Birth { departures, .. } = &mut fact.change {
            departures.push(id(2));
        }
    }
    let view = converges(&[
        birth(1, None, "D", Kind::Directory, &[]),
        edit(2, 1, None, Some("E"), false, &[1]),
        delete(3, 1, &[1, 2]),
        first,
        second,
    ]);
    assert_eq!(view.canonical[&id(4)], id(5));
    assert!(!view.entries[&id(1)].visible);
    assert!(view.entries[&id(5)].visible);
}

#[test]
fn observed_destination_cannot_be_recreated_until_it_leaves() {
    let facts =
        [birth(1, None, "D", Kind::Directory, &[]), birth(2, None, "D", Kind::Directory, &[1])];
    assert!(matches!(derive(&facts), Err(Error::AlreadyExists("observed birth destination"))));
}

#[test]
fn fixed_generation_allocation_handles_supported_entry_limit() {
    let facts: Vec<_> = (1..=MAX_ENTRIES as u16)
        .map(|number| birth(number, None, "notes.txt", Kind::File, &[]))
        .collect();
    let view = derive(&facts).expect("supported entry count");
    assert_eq!(view.entries.len(), MAX_ENTRIES);
    let names: BTreeSet<_> = view.entries.values().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names.len(), MAX_ENTRIES);
    assert_eq!(view.entries[&id(MAX_ENTRIES as u16)].name, "notes.txt");
    assert!(view.entries.values().all(|entry| entry.name.len() <= MAX_COMPONENT_BYTES));
    let mut overflow = facts;
    overflow.push(birth(MAX_ENTRIES as u16 + 1, None, "another", Kind::File, &[]));
    assert!(matches!(derive(&overflow), Err(Error::LimitExceeded)));
}

#[test]
fn wire_roundtrip_preserves_moves_to_root_and_unchanged_parents() {
    let prefix =
        [birth(1, None, "D", Kind::Directory, &[]), birth(2, Some(1), "file", Kind::File, &[1])];
    for parent in [None, Some(None)] {
        let moved = edit(3, 2, parent, Some("renamed"), false, &[1, 2]);
        let bytes = serde_json::to_vec(&moved).expect("namespace event encoding");
        let decoded: Fact = serde_json::from_slice(&bytes).expect("namespace event decoding");
        assert_eq!(decoded, moved);
        let facts = [prefix[0].clone(), prefix[1].clone(), decoded];
        let view = derive(&facts).expect("roundtripped edit");
        assert_eq!(view.entries[&id(2)].parent, if parent.is_some() { None } else { Some(id(1)) });
    }
    let moved = edit(3, 1, Some(Some(2)), None, false, &[1, 2]);
    assert_eq!(
        serde_json::from_slice::<Fact>(&serde_json::to_vec(&moved).expect("encoding"))
            .expect("decoding"),
        moved,
    );
}

#[test]
fn causal_admission_quarantines_invalid_edits_without_poisoning_other_entries() {
    let mut facts = vec![
        birth(1, None, "A", Kind::File, &[]),
        birth(2, None, "B", Kind::File, &[]),
        // An entry writer cannot invent an observed parent or restore an edit
        // rejected at its own causal cut by declaring a later dependent edit.
        edit(3, 1, Some(Some(99)), None, false, &[1]),
        edit(4, 1, None, Some("hidden"), false, &[1, 3]),
        edit(5, 2, None, Some("visible"), false, &[2]),
    ];
    let expected = admit(&facts).expect("bounded source cut");
    assert_eq!(expected.events[&id(3)], EventState::Rejected);
    assert_eq!(expected.events[&id(4)], EventState::Rejected);
    assert_eq!(expected.events[&id(5)], EventState::Accepted);
    assert_eq!(expected.view.entries[&id(1)].name, "A");
    assert_eq!(expected.view.entries[&id(2)].name, "visible");
    facts.reverse();
    assert_eq!(admit(&facts).expect("reordered cut"), expected);
}

#[test]
fn incomplete_causal_cuts_retain_accepted_view_until_dependencies_arrive() {
    let a = birth(1, None, "D", Kind::Directory, &[]);
    let b = birth(2, Some(1), "child", Kind::File, &[1]);
    let pending = admit(std::slice::from_ref(&b)).expect("pending cut");
    assert_eq!(pending.events[&id(2)], EventState::Pending);
    assert!(pending.view.entries.is_empty());
    let complete = admit(&[b.clone(), a.clone()]).expect("complete cut");
    assert_eq!(complete.view, derive(&[a.clone(), b.clone()]).expect("valid full projection"));
    assert!(complete.events.values().all(|state| *state == EventState::Accepted));
    assert_eq!(admit(&[a.clone(), b.clone(), a, b]).expect("identical replay"), complete);
}

#[test]
fn event_ids_bind_entry_revision_and_canonical_source() {
    assert_ne!(
        event_id(id(1), id(3), [7; 32]).expect("provisional payload"),
        event_id(id(1), id(3), [8; 32]).expect("authoritative replacement payload")
    );
    assert_ne!(
        event_id(id(1), id(3), [7; 32]).expect("event ID"),
        event_id(id(2), id(3), [7; 32]).expect("event ID")
    );
    assert_ne!(
        event_id(id(1), id(3), [7; 32]).expect("event ID"),
        event_id(id(1), id(4), [7; 32]).expect("event ID")
    );
    assert_eq!(
        event_id(id(1), id(3), [7; 32]).expect("event ID"),
        event_id(id(1), id(3), [7; 32]).expect("stable ID")
    );
}

#[test]
fn one_transaction_can_create_parents_and_child_at_one_storage_clock() {
    // Reverse ID priority ensures admission uses causality, not tie-break rank.
    let mut facts = [
        birth(3, None, "parent", Kind::Directory, &[]),
        birth(2, Some(3), "child", Kind::Directory, &[3]),
        birth(1, Some(2), "file", Kind::File, &[2, 3]),
    ];
    for fact in &mut facts {
        fact.clock = 10;
    }
    let view = converges(&facts);
    assert_eq!(view.entries[&id(1)].parent, Some(id(2)));
    let admission = admit(&facts).expect("same-transaction causal chain");
    assert_eq!(admission.view, view);
    assert!(admission.events.values().all(|state| *state == EventState::Accepted));
    facts.reverse();
    assert_eq!(admit(&facts).expect("reordered chain"), admission);

    // Equal clocks do not permit cycles or incomplete transitive contexts.
    facts[0].observed = vec![id(2)].into();
    assert_eq!(
        admit(&facts).expect("invalid context classified").events[&id(1)],
        EventState::Rejected
    );
    let mut cycle = [
        birth(1, Some(2), "a", Kind::Directory, &[2]),
        birth(2, Some(1), "b", Kind::Directory, &[1]),
    ];
    for fact in &mut cycle {
        fact.clock = 10;
    }
    assert!(derive(&cycle).is_err());
    assert!(
        admit(&cycle)
            .expect("cycle classified")
            .events
            .values()
            .all(|state| *state == EventState::Rejected)
    );
}

#[test]
fn assignments_must_follow_observed_clocks_including_coalesced_aliases() {
    let births = [
        birth(1, None, "shared", Kind::Directory, &[]),
        birth(2, None, "shared", Kind::Directory, &[]),
    ];
    let mut first = edit(9, 1, None, Some("first"), false, &[1, 2]);
    first.clock = 10;
    for target in [1, 2] {
        for mut later in [
            edit(3, target, None, Some("later"), false, &[1, 2, 9]),
            edit(3, target, None, None, true, &[1, 2, 9]),
            delete(3, target, &[1, 2, 9]),
        ] {
            later.clock = 10;
            let facts = [births[0].clone(), births[1].clone(), first.clone(), later];
            assert!(derive(&facts).is_err());
            let result = admit(&facts).expect("invalid equal-clock assignment is classified");
            assert_eq!(result.events[&id(3)], EventState::Rejected);
            assert_eq!(result.events[&id(9)], EventState::Accepted);
            let mut reversed = facts;
            reversed.reverse();
            assert_eq!(admit(&reversed).expect("classification is order independent"), result);
        }
    }
    let mut change = edit(3, 1, None, Some("later"), false, &[1]);
    change.clock = births[0].clock;
    assert!(derive(&[births[0].clone(), change.clone()]).is_err());
    assert_eq!(
        admit(&[births[0].clone(), change]).expect("equal-clock edit rejected").events[&id(3)],
        EventState::Rejected
    );
}

#[test]
fn one_thousand_sequential_publications_preserve_closed_context_within_work_budget() {
    // Match the file-write qualification's history shape: one birth followed
    // by 1,000 publications, each retaining the full exact observed context.
    let mut facts = vec![birth(1, None, "file", Kind::File, &[])];
    for number in 2..=1001 {
        facts.push(edit(number, 1, None, None, true, &(1..number).collect::<Vec<_>>()));
    }
    let admission = admit(&facts).expect("1,000 file publications fit the supported work budget");
    assert_eq!(admission.events.len(), 1001);
    assert!(admission.events.values().all(|state| *state == EventState::Accepted));
    assert!(admission.view.entries[&id(1)].visible);
    assert_eq!(
        derive(&facts).expect("strict validation shares the closure algorithm"),
        admission.view
    );
    // A claimed sequential extension that swaps one ancestor for an unrelated
    // event must not get the inductive fast path's proof of closure.
    let mut unrelated = birth(1002, None, "other", Kind::File, &[]);
    unrelated.clock = 1;
    facts.push(unrelated);
    let mut observed: Vec<_> = facts[1000].observed.iter().copied().skip(1).collect();
    observed.push(id(1002));
    facts[1000].observed = observed.into();
    let rejected = admit(&facts).expect("malformed context is classified");
    assert_eq!(rejected.events[&id(1001)], EventState::Rejected);
}

#[test]
fn sequential_assignments_validate_their_own_scope_after_concurrency_and_deletion() {
    // A later source can observe an older prefix. Its no-op and deletion checks
    // must use that prefix even after intervening accepted sequential edits.
    let facts = [
        birth(1, None, "a", Kind::File, &[]),
        edit(2, 1, None, Some("b"), false, &[1]),
        edit(3, 1, None, Some("c"), false, &[1, 2]),
        edit(4, 1, None, Some("b"), false, &[1, 2]),
        edit(5, 1, None, Some("d"), false, &[1, 2, 3]),
        delete(6, 1, &[1, 2, 3, 5]),
        edit(7, 1, None, Some("e"), false, &[1, 2, 3, 5, 6]),
        edit(8, 1, None, Some("f"), false, &[1, 2, 3, 5]),
    ];
    let expected = admit(&facts).expect("each exact causal cut is classified");
    for number in [1, 2, 3, 5, 6, 8] {
        assert_eq!(expected.events[&id(number)], EventState::Accepted);
    }
    for number in [4, 7] {
        assert_eq!(expected.events[&id(number)], EventState::Rejected);
    }
    assert_eq!(expected.view.entries[&id(1)].requested_name, "f");
    assert!(!expected.view.entries[&id(1)].visible);
    let mut reversed = facts;
    reversed.reverse();
    assert_eq!(admit(&reversed).expect("arrival order does not change scopes"), expected);
}

#[test]
fn competing_writer_intent_checks_open_time_absence_and_publication_time_deletion() {
    let parent = birth(1, None, "folder", Kind::Directory, &[]);
    let first = birth(2, Some(1), "file", Kind::File, &[1]);
    let mut second = birth(3, Some(1), "file", Kind::File, &[1, 2]);
    let Change::Birth { creation_context, .. } = &mut second.change else { unreachable!() };
    *creation_context = Some(FactCreationContext { observed: vec![id(1)].into() });
    let admitted = admit(&[parent.clone(), first.clone(), second.clone()])
        .expect("bounded namespace admission classifies the supplied facts");
    assert!(admitted.events.values().all(|state| *state == EventState::Accepted));
    assert_eq!(admitted.view.entries.values().filter(|entry| entry.visible).count(), 3);
    let Change::Birth { creation_context, .. } = &mut second.change else { unreachable!() };
    *creation_context = Some(FactCreationContext { observed: vec![id(2)].into() });
    assert_eq!(
        admit(&[parent.clone(), first.clone(), second.clone()])
            .expect("bounded namespace admission classifies the supplied facts")
            .events[&second.id],
        EventState::Rejected
    );
    let Change::Birth { creation_context, .. } = &mut second.change else { unreachable!() };
    *creation_context = Some(FactCreationContext { observed: vec![id(1), id(2)].into() });
    assert_eq!(
        admit(&[parent.clone(), first.clone(), second.clone()])
            .expect("bounded namespace admission classifies the supplied facts")
            .events[&second.id],
        EventState::Rejected
    );
    let deletion = delete(4, 1, &[1, 2]);
    second.clock = 5;
    second.observed = second.observed.iter().copied().chain([id(4)]).collect::<Vec<_>>().into();
    let Change::Birth { creation_context, .. } = &mut second.change else { unreachable!() };
    *creation_context = Some(FactCreationContext { observed: vec![id(1)].into() });
    assert_eq!(
        admit(&[parent, first, deletion, second.clone()])
            .expect("bounded namespace admission classifies the supplied facts")
            .events[&second.id],
        EventState::Rejected
    );
}

#[test]
fn sequential_writer_creation_contexts_fit_the_namespace_work_budget() {
    let mut facts = Vec::new();
    let mut observed = Vec::new();
    for number in 1..=300 {
        let mut fact = birth(number, None, &format!("file-{number}"), Kind::File, &observed);
        let Change::Birth { creation_context, .. } = &mut fact.change else { unreachable!() };
        *creation_context = Some(FactCreationContext { observed: fact.observed.clone() });
        facts.push(fact);
        observed.push(number);
    }
    let admission =
        admit(&facts).expect("bounded namespace admission classifies the supplied facts");
    assert_eq!(admission.view.entries.len(), 300);
    assert!(admission.events.values().all(|state| *state == EventState::Accepted));
}

#[test]
fn combined_intent_normalizes_only_against_its_closed_observed_scope() {
    // A redundant parent must not reject a rename or create preservation of
    // its own. An entirely unchanged intent retains causality but no edit effect.
    let facts = vec![
        birth(1, None, "D", Kind::Directory, &[]),
        birth(2, Some(1), "S", Kind::Directory, &[1]),
        birth(3, Some(2), "old", Kind::File, &[1, 2]),
        edit(4, 2, Some(Some(1)), Some("S"), false, &[1, 2, 3]),
        delete(5, 1, &[1, 2, 3]),
    ];
    let admitted = admit(&facts).expect("neutral combined intent remains causally admitted");
    assert_eq!(admitted.events[&id(4)], EventState::Accepted);
    assert_eq!(admitted.view, converges(&facts));
    assert!(admitted.view.entries.values().all(|entry| !entry.visible));
    assert_eq!(admitted.view.entries[&id(2)].name_event, id(2));
    assert_eq!(admitted.view.entries[&id(2)].parent_event, id(2));

    let mut renamed = facts.clone();
    renamed[3] = edit(4, 2, Some(Some(1)), Some("R"), false, &[1, 2, 3]);
    let view = converges(&renamed);
    assert!(view.entries[&id(3)].visible);
    assert_eq!(view.entries[&id(2)].name, "R");
    assert_eq!(view.entries[&id(2)].name_event, id(4));
    assert_eq!(view.entries[&id(2)].parent_event, id(2));

    let mut observed_deletion = facts.clone();
    observed_deletion[3] = edit(6, 2, Some(Some(1)), Some("S"), false, &[1, 2, 3, 5]);
    assert_eq!(
        admit(&observed_deletion).expect("deleted target is classified").events[&id(6)],
        EventState::Rejected
    );
    let mut with_content = facts.clone();
    with_content[3] = edit(4, 3, Some(Some(2)), Some("old"), true, &[1, 2, 3]);
    assert!(converges(&with_content).entries[&id(3)].visible);

    let mut later = facts;
    later.push(birth(6, None, "later", Kind::File, &[1, 2, 3, 4, 5]));
    assert!(converges(&later).entries[&id(6)].visible);
}

#[test]
fn concurrent_rename_and_combined_move_keep_independent_assignment_evidence() {
    let facts = [
        birth(1, None, "D", Kind::Directory, &[]),
        birth(2, Some(1), "photos", Kind::File, &[1]),
        edit(20, 2, None, Some("pictures"), false, &[1, 2]),
        edit(30, 2, Some(None), Some("photos"), false, &[1, 2]),
    ];
    let admitted = admit(&facts).expect("concurrent rename and move are admitted");
    assert_eq!(admitted.view, converges(&facts));
    let entry = &admitted.view.entries[&id(2)];
    assert_eq!(entry.name, "pictures");
    assert_eq!(entry.parent, None);
    assert_eq!(entry.name_event, id(20));
    assert_eq!(entry.parent_event, id(30));
    assert_eq!(admitted.assignments[&id(20)], Assignments { name: true, parent: false });
    assert_eq!(admitted.assignments[&id(30)], Assignments { name: false, parent: true });
    let (receipt, bytes) = receipt::Receipt::new(Some(id(2)), &admitted.assignments)
        .expect("normalized assignments encode");
    assert_eq!(
        receipt.verify(Some(id(2)), &bytes).expect("assignment evidence verifies"),
        admitted.assignments
    );
    let partial = admit(&facts[1..]).expect("selected sources remain pending without their parent");
    assert!(partial.assignments.is_empty());
}

#[test]
fn combined_intent_preserves_requests_across_coalesced_names_and_cycle_fallback() {
    // Raw fields on a selected alias can differ from effective fields. Resolve
    // redundant assignments only after the omitted causal sources are admitted.
    let facts = vec![
        birth(1, None, "folder", Kind::Directory, &[]),
        birth(2, None, "folder", Kind::Directory, &[]),
        edit(3, 2, None, Some("renamed"), false, &[1, 2]),
        edit(4, 1, Some(None), Some("folder"), false, &[1, 2, 3]),
    ];
    let view = converges(&facts);
    assert_eq!(view.entries[&id(2)].name, "folder");
    assert_eq!(view.entries[&id(2)].name_event, id(4));
    assert_eq!(view.entries[&id(2)].parent_event, id(2));
    let partial =
        admit(&[facts[0].clone(), facts[3].clone()]).expect("opaque sources stay pending");
    assert_eq!(partial.events[&id(4)], EventState::Pending);

    let mut cycle = vec![
        birth(1, None, "A", Kind::Directory, &[]),
        birth(2, None, "B", Kind::Directory, &[1]),
        edit(3, 1, Some(Some(2)), None, false, &[1, 2]),
        edit(4, 2, Some(Some(1)), None, false, &[1, 2]),
    ];
    let before = converges(&cycle);
    let root = before.entries.values().find(|entry| entry.parent.is_none()).expect("rooted cycle");
    let raw_parent_event = cycle.iter().find(|fact| {
        matches!(fact.change, Change::Edit { entry, parent: Some(Some(_)), .. } if entry == root.id)
    }).expect("root has a requested cyclic parent");
    assert_ne!(root.parent_event, raw_parent_event.id);
    let number = u16::from_be_bytes(root.id.as_bytes()[13..].try_into().expect("fixture ID"));
    cycle.push(edit(5, number, Some(None), Some("new"), false, &[1, 2, 3, 4]));
    let after = converges(&cycle);
    assert_eq!(after.entries[&root.id].parent_event, root.parent_event);
    assert_eq!(after.entries[&root.id].name, "new");
}

#[test]
fn content_admission_reuse_preserves_fresh_projection_and_limits() {
    let mut facts = vec![
        birth(1, None, "folder", Kind::Directory, &[]),
        birth(2, None, "folder", Kind::Directory, &[]),
        birth(3, Some(1), "note", Kind::File, &[1, 2]),
        birth(4, Some(2), "note", Kind::File, &[1, 2]),
        edit(5, 3, Some(None), Some("note"), false, &[1, 2, 3, 4]),
        edit(6, 3, None, None, true, &[99]),
        edit(7, 3, None, None, true, &[]),
    ];
    let (mut prior, mut budget) =
        admission::admit_budgeted(&facts).expect("bounded fixture admission");
    assert_eq!(prior.assignments[&id(5)], Assignments { parent: true, name: false });
    assert_eq!(prior.events[&id(6)], EventState::Pending);
    assert_eq!(prior.events[&id(7)], EventState::Rejected);
    for number in 8..40 {
        let mut next = edit(number, 3, None, None, true, &[]);
        next.observed = prior.assignments.keys().copied().collect::<Vec<_>>().into();
        budget = admission::content_append_budget(&prior, budget, &facts, &next)
            .expect("valid append preview")
            .expect("complete content context qualifies");
        let before = prior.view.clone();
        facts.push(next);
        let (fresh, measured) =
            admission::admit_budgeted(&facts).expect("bounded fixture admission");
        assert_eq!(before, fresh.view);
        prior = fresh;
        assert!(budget.work >= measured.work);
        assert_eq!(budget.projection, measured.projection);
        assert_eq!(budget.source_bytes, measured.source_bytes);
    }
    let mut next = edit(40, 3, None, None, true, &[]);
    next.observed = prior.assignments.keys().copied().collect::<Vec<_>>().into();
    let added = next.observed.len() + budget.projection + 2;
    for (extra, expected) in [(0, true), (1, false)] {
        let bounded = admission::Budget { work: MAX_WORK - added + extra, ..budget };
        let result = admission::content_append_budget(&prior, bounded, &facts, &next)
            .expect("bounded fixture admission");
        assert_eq!(result.is_some(), expected);
        if let Some(result) = result {
            assert_eq!(result.work, MAX_WORK);
        }
    }
    let bounded = admission::Budget {
        source_bytes: MAX_SOURCE_BYTES - next.validate_shape().expect("bounded fixture admission")
            + 1,
        ..budget
    };
    assert!(
        admission::content_append_budget(&prior, bounded, &facts, &next)
            .expect("bounded fixture admission")
            .is_none()
    );

    // A raw deletion takes the full path regardless of its classification.
    for observed in [vec![], vec![99], (1..=5).collect()] {
        let mut with_delete = facts.clone();
        with_delete.push(delete(41, 4, &observed));
        let (candidate, bounded) =
            admission::admit_budgeted(&with_delete).expect("bounded fixture admission");
        let mut after = edit(42, 3, None, None, true, &[]);
        after.observed = candidate.assignments.keys().copied().collect::<Vec<_>>().into();
        assert!(
            admission::content_append_budget(&candidate, bounded, &with_delete, &after)
                .expect("bounded fixture admission")
                .is_none()
        );
    }
    // An old missing dependency must be reclassified when its source arrives.
    let mut arrives = next.clone();
    arrives.id = id(99);
    arrives.clock = 100;
    assert!(
        admission::content_append_budget(&prior, budget, &facts, &arrives)
            .expect("bounded fixture admission")
            .is_none()
    );
    {
        let mut invalid = next.clone();
        // Each condition separately exercises a semantic guard.
        invalid.clock = 39;
        assert!(
            admission::content_append_budget(&prior, budget, &facts, &invalid)
                .expect("bounded fixture admission")
                .is_none()
        );
        invalid.clock = 40;
        invalid.observed = vec![id(1)].into();
        assert!(
            admission::content_append_budget(&prior, budget, &facts, &invalid)
                .expect("bounded fixture admission")
                .is_none()
        );
        invalid.observed = next.observed.clone();
        invalid.change = Change::Edit { entry: id(1), parent: None, name: None, content: true };
        assert!(
            admission::content_append_budget(&prior, budget, &facts, &invalid)
                .expect("bounded fixture admission")
                .is_none()
        );
    }
}
