//! Native images cross a stronger trust boundary than portable Wasm. Only the
//! local compiler and the trusted host cache construct these values.
use super::Runtime;
#[cfg(feature = "local-store")]
use super::RuntimeError;
use std::hash::{Hash, Hasher};

#[cfg(feature = "local-store")]
pub(crate) const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Descriptor {
    #[serde(with = "crate::identifier::serde")]
    pub wasm: [u8; 32],
    #[serde(with = "crate::identifier::serde")]
    pub compatibility: [u8; 32],
    #[serde(with = "crate::identifier::serde")]
    pub image: [u8; 32],
    pub bytes: u64,
}
impl Descriptor {
    #[cfg(feature = "local-store")]
    pub(crate) fn key(&self) -> [u8; 32] {
        key(self.wasm, self.compatibility)
    }
}

#[cfg(feature = "local-store")]
pub(crate) fn key(wasm: [u8; 32], compatibility: [u8; 32]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(&wasm);
    hash.update(&compatibility);
    *hash.finalize().as_bytes()
}

pub(crate) struct Image {
    pub descriptor: Descriptor,
    pub bytes: Vec<u8>,
}
impl Image {
    /// The expected descriptor comes from the compiler, trusted local cache or
    /// authenticated engine IPC. The image bytes are privately owned.
    #[cfg(feature = "local-store")]
    pub(crate) fn verified(descriptor: Descriptor, bytes: Vec<u8>) -> Result<Self, RuntimeError> {
        if bytes.len() > MAX_IMAGE_BYTES
            || descriptor.bytes != bytes.len() as u64
            || blake3::hash(&bytes).as_bytes() != &descriptor.image
        {
            return Err(RuntimeError::InvalidModule("compiled image digest mismatch".into()));
        }
        Ok(Self { descriptor, bytes })
    }
}

pub(crate) trait Cache: Send + Sync {
    fn load(&self, wasm: [u8; 32], compatibility: [u8; 32]) -> Option<Image>;
    fn publish(&self, image: &Image);
    #[cfg(feature = "local-store")]
    fn descriptor(&self, wasm: [u8; 32], compatibility: [u8; 32]) -> Option<Descriptor>;
}

struct Fingerprint(blake3::Hasher);
impl Hasher for Fingerprint {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    fn finish(&self) -> u64 {
        u64::from_le_bytes(self.0.finalize().as_bytes()[..8].try_into().expect("hash width"))
    }
}
impl Runtime {
    pub(crate) fn compatibility(&self) -> [u8; 32] {
        let mut hash = Fingerprint(blake3::Hasher::new());
        // Change this whenever atomics lowering, ABI admission, or image layout
        // changes. Wasmtime additionally binds its target, CPU and VM settings.
        hash.write(b"playbit-compiled-module-1");
        self.engine.precompile_compatibility_hash().hash(&mut hash);
        *hash.0.finalize().as_bytes()
    }
    #[cfg(feature = "local-store")]
    pub(crate) fn with_cache(mut self, cache: std::sync::Arc<dyn Cache>) -> Self {
        self.cache = Some(cache);
        self
    }
    #[cfg(feature = "local-store")]
    pub(crate) fn compiled_descriptor(
        &self,
        wasm: [u8; 32],
        compatibility: [u8; 32],
    ) -> Option<Descriptor> {
        self.cache.as_ref()?.descriptor(wasm, compatibility)
    }
}
