//! Bounded archive construction and validated borrowed access for local IPC.
//!
//! The socket prefix is kept separate so its size cannot misalign the archive.
//! No shared pointers or recursive types belong in the local message schema.
use rkyv::{
    Portable, Serialize,
    api::{
        access_with_context,
        low::{LowSerializer, LowValidator, to_bytes_in_with_alloc},
    },
    bytecheck::CheckBytes,
    rancor::{Error, Source},
    ser::{Positional, Writer, allocator::SubAllocator},
    util::AlignedVec,
    validation::{Validator, archive::ArchiveValidator},
};
use std::{io, mem::MaybeUninit, num::NonZeroUsize};

pub const MAX_MESSAGE: usize = 4 * 1024 * 1024;
pub const IDENTITY_LEN: usize = 24;
pub const PREFIX_LEN: usize = IDENTITY_LEN;
pub const HEADER_LEN: usize = 4 + PREFIX_LEN;
pub const MAX_ARCHIVE: usize = MAX_MESSAGE - PREFIX_LEN;
/// Per synchronous serialization, released before the result is queued.
pub const MAX_SCRATCH: usize = 64 * 1024;
const MAX_DEPTH: usize = 32;
/// Framing revision. BUILD_ID automatically tracks source and dependency changes.
pub const ABI: u32 = 2;
include!(concat!(env!("OUT_DIR"), "/ipc_identity.rs"));
pub const PREFIX: [u8; PREFIX_LEN] = {
    let mut bytes = [0; PREFIX_LEN];
    bytes[0] = b'P';
    bytes[1] = b'B';
    bytes[2] = b'I';
    bytes[3] = b'P';
    let abi = ABI.to_le_bytes();
    let mut i = 0;
    while i < 4 {
        bytes[4 + i] = abi[i];
        i += 1;
    }
    i = 0;
    while i < BUILD_ID.len() {
        bytes[8 + i] = BUILD_ID[i];
        i += 1;
    }
    bytes
};
pub type Bytes = AlignedVec<16>;

const _: () = assert!(u32::from_le_bytes([PREFIX[4], PREFIX[5], PREFIX[6], PREFIX[7]]) == ABI);
const _: () = assert!(MAX_MESSAGE <= u32::MAX as usize);

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Validate framing before reserving or allocating any archive storage.
pub fn archive_length(body_length: u32) -> io::Result<usize> {
    let body_length = body_length as usize;
    if !(PREFIX_LEN..=MAX_MESSAGE).contains(&body_length) {
        return Err(invalid("local message length is invalid"));
    }
    Ok(body_length - PREFIX_LEN)
}

pub fn check_prefix(prefix: &[u8; PREFIX_LEN]) -> io::Result<()> {
    if prefix[..IDENTITY_LEN] != PREFIX[..IDENTITY_LEN] {
        return Err(invalid("The local Playbit engine uses a different IPC build."));
    }
    Ok(())
}

/// Frame one bounded archive with its protocol/build identity.
pub fn frame_header(archive_length: usize) -> io::Result<[u8; HEADER_LEN]> {
    let body_length = framed_body_length(archive_length)
        .ok_or_else(|| invalid("local archive exceeds its byte limit"))?;
    let mut header = [0; HEADER_LEN];
    header[..4].copy_from_slice(&body_length.to_le_bytes());
    header[4..].copy_from_slice(&PREFIX);
    Ok(header)
}

/// Pure framing arithmetic shared by the production header and its proof.
fn framed_body_length(archive_length: usize) -> Option<u32> {
    if archive_length > MAX_ARCHIVE {
        return None;
    }
    Some((PREFIX_LEN + archive_length) as u32)
}

/// Read a single frame. The caller supplies its available archive-storage
/// credit (or a smaller operation limit); framing is checked before allocation.
/// Socket deadlines and peer verification belong to the caller.
pub fn read(reader: &mut impl io::Read, credit: usize) -> io::Result<Bytes> {
    let mut header = [0; 4 + PREFIX_LEN];
    let mut received_bytes = 0;
    let mut length = 0;
    while received_bytes < header.len() {
        match reader.read(&mut header[received_bytes..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => received_bytes += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        // A read may return only the length. Reject it now rather than waiting
        // for a peer that never sends the prefix; never read archive bytes here.
        if received_bytes >= 4 {
            length = archive_length(u32::from_le_bytes(
                header[..4].try_into().expect("four-byte length"),
            ))?;
            if length > credit {
                return Err(invalid("local archive exceeds available receive credit"));
            }
        }
    }
    check_prefix(header[4..].try_into().expect("fixed-size prefix"))?;
    let mut bytes = Bytes::with_capacity(length);
    bytes.resize(length, 0);
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Write one current-ABI archive with its framing, without copying the archive
/// into another buffer. A failed write must not trigger mutation replay.
pub fn write(writer: &mut impl io::Write, archive: &[u8]) -> io::Result<()> {
    let header = frame_header(archive.len())?;
    writer.write_all(&header)?;
    writer.write_all(archive)?;
    Ok(())
}

/// An output writer whose retained allocation never exceeds its admitted limit.
/// Growth is geometric, capped by the caller's available byte credit.
pub struct Output {
    bytes: Bytes,
    limit: usize,
}
impl Positional for Output {
    fn pos(&self) -> usize {
        self.bytes.len()
    }
}
impl Writer<Error> for Output {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let end = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|end| *end <= self.limit)
            .ok_or_else(|| Error::new(invalid("local archive exceeds its byte limit")))?;
        if end > self.bytes.capacity() {
            let capacity = end.next_power_of_two().min(self.limit);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        assert!(self.bytes.capacity() <= self.limit, "archive output owns admitted byte credit");
        Ok(())
    }
}

/// Serialize directly into bounded aligned storage, with fixed scratch and no
/// shared-pointer registry. The caller accounts for retained capacity, not length.
pub fn encode<T>(value: &T, limit: usize) -> Result<Bytes, Error>
where
    T: for<'a> Serialize<LowSerializer<Output, SubAllocator<'a>, Error>>,
{
    let mut scratch = [MaybeUninit::uninit(); MAX_SCRATCH];
    let output = Output { bytes: Bytes::new(), limit: limit.min(MAX_ARCHIVE) };
    let output =
        to_bytes_in_with_alloc::<_, _, Error>(value, output, SubAllocator::new(&mut scratch))?;
    Ok(output.bytes)
}

/// Validate every relative reference and discriminant before exposing a borrow.
/// The borrow cannot outlive or mutate its owning bytes. Operation-level limits
/// and authorization must still be checked before acting on the request.
pub fn access<T>(bytes: &[u8]) -> Result<&T, Error>
where
    T: Portable + for<'a> CheckBytes<LowValidator<'a, Error>>,
{
    if bytes.len() > MAX_ARCHIVE {
        return Err(Error::new(invalid("local archive exceeds its byte limit")));
    }
    let mut validator =
        Validator::new(ArchiveValidator::with_max_depth(bytes, NonZeroUsize::new(MAX_DEPTH)), ());
    access_with_context::<T, _, Error>(bytes, &mut validator)
}

#[allow(
    unsafe_code,
    reason = "Verify checks cursor invariants after bytecheck validates its String field."
)]
// SAFETY: the String has already been validated. This checks the same complete
// cursor invariants as parse, without allocating or assuming valid contents.
unsafe impl<C> rkyv::bytecheck::Verify<C> for crate::store::bloblog::ArchivedCursor
where
    C: rkyv::rancor::Fallible + ?Sized,
    C::Error: rkyv::rancor::Source,
{
    fn verify(&self, _: &mut C) -> Result<(), C::Error> {
        crate::store::bloblog::Cursor::validate(self.as_str()).map_err(rkyv::rancor::Source::new)
    }
}

#[allow(
    unsafe_code,
    reason = "Verify rejects zero entry IDs after bytecheck validates their bytes."
)]
// SAFETY: the fixed byte array has already been validated by bytecheck. The
// scalar check enforces the constructor invariant without unchecked access.
unsafe impl<C> rkyv::bytecheck::Verify<C> for crate::store::ArchivedEntryId
where
    C: rkyv::rancor::Fallible + ?Sized,
    C::Error: rkyv::rancor::Source,
{
    fn verify(&self, _: &mut C) -> Result<(), C::Error> {
        self.validate().map_err(rkyv::rancor::Source::new)
    }
}

/// Unix paths retain their bytes across IPC, including non-UTF-8 filenames.
pub struct UnixPath;
impl rkyv::with::ArchiveWith<std::path::PathBuf> for UnixPath {
    type Archived = rkyv::vec::ArchivedVec<u8>;
    type Resolver = rkyv::vec::VecResolver;

    fn resolve_with(
        field: &std::path::PathBuf,
        resolver: Self::Resolver,
        out: rkyv::Place<Self::Archived>,
    ) {
        use std::os::unix::ffi::OsStrExt;
        Self::Archived::resolve_from_slice(field.as_os_str().as_bytes(), resolver, out);
    }
}
impl<S> rkyv::with::SerializeWith<std::path::PathBuf, S> for UnixPath
where
    S: rkyv::rancor::Fallible + rkyv::ser::Allocator + Writer + ?Sized,
{
    fn serialize_with(
        field: &std::path::PathBuf,
        serializer: &mut S,
    ) -> Result<Self::Resolver, S::Error> {
        use std::os::unix::ffi::OsStrExt;
        Self::Archived::serialize_from_slice(field.as_os_str().as_bytes(), serializer)
    }
}
impl<D: rkyv::rancor::Fallible + ?Sized>
    rkyv::with::DeserializeWith<rkyv::vec::ArchivedVec<u8>, std::path::PathBuf, D> for UnixPath
{
    fn deserialize_with(
        field: &rkyv::vec::ArchivedVec<u8>,
        _deserializer: &mut D,
    ) -> Result<std::path::PathBuf, D::Error> {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(field.as_slice().to_vec()).into())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful archive fixtures.")]
mod tests {
    use super::*;

    #[derive(rkyv::Archive, rkyv::Serialize)]
    struct Presentation {
        session: String,
        scene: Vec<u8>,
        logs: Vec<String>,
    }

    #[test]
    fn scene_bytes_are_borrowed_verbatim_from_the_archive() {
        for length in [0, 1, 1024 * 1024] {
            let value = Presentation {
                session: "session".into(),
                scene: (0..length).map(|i| i as u8).collect(),
                logs: vec!["hello".into(), "Unicode: λ".into()],
            };
            let bytes = encode(&value, MAX_ARCHIVE).unwrap();
            let archived = access::<ArchivedPresentation>(&bytes).unwrap();
            assert_eq!(archived.session.as_str(), value.session);
            assert_eq!(archived.scene.as_slice(), value.scene);
            assert_eq!(archived.logs[1].as_str(), value.logs[1]);
            let start = bytes.as_ptr() as usize;
            let scene_start = archived.scene.as_ptr() as usize;
            assert!((start..=start + bytes.len()).contains(&scene_start));
            assert!(scene_start + length <= start + bytes.len());
            assert!(bytes.len() < length + 256, "scene has no per-byte framing");
        }
    }

    #[test]
    fn output_limits_apply_before_growth_including_the_root() {
        let value = vec![1u8; 1024];
        let complete = encode(&value, MAX_ARCHIVE).unwrap();
        assert!(encode(&value, complete.len() - 1).is_err());
        let exact = encode(&value, complete.len()).unwrap();
        assert_eq!(exact.len(), complete.len());
        assert_eq!(exact.capacity(), complete.len());
        assert!(encode(&value, 0).is_err());
        let mut output = Output { bytes: Bytes::new(), limit: 8 };
        output.write(&[1; 8]).unwrap();
        assert!(output.write(&[2]).is_err());
        assert_eq!(output.bytes.as_slice(), &[1; 8]);
        assert_eq!(output.bytes.capacity(), 8);
    }

    #[test]
    fn framing_rejects_oversized_truncated_and_mismatched_messages() {
        assert!(archive_length(0).is_err());
        assert!(archive_length((PREFIX_LEN - 1) as u32).is_err());
        assert_eq!(archive_length(MAX_MESSAGE as u32).unwrap(), MAX_ARCHIVE);
        assert!(archive_length((MAX_MESSAGE + 1) as u32).is_err());
        assert!(archive_length(u32::MAX).is_err());
        check_prefix(&PREFIX).unwrap();
        for i in 0..IDENTITY_LEN {
            let mut prefix = PREFIX;
            prefix[i] ^= 1;
            assert!(check_prefix(&prefix).is_err());
        }
    }

    #[test]
    fn framed_archive_survives_partial_io_without_losing_alignment() {
        struct Partial<T>(T);
        impl<T: io::Read> io::Read for Partial<T> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let length = bytes.len().min(3);
                self.0.read(&mut bytes[..length])
            }
        }
        impl<T: io::Write> io::Write for Partial<T> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.write(&bytes[..bytes.len().min(3)])
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.flush()
            }
        }
        let payload = vec![0u64, 1, u64::MAX];
        let archive = encode(&payload, MAX_ARCHIVE).unwrap();
        let mut framed = Partial(Vec::new());
        write(&mut framed, &archive).unwrap();
        assert_eq!(framed.0.len(), 4 + PREFIX_LEN + archive.len());
        let received = read(&mut Partial(framed.0.as_slice()), archive.len()).unwrap();
        assert_eq!(received.capacity(), archive.len());
        assert_eq!(received.as_ptr() as usize % 16, 0);
        let borrowed = access::<rkyv::Archived<Vec<u64>>>(&received).unwrap();
        assert_eq!(borrowed[2].to_native(), u64::MAX);
    }

    #[test]
    fn ready_header_is_one_read_and_interruption_preserves_framing() {
        struct Reader<'a> {
            bytes: &'a [u8],
            reads: usize,
            interrupt: bool,
        }
        impl io::Read for Reader<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if std::mem::take(&mut self.interrupt) {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.bytes.read(output)
            }
        }
        let archive = encode(&vec![42u8; 32], MAX_ARCHIVE).unwrap();
        let mut frame = Vec::new();
        write(&mut frame, &archive).unwrap();
        for interrupted in [false, true] {
            let mut reader = Reader { bytes: &frame, reads: 0, interrupt: interrupted };
            assert_eq!(read(&mut reader, MAX_ARCHIVE).unwrap().as_slice(), archive.as_slice());
            assert!(reader.bytes.is_empty());
            assert_eq!(reader.reads, 2 + usize::from(interrupted));
        }
    }

    #[test]
    fn frame_limits_and_abi_are_checked_before_reading_the_archive() {
        let archive = encode(&vec![7u8; 32], MAX_ARCHIVE).unwrap();
        let mut frame = Vec::new();
        write(&mut frame, &archive).unwrap();
        let mut reader = io::Cursor::new(&frame);
        assert!(read(&mut reader, archive.len() - 1).is_err());
        assert_eq!(reader.position(), (4 + PREFIX_LEN) as u64, "credit precedes archive reads");
        frame[4] ^= 1;
        let mut reader = io::Cursor::new(&frame);
        assert!(read(&mut reader, MAX_ARCHIVE).is_err());
        assert_eq!(reader.position(), (4 + PREFIX_LEN) as u64);
        frame[4] ^= 1;
        for end in 0..frame.len() {
            assert!(read(&mut &frame[..end], MAX_ARCHIVE).is_err());
        }
        let mut oversized = io::Cursor::new(u32::MAX.to_le_bytes());
        assert!(read(&mut oversized, usize::MAX).is_err());
        assert_eq!(oversized.position(), 4);
    }

    #[test]
    fn malformed_offsets_lengths_and_alignment_are_rejected() {
        let value = vec![42u64, 17, 99];
        let bytes = encode(&value, MAX_ARCHIVE).unwrap();
        assert!(access::<rkyv::Archived<Vec<u64>>>(&bytes).is_ok());
        // An archived Vec root is an eight-byte relative pointer/length pair
        // under the explicitly selected 32-bit pointer format.
        let root = bytes.len() - 8;
        for offset in [0, 4] {
            let mut invalid = bytes.clone();
            invalid[root + offset..root + offset + 4].fill(0xff);
            assert!(access::<rkyv::Archived<Vec<u64>>>(&invalid).is_err());
        }
        assert!(access::<rkyv::Archived<Vec<u64>>>(&bytes[..4]).is_err());
        let mut shifted = Bytes::new();
        shifted.push(0);
        shifted.extend_from_slice(&bytes);
        assert!(access::<rkyv::Archived<Vec<u64>>>(&shifted[1..]).is_err());
    }

    #[test]
    fn invalid_enum_discriminant_is_rejected() {
        #[derive(rkyv::Archive, rkyv::Serialize)]
        enum Reply {
            Ready,
            Failed,
        }
        let mut bytes = encode(&Reply::Ready, MAX_ARCHIVE).unwrap();
        assert!(access::<ArchivedReply>(&bytes).is_ok());
        assert!(access::<ArchivedReply>(&encode(&Reply::Failed, MAX_ARCHIVE).unwrap()).is_ok());
        bytes[0] = 255;
        assert!(access::<ArchivedReply>(&bytes).is_err());
    }

    #[test]
    fn scratch_exhaustion_returns_an_error() {
        // String resolvers need scratch even when the strings themselves are
        // empty. A large resolver array exceeds the fixed scratch allowance.
        let strings = vec![String::new(); MAX_SCRATCH];
        assert!(encode(&strings, MAX_ARCHIVE).is_err());
    }

    #[test]
    fn unix_path_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        #[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
        struct File {
            #[rkyv(with = UnixPath)]
            path: std::path::PathBuf,
        }
        let path = std::ffi::OsString::from_vec(b"/tmp/non-utf8-\xff".to_vec()).into();
        let bytes = encode(&File { path }, MAX_ARCHIVE).unwrap();
        let archived = access::<ArchivedFile>(&bytes).unwrap();
        assert_eq!(archived.path.as_slice(), b"/tmp/non-utf8-\xff");
        let file: File = rkyv::api::low::from_bytes::<File, Error>(&bytes).unwrap();
        assert_eq!(file.path.as_os_str().as_bytes(), b"/tmp/non-utf8-\xff");
    }
}

#[cfg(all(test, feature = "app-host"))]
#[allow(clippy::unwrap_used, reason = "Tests assert successful production message archives.")]
mod message_tests {
    use super::*;
    use crate::engine::{
        ArchivedRequest, Request, UpdateKind, UpdateStatus,
        reply::{ArchivedPreparedSession, PreparedSession},
    };
    use crate::store::{BuildSelector, Id, Session, UpdateAuthority};

    #[test]
    fn production_syscall_borrows_its_unchanged_data_payload() {
        let input = Request::Syscall {
            context: crate::engine::service_call::Context::Tool {
                handle: Id::from_bytes([1; 16]),
                incarnation: Id::from_bytes([2; 16]),
            },
            operation: 7,
            mutation: 3,
            call: Box::new(crate::runtime::host::DataCall {
                volume: [3; 16],
                operation: crate::runtime::host::DataOperation::ChannelCommand {
                    instance: [1; 16],
                    name: "test".into(),
                    action: 1,
                    data: vec![0, 127, 128, 255],
                },
            }),
        };
        let bytes = encode(&input, MAX_ARCHIVE).unwrap();
        let ArchivedRequest::Syscall { operation, mutation, call, .. } = access(&bytes).unwrap()
        else {
            panic!("syscall archive changed its operation");
        };
        assert_eq!(operation.to_native(), 7);
        assert_eq!(mutation.to_native(), 3);
        let crate::runtime::host::ArchivedDataOperation::ChannelCommand { data, .. } =
            &call.operation
        else {
            panic!("expected session data");
        };
        assert_eq!(data.as_slice(), [0, 127, 128, 255]);
        let decoded = rkyv::api::low::from_bytes::<Request, Error>(&bytes).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), serde_json::to_value(input).unwrap());
    }

    #[test]
    fn root_schema_and_initial_objects_round_trip_without_json_on_the_wire() {
        let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../guests/todo/playbit-volumes.json"
        )))
        .unwrap()
        .for_role(crate::package::AutomaticVolumeRole::Session);
        let request = Request::InitializeRoot {
            volume: Id::from_bytes([42; 16]),
            name: "TODO".into(),
            declaration: Box::new(declaration),
        };
        let bytes = encode(&request, MAX_ARCHIVE).unwrap();
        let decoded = rkyv::api::low::from_bytes::<Request, Error>(&bytes).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), serde_json::to_value(request).unwrap());
    }

    #[test]
    fn production_preparation_has_typed_metadata_without_guest_state() {
        let presentation = PreparedSession {
            session: Session {
                id: crate::identifier::encode(&[0xab; 32]),
                selector: BuildSelector::Exact { build_id: crate::identifier::encode(&[0xcd; 32]) },
                resolved_build_id: crate::identifier::encode(&[0xcd; 32]),
                root_volume: Id::from_bytes([1; 16]),
                shared_volume: None,
                app: crate::package::AppIdentity {
                    publisher: Id::from_bytes([3; 16]),
                    app_id: "test".into(),
                },

                creator: Id::from_bytes([3; 16]),
                update_authority: UpdateAuthority::Creator,
            },
            invitation: "playbit:test".into(),
            availability: "Local".into(),
            update: Some(UpdateStatus {
                kind: UpdateKind::Failed,
                build_id: Some(crate::identifier::encode(&[0xef; 32])),
                message: "Review access".into(),
            }),
        };
        let bytes = encode(&presentation, MAX_ARCHIVE).unwrap();
        let archived = access::<ArchivedPreparedSession>(&bytes).unwrap();
        assert_eq!(archived.update.as_ref().unwrap().message.as_str(), "Review access");
        assert!(bytes.len() < 1024);
        let decoded = rkyv::api::low::from_bytes::<PreparedSession, Error>(&bytes).unwrap();
        assert_eq!(decoded.session, presentation.session);
        assert_eq!(decoded.update, presentation.update);
    }
}
