//! Ordered, coalesced delivery fences and current mount snapshots. No guest data or scenes.
use super::session_instance::Context;
use crate::store::Id;

pub const MAX_CHANNELS: usize = 32;
pub const MAX_CHANNEL_BYTES: usize = 1024 * 1024;
pub const MAX_PROFILE_BYTES: usize = 16 * 1024 * 1024;

#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    serde::Serialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum StopReason {
    SelectedBuildChanged,
    ContextUnavailable,
}
impl StopReason {
    pub fn message(self) -> &'static str {
        match self {
            Self::SelectedBuildChanged => {
                "The app's selected build changed. Reload saved data to use the new build. Earlier accepted changes may have persisted; no input was repeated."
            }
            Self::ContextUnavailable => {
                "The app's data context is no longer available. Reload saved data after access or engine recovery. Earlier accepted changes may have persisted; no input was repeated."
            }
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Notice {
    #[serde(skip)]
    pub display_name: String,
    pub context: Context,
    pub sequence: u64,
    pub revision: Option<(Id, u64)>,
    /// Latest revision affecting captured volume dependencies or profile policy. The global
    /// revision still orders results without waking unrelated guests.
    pub data_revision: u64,
    /// Device-local mount edits are ordered independently of replicated data.
    pub mount_revision: u64,
    pub stop: Option<StopReason>,
    pub gap: bool,
    pub volumes: Vec<crate::runtime::host::AvailableVolume>,
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub(super) mod channel {
    use super::*;
    use crate::engine::{Result, ipc::codec, reply::Reply};
    use std::{
        io::{ErrorKind, Write},
        os::unix::net::UnixStream,
    };

    // Production mounts have at most one role tag, bounded labels and <=256 rows.
    // Retain one frame and one coalesced successor, including their owned DTOs.
    const NOTICE_BYTES: usize = 128 * 1024;
    const MOUNT_METADATA_BYTES: usize = 128 * 1024;
    pub(crate) struct Channel {
        pub context: Context,
        notice: Notice,
        bytes: Vec<u8>,
        written: usize,
        successor: Option<Notice>,

        observed: Option<(Id, u64)>,
        terminal: bool,
    }
    impl Channel {
        pub fn new(
            context: Context,
            revision: Option<(Id, u64)>,
            mount_revision: u64,
            volumes: Vec<crate::runtime::host::AvailableVolume>,
            display_name: String,
        ) -> Result<Self> {
            let notice = Notice {
                context,
                display_name: if revision.is_some() { display_name } else { String::new() },
                sequence: 1,
                revision,
                data_revision: revision.map_or(0, |(_, revision)| revision),
                mount_revision,
                stop: revision.is_none().then_some(StopReason::ContextUnavailable),
                gap: false,
                volumes,
            };
            let bytes = frame(&notice)?;
            let terminal = notice.stop.is_some();
            Ok(Self {
                context,

                bytes,
                notice,
                written: 0,
                successor: None,
                observed: revision,
                terminal,
            })
        }
        pub fn allocated(&self) -> usize {
            self.bytes.capacity()
                + metadata_bytes(&self.notice).expect("accepted mount allocation is bounded")
                + self.successor.as_ref().map_or(0, |notice| {
                    metadata_bytes(notice).expect("accepted successor allocation is bounded")
                })
                + std::mem::size_of::<Self>()
        }
        pub fn writing(&self) -> bool {
            !self.bytes.is_empty()
        }
        pub fn needs_update(
            &self,
            revision: Option<(Id, u64)>,
            mount_revision: u64,
            stop: Option<StopReason>,
        ) -> bool {
            !self.terminal
                && (revision != self.observed
                    || mount_revision != self.mount_revision()
                    || stop.is_some())
        }
        pub fn observed(&self) -> Option<(Id, u64)> {
            self.observed
        }
        pub fn data_revision(&self) -> u64 {
            self.successor.as_ref().unwrap_or(&self.notice).data_revision
        }
        pub fn mount_revision(&self) -> u64 {
            self.successor.as_ref().unwrap_or(&self.notice).mount_revision
        }
        pub fn mounts(&self) -> Vec<crate::runtime::host::AvailableVolume> {
            self.successor.as_ref().unwrap_or(&self.notice).volumes.clone()
        }
        pub fn observe(
            &mut self,
            revision: Option<(Id, u64)>,
            data_revision: u64,
            mount_revision: u64,
            stop: Option<StopReason>,
            volumes: Vec<crate::runtime::host::AvailableVolume>,
            display_name: String,
        ) -> Result<()> {
            if !self.needs_update(revision, mount_revision, stop) {
                return Ok(());
            }
            if data_revision < self.data_revision() || mount_revision < self.mount_revision() {
                return Err("Scoped data revision cannot move backward".into());
            }
            let sequence =
                self.notice.sequence.checked_add(1).ok_or("Guest event sequence exhausted")?;
            let gap = self.successor.is_some()
                || match (self.observed, revision) {
                    (Some((epoch, before)), Some((next_epoch, after))) => {
                        epoch != next_epoch
                            || (before != after && before.checked_add(1) != Some(after))
                    }
                    _ => true,
                };
            let stop =
                stop.or_else(|| revision.is_none().then_some(StopReason::ContextUnavailable));
            let notice = Notice {
                context: self.context,
                display_name: if stop.is_none() { display_name } else { String::new() },
                sequence,
                revision,
                data_revision,
                mount_revision,
                stop,
                gap,
                volumes,
            };
            validate_notice(&notice)?;
            self.observed = revision;
            self.terminal = notice.stop.is_some();
            if self.writing() {
                self.successor = Some(notice);
            } else {
                self.install(notice)?;
            }
            assert!(
                self.allocated() <= MAX_PROFILE_BYTES / MAX_CHANNELS,
                "mount snapshots fit each channel's profile share"
            );
            Ok(())
        }
        fn install(&mut self, notice: Notice) -> Result<()> {
            self.bytes = frame(&notice)?;
            self.notice = notice;
            self.written = 0;
            Ok(())
        }
        /// Return false only after the terminal notice is completely delivered.
        pub fn write(&mut self, stream: &mut UnixStream, budget: &mut usize) -> Result<bool> {
            let mut remaining = (*budget).min(64 * 1024);
            while remaining > 0 && self.writing() {
                let count = (self.bytes.len() - self.written).min(remaining);
                match stream.write(&self.bytes[self.written..self.written + count]) {
                    Ok(0) => return Err("Guest event channel closed during delivery".into()),
                    Ok(count) => {
                        self.written += count;
                        remaining -= count;
                        *budget -= count;
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(true),
                    Err(error) => return Err(error.into()),
                }
                if self.written == self.bytes.len() {
                    if self.notice.stop.is_some() {
                        return Ok(false);
                    }
                    self.bytes.clear();
                    if let Some(next) = self.successor.take() {
                        self.install(next)?;
                    }
                }
            }
            Ok(true)
        }
    }
    fn metadata_bytes(notice: &Notice) -> Option<usize> {
        let records = notice
            .volumes
            .capacity()
            .checked_mul(std::mem::size_of::<crate::runtime::host::AvailableVolume>())?
            .checked_add(notice.display_name.capacity())?;
        notice.volumes.iter().try_fold(records, |bytes, volume| {
            let tags = volume.tags.capacity().checked_mul(std::mem::size_of::<String>())?;
            let bytes = bytes.checked_add(volume.name.capacity())?.checked_add(tags)?;
            volume.tags.iter().try_fold(bytes, |bytes, tag| bytes.checked_add(tag.capacity()))
        })
    }
    fn validate_notice(notice: &Notice) -> Result<()> {
        if !notice.display_name.is_empty() {
            crate::store::account::validate_display_name(&notice.display_name)?;
        }
        if metadata_bytes(notice).is_none_or(|bytes| bytes > MOUNT_METADATA_BYTES) {
            return Err("Mount snapshot allocation exceeds its bounded reservation".into());
        }
        if notice.stop.is_none() {
            if notice.revision.is_none_or(|(_, revision)| notice.data_revision > revision) {
                return Err("Scoped data revision exceeds its ordered fence".into());
            }
            crate::runtime::syscall::validate_volumes(&notice.volumes)
                .map_err(|error| format!("Invalid mount notice: {error:?}"))?;
            if notice.volumes.iter().any(|volume| volume.tags.len() > 1) {
                return Err("Mount notices allow at most one bounded role tag per volume".into());
            }
        } else if !notice.volumes.is_empty() || !notice.display_name.is_empty() {
            return Err("A revoked context cannot retain guest metadata".into());
        }
        Ok(())
    }
    fn frame(notice: &Notice) -> Result<Vec<u8>> {
        validate_notice(notice)?;
        let archive = codec::encode(
            &Reply::InstanceEvents(notice.clone()),
            NOTICE_BYTES - codec::HEADER_LEN,
        )?;
        let mut bytes = Vec::with_capacity(codec::HEADER_LEN + archive.len());
        bytes.extend_from_slice(&codec::frame_header(archive.len())?);
        bytes.extend_from_slice(&archive);
        assert!(bytes.capacity() <= NOTICE_BYTES, "guest notices fit their frame reservation");
        Ok(bytes)
    }

    #[cfg(test)]
    #[allow(
        clippy::unwrap_used,
        reason = "Tests own isolated event sockets and exact frame boundaries."
    )]
    mod tests {
        use super::*;
        fn context() -> Context {
            Context {
                instance: Id::from_bytes([1; 16]),
                handle: Id::from_bytes([2; 16]),
                incarnation: Id::from_bytes([3; 16]),
                owner_generation: 1,
            }
        }
        fn volumes() -> Vec<crate::runtime::host::AvailableVolume> {
            vec![crate::runtime::host::AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: [4; 16],
                name: "Root".into(),
                tags: vec!["root".into()],
                rights: crate::runtime::host::VolumeRights::ALL,
                is_root: true,
                role: 0,
            }]
        }
        fn receive(stream: &mut UnixStream) -> Notice {
            let bytes = crate::engine::ipc::read(stream).unwrap();
            let Reply::InstanceEvents(notice) =
                crate::engine::ipc::Response::new(bytes).unwrap().into_owned().unwrap()
            else {
                panic!("event notice");
            };
            notice
        }
        #[test]
        fn partial_delivery_keeps_its_frame_and_coalesces_a_bounded_successor() {
            let (mut server, mut client) = UnixStream::pair().unwrap();
            let epoch = Id::from_bytes([4; 16]);
            let mut channel =
                Channel::new(context(), Some((epoch, 0)), 0, volumes(), String::new()).unwrap();
            assert!(channel.write(&mut server, &mut 1).unwrap());
            for revision in 1..100 {
                channel
                    .observe(Some((epoch, revision)), revision, 0, None, volumes(), String::new())
                    .unwrap();
            }
            assert!(channel.allocated() < 4096);
            assert!(channel.write(&mut server, &mut (64 * 1024)).unwrap());
            let first = receive(&mut client);
            let second = receive(&mut client);
            assert_eq!((first.sequence, first.revision, first.gap), (1, Some((epoch, 0)), false));
            assert_eq!(
                (second.sequence, second.revision, second.gap),
                (2, Some((epoch, 99)), true)
            );
            assert!(!channel.writing());
        }
        #[test]
        fn coalescing_an_unrelated_fence_retains_the_mounted_change_revision() {
            let (mut server, mut client) = UnixStream::pair().unwrap();
            let epoch = Id::from_bytes([4; 16]);
            let mut channel =
                Channel::new(context(), Some((epoch, 0)), 0, volumes(), String::new()).unwrap();
            channel.write(&mut server, &mut 1).unwrap();
            channel.observe(Some((epoch, 1)), 1, 0, None, volumes(), String::new()).unwrap();
            channel.observe(Some((epoch, 2)), 1, 0, None, volumes(), String::new()).unwrap();
            channel.write(&mut server, &mut (64 * 1024)).unwrap();
            let first = receive(&mut client);
            let last = receive(&mut client);
            assert_eq!(first.data_revision, 0);
            assert_eq!(last.revision, Some((epoch, 2)));
            assert_eq!(last.data_revision, 1);
            assert!(last.gap);
            assert!(
                channel.observe(Some((epoch, 3)), 0, 0, None, volumes(), String::new()).is_err()
            );
        }
        #[test]
        fn mount_edits_deliver_without_a_replicated_data_change() {
            let (mut server, mut client) = UnixStream::pair().unwrap();
            let revision = Some((Id::from_bytes([4; 16]), 7));
            let mut channel =
                Channel::new(context(), revision, 0, volumes(), String::new()).unwrap();
            channel.write(&mut server, &mut (64 * 1024)).unwrap();
            let first = receive(&mut client);
            assert!(channel.needs_update(revision, 1, None));
            let mut mounts = volumes();
            mounts[0].name = "Renamed".into();
            channel.observe(revision, 7, 1, None, mounts.clone(), String::new()).unwrap();
            channel.write(&mut server, &mut (64 * 1024)).unwrap();
            let next = receive(&mut client);
            assert_eq!(next.revision, first.revision);
            assert_eq!(next.data_revision, first.data_revision);
            assert_eq!(next.mount_revision, 1);
            assert_eq!(next.sequence, first.sequence + 1);
            assert_eq!(next.volumes, mounts);
            assert!(!next.gap);
            assert!(channel.observe(revision, 7, 0, None, volumes(), String::new()).is_err());
        }
        #[test]
        fn retired_context_delivers_an_ordered_terminal_before_closure() {
            let (mut server, mut client) = UnixStream::pair().unwrap();
            let epoch = Id::from_bytes([4; 16]);
            let mut channel =
                Channel::new(context(), Some((epoch, 0)), 0, volumes(), String::new()).unwrap();
            channel
                .observe(
                    Some((epoch, 1)),
                    1,
                    0,
                    Some(StopReason::ContextUnavailable),
                    vec![],
                    String::new(),
                )
                .unwrap();
            assert!(!channel.write(&mut server, &mut (64 * 1024)).unwrap());
            assert!(receive(&mut client).stop.is_none());
            let terminal = receive(&mut client);
            assert_eq!(terminal.sequence, 2);
            assert_eq!(terminal.stop, Some(StopReason::ContextUnavailable));
            assert!(terminal.volumes.is_empty());
        }
        #[test]
        fn maximum_mount_snapshots_keep_two_versions_inside_profile_credit() {
            let mut mounts = Vec::new();
            for index in 0..playbit_api::abi::syscall::MAX_VOLUMES {
                let mut id = [0; 16];
                id[..2].copy_from_slice(&(index as u16).to_le_bytes());
                mounts.push(crate::runtime::host::AvailableVolume {
                    backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                    source_epoch: [0; 16],
                    id,
                    name: "v".repeat(248),
                    tags: vec![
                        match index {
                            0 => "root",
                            1 => "session",
                            _ => "personal",
                        }
                        .into(),
                    ],
                    rights: crate::runtime::host::VolumeRights::ALL,
                    is_root: index == 0,
                    role: 0,
                });
            }
            let epoch = Id::from_bytes([4; 16]);
            let (mut server, mut client) = UnixStream::pair().unwrap();
            let name = "a".repeat(playbit_api::abi::environment::DISPLAY_NAME_BYTES);
            let mut channel =
                Channel::new(context(), Some((epoch, 0)), 0, mounts.clone(), name.clone()).unwrap();
            channel.write(&mut server, &mut 1).unwrap();
            mounts[2].rights = crate::runtime::host::VolumeRights::READ;
            channel.observe(Some((epoch, 1)), 1, 0, None, mounts.clone(), name.clone()).unwrap();
            mounts.remove(3);
            channel.observe(Some((epoch, 2)), 2, 0, None, mounts.clone(), "Zoë".into()).unwrap();
            assert!(channel.allocated() * MAX_CHANNELS <= MAX_PROFILE_BYTES);
            let receiver = std::thread::spawn(move || (receive(&mut client), receive(&mut client)));
            while channel.writing() {
                channel.write(&mut server, &mut (64 * 1024)).unwrap();
            }
            let (first, last) = receiver.join().unwrap();
            assert_eq!(first.display_name, name);
            assert_eq!(last.display_name, "Zoë");
            assert_eq!(first.volumes.len(), playbit_api::abi::syscall::MAX_VOLUMES);
            assert_eq!(last.volumes, mounts);
            assert!(last.gap);
        }
        #[test]
        fn oversized_successor_is_rejected_before_coalesced_admission() {
            let epoch = Id::from_bytes([4; 16]);
            let (mut server, _client) = UnixStream::pair().unwrap();
            let mut channel =
                Channel::new(context(), Some((epoch, 0)), 0, volumes(), String::new()).unwrap();
            channel.write(&mut server, &mut 1).unwrap();
            let mut oversized = volumes();
            oversized[0].name.reserve(MOUNT_METADATA_BYTES);
            assert!(
                channel.observe(Some((epoch, 1)), 1, 0, None, oversized, String::new()).is_err()
            );
            assert!(channel.successor.is_none());
            assert_eq!(channel.observed, Some((epoch, 0)));
            channel.observe(Some((epoch, 1)), 1, 0, None, volumes(), String::new()).unwrap();
            assert_eq!(channel.successor.as_ref().unwrap().sequence, 2);
        }
    }
}
