//! Durable storage for Archipelago.

#![deny(clippy::unwrap_used)]
#![forbid(unsafe_code)]

pub mod account;
mod app_scope;
pub use app_scope::AppScope;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub mod account_key;
pub(crate) mod authority;
mod blob;
pub mod browser_drafts;
pub mod catalog;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub mod change_feed;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
mod file_backend;
pub mod file_provider;
pub mod initialization;
pub mod key_value;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
mod local;
pub mod object_contract;
pub mod object_database;
pub mod profile;
pub mod query;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub mod recovery;
mod replica_delta;
pub mod volume_index;
pub use blob::{
    BLOB_CHUNK_BYTES, BLOB_ENVELOPE_VERSION, BLOB_HEADER_BYTES, MAX_BLOB_ENVELOPE_BYTES,
    MAX_BLOB_PLAINTEXT_BYTES, blob_block_aad, blob_block_nonce, blob_content_key,
    blob_envelope_len,
};
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub use blob::{open_blob_to, seal_blob_to};
pub use catalog::*;
pub use initialization::{
    InitialBlob, InitialDatabase, InitialDatabaseKind, InitialObject, MAX_ROOT_INITIAL_OBJECTS,
    MAX_ROOT_INITIALIZATION_BYTES, RootInitialization,
};
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub use local::*;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
mod session_reference;
pub use profile::Account;
pub use query::ObjectPage;
pub use replica_delta::{
    MAX_REPLICA_DELTA_BYTES, authorize_publication_delta, project_replica_delta,
};
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub use session_reference::SessionReference;

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
use std::time::{SystemTime, UNIX_EPOCH};

mod database_id;
mod database_reference;
mod entry_id;
mod error;
mod id;
mod invitation;
pub use database_id::DatabaseId;
pub use database_reference::{DatabaseReference, MAX_DATABASE_REFERENCE_BYTES};
#[cfg(feature = "app-host")]
pub(crate) use entry_id::ArchivedEntryId;
pub use entry_id::EntryId;
pub use error::{Error, Result};
pub use id::Id;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub use invitation::session_id;
pub use invitation::{
    Invitation, MAX_ENVELOPE_BYTES, SEED_BYTES, Seed, SharedVolume, envelope_text,
    parse_envelope_text, parse_session_code, parse_session_link, session_code, session_link,
    session_uri,
};

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
impl Id {
    /// Creates a sortable, globally unique identifier.
    ///
    /// The first four bytes are coarse Unix time in big-endian order. The
    /// remaining twelve bytes come from the operating system random source.
    pub fn generate() -> Result<Id> {
        let mut bytes = [0; 16];
        let timestamp = u32::try_from(now_unix_secs()).unwrap_or(u32::MAX);
        bytes[..4].copy_from_slice(&timestamp.to_be_bytes());
        bytes[4..].copy_from_slice(&random_array::<12>()?);
        Ok(Id(bytes))
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
fn random_array<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| Error::System(format!("system random source failed: {error}")))?;
    Ok(bytes)
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub(crate) fn now_unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_contain_coarse_time_and_randomness() {
        let before = u32::try_from(now_unix_secs()).unwrap();
        let first = Id::generate().unwrap();
        let second = Id::generate().unwrap();
        let after = u32::try_from(now_unix_secs()).unwrap();

        let timestamp = u32::from_be_bytes(first.as_bytes()[..4].try_into().unwrap());
        assert!((before..=after).contains(&timestamp));
        assert_ne!(first, second);
        assert_eq!(&first.as_bytes()[..4], &second.as_bytes()[..4]);
        assert_ne!(&first.as_bytes()[4..], &second.as_bytes()[4..]);
    }

    #[test]
    fn ids_display_debug_and_serialize_as_canonical_base58() {
        let id = Id::from_bytes([0xab; 16]);
        let encoded = serde_json::to_string(&id).unwrap();
        assert_eq!(encoded, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<Id>(&encoded).unwrap(), id);
        assert!(serde_json::from_str::<Id>("\"not-an-id\"").is_err());
    }

    #[test]
    fn ids_have_exactly_one_spelling() {
        let id = Id::from_bytes([0xab; 16]);
        let canonical = id.to_string();
        assert_eq!(canonical, "NCX9PNbvZtwyXe5YjATFzi");
        assert_eq!(format!("{id:?}"), format!("Id({canonical})"));
        assert_eq!(Id::parse(&canonical).unwrap(), id);
        for other in [
            format!("1{canonical}"),
            format!("{canonical}0"),
            format!(" {canonical}"),
            "ab".repeat(16),
        ] {
            assert!(Id::parse(&other).is_err(), "{other:?}");
            assert!(serde_json::from_str::<Id>(&format!("\"{other}\"")).is_err(), "{other:?}");
        }
    }
}

pub mod bloblog;
