use super::*;
use crate::store::change_feed::CommittedChangeKind;
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Change {
    pub size: u32,
    pub written: u32,
    pub volume: [u8; 16],
    pub sequence: u64,
    pub kind: u32,
    pub database: [u8; 16],
    pub reserved: u32,
}
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Changes {
    pub size: u32,
    pub written: u32,
    pub epoch: [u8; 16],
    pub gap_generation: u64,
    pub account_generation: u64,
    pub closed: u32,
    pub records: u32,
    pub remaining: u32,
    pub reserved: u32,
}
memory::record!(Change, 56, {
    volume: [u8; 16], sequence: u64, kind: u32, database: [u8; 16], reserved: u32,
});
memory::record!(Changes, 56, {
    epoch: [u8; 16], gap_generation: u64, account_generation: u64,
    closed: u32, records: u32, remaining: u32, reserved: u32,
});
pub type Callback = unsafe extern "C" fn(*mut c_void, *const Change, *const u8, u32);
/// # Safety
/// Handle, callback, callback data and output obey playbit/engine.h.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_drain(
    engine: *mut Engine,
    callback: Option<Callback>,
    data: *mut c_void,
    output: *mut Changes,
) -> i32 {
    // SAFETY: caller retains all borrowed objects; no pointer survives this call.
    unsafe {
        with_engine(engine, |inner| {
            memory::native_region(output, true)?;
            memory::clear_native(output)?;
            let callback = callback.ok_or(Error::InvalidArgument)?;
            let batch = inner.changes.drain();
            #[cfg(feature = "native-runtime")]
            {
                inner.apps.committed(&batch);
            }
            for item in &batch.records {
                let (kind, database) = match item.kind() {
                    CommittedChangeKind::Database(database) => (1, database.to_bytes()),
                    CommittedChangeKind::VolumeAvailable => (4, [0; 16]),
                };
                let change = Change {
                    size: std::mem::size_of::<Change>() as u32,
                    written: std::mem::size_of::<Change>() as u32,
                    volume: *item.volume().as_bytes(),
                    sequence: item.sequence(),
                    kind,
                    database,
                    reserved: 0,
                };
                let length =
                    u32::try_from(item.bytes().len()).expect("feed payload is bounded below u32");
                callback(data, &change, item.bytes().as_ptr(), length);
            }
            let records = batch.records.len();
            assert!(records <= 8, "feed drain is bounded");
            // Pending credit includes the batch until its payloads are dropped.
            // Release it after callbacks and before reporting work left to drain.
            drop(batch.records);
            memory::write_native(
                output,
                Changes {
                    size: std::mem::size_of::<Changes>() as u32,
                    written: 0,
                    epoch: *batch.status.epoch.as_bytes(),
                    gap_generation: batch.status.gap_generation,
                    account_generation: batch.account_generation,
                    closed: batch.status.closed.map_or(0, |reason| reason as u32),
                    records: records as u32,
                    remaining: inner.services.store.pending_change_count() as u32,
                    reserved: 0,
                },
            )?;
            Ok(())
        })
    }
}
