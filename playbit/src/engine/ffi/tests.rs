//! Structural rejection, durable ownership and sticky feed status at the C boundary.
#![allow(clippy::unwrap_used, reason = "Tests assert fixture outcomes.")]
use super::*;
use std::sync::atomic::AtomicUsize;

fn prefix<T: playbit_api::abi::record::Record>(
    size: u32,
    alignment: usize,
    fields: &[(&str, &str, u32, u32)],
) {
    assert!(std::mem::size_of::<T>() >= size as usize);
    assert_eq!(std::mem::align_of::<T>(), alignment);
    assert_eq!(T::MIN_SIZE, size);
    assert!(T::FIELDS.len() >= fields.len() + 2);
    assert_eq!(
        (T::FIELDS[0].name, T::FIELDS[0].kind, T::FIELDS[0].offset, T::FIELDS[0].width),
        ("size", "u32", 0, 4)
    );
    assert_eq!(
        (T::FIELDS[1].name, T::FIELDS[1].kind, T::FIELDS[1].offset, T::FIELDS[1].width),
        ("written", "u32", 4, 4)
    );
    for (actual, expected) in T::FIELDS[2..].iter().zip(fields) {
        assert_eq!(actual.name, expected.0);
        assert_eq!(
            actual.kind.split_whitespace().collect::<String>(),
            expected.1.split_whitespace().collect::<String>()
        );
        assert_eq!((actual.offset, actual.width), (expected.2, expected.3));
    }
    for added in &T::FIELDS[fields.len() + 2..] {
        assert!(added.offset >= size);
    }
}

#[test]
fn published_engine_record_prefixes_preserve_names_types_and_complete_allocations() {
    prefix::<Volume>(
        304,
        4,
        &[
            ("id", "[u8;16]", 8, 16),
            ("owner", "[u8;16]", 24, 16),
            ("name", "[u8;256]", 40, 256),
            ("name_length", "u32", 296, 4),
            ("share_key_with_service", "u32", 300, 4),
        ],
    );
    prefix::<Change>(
        56,
        8,
        &[
            ("volume", "[u8;16]", 8, 16),
            ("sequence", "u64", 24, 8),
            ("kind", "u32", 32, 4),
            ("database", "[u8;16]", 36, 16),
            ("reserved", "u32", 52, 4),
        ],
    );
    prefix::<Changes>(
        56,
        8,
        &[
            ("epoch", "[u8;16]", 8, 16),
            ("gap_generation", "u64", 24, 8),
            ("account_generation", "u64", 32, 8),
            ("closed", "u32", 40, 4),
            ("records", "u32", 44, 4),
            ("remaining", "u32", 48, 4),
            ("reserved", "u32", 52, 4),
        ],
    );
    #[cfg(feature = "native-runtime")]
    {
        let pointer = if cfg!(target_pointer_width = "64") { 8 } else { 4 };
        prefix::<AppOutput>(
            8 + 3 * pointer,
            pointer as usize,
            &[
                (
                    "view",
                    "Option<unsafe extern \"C\" fn(*mut c_void,u64,*const u8,u32)>",
                    8,
                    pointer,
                ),
                (
                    "log",
                    "Option<unsafe extern \"C\" fn(*mut c_void,u64,*const u8,u32)>",
                    8 + pointer,
                    pointer,
                ),
                ("user_data", "*mut c_void", 8 + 2 * pointer, pointer),
            ],
        );
        prefix::<AppStep>(
            24,
            8,
            &[("wake_after_ns", "u64", 8, 8), ("flags", "u32", 16, 4), ("reserved", "u32", 20, 4)],
        );
    }
}
unsafe extern "C" fn wake(data: *mut c_void) {
    // SAFETY: each test retains its counter through engine close.
    unsafe { &*data.cast::<AtomicUsize>() }.fetch_add(1, Ordering::Relaxed);
}
unsafe extern "C" fn discard(_: *mut c_void, _: *const Change, _: *const u8, _: u32) {}
fn open(path: &std::path::Path, counter: &AtomicUsize) -> *mut Engine {
    let path = path.to_str().unwrap();
    let mut engine = std::ptr::null_mut();
    // SAFETY: fixture owns path, callback and counter through close.
    assert_eq!(
        unsafe {
            pb_engine_open(
                path.as_ptr(),
                path.len() as u32,
                Some(wake),
                (counter as *const AtomicUsize).cast_mut().cast(),
                &mut engine,
            )
        },
        0
    );
    engine
}
#[test]
fn invalid_open_admits_no_path_or_output_effects() {
    let mut output = std::ptr::dangling_mut::<Engine>();
    // SAFETY: malformed structural spans must be rejected without dereference.
    unsafe {
        for (path, length) in [(std::ptr::null(), 0), (usize::MAX as *const u8, 2)] {
            assert_eq!(
                pb_engine_open(path, length, Some(wake), std::ptr::null_mut(), &mut output),
                Error::InvalidArgument as i32
            );
            assert_eq!(output, std::ptr::dangling_mut::<Engine>());
        }
        assert_eq!(
            pb_engine_open(b"x".as_ptr(), 4097, Some(wake), std::ptr::null_mut(), &mut output),
            Error::LimitExceeded as i32
        );
        assert_eq!(
            pb_engine_open(b"x".as_ptr(), 1, Some(wake), std::ptr::null_mut(), &mut output),
            Error::InvalidArgument as i32
        );
        assert_eq!(pb_engine_close(std::ptr::null_mut()), Error::InvalidArgument as i32);
    }
}
#[test]
fn durable_volume_and_failed_output_admission() {
    let directory = tempfile::tempdir().unwrap();
    let counter = AtomicUsize::new(0);
    let mut engine = open(directory.path(), &counter);
    let mut volume = Volume {
        size: std::mem::size_of::<Volume>() as u32,
        written: 0,
        id: [0; 16],
        owner: [0; 16],
        name: [0; 256],
        name_length: 0,
        share_key_with_service: 0,
    };
    // SAFETY: fixtures retain live handle, fixed outputs and input strings.
    unsafe {
        let before = counter.load(Ordering::Relaxed);
        assert_eq!(
            volumes::pb_engine_volume_create(engine, b"item".as_ptr(), 4, std::ptr::null_mut()),
            Error::InvalidArgument as i32
        );
        assert_eq!(counter.load(Ordering::Relaxed), before);
        #[repr(C)]
        struct Header {
            size: u32,
            written: u32,
        }
        let mut short = Header { size: 8, written: 0 };
        assert_eq!(
            volumes::pb_engine_volume_create(
                engine,
                b"item".as_ptr(),
                4,
                (&mut short as *mut Header).cast()
            ),
            Error::BufferTooSmall as i32
        );
        assert_eq!(counter.load(Ordering::Relaxed), before);
        assert_eq!(volumes::pb_engine_volume_create(engine, b"item".as_ptr(), 4, &mut volume), 0);
        assert_eq!(volume.written, 304);
        assert_eq!(volume.size, 304);
        assert!(counter.load(Ordering::Relaxed) > before);
        let mut batch = Changes {
            size: std::mem::size_of::<Changes>() as u32,
            written: 0,
            epoch: [0; 16],
            gap_generation: 0,
            account_generation: 0,
            closed: 0,
            records: 0,
            remaining: 0,
            reserved: 0,
        };
        assert_eq!(
            changes::pb_engine_drain(engine, Some(discard), std::ptr::null_mut(), &mut batch),
            0
        );
        assert!(batch.records > 0);
        assert_eq!(
            batch.remaining, 0,
            "consumed batch releases queue credit before reporting remaining work"
        );
        assert_eq!(pb_engine_close(&mut engine), 0);
        assert!(engine.is_null());
        let before = counter.load(Ordering::Relaxed);
        assert_eq!(pb_engine_close(&mut engine), Error::InvalidArgument as i32);
        assert_eq!(counter.load(Ordering::Relaxed), before);
        engine = open(directory.path(), &counter);
        let id = volume.id;
        volume.name.fill(0);
        assert_eq!(volumes::pb_engine_volume_read(engine, &id, &mut volume), 0);
        assert_eq!(&volume.name[..volume.name_length as usize], b"item");
        #[repr(C)]
        struct FutureVolume {
            volume: Volume,
            tail: [u8; 16],
        }
        let mut future = FutureVolume { volume, tail: [0xa5; 16] };
        future.volume.size = std::mem::size_of::<FutureVolume>() as u32;
        assert_eq!(volumes::pb_engine_volume_read(engine, &id, &mut future.volume), 0);
        assert_eq!(future.volume.size, 320);
        assert_eq!(future.volume.written, 304);
        assert_eq!(future.tail, [0xa5; 16]);
        assert_eq!(
            volumes::pb_engine_volume_read(engine, &[0; 16], &mut future.volume),
            Error::NotFound as i32
        );
        assert_eq!(future.volume.written, 0);
        assert_eq!(pb_engine_close(&mut engine), 0);
    }
    assert!(!directory.path().join(crate::paths::ENGINE_SOCKET).exists());
}
#[test]
fn bounded_drain_reports_sticky_gaps_and_preserves_rejected_batches() {
    use crate::store::change_feed::CommittedChangeFeed;
    // Use a real feed with fixed payloads to exercise saturation without hundreds
    // of filesystem transactions. Production drain logic/credits remain unchanged.
    let directory = tempfile::tempdir().unwrap();
    let counter = AtomicUsize::new(0);
    let mut engine = open(directory.path(), &counter);
    let feed = CommittedChangeFeed::with_wake(Id::generate().unwrap(), || {});
    let receiver = feed.take_receiver().unwrap();
    // SAFETY: test exclusively owns the live engine between foreign calls.
    unsafe { &*engine }.inner.lock().unwrap().changes = receiver;
    for _ in 0..crate::store::change_feed::RECORD_LIMIT + 44 {
        feed.publish(
            Id::from_bytes([1; 16]),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([7; 15]).unwrap()),
            kvdb::sync::CommittedDelta::Bytes(vec![1; 32]),
        );
    }
    assert_eq!(feed.pending_count(), crate::store::change_feed::RECORD_LIMIT);
    let mut status = Changes {
        size: std::mem::size_of::<Changes>() as u32,
        written: 0,
        epoch: [0; 16],
        gap_generation: 0,
        account_generation: 0,
        closed: 0,
        records: 0,
        remaining: 0,
        reserved: 0,
    };
    // SAFETY: callback borrows nothing; engine and outputs stay alive until close.
    unsafe {
        assert_eq!(
            changes::pb_engine_drain(
                engine,
                Some(discard),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            Error::InvalidArgument as i32
        );
        assert_eq!(feed.pending_count(), crate::store::change_feed::RECORD_LIMIT);
        #[repr(C, align(8))]
        struct Header {
            size: u32,
            written: u32,
        }
        let mut short = Header { size: 8, written: 0 };
        assert_eq!(
            changes::pb_engine_drain(
                engine,
                Some(discard),
                std::ptr::null_mut(),
                (&mut short as *mut Header).cast()
            ),
            Error::BufferTooSmall as i32
        );
        assert_eq!(feed.pending_count(), crate::store::change_feed::RECORD_LIMIT);
        for _ in 0..crate::store::change_feed::RECORD_LIMIT / 8 {
            assert_eq!(
                changes::pb_engine_drain(engine, Some(discard), std::ptr::null_mut(), &mut status),
                0
            );
            assert_eq!(status.records, 8);
            assert!(status.gap_generation > 0);
        }
        let gap = status.gap_generation;
        assert_eq!(feed.pending_count(), 0);
        assert_eq!(
            changes::pb_engine_drain(engine, Some(discard), std::ptr::null_mut(), &mut status),
            0
        );
        assert_eq!(status.records, 0);
        assert_eq!(status.gap_generation, gap);
        drop(feed);
        assert_eq!(
            changes::pb_engine_drain(engine, Some(discard), std::ptr::null_mut(), &mut status),
            0
        );
        assert_eq!(status.closed, 1);
        assert_eq!(pb_engine_close(&mut engine), 0);
    }
}
