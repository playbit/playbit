//! Fuzz the sync wire input: arbitrary bytes and corrupted legitimate
//! streams must never panic either side of a session — every malformed
//! input has to surface as an `Err`, never as a crash. (The C
//! implementation this replaces had a deliberate crash on malformed
//! input; this target guards the opposite guarantee.)
#![no_main]

mod support;

use kvdb::Lww;
use kvdb::sync::{SyncReceiver, SyncSender, SyncStatus};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    match mode % 3 {
        0 => raw_to_sender(rest),
        1 => raw_to_receiver(rest),
        _ => corrupt_stream(rest),
    }
});

/// Arbitrary bytes into a serving sender, delivered in fuzz-chosen chunks.
fn raw_to_sender(data: &[u8]) {
    let Some((&chunk, data)) = data.split_first() else {
        return;
    };
    let db = support::small_db(7);
    let mut tx = SyncSender::new(&db).unwrap();
    let mut out = Vec::new();
    for chunk in data.chunks(chunk.max(1) as usize) {
        match support::drain(&mut tx, chunk, &mut out) {
            Ok(SyncStatus::Done) | Err(_) => return,
            Ok(SyncStatus::Running) => out.clear(),
        }
    }
}

/// Arbitrary bytes into a pulling receiver.
fn raw_to_receiver(data: &[u8]) {
    let Some((&chunk, data)) = data.split_first() else {
        return;
    };
    let db = support::small_db(9);
    let mut out = Vec::new();
    let mut rx =
        SyncReceiver::new(&db, Lww, support::Staging::default(), support::WALL, &mut out).unwrap();
    for chunk in data.chunks(chunk.max(1) as usize) {
        out.clear();
        match rx.process(chunk, &mut out) {
            Ok(SyncStatus::Done) | Err(_) => break,
            Ok(SyncStatus::Running) => {}
        }
    }
    let _ = rx.finish(); // must not panic in any state
    db.verify().unwrap();
}

/// Run a legitimate session but XOR-corrupt the sender→receiver bytes at
/// fuzz-chosen positions, exercising deep protocol states with
/// almost-valid input.
fn corrupt_stream(data: &[u8]) {
    let a = support::small_db(3);
    let b = support::small_db(11);
    let mut corruptions = data.as_chunks::<3>().0.iter();

    let mut r_out = Vec::new();
    let Ok(mut rx) =
        SyncReceiver::new(&a, Lww, support::Staging::default(), support::WALL, &mut r_out)
    else {
        return;
    };
    let Ok(mut tx) = SyncSender::new(&b) else {
        return;
    };
    let mut s_out = Vec::new();
    for _ in 0..10_000 {
        if !r_out.is_empty() {
            let data = std::mem::take(&mut r_out);
            match support::drain(&mut tx, &data, &mut s_out) {
                Ok(SyncStatus::Done) | Err(_) => break,
                Ok(SyncStatus::Running) => {}
            }
        } else if !s_out.is_empty() {
            let mut data = std::mem::take(&mut s_out);
            if let Some(c) = corruptions.next()
                && !data.is_empty()
            {
                let pos = u16::from_le_bytes([c[0], c[1]]) as usize % data.len();
                data[pos] ^= c[2];
            }
            match rx.process(&data, &mut r_out) {
                Ok(SyncStatus::Done) | Err(_) => break,
                Ok(SyncStatus::Running) => {}
            }
        } else {
            break;
        }
    }
    let _ = rx.finish();
    a.verify().unwrap();
    b.verify().unwrap();
}
