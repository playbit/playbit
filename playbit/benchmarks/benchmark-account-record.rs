//! Single-threaded decoder latency/allocation diagnostic. No storage or network.
//! Run: cargo run --release -p playbit --example benchmark-account-record
#![allow(unsafe_code, reason = "The measurement allocator forwards the GlobalAlloc contract.")]
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use playbit::store::account::{Credentials, MAX_PLAINTEXT_BYTES, Record};
use playbit::store::catalog::record;
use playbit::store::{Id, PersonalVolumeSelection, Volume};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;
struct Allocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(live, Relaxed);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }
}
fn fixtures() -> [(&'static str, Vec<u8>, bool); 6] {
    let account = Id::from_bytes([0x12; 16]);
    let mut volume = Record::Volume {
        volume: Volume {
            id: account,
            owner: account,
            name: "x".into(),
            personal: true,
            share_key_with_service: true,
        },
        credentials: Credentials { database_key: [1; 32], blob_key: [2; 32] },
    };
    let size = volume.encode().unwrap().len();
    if let Record::Volume { volume, .. } = &mut volume {
        volume.name = "x".repeat(1 + MAX_PLAINTEXT_BYTES - size);
    }
    let volume = volume.encode().unwrap();
    assert_eq!(volume.len(), MAX_PLAINTEXT_BYTES);
    let maximum_grants = playbit_api::abi::syscall::MAX_VOLUMES - 1;
    let mut grants: Vec<_> = (1..=maximum_grants)
        .map(|index| PersonalVolumeSelection {
            volume: Id::from_bytes((index as u128).to_be_bytes()),
            write: true,
            incarnation: Id::from_bytes((index as u128).to_be_bytes()),
        })
        .collect();
    let session = playbit::identifier::encode(&[0x34; 32]);
    let maximum_grants =
        Record::PersonalVolumes { session: session.clone(), grants: grants.clone() }
            .encode()
            .unwrap();
    grants.push(PersonalVolumeSelection {
        volume: Id::from_bytes(((grants.len() + 1) as u128).to_be_bytes()),
        write: true,
        incarnation: Id::from_bytes(((grants.len() + 1) as u128).to_be_bytes()),
    });
    // Bypass semantic validation only for the invalid payload; retain the valid
    // account identity and framing produced by the public encoder.
    let mut too_many_grants = maximum_grants.clone();
    let fields = record::View::record(&too_many_grants)
        .unwrap()
        .fields(record::Mode::Write, &[1, 2, 3, 4, 5, 6])
        .unwrap();
    let payload = fields.get(6).unwrap().0;
    assert!(too_many_grants.ends_with(payload));
    let header_size = too_many_grants.len() - payload.len() - 6;
    too_many_grants.truncate(header_size);
    let mut payload = vec![record::BYTES];
    payload
        .extend_from_slice(&record::encode(&Record::PersonalVolumes { session, grants }).unwrap());
    record::put_raw(&mut too_many_grants, 6, &payload).unwrap();
    let small_display_name = Record::DisplayName { name: "Robin".into() }.encode().unwrap();
    let mut unknown_field_array = small_display_name.clone();
    // A well-formed optional header extension exceeds the opaque-field budget,
    // while the complete input remains below the account plaintext limit.
    unknown_field_array.extend_from_slice(&7u16.to_le_bytes());
    record::element(&mut unknown_field_array, &vec![0u64; 3000]).unwrap();
    assert!(unknown_field_array.len() - small_display_name.len() > record::MAX_EXTENSION_BYTES);
    [
        (
            "maximum_display_name",
            Record::DisplayName { name: "é".repeat(128) }.encode().unwrap(),
            true,
        ),
        ("small_display_name", small_display_name, true),
        ("maximum_volume", volume, true),
        ("maximum_grants", maximum_grants, true),
        ("too_many_grants", too_many_grants, false),
        ("unknown_field_array", unknown_field_array, false),
    ]
}

fn main() {
    let mut passed = true;
    for (name, plain, accepted) in fixtures() {
        assert!(plain.len() <= MAX_PLAINTEXT_BYTES);
        let mut samples = Vec::with_capacity(1000);
        let mut scratch = 0;
        for _ in 0..1000 {
            let baseline = LIVE.load(Relaxed);
            PEAK.store(baseline, Relaxed);
            let start = Instant::now();
            let result = Record::browser_plaintext(&plain);
            let elapsed = start.elapsed().as_nanos();
            scratch = scratch.max(PEAK.load(Relaxed) - baseline);
            assert_eq!(result.is_ok(), accepted);
            drop(result);
            samples.push(elapsed);
        }
        samples.sort_unstable();
        // Include one input copy in addition to the measured typed/output heap.
        let scratch_with_input = scratch + plain.len();
        passed &= scratch_with_input <= 256 * 1024;
        println!(
            "{}",
            serde_json::json!({"fixture":name,"samples":1000,"plaintext_bytes":plain.len(),
            "p99_ns":samples[989],"max_ns":samples[999],"peak_heap_bytes_including_input":scratch_with_input,
            "scratch_budget_bytes":256*1024,"accepted":accepted})
        );
    }
    assert!(passed, "account decode exceeds scratch allocation");
}

// One decode per fixture, no timers or repeated sampling. The example's test
// binary contains only this test, so other tests cannot perturb its allocator.
#[test]
fn account_decoding_correctness_and_scratch_bound() {
    for (name, plain, accepted) in fixtures() {
        assert!(plain.len() <= MAX_PLAINTEXT_BYTES);
        let baseline = LIVE.load(Relaxed);
        PEAK.store(baseline, Relaxed);
        let result = Record::browser_plaintext(&plain);
        let scratch_with_input = PEAK.load(Relaxed) - baseline + plain.len();
        assert_eq!(result.is_ok(), accepted, "{name}");
        assert!(
            scratch_with_input <= 256 * 1024,
            "{name}: account decode exceeds scratch allocation"
        );
    }
}
