//! OPFS-backed bundle admission. JavaScript lends a bounded, exclusive file
//! slot; the backing closes it after the last File or candidate retires.
use super::*;
use crate::package::{
    AppIdentity, AutomaticVolumeRole, Package,
    codec::{self, Backing},
    signature::{
        PublisherTrust,
        status::{KeyStatus, issuer_key_from_certificate},
    },
};
use std::{
    io::{self, Read},
    sync::{Arc, Mutex},
};
#[link(wasm_import_module = "opfs")]
unsafe extern "C" {
    fn bundle_length(slot: u32) -> i32;
    fn bundle_read(slot: u32, offset: u32, output: *mut u8, length: u32) -> i32;
    fn bundle_close(slot: u32);
}
struct Opfs {
    slot: u32,
    length: u64,
}
impl Backing for Opfs {
    fn read_at(&self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
        if offset.checked_add(bytes.len() as u64).is_none_or(|end| end > self.length) {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        for (index, chunk) in bytes.chunks_mut(65536).enumerate() {
            // SAFETY: synchronous import writes only this exclusively borrowed span.
            let read = unsafe {
                bundle_read(
                    self.slot,
                    (offset + index as u64 * 65536) as u32,
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                )
            };
            if read != chunk.len() as i32 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(())
    }
}
impl Drop for Opfs {
    fn drop(&mut self) {
        unsafe { bundle_close(self.slot) }
    }
}
struct Input {
    backing: Arc<Opfs>,
    position: u64,
}
impl Read for Input {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = out.len().min((self.backing.length - self.position) as usize);
        self.backing.read_at(self.position, &mut out[..n])?;
        self.position += n as u64;
        Ok(n)
    }
}
enum Candidate {
    Verifying { backing: Arc<Opfs>, verifier: Box<codec::Verifier<Input>> },
    Ready(Box<Package>),
}
static CANDIDATES: Mutex<BTreeMap<u32, Candidate>> = Mutex::new(BTreeMap::new());
#[unsafe(no_mangle)]
pub extern "C" fn pb_package(slot: u32) -> i32 {
    crate::platform::failure::guard();
    let result = (|| -> Result<Option<Vec<u8>>> {
        let mut candidates = CANDIDATES.lock().expect("package candidates lock");
        let candidate = if let Some(candidate) = candidates.remove(&slot) {
            candidate
        } else {
            if candidates.len() >= 4 {
                return Err(Error::LimitExceeded);
            }
            let length = unsafe { bundle_length(slot) };
            if length < 0 || length as usize > crate::package::MAX_BUNDLE_BYTES {
                return Err(Error::LimitExceeded);
            }
            let backing = Arc::new(Opfs { slot, length: length as u64 });
            let verifier = codec::Verifier::new(Input { backing: backing.clone(), position: 0 })
                .map_err(map_error)?;
            Candidate::Verifying { backing, verifier: Box::new(verifier) }
        };
        let Candidate::Verifying { backing, mut verifier } = candidate else {
            candidates.insert(slot, candidate);
            return Err(Error::Conflict);
        };
        if !verifier.step().map_err(map_error)? {
            candidates.insert(slot, Candidate::Verifying { backing, verifier });
            return Ok(None);
        }
        let package = Package::from_verified(backing, verifier.finish().map_err(map_error)?);
        let executable = package.index().entry(codec::EXECUTABLE).ok_or(Error::Internal)?;
        let certificate = package.publisher_certificate().map(|certificate| &certificate.claims);
        let unsupported: Vec<_> = package
            .required_host_features()
            .iter()
            .filter(|feature| !operations::supports(feature))
            .collect();
        let result = serde_json::to_vec(&serde_json::json!({
            "app_id": package.app_id(),
            "name": package.name(),
            "build_id": package.id().map_err(map_error)?,
            "offset": executable.offset,
            "length": executable.length,
            "publisher": certificate.map(|c| c.publisher),
            "signer": certificate.map(|c| c.signer),
            "required_host_features": package.required_host_features(),
            "unsupported_host_features": unsupported,
        }))
        .map_err(map_error)?;
        candidates.insert(slot, Candidate::Ready(Box::new(package)));
        Ok(Some(result))
    })();
    match result {
        Ok(Some(bytes)) => publish_output(bytes),
        Ok(None) => 0,
        Err(error) => error as i32,
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authentication {
    anchor: Anchor,
    status: Vec<u8>,
    admission: Option<Vec<u8>>,
    retained_example: Option<Vec<u8>>,
    session_id: String,
    expected_app: Option<AppIdentity>,
    bound_session: Option<Session>,
}

impl Authentication {
    fn authenticate_package(
        &mut self,
        package: &Package,
        trust: &PublisherTrust<'_>,
        now: u64,
    ) -> Result<(Package, Option<Vec<u8>>)> {
        use crate::package::signature::{PackageAdmission, admission::BuildAdmissionReceipt};
        match (&self.anchor, self.admission.take(), self.retained_example.take()) {
            (Anchor::ReleaseIssuer(_), None, None) => {
                let (package, record) =
                    package.clone().authenticate_first(trust, now).map_err(map_error)?;
                Ok((package, Some(record.to_bytes().to_vec())))
            }
            (Anchor::ReleaseIssuer(_), None, Some(bytes)) => {
                let saved = PackageAdmission::from_trusted_bytes(&bytes).map_err(map_error)?;
                let package =
                    package.clone().authenticate_admitted(trust, &saved, now).map_err(map_error)?;
                Ok((package, Some(bytes)))
            }
            (Anchor::EnrolledCa(_), Some(bytes), None) => {
                let receipt = BuildAdmissionReceipt::from_bytes(&bytes).map_err(map_error)?;
                let admission =
                    receipt.verify(trust.issuers, &package.digest(), now).map_err(map_error)?;
                let package = package
                    .clone()
                    .authenticate_admitted(trust, &admission, now)
                    .map_err(map_error)?;
                Ok((package, None))
            }
            _ => Err(Error::InvalidArgument),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Anchor {
    ReleaseIssuer([u8; 32]),
    EnrolledCa(Vec<u8>),
}

impl Anchor {
    fn issuer(&self) -> Result<[u8; 32]> {
        match self {
            Self::ReleaseIssuer(key) => Ok(*key),
            Self::EnrolledCa(der) => issuer_key_from_certificate(der).map_err(map_error),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusUpdate {
    anchor: Anchor,
    publisher: Id,
    signer: [u8; 32],
    previous: Option<Vec<u8>>,
    status: Vec<u8>,
}
/// The worker performs this check inside the installation's write transaction.
/// Publish verified revocations even when subsequent package admission fails.
#[unsafe(no_mangle)]
pub extern "C" fn pb_publisher_status(length: usize) -> i32 {
    crate::platform::failure::guard();
    let result = (|| -> Result<i32> {
        let input = admit_input(length, 24 * 1024)?;
        let update: StatusUpdate = serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let issuer = update.anchor.issuer()?;
        let now = unsafe { now() } as u64;
        let status = KeyStatus::from_bytes(&update.status).map_err(map_error)?;
        let signer = status.verify(&[issuer], now).map_err(map_error)?;
        if signer.publisher != update.publisher || signer.signer != update.signer {
            return Err(Error::AccessDenied);
        }
        if let Some(previous) = update.previous {
            let previous = KeyStatus::from_bytes(&previous).map_err(map_error)?;
            previous.verify(&[issuer], now).map_err(map_error)?;
            if matches!(update.anchor, Anchor::ReleaseIssuer(_))
                && previous.admit_update(&status).is_ok()
            {
                // An offline archive may carry an older authenticated snapshot.
                return Ok(1);
            }
            status.admit_update(&previous).map_err(map_error)?;
        }
        Ok(0)
    })();
    result.unwrap_or_else(|error| error as i32)
}

fn admit_execution(package: &Package, shared: bool) -> Result<()> {
    package
        .admit_execution(|name| {
            operations::supports(name)
                && (name != playbit_api::abi::data::SESSION_VOLUME_FEATURE || shared)
        })
        .map_err(|error| {
            map_error(error);
            Error::Unsupported
        })
}

/// Bind verified OPFS bytes to an independently obtained AP issuer and signed
/// signer state before returning either scope addresses or initializer data.
#[unsafe(no_mangle)]
pub extern "C" fn pb_package_authenticate(slot: u32, length: usize) -> i32 {
    crate::platform::failure::guard();
    let result = (|| -> Result<Vec<u8>> {
        let input = admit_input(length, 24 * 1024)?;
        let mut request: Authentication =
            serde_json::from_slice(&input[..length]).map_err(map_error)?;
        drop(input);
        let issuer = request.anchor.issuer()?;
        let now = unsafe { now() } as u64;
        let status = KeyStatus::from_bytes(&request.status).map_err(map_error)?;
        let signer = status.verify(&[issuer], now).map_err(map_error)?;
        let mut candidates = CANDIDATES.lock().expect("package candidates lock");
        let Some(Candidate::Ready(package)) = candidates.get_mut(&slot) else {
            return Err(Error::InvalidArgument);
        };
        let trust = PublisherTrust { issuers: &[issuer], signers: &[signer] };
        let (authenticated, local_admission) =
            request.authenticate_package(package, &trust, now)?;
        let app = authenticated.identity().map_err(map_error)?;
        if request.expected_app.as_ref().is_some_and(|expected| expected != app) {
            return Err(Error::AccessDenied);
        }
        let world =
            app.volume_id(AutomaticVolumeRole::Root, None, None, None).map_err(map_error)?;
        let session = crate::identifier::decode(&request.session_id).map_err(map_error)?;
        let shared = authenticated.initializers().declares(AutomaticVolumeRole::Session);
        let available = if let Some(bound) = &request.bound_session {
            bound.validate_shared_volume().map_err(store_error)?;
            if bound.id != request.session_id || &bound.app != app {
                return Err(Error::AccessDenied);
            }
            bound.shared_volume.is_some()
        } else {
            shared
        };
        admit_execution(&authenticated, available)?;
        let shared_volume = shared
            .then(|| app.volume_id(AutomaticVolumeRole::Session, Some(session), None, None))
            .transpose()
            .map_err(map_error)?;
        let mut initializer = authenticated
            .initializer(if shared {
                AutomaticVolumeRole::Session
            } else {
                AutomaticVolumeRole::Root
            })
            .map_err(map_error)?;
        // Local examples only grant authority to the local creator.
        initializer.permissions.grants.clear();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "app": app, "world": world, "shared": shared_volume, "initializer": initializer, "local_admission": local_admission,
        }))
        .map_err(map_error)?;
        **package = authenticated;
        Ok(bytes)
    })();
    match result {
        Ok(bytes) => publish_output(bytes),
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_package_adopt(slot: u32) -> i32 {
    crate::platform::failure::guard();
    let package = CANDIDATES.lock().expect("package candidates lock").remove(&slot);
    let Some(Candidate::Ready(package)) = package else {
        return Error::InvalidArgument as i32;
    };
    operations::call(|host, _| {
        let session = host.session.borrow();
        admit_execution(&package, session.shared_volume.is_some())?;
        if package.identity().map_err(map_error)? != &session.app {
            return Err(Error::AccessDenied);
        }
        let mut files = host.files.borrow_mut();
        if !files.ready_for_bundle() {
            return Err(Error::Busy);
        }
        files.bundle = Some((*package, new_bundle_id()));
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_package_discard(slot: u32) -> i32 {
    crate::platform::failure::guard();
    CANDIDATES.lock().expect("package candidates lock").remove(&slot);
    0
}
