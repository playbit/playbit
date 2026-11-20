#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;

#[test]
#[ignore = "reference-device blob storage timings; not a CI timing assertion"]
fn benchmark_authorized_blob_reads_and_writes() {
    fn report(name: &str, mut samples: Vec<f64>) {
        samples.sort_by(f64::total_cmp);
        crate::diagnostic!(
            "{name} n={} p50={:.6} p99={:.6} max={:.6} ms",
            samples.len(),
            samples[499],
            samples[989],
            samples[999]
        );
    }
    let (_directory, store, first, access) = setup();
    let mut volumes = vec![first.id];
    for _ in 0..3 {
        let volume = store.create_volume("Blob workload", true).unwrap();
        volumes.push(volume.id);
    }
    let mut bytes = vec![42; 64 * 1024];
    let entries: Vec<_> = volumes
        .iter()
        .map(|volume| store.create_blob(&access, *volume, "file", &bytes).unwrap().id)
        .collect();
    let mut writes = Vec::with_capacity(1000);
    let mut pending_reads = Vec::with_capacity(1000);
    for sample in 0u64..1000 {
        let index = sample as usize % 4;
        bytes[..8].copy_from_slice(&sample.to_le_bytes());
        let start = std::time::Instant::now();
        store.write_blob(&access, volumes[index], entries[index], &bytes).unwrap();
        writes.push(start.elapsed().as_secs_f64() * 1000.0);
        let start = std::time::Instant::now();
        let read = store.read_blob_entry(&access, volumes[index], entries[index]).unwrap();
        pending_reads.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(read, bytes);
    }
    // One plaintext hash shared across four separately acknowledged envelopes.
    for index in 0..4 {
        store.write_blob(&access, volumes[index], entries[index], &bytes).unwrap();
        let version = store
            .blob_version(
                &access,
                crate::store::volume_index::EntryAddress {
                    volume: volumes[index],
                    entry: entries[index],
                },
                None,
            )
            .unwrap();
        let envelope = encrypted_bytes(&store, &access, &version).unwrap();
        // Fixture receipt; this workload measures local storage, not remote AP durability.
        store.receive_blob_version(&access, &version, &mut &envelope[..], true).unwrap();
    }
    let mut cache_reads = Vec::with_capacity(1000);
    for sample in 0..1000 {
        let index = sample % 4;
        let start = std::time::Instant::now();
        let read = store.read_blob_entry(&access, volumes[index], entries[index]).unwrap();
        cache_reads.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(read, bytes);
    }
    report("blob-write-64KiB", writes);
    report("blob-pending-read-64KiB", pending_reads);
    report("blob-cache-read-64KiB", cache_reads);
    crate::trace::mark("storage.blob_workload_complete");
}
