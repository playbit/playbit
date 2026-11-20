#![cfg(test)]

use std::io::Cursor;

use crate::ChangeToListOfInteger;
use crate::ChangeToUTF8String;
use crate::Doc;
use crate::RANGE_DATA_LEN;
use crate::RNG;
use crate::RichTextDocument;
use crate::RichTextSpan;
use crate::UUID;
use crate::blob;
use crate::types::Animal;
use crate::types::Change;
use crate::types::ChangeToAnimal;
use crate::types::ChangeToShow;
use crate::types::ChangeToShowTable;
use crate::types::EditBox;
use crate::types::NotebookPage;
use crate::types::NotebookRoot;
use crate::types::Show;
use crate::types::ShowTable;

struct NativeRNG {}

impl RNG for NativeRNG {
    fn secure_random_bytes(&mut self, buf: *mut u8, len: usize) {
        unsafe {
            if libc::getentropy(buf as *mut _, len) != 0 {
                panic!("getentropy failed: {}", std::io::Error::last_os_error());
            }
        }
    }
}

#[test]
fn tests1() {
    let mut rng = NativeRNG {};

    let mut doc1 = Doc::new();
    doc1.mod_insert(b"obj1", 0, b"apple");
    doc1.mod_insert(b"obj1", 5, b" banana");
    doc1.mod_insert(b"obj2", 0, b"pear");
    doc1.mod_remove(b"obj1", 1, 5);

    let mut buffer = Vec::new();
    let _ = doc1.serialize_history(&mut buffer, 0, doc1.get_history_len(), &mut rng);

    let mut doc2 = Doc::new();
    doc2.mod_insert(b"obj1", 0, b"conflict");
    let mut cursor = Cursor::new(buffer);
    let _ = doc2.import(&mut cursor, &mut rng);

    let mut doc3 = Doc::new();
    let mut buffer = Vec::new();
    let _ = doc1.serialize_all(&mut buffer, &mut rng);
    let mut cursor = Cursor::new(buffer);
    let _ = doc3.import(&mut cursor, &mut rng);

    let mut doc4 = Doc::new();

    let mut buffer = Vec::new();
    let _ = doc2.serialize_history(&mut buffer, 0, doc2.get_history_len(), &mut rng);
    let mut cursor = Cursor::new(buffer);
    let _ = doc1.import(&mut cursor, &mut rng);

    let mut buffer = Vec::new();
    let _ = doc2.serialize_history(&mut buffer, 0, doc2.get_history_len(), &mut rng);
    let mut cursor = Cursor::new(buffer);
    let _ = doc1.import(&mut cursor, &mut rng);

    let mut buffer = Vec::new();
    let _ = doc2.serialize_history(&mut buffer, 0, doc2.get_history_len(), &mut rng);
    let mut cursor = Cursor::new(buffer);
    let _ = doc3.import(&mut cursor, &mut rng);

    let mut buffer = Vec::new();
    let _ = doc1.serialize_all(&mut buffer, &mut rng);
    let mut cursor = Cursor::new(buffer);
    let _ = doc4.import(&mut cursor, &mut rng);
    let mut buffer = Vec::new();
    let _ = doc2.serialize_history(&mut buffer, 0, doc2.get_history_len(), &mut rng);
    let mut cursor = Cursor::new(buffer);
    let _ = doc4.import(&mut cursor, &mut rng);

    print!(
        "{} {}\n{} {}\n{} {}\n{} {}\n",
        String::from_utf8(doc1.get_data(b"obj1").to_vec()).unwrap(),
        String::from_utf8(doc1.get_data(b"obj2").to_vec()).unwrap(),
        String::from_utf8(doc2.get_data(b"obj1").to_vec()).unwrap(),
        String::from_utf8(doc2.get_data(b"obj2").to_vec()).unwrap(),
        String::from_utf8(doc3.get_data(b"obj1").to_vec()).unwrap(),
        String::from_utf8(doc3.get_data(b"obj2").to_vec()).unwrap(),
        String::from_utf8(doc4.get_data(b"obj1").to_vec()).unwrap(),
        String::from_utf8(doc4.get_data(b"obj2").to_vec()).unwrap(),
    );
}

#[test]
fn tests2() {
    let mut rng = NativeRNG {};

    let mut d = Doc::new();
    let animal = Animal::root();
    animal.set_age(&mut d, 10);
    animal.set_weight(&mut d, 200);
    animal.name().set(&mut d, b"Mittens");
    animal.discussion().insert(&mut d, 0, "Likes milk");
    d.commit_transaction(&mut rng); // commit transaction
    println!(
        "{} {} {} {}",
        animal.get_age(&d),
        animal.get_weight(&d),
        str::from_utf8(animal.name().get(&d)).unwrap(),
        animal.discussion().get(&d)
    );
    animal.discussion().remove(&mut d, 1, 2);
    animal.discussion().insert(&mut d, 0, "He is a very good cat");
    animal.name().set(&mut d, b"Adam");
    animal.set_age(&mut d, 500);
    d.commit_transaction(&mut rng); // commit transaction
    animal.discussion().insert(&mut d, 0, "meow ");
    println!(
        "{} {} {} {}",
        animal.get_age(&d),
        animal.get_weight(&d),
        str::from_utf8(animal.name().get(&d)).unwrap(),
        animal.discussion().get(&d)
    );
    d.mod_undo(1, &mut rng);
    println!(
        "{} {} {} {}",
        animal.get_age(&d),
        animal.get_weight(&d),
        str::from_utf8(animal.name().get(&d)).unwrap(),
        animal.discussion().get(&d)
    );
    animal.discussion().remove(&mut d, 0, 5);
    d.commit_transaction(&mut rng); // commit transaction
    d.mod_undo(4, &mut rng);
    println!(
        "{} {} {} {}",
        animal.get_age(&d),
        animal.get_weight(&d),
        str::from_utf8(animal.name().get(&d)).unwrap(),
        animal.discussion().get(&d)
    );
    d.mod_undo(3, &mut rng);
    println!(
        "{} {} {} {}",
        animal.get_age(&d),
        animal.get_weight(&d),
        str::from_utf8(animal.name().get(&d)).unwrap(),
        animal.discussion().get(&d)
    );

    for i in 0..d.get_history_len() {
        println!("======");
        for j in d.read_mod_group(i, &mut rng) {
            println!("(local = {})", j.local);
            let c = Change::decode(&j);
            println!("{:?}", c);
        }
    }

    println!("======");
}

#[test]
fn tests3() {
    let mut rng = NativeRNG {};

    let mut d = Doc::new();
    let root = NotebookRoot::root();
    let id = root.pages().append(&mut d, UUID::generate(&mut rng));
    root.pages().append(&mut d, UUID::generate(&mut rng));
    root.pages().remove(&mut d, 1, 1);
    let page = NotebookPage::from_link(id);
    page.name().set(&mut d, "January 2026");

    for i in 0..d.get_history_len() {
        println!("======");
        for j in d.read_mod_group(i, &mut rng) {
            println!("(local = {})", j.local);
            let c = Change::decode(&j);
            println!("{:?}", c);
        }
    }

    println!("======");
}

#[test]
fn tests5() {
    let mut rng = NativeRNG {};

    for test_index in 0..3 {
        blob::reset();

        let mut d1 = Doc::new();
        let mut d2 = Doc::new();
        let mut d3 = Doc::new();

        let b = Animal::root().discussion();

        b.insert(&mut d1, 0, "abcde");

        for _ in 0..5 {
            d1.sync(&mut rng);
            d2.sync(&mut rng);
            d3.sync(&mut rng);
        }

        b.insert(&mut d1, 0, "x");
        b.insert(&mut d2, 1, "y");
        b.insert(&mut d3, 2, "z");

        for _ in 0..5 {
            d1.sync(&mut rng);
            d2.sync(&mut rng);
        }

        b.insert(&mut d1, 2, "s");
        b.insert(&mut d2, 3, "t");

        for _ in 0..5 {
            if test_index != 0 {
                d1.sync(&mut rng);
            }
            if test_index != 1 {
                d2.sync(&mut rng);
            }
            if test_index != 2 {
                d3.sync(&mut rng);
            }
        }

        for _ in 0..5 {
            d1.sync(&mut rng);
            d2.sync(&mut rng);
            d3.sync(&mut rng);
        }

        assert_eq!(b.get(&d1), b.get(&d2));
        assert_eq!(b.get(&d1), b.get(&d3));
    }
}

#[test]
fn tests7() {
    let mut rng = NativeRNG {};

    let mut d = Doc::new();
    let animal = Animal::root();
    let list = animal.favorite_numbers();
    list.append(&mut d, 10000);
    list.append(&mut d, 20000);
    list.append(&mut d, 30000);
    list.append(&mut d, 40000);
    list.append(&mut d, 50000);
    list.remove(&mut d, 2, 1);
    assert_eq!(list.len(&d), 4);
    assert_eq!(list.get(&d, 0), 10000);
    assert_eq!(list.get(&d, 1), 20000);
    assert_eq!(list.get(&d, 2), 40000);
    assert_eq!(list.get(&d, 3), 50000);

    let mut index = 0;
    for i in 0..d.get_history_len() {
        for j in d.read_mod_group(i, &mut rng) {
            let c = Change::decode(&j);
            let Change::ToRootAnimal(c) = c else {
                panic!();
            };
            let ChangeToAnimal::Update_favorite_numbers(c) = c else {
                panic!();
            };
            if index == 0 {
                let ChangeToListOfInteger::Insert(a, b) = c else {
                    panic!();
                };
                assert_eq!(a, 0);
                assert_eq!(b, 10000);
            } else if index == 1 {
                let ChangeToListOfInteger::Insert(a, b) = c else {
                    panic!();
                };
                assert_eq!(a, 1);
                assert_eq!(b, 20000);
            } else if index == 2 {
                let ChangeToListOfInteger::Insert(a, b) = c else {
                    panic!();
                };
                assert_eq!(a, 2);
                assert_eq!(b, 30000);
            } else if index == 3 {
                let ChangeToListOfInteger::Insert(a, b) = c else {
                    panic!();
                };
                assert_eq!(a, 3);
                assert_eq!(b, 40000);
            } else if index == 4 {
                let ChangeToListOfInteger::Insert(a, b) = c else {
                    panic!();
                };
                assert_eq!(a, 4);
                assert_eq!(b, 50000);
            } else if index == 5 {
                let ChangeToListOfInteger::Remove(a, b) = c else {
                    panic!();
                };
                assert_eq!(a, 2);
                assert_eq!(b, 1);
            }
            index += 1;
        }
    }

    assert_eq!(index, 6);
}

#[test]
fn tests8() {
    {
        let mut d = Doc::new();
        let edit_box = EditBox::root();
        let string = edit_box.text();
        string.set(&mut d, "hello");
        assert_eq!(string.get(&d), "hello");
        string.set(&mut d, "world");
        assert_eq!(string.get(&d), "world");
        string.set(&mut d, "");
        assert_eq!(string.get(&d), "");
    }
    {
        let mut d = Doc::new();
        let edit_box = EditBox::root();
        let string = edit_box.text();
        string.insert(&mut d, 0, "test");
        string.insert(&mut d, 2, "hello");
        string.remove(&mut d, 3, 2);
        assert_eq!(string.get(&d), "tehlost");
    }
}

#[test]
fn tests9() {
    let mut rng = NativeRNG {};

    let mut d = Doc::new();
    let table = ShowTable::root();
    assert_eq!(table.len(&d), 0);
    let x0 = table.add(&mut d, &mut rng);
    let s0 = Show::from_row(x0);
    s0.name().set(&mut d, "Romeo and Juliet");
    let x1 = table.add(&mut d, &mut rng);
    let s1 = Show::from_row(x1);
    s1.name().set(&mut d, "Macbeth");
    let x2 = table.add(&mut d, &mut rng);
    let s2 = Show::from_row(x2);
    s2.name().set(&mut d, "Hamlet");
    assert_eq!(table.len(&d), 3);
    table.remove(&mut d, x1);
    assert_eq!(table.len(&d), 2);
    assert!(table.get(&d, 0) == x0 || table.get(&d, 0) == x2);
    assert!(table.get(&d, 1) == x0 || table.get(&d, 1) == x2);
    assert!(table.get(&d, 0) != table.get(&d, 1));

    for i in 0..d.get_history_len() {
        println!("======");
        for j in d.read_mod_group(i, &mut rng) {
            println!("(local = {})", j.local);
            let c = Change::decode(&j);
            println!("{:?}", c);
        }
    }

    println!("======");

    let mut index = 0;
    for i in 0..d.get_history_len() {
        for j in d.read_mod_group(i, &mut rng) {
            let c = Change::decode(&j);
            if index == 0 {
                let Change::ToTableShow(c) = c else {
                    panic!();
                };
                let ChangeToShowTable::Add(u) = c else {
                    panic!();
                };
                assert_eq!(u, x0);
            } else if index == 1 {
                let Change::ToRowShow(x, c) = c else {
                    panic!();
                };
                let ChangeToShow::Update_name(c) = c else {
                    panic!();
                };
                let ChangeToUTF8String::ReplaceAll(u) = c else {
                    panic!();
                };
                assert_eq!(u, "Romeo and Juliet");
                assert_eq!(x, x0);
            } else if index == 2 {
                let Change::ToTableShow(c) = c else {
                    panic!();
                };
                let ChangeToShowTable::Add(u) = c else {
                    panic!();
                };
                assert_eq!(u, x1);
            } else if index == 3 {
                let Change::ToRowShow(x, c) = c else {
                    panic!();
                };
                let ChangeToShow::Update_name(c) = c else {
                    panic!();
                };
                let ChangeToUTF8String::ReplaceAll(u) = c else {
                    panic!();
                };
                assert_eq!(u, "Macbeth");
                assert_eq!(x, x1);
            } else if index == 4 {
                let Change::ToTableShow(c) = c else {
                    panic!();
                };
                let ChangeToShowTable::Add(u) = c else {
                    panic!();
                };
                assert_eq!(u, x2);
            } else if index == 5 {
                let Change::ToRowShow(x, c) = c else {
                    panic!();
                };
                let ChangeToShow::Update_name(c) = c else {
                    panic!();
                };
                let ChangeToUTF8String::ReplaceAll(u) = c else {
                    panic!();
                };
                assert_eq!(u, "Hamlet");
                assert_eq!(x, x2);
            } else if index == 6 {
                let Change::ToTableShow(c) = c else {
                    panic!();
                };
                let ChangeToShowTable::Remove(u) = c else {
                    panic!();
                };
                assert_eq!(u, 1);
            }
            index += 1;
        }
    }

    assert_eq!(index, 7);
}

#[test]
fn tests10() {
    let mut rng = NativeRNG {};

    const KEY_FONT_WEIGHT: u16 = 1;

    let mut doc1 = Doc::new();
    let mut doc2 = Doc::new();
    let mut doc3 = Doc::new();

    doc1.mod_insert(b"obj1", 0, b"apple");
    let mut range_data = [0u8; 16];
    doc1.mod_range(b"obj1", 0, 5, KEY_FONT_WEIGHT, range_data);
    doc1.commit_transaction(&mut rng); // commit transaction
    range_data[0..2].copy_from_slice(&400_u16.to_le_bytes());
    doc1.mod_range(b"obj1", 1, 3, KEY_FONT_WEIGHT, range_data);
    doc1.commit_transaction(&mut rng); // commit transaction
    range_data[0..2].copy_from_slice(&700_u16.to_le_bytes());
    doc1.mod_range(b"obj1", 0, 2, KEY_FONT_WEIGHT, range_data);
    doc1.commit_transaction(&mut rng); // commit transaction
    doc1.mod_undo(1, &mut rng);

    let mut buffer = Vec::new();
    let _ = doc1.serialize_history(&mut buffer, 0, doc1.get_history_len(), &mut rng);
    let mut cursor = Cursor::new(buffer);
    let _ = doc2.import(&mut cursor, &mut rng);

    let mut buffer = Vec::new();
    let _ = doc1.serialize_all(&mut buffer, &mut rng);
    let mut cursor = Cursor::new(buffer);
    let _ = doc3.import(&mut cursor, &mut rng);

    for i in 0..2 {
        let ranges = if i == 0 { &doc2 } else { &doc3 }.get_ranges(b"obj1");
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0].offset, 4);
        assert_eq!(ranges[0].count, 1);
        assert_eq!(ranges[0].key, 1);
        assert_eq!(ranges[0].data[0], 0);
        assert_eq!(ranges[1].offset, 0);
        assert_eq!(ranges[1].count, 1);
        assert_eq!(ranges[1].key, 1);
        assert_eq!(ranges[1].data[0], 188);
        assert_eq!(ranges[2].offset, 1);
        assert_eq!(ranges[2].count, 3);
        assert_eq!(ranges[2].key, 1);
        assert_eq!(ranges[2].data[0], 0);
    }

    let mut doc4 = Doc::new();
    doc4.mod_insert(b"obj1", 0, b"apple");
    doc4.commit_transaction(&mut rng);
    range_data[0..2].copy_from_slice(&700_u16.to_le_bytes());
    doc4.mod_range(b"obj1", 1, 3, KEY_FONT_WEIGHT, range_data);
    doc4.commit_transaction(&mut rng);
    doc4.mod_undo(1, &mut rng);
    let ranges = doc4.get_ranges(b"obj1");
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].offset, 1);
    assert_eq!(ranges[0].count, 3);
    assert_eq!(ranges[0].key, 1);
    assert_eq!(ranges[0].data[0], 0);
}

#[test]
fn tests11() {
    let mut rng = NativeRNG {};

    let mut doc = Doc::new();
    let edit_box = EditBox::root();
    let text = edit_box.rich_text();
    let block_title = UUID::generate(&mut rng);
    let block_paragraph = UUID::generate(&mut rng);
    let block_list_item = UUID::generate(&mut rng);
    text.insert_first_line(&mut doc, block_title);
    let offset = text.get(&doc, 0).storage_start;
    text.insert_text(&mut doc, offset, "hello", &[]);
    let offset = text.get(&doc, 0).storage_start + 5;
    text.insert_text(&mut doc, offset, " world\nthis the first paragraph", &[block_paragraph]);
    let span = RichTextSpan {
        start: text.get(&doc, 1).storage_start + 5,
        end: text.get(&doc, 1).storage_start + 8,
    };
    text.merge_range(&mut doc, span, 100, [1; RANGE_DATA_LEN]);
    let offset = text.get(&doc, 1).storage_start + 5;
    text.insert_text(&mut doc, offset, "is ", &[]);
    print_rich_text_document(&doc, text, block_title, block_paragraph, block_list_item);

    for i in 0..doc.get_history_len() {
        println!("======");
        for j in doc.read_mod_group(i, &mut rng) {
            println!("(local = {})", j.local);
            let c = Change::decode(&j);
            println!("{:?}", c);
        }
    }
}

fn print_rich_text_document(
    doc: &Doc,
    text: RichTextDocument,
    block_title: UUID,
    block_paragraph: UUID,
    block_list_item: UUID,
) {
    println!("----");
    for i in 0..text.len(doc) {
        let block = text.get(doc, i);
        if block.properties_link == block_title {
            println!("[title]");
        } else if block.properties_link == block_paragraph {
            println!("[paragraph]");
        } else if block.properties_link == block_list_item {
            println!("[list item]");
        }
        println!("{}", block.text);
        for j in 0..block.text.len() {
            let mut bold = false;
            for k in &block.ranges {
                if j >= (k.offset as usize) && j < ((k.offset + k.count) as usize) && k.key == 100 {
                    bold = k.data[0] == 1;
                }
            }
            if bold {
                print!("*");
            } else {
                print!(" ");
            }
        }
        println!();
    }
}

mod rebase_tests {
    use crate::{ModGroup, ModKind, Modification, UUID, rebase};

    fn modification(kind: ModKind, offset: u32, count: u32) -> Modification {
        Modification { key_len: 1, offset, count, range_key: 1, kind }
    }

    fn group(id: UUID, entries: &[(u8, Modification)], has_bytes: bool) -> ModGroup {
        let mut group = ModGroup::new_transaction(0);
        group.abstract_id = id;
        group.has_bytes = has_bytes;
        for &(key, modification) in entries {
            group.data.push(key);
            if has_bytes {
                group.data.resize(group.data.len() + modification.has_bytes(), 7);
            }
            group.modifications.push(modification);
        }
        group
    }

    fn apply(bytes: &mut Vec<u8>, m: Modification, value: u8) {
        let offset = m.offset as usize;
        match m.kind {
            ModKind::Insert => {
                bytes.splice(offset..offset, vec![value; m.count as usize]);
            }
            ModKind::Remove => {
                bytes.drain(offset..offset + m.count as usize);
            }
            ModKind::Nop => {}
            _ => panic!("expected a sequence edit"),
        }
    }

    #[test]
    fn sequence_edits_converge_for_both_id_orders_and_payload_modes() {
        for x_kind in [ModKind::Insert, ModKind::Remove] {
            for y_kind in [ModKind::Insert, ModKind::Remove] {
                for x_offset in 0..7 {
                    for y_offset in 0..7 {
                        for x_count in 1..4 {
                            for y_count in 1..4 {
                                let x = modification(x_kind, x_offset, x_count);
                                let y = modification(y_kind, y_offset, y_count);
                                for id in [UUID { p0: 0, p1: 2 }, UUID { p0: 2, p1: 0 }] {
                                    for x_bytes in [false, true] {
                                        for y_bytes in [false, true] {
                                            let mut xg = group(id, &[(b'a', x)], x_bytes);
                                            let mut yg =
                                                group(UUID { p0: 1, p1: 0 }, &[(b'a', y)], y_bytes);
                                            rebase(&mut xg, &mut yg);
                                            let mut xy = b"0123456789".to_vec();
                                            let mut yx = xy.clone();
                                            apply(&mut xy, x, b'x');
                                            apply(&mut xy, yg.modifications[0], b'y');
                                            apply(&mut yx, y, b'y');
                                            apply(&mut yx, xg.modifications[0], b'x');
                                            assert_eq!(xy, yx);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn overlapping_ranges_and_scalar_writes_keep_their_conflict_rules() {
        use ModKind::{Insert, Nop, Range, Remove, ReplaceAll, Set};
        let cases = [
            ((Range, 2, 4), (Range, 3, 2), (Range, 2, 4), (Nop, 3, 2)),
            ((Range, 3, 2), (Range, 2, 4), (Nop, 3, 2), (Range, 2, 4)),
            ((Range, 2, 4), (Range, 4, 4), (Range, 2, 2), (Range, 4, 4)),
            ((Range, 4, 4), (Range, 2, 4), (Range, 4, 4), (Range, 2, 2)),
            ((Range, 2, 4), (Range, 2, 4), (Nop, 2, 4), (Range, 2, 4)),
            ((Range, 2, 4), (Range, 6, 2), (Range, 2, 4), (Range, 6, 2)),
            ((Range, 2, 4), (Insert, 4, 2), (Range, 2, 6), (Insert, 4, 2)),
            ((Range, 2, 4), (Remove, 3, 2), (Range, 2, 2), (Remove, 3, 2)),
            ((Range, 2, 4), (Remove, 1, 6), (Nop, 2, 4), (Remove, 1, 6)),
            ((Set, 2, 4), (Set, 2, 4), (Nop, 2, 4), (Set, 2, 4)),
            ((ReplaceAll, 0, 4), (ReplaceAll, 0, 2), (Nop, 0, 4), (ReplaceAll, 0, 2)),
        ];
        for (x, y, expected_x, expected_y) in cases {
            for reverse in [false, true] {
                let mut xg =
                    group(UUID { p0: 1, p1: 1 }, &[(b'a', modification(x.0, x.1, x.2))], true);
                let mut yg =
                    group(UUID { p0: 1, p1: 2 }, &[(b'a', modification(y.0, y.1, y.2))], true);
                if reverse {
                    rebase(&mut yg, &mut xg);
                } else {
                    rebase(&mut xg, &mut yg);
                }
                for (group, expected) in [(xg, expected_x), (yg, expected_y)] {
                    let actual = group.modifications[0];
                    assert_eq!(
                        (actual.kind as u16, actual.offset, actual.count),
                        (expected.0 as u16, expected.1, expected.2)
                    );
                }
            }
        }
    }

    #[test]
    fn packed_keys_skip_payloads_and_only_rebase_matching_objects() {
        use ModKind::{Insert, ReplaceAll, Set};
        for x_bytes in [false, true] {
            for y_bytes in [false, true] {
                let x_id = UUID { p0: 1, p1: 0 };
                let y_id = UUID { p0: 2, p1: 0 };
                let mut xg = group(
                    x_id,
                    &[(b'a', modification(Insert, 0, 2)), (b'b', modification(Set, 0, 3))],
                    x_bytes,
                );
                let mut yg = group(
                    y_id,
                    &[(b'c', modification(ReplaceAll, 0, 5)), (b'a', modification(Insert, 0, 4))],
                    y_bytes,
                );
                let x_data = xg.data.clone();
                let y_data = yg.data.clone();
                rebase(&mut xg, &mut yg);
                assert_eq!(xg.modifications[0].offset, 4);
                assert_eq!(xg.modifications[1].kind as u16, Set as u16);
                assert_eq!(yg.modifications[0].kind as u16, ReplaceAll as u16);
                assert_eq!(yg.modifications[1].offset, 0);
                assert_eq!(xg.data, x_data);
                assert_eq!(yg.data, y_data);
                assert_eq!(xg.z_base, UUID::zero().mix(y_id));
                assert_eq!(yg.z_base, UUID::zero().mix(x_id));
            }
        }
    }
}
