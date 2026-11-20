//! Engine file backends; all filesystem I/O runs on bounded workers.
pub mod authority;
pub mod budget;
pub mod captured_list;
pub mod executor;
pub mod host;
pub mod listing;
pub mod metadata;
pub mod privacy;
pub mod private_storage;
pub mod publication;
pub mod resource;
pub mod snapshot;
pub mod staging;
pub mod status;

pub mod volume;

pub mod owner;

/// Match std::fs::MetadataExt::dev across Darwin's signed dev_t and Linux's u64.
fn device_id(value: libc::dev_t) -> u64 {
    #[cfg(target_vendor = "apple")]
    {
        value as u64
    }
    #[cfg(target_os = "linux")]
    {
        value
    }
}
