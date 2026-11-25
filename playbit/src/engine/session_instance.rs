//! Session-instance launch and service bindings. A session is shared; this ID is
//! one independent execution and its device-local host-permission configuration.
use crate::store::{Id, PersonalVolumeGrant};

pub(crate) const MAX_PROCESSES: usize = 32;

#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum StartupPhase {
    PreparingApp,
    CreatingSession,
    PreparingApplication,
    Starting,
    Loading,
    Instantiating,
    OpeningWindow,
    Ready,
    Failed,
}
impl StartupPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::PreparingApp | Self::Loading => "Preparing app…",
            Self::CreatingSession => "Creating session…",
            Self::PreparingApplication => "Preparing application…",
            Self::Starting => "Starting app…",
            Self::Instantiating | Self::OpeningWindow => "Opening window…",
            Self::Ready => "Ready",
            Self::Failed => "The app could not finish opening. Inspect the session log.",
        }
    }
}
// At most 128 live reservations/owners plus 32 terminal instances.
pub const MAX_STARTUP_PROGRESS: usize = 160;

#[derive(Clone, Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct StartupProgress {
    pub incarnation: Id,
    pub instance: Id,
    pub session: String,
    pub owner_generation: u64,
    pub presentation: u64,
    pub phase: StartupPhase,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum State {
    Reserved,
    Owned,
    Unknown,
    Draining,
    Exited,
    LaunchFailed,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct SessionInstance {
    pub id: Id,
    pub launch_request: Id,
    pub account: Id,
    pub session: String,
    pub build: String,
    pub root: Id,

    pub extra_volumes: Vec<PersonalVolumeGrant>,
    pub host_mounts: Box<[crate::engine::host_mount::HostMount]>,
    /// GUI configurations restore available roots; unavailable paths grant nothing.
    pub restore_host_mounts: bool,
    pub owner_generation: u64,
    pub state: State,
}
/// The registry allocates this time-prefixed ID when it admits the launch reservation.
pub fn reserved_unix_secs(id: Id) -> u32 {
    u32::from_be_bytes(id.as_bytes()[..4].try_into().expect("four-byte ID prefix"))
}

impl SessionInstance {
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(crate) fn delivery_volumes(&self) -> Vec<Id> {
        let mut volumes = Vec::with_capacity(2 + self.extra_volumes.len());
        volumes.extend([self.root]);
        volumes.extend(self.extra_volumes.iter().map(|grant| grant.volume));
        volumes
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct Context {
    pub instance: Id,
    pub handle: Id,
    pub incarnation: Id,
    pub owner_generation: u64,
}

#[derive(
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct ReconstructionTarget {
    pub incarnation: Id,
    pub build: String,
}

#[derive(Clone, Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Admission {
    #[serde(skip)]
    pub display_name: String,
    /// Sent only to the authenticated owner in the private, build-matched IPC archive.
    #[serde(skip)]
    pub(crate) session_reference: crate::store::SessionReference,
    pub instance: SessionInstance,
    pub context: Context,
    pub session: crate::store::Session,
    pub volumes: Vec<crate::runtime::host::AvailableVolume>,
}

#[derive(
    Clone, Debug, Eq, PartialEq, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct ProcessIdentity {
    #[serde(serialize_with = "serialize_boot")]
    pub(super) boot: [u8; 36],
    pub(super) pid: i32,
    pub(super) token: [u32; 8],
    pub(super) started: u64,
}
fn serialize_boot<S: serde::Serializer>(
    boot: &[u8; 36],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_bytes(boot)
}
impl ProcessIdentity {
    pub fn pid(&self) -> u32 {
        assert!(self.pid > 1, "captured owner has a real process ID");
        self.pid as u32
    }
}

#[derive(Clone, Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct LaunchOwner {
    pub instance: SessionInstance,
    pub process: Option<ProcessIdentity>,
    pub presentation: u64,
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub(super) mod registry {
    use super::*;
    use crate::engine::{Result, ipc::codec, process_owner::Owner};
    use std::{
        collections::BTreeMap,
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::OwnedFd,
            unix::{fs::OpenOptionsExt, net::UnixStream},
        },
        path::{Path, PathBuf},
    };

    pub(crate) const MAX_LIVE: usize = 128;
    const MAX_RECORDS: usize = 1024;
    const MAX_BYTES: usize = 1024 * 1024;
    // Reservations leave room for all 32 live owners to acquire their fixed
    // identity/context records. Permission drafts cannot consume that room.
    const OWNER_RESERVATION: usize = MAX_PROCESSES
        * (std::mem::size_of::<ProcessIdentity>() + std::mem::size_of::<Context>() + 64);
    const MAGIC: &[u8; 16] = b"PB-INSTANCE-0001";

    #[derive(Clone, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
    struct Record {
        instance: SessionInstance,
        created: Id,
        owner: Option<ProcessIdentity>,
        context: Option<Context>,
        saved: bool,
        restored_from: Option<Id>,
        mount_revision: u64,
    }
    #[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
    struct Records(Vec<Record>);

    struct Startup {
        context: Context,
        presentation: u64,
        phase: StartupPhase,
    }

    pub(crate) struct Registry {
        path: PathBuf,
        incarnation: Id,
        records: Vec<Record>,
        owners: BTreeMap<Id, Owner>,
        failed: bool,
        startup: BTreeMap<Id, Startup>,
    }

    fn terminal(state: State) -> bool {
        matches!(state, State::Exited | State::LaunchFailed)
    }
    fn live_context(state: State, context: Option<Context>, incarnation: Id) -> Option<Context> {
        context.filter(|context| state == State::Owned && context.incarnation == incarnation)
    }
    fn can_reject_claim(state: State, has_owner: bool, has_context: bool) -> bool {
        state == State::Reserved && !has_owner && !has_context
    }
    fn next_generation(generation: u64) -> Option<u64> {
        generation.checked_add(1)
    }
    fn reconstruction_state(
        state: State,
        restarted: bool,
        has_context: bool,
        context_matches: bool,
        build_changed: bool,
    ) -> bool {
        if restarted {
            state == State::Unknown && !has_context
        } else {
            state == State::Owned && context_matches && build_changed
        }
    }
    fn valid(instance: &SessionInstance) -> bool {
        instance.id != Id::from_bytes([0; 16])
            && instance.launch_request != Id::from_bytes([0; 16])
            && instance.account != Id::from_bytes([0; 16])
            && instance.root != Id::from_bytes([0; 16])
            && crate::identifier::valid(&instance.session, 32)
            && crate::identifier::valid(&instance.build, 32)
            && instance.extra_volumes.len() + instance.host_mounts.len()
                < playbit_api::abi::syscall::MAX_VOLUMES
            && super::super::host_mount::validate(&instance.host_mounts).is_ok()
            && instance.extra_volumes.iter().enumerate().all(|(index, volume)| {
                volume.volume != instance.root
                    && !instance.extra_volumes[..index]
                        .iter()
                        .any(|previous| previous.volume == volume.volume)
            })
    }

    impl Registry {
        pub fn open(profile: &Path, incarnation: Id) -> Result<Self> {
            let path = profile.join("session-instances");
            let mut records =
                match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&path) {
                    Ok(mut file) => {
                        let size = usize::try_from(file.metadata()?.len())?;
                        if !(MAGIC.len()..=MAX_BYTES + MAGIC.len()).contains(&size) {
                            return Err("Invalid session-instance registry size".into());
                        }
                        let mut magic = [0; 16];
                        file.read_exact(&mut magic)?;
                        if magic != *MAGIC {
                            return Err("Invalid session-instance registry version".into());
                        }
                        let mut bytes = codec::Bytes::with_capacity(size - MAGIC.len());
                        bytes.resize(size - MAGIC.len(), 0);
                        file.read_exact(&mut bytes)?;
                        let archived = codec::access::<ArchivedRecords>(&bytes)?;
                        if archived.0.len() > MAX_RECORDS {
                            return Err("Session-instance registry exceeds record limit".into());
                        }
                        // Check variable allocation bounds before deserializing, just
                        // as IPC admission checks its owned-memory budget.
                        let mut owned = archived
                            .0
                            .len()
                            .checked_mul(std::mem::size_of::<Record>())
                            .ok_or("Registry allocation overflow")?;
                        for record in archived.0.iter() {
                            if !crate::identifier::valid(&record.instance.session, 32)
                                || !crate::identifier::valid(&record.instance.build, 32)
                                || record.instance.extra_volumes.len()
                                    > playbit_api::abi::syscall::MAX_VOLUMES - 1
                                || record.instance.host_mounts.len()
                                    > super::super::host_mount::MAX_MOUNTS
                            {
                                return Err("Invalid session-instance registry record".into());
                            }
                            owned = owned
                                .checked_add(128)
                                .and_then(|bytes| {
                                    record
                                        .instance
                                        .extra_volumes
                                        .len()
                                        .checked_mul(std::mem::size_of::<PersonalVolumeGrant>())
                                        .and_then(|extra| bytes.checked_add(extra))
                                })
                                .filter(|bytes| *bytes <= MAX_BYTES)
                                .ok_or("Session-instance registry exceeds owned-memory limit")?;
                            for mount in record.instance.host_mounts.iter() {
                                if mount.path.len() > super::super::host_mount::MAX_PATH_BYTES
                                    || mount.label.len() > super::super::host_mount::MAX_LABEL_BYTES
                                {
                                    return Err(
                                        "Invalid host mount in session-instance registry".into()
                                    );
                                }
                                owned = owned
                                    .checked_add(
                                        std::mem::size_of::<super::super::host_mount::HostMount>()
                                            + mount.path.len()
                                            + mount.label.len(),
                                    )
                                    .filter(|bytes| *bytes <= MAX_BYTES)
                                    .ok_or(
                                        "Session-instance registry exceeds owned-memory limit",
                                    )?;
                            }
                        }
                        rkyv::api::low::deserialize::<Records, rkyv::rancor::Error>(archived)?.0
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                    Err(error) => return Err(error.into()),
                };
            for (index, record) in records.iter().enumerate() {
                if !valid(&record.instance)
                    || records[..index].iter().any(|previous| {
                        previous.instance.id == record.instance.id
                            || previous.instance.launch_request == record.instance.launch_request
                    })
                {
                    return Err("Invalid session-instance registry binding".into());
                }
            }
            let mut owners = BTreeMap::new();
            for record in &mut records {
                record.context = None;
                if terminal(record.instance.state) {
                    continue;
                }
                if let Some(identity) = &record.owner {
                    match Owner::recover(identity) {
                        Ok(Some(owner)) => {
                            owners.insert(record.instance.id, owner);
                            record.instance.state = State::Unknown;
                        }
                        Ok(None) => record.instance.state = State::Exited,
                        Err(error) => {
                            crate::diagnostic!(
                                "Session instance {} needs ownership recovery: {error}",
                                record.instance.id
                            );
                            record.instance.state = State::Unknown;
                        }
                    }
                }
            }
            // Request IDs are scoped by incarnation. Keep current history for
            // lost-reply lookup; only confirmed terminal records from a previous
            // incarnation can be pruned. Unknown launches never expire.
            records.retain(|record| {
                record.saved || !terminal(record.instance.state) || record.created == incarnation
            });
            let mut registry = Self {
                path,
                incarnation,
                records,
                owners,
                failed: false,
                startup: BTreeMap::new(),
            };
            if !registry.records.is_empty() || registry.path.exists() {
                registry.persist()?;
            }
            Ok(registry)
        }
        fn available(&self) -> Result<()> {
            if self.failed {
                Err("Session-instance registry needs recovery; \
                     no new ownership or data work was admitted"
                    .into())
            } else {
                Ok(())
            }
        }
        fn persist(&mut self) -> Result<()> {
            let bytes = codec::encode(&Records(self.records.clone()), MAX_BYTES)?;
            self.persist_bytes(&bytes)
        }
        fn persist_candidate(&mut self, records: Vec<Record>) -> Result<()> {
            // Serialization/size rejection is definitive and leaves every live
            // binding intact. Only disk failure after admission is uncertain.
            let candidate = Records(records);
            let bytes = codec::encode(&candidate, MAX_BYTES - OWNER_RESERVATION)?;
            self.persist_bytes(&bytes)?;
            self.records = candidate.0;
            Ok(())
        }
        fn persist_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            let result = (|| -> Result<()> {
                let temp = self.path.with_extension("pending");
                let mut file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&temp)?;
                file.write_all(MAGIC)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                fs::rename(&temp, &self.path)?;
                File::open(self.path.parent().expect("profile is registry parent"))?.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                self.failed = true;
            }
            result
        }
        pub fn incarnation(&self) -> Id {
            self.incarnation
        }
        pub fn descriptor(&self, instance: Id) -> Result<SessionInstance> {
            self.available()?;
            self.records
                .iter()
                .find(|record| record.instance.id == instance)
                .map(|record| record.instance.clone())
                .ok_or_else(|| "Unknown session instance".into())
        }
        pub fn inspect(&self, request: Id) -> Result<Option<SessionInstance>> {
            self.available()?;
            Ok(self
                .records
                .iter()
                .find(|record| record.instance.launch_request == request)
                .map(|record| record.instance.clone()))
        }
        /// Bounded metadata observers follow claimed owners and unknown survivors.
        /// An unclaimed reservation has no running app and owns no network watch.
        pub fn active_sessions(&self) -> Vec<String> {
            let mut sessions = self
                .records
                .iter()
                .filter(|record| {
                    matches!(record.instance.state, State::Owned | State::Unknown | State::Draining)
                })
                .map(|record| record.instance.session.clone())
                .collect::<Vec<_>>();
            assert!(sessions.len() <= MAX_LIVE, "live registry admission bounds observers");
            sessions.sort_unstable();
            sessions.dedup();
            sessions
        }
        /// Sample only registry state. Unknown survivors remain distinct from
        /// owned execution; this does not probe processes or claim VM activity.
        pub(crate) fn trace_counts(&self) -> [u64; 4] {
            let mut counts = [0; 4];
            for record in &self.records {
                let index = match record.instance.state {
                    State::Owned => 0,
                    State::Reserved => 1,
                    State::Unknown => 2,
                    State::Draining => 3,
                    State::Exited | State::LaunchFailed => continue,
                };
                counts[index] += 1;
            }
            assert!(counts.iter().sum::<u64>() <= MAX_LIVE as u64);
            counts
        }
        pub fn begin_show(&mut self, instance: Id, owner_generation: u64) -> Result<u64> {
            self.available()?;
            let context = self
                .records
                .iter()
                .find(|record| {
                    record.instance.id == instance
                        && record.instance.owner_generation == owner_generation
                        && record.instance.state == State::Owned
                })
                .and_then(|record| record.context)
                .ok_or("The app owner changed before Show; no action was repeated")?;
            // A previous Ready can still be in flight. Every accepted Show
            // needs a fresh generation, including during initial startup.
            if self.startup.get(&instance).is_none_or(|startup| startup.context != context) {
                self.set_startup(context, None, StartupPhase::Loading)?;
            }
            let startup = self.startup.get_mut(&instance).expect("startup owner initialized");
            startup.presentation = startup
                .presentation
                .checked_add(1)
                .ok_or("App presentation generation exhausted")?;
            startup.phase = StartupPhase::OpeningWindow;
            Ok(startup.presentation)
        }
        pub fn set_startup(
            &mut self,
            context: Context,
            presentation: Option<u64>,
            phase: StartupPhase,
        ) -> Result<()> {
            if !self.owns_context(context) {
                return Err("Startup context has lost ownership".into());
            }
            if !matches!(
                phase,
                StartupPhase::Loading
                    | StartupPhase::Instantiating
                    | StartupPhase::OpeningWindow
                    | StartupPhase::Ready
                    | StartupPhase::Failed
            ) {
                return Err("Invalid guest startup phase".into());
            }
            if presentation.is_none() && phase == StartupPhase::Ready {
                return Err("Ready requires a native presentation generation".into());
            }
            let value = self.startup_owner(context)?;
            if let Some(presentation) = presentation {
                if presentation != value.presentation {
                    return Err("App presentation generation changed".into());
                }
            } else if value.presentation != 0 && phase != StartupPhase::Failed {
                // Initial worker setup continues independently of Show. Its
                // progress cannot regress or acknowledge a native presentation.
                return Ok(());
            }
            if phase < value.phase {
                return Err("Guest startup phase moved backwards".into());
            }
            value.phase = phase;
            Ok(())
        }
        fn startup_owner(&mut self, context: Context) -> Result<&mut Startup> {
            let history: std::collections::BTreeSet<_> = self
                .records
                .iter()
                .rev()
                .filter(|record| terminal(record.instance.state))
                .take(MAX_PROCESSES)
                .map(|record| record.instance.id)
                .collect();
            self.startup.retain(|id, startup| {
                let prior = startup.context;
                self.records.iter().any(|record| {
                    record.instance.id == *id
                        && record.instance.owner_generation == prior.owner_generation
                        && ((record.context == Some(prior)
                            && record.instance.state == State::Owned)
                            || history.contains(id))
                })
            });
            // Keep the last phase for 32 live owners and 32 terminal instances.
            if self.startup.len() >= 2 * MAX_PROCESSES
                && !self.startup.contains_key(&context.instance)
            {
                return Err("Guest startup observer capacity exceeded".into());
            }
            let value = self.startup.entry(context.instance).or_insert(Startup {
                context,
                presentation: 0,
                phase: StartupPhase::Loading,
            });
            if value.context != context {
                *value = Startup { context, presentation: 0, phase: StartupPhase::Loading };
            }
            Ok(value)
        }
        pub fn startup_progress(&self) -> Vec<StartupProgress> {
            let mut terminal_count = 0;
            self.records
                .iter()
                .rev()
                .filter(|record| {
                    if terminal(record.instance.state) {
                        terminal_count += 1;
                        terminal_count <= MAX_PROCESSES
                    } else {
                        true
                    }
                })
                .map(|record| {
                    let phase = match record.instance.state {
                        State::Reserved => StartupPhase::Starting,
                        State::Owned => self
                            .startup
                            .get(&record.instance.id)
                            .filter(|startup| record.context == Some(startup.context))
                            .map_or(StartupPhase::Loading, |startup| startup.phase),
                        State::Exited
                            if self.startup.get(&record.instance.id).is_some_and(|startup| {
                                startup.context.incarnation == self.incarnation
                                    && startup.context.owner_generation
                                        == record.instance.owner_generation
                                    && startup.phase == StartupPhase::Ready
                            }) =>
                        {
                            StartupPhase::Ready
                        }
                        _ => StartupPhase::Failed,
                    };
                    StartupProgress {
                        incarnation: self.incarnation,
                        instance: record.instance.id,
                        session: record.instance.session.clone(),
                        owner_generation: if record.owner.is_none()
                            && matches!(
                                record.instance.state,
                                State::Reserved | State::LaunchFailed
                            ) {
                            record
                                .instance
                                .owner_generation
                                .checked_add(1)
                                .expect("reserved instance can be claimed")
                        } else {
                            record.instance.owner_generation
                        },
                        presentation: self
                            .startup
                            .get(&record.instance.id)
                            .filter(|startup| {
                                startup.context.incarnation == self.incarnation
                                    && startup.context.owner_generation
                                        == record.instance.owner_generation
                            })
                            .map_or(0, |startup| startup.presentation),
                        phase,
                    }
                })
                .collect()
        }
        pub fn list(&self, session: &str) -> Result<Vec<LaunchOwner>> {
            self.available()?;
            if !crate::identifier::valid(session, 32) {
                return Err("Invalid launch session identity".into());
            }
            self.records
                .iter()
                .filter(|record| {
                    record.instance.session == session && !terminal(record.instance.state)
                })
                .map(|record| {
                    let process = record
                        .owner
                        .as_ref()
                        .map(|identity| {
                            Owner::recover(identity).map(|owner| owner.map(|owner| owner.identity))
                        })
                        .transpose()?
                        .flatten();
                    Ok(self.launch_owner(record, process))
                })
                .collect()
        }
        /// Snapshot candidates on the owner thread. The removal worker checks
        /// their session and kernel process identities before sending Quit.
        pub fn writer_candidates(&self, account: Id) -> Result<Vec<LaunchOwner>> {
            self.available()?;
            Ok(self
                .records
                .iter()
                .filter(|record| {
                    record.instance.account == account
                        && !terminal(record.instance.state)
                        && record.owner.is_some()
                })
                .map(|record| self.launch_owner(record, record.owner.clone()))
                .collect())
        }
        pub fn owns_session(&self, session: &str, account: Id, peer: &UnixStream) -> Result<bool> {
            self.available()?;
            let owner = Owner::capture(peer)?;
            Ok(self.records.iter().any(|record| {
                record.instance.session == session
                    && record.instance.account == account
                    && matches!(record.instance.state, State::Owned | State::Unknown)
                    && record.owner.as_ref() == Some(&owner.identity)
            }))
        }
        fn launch_owner(&self, record: &Record, process: Option<ProcessIdentity>) -> LaunchOwner {
            LaunchOwner {
                instance: record.instance.clone(),
                process,
                presentation: self
                    .startup
                    .get(&record.instance.id)
                    .filter(|startup| record.context == Some(startup.context))
                    .map_or(0, |startup| startup.presentation),
            }
        }
        /// Saved GUI configurations are local metadata, never shared session data.
        pub fn configurations(&self, session: &str, account: Id) -> Result<Vec<Id>> {
            self.available()?;
            if !crate::identifier::valid(session, 32) {
                return Err("Invalid configuration session identity".into());
            }
            Ok(self
                .records
                .iter()
                .filter(|record| {
                    record.saved
                        && record.instance.session == session
                        && record.instance.account == account
                })
                .map(|record| record.instance.id)
                .collect())
        }
        pub fn configuration(&self, instance: Id, account: Id) -> Result<SessionInstance> {
            self.available()?;
            self.records
                .iter()
                .find(|record| {
                    record.saved
                        && record.instance.id == instance
                        && record.instance.account == account
                })
                .map(|record| record.instance.clone())
                .ok_or_else(|| "Unknown saved instance".into())
        }
        pub fn mount_revision(&self, instance: Id) -> u64 {
            self.records
                .iter()
                .find(|record| record.instance.id == instance)
                .map_or(0, |record| record.mount_revision)
        }
        pub fn set_mounts(
            &mut self,
            instance: Id,
            expected: &[crate::engine::host_mount::HostMount],
            mut mounts: Box<[crate::engine::host_mount::HostMount]>,
        ) -> Result<SessionInstance> {
            self.available()?;
            super::super::host_mount::validate(&mounts)?;
            let mut candidate = self.records.clone();
            let record = candidate
                .iter_mut()
                .find(|record| record.instance.id == instance)
                .ok_or("Unknown session instance")?;
            if !record.saved || record.instance.host_mounts.as_ref() != expected {
                return Err("Folder configuration changed; reload it before saving".into());
            }
            if record.instance.extra_volumes.len() + mounts.len()
                > playbit_api::abi::syscall::MAX_VOLUMES - 1
            {
                return Err("Too many mounts for this instance".into());
            }
            for mount in mounts.iter_mut() {
                if expected.iter().any(|old| {
                    old.id == mount.id && (old.path != mount.path || old.write != mount.write)
                }) {
                    return Err(
                        "A changed folder or permission requires a fresh grant identity".into()
                    );
                }
                if !expected.iter().any(|old| old.id == mount.id) {
                    // Callers cannot revive an old grant by reusing a removed ID.
                    mount.id = Id::generate()?;
                }
            }
            let revision =
                record.mount_revision.checked_add(1).ok_or("Folder revision exhausted")?;
            record.instance.host_mounts = mounts;
            record.mount_revision = revision;
            let instance = record.instance.clone();
            self.persist_candidate(candidate)?;
            Ok(instance)
        }
        pub fn restore(
            &mut self,
            incarnation: Id,
            request: Id,
            prior: Id,
            build: String,
            extra_volumes: Vec<PersonalVolumeGrant>,
        ) -> Result<SessionInstance> {
            self.available()?;
            if let Some(record) =
                self.records.iter().find(|record| record.instance.launch_request == request)
            {
                if record.restored_from != Some(prior) {
                    return Err("Launch request belongs to another configuration".into());
                }
                return Ok(record.instance.clone());
            }
            let old = self
                .records
                .iter()
                .find(|record| record.instance.id == prior)
                .ok_or("Unknown saved instance")?;
            if !old.saved || !terminal(old.instance.state) {
                return Err("Only a confirmed stopped GUI instance can be restored".into());
            }
            let mut instance = old.instance.clone();
            instance.launch_request = request;
            instance.build = build;
            instance.extra_volumes = extra_volumes;
            for mount in instance.host_mounts.iter_mut() {
                mount.id = Id::generate()?;
            }
            self.reserve_from(incarnation, instance, Some(prior))
        }
        pub fn reserve(
            &mut self,
            incarnation: Id,
            instance: SessionInstance,
        ) -> Result<SessionInstance> {
            self.reserve_from(incarnation, instance, None)
        }
        fn reserve_from(
            &mut self,
            incarnation: Id,
            mut instance: SessionInstance,
            restored_from: Option<Id>,
        ) -> Result<SessionInstance> {
            self.available()?;
            if let Some(previous) = self
                .records
                .iter()
                .find(|record| record.instance.launch_request == instance.launch_request)
            {
                let binding = &previous.instance;
                if binding.account != instance.account
                    || binding.session != instance.session
                    || binding.build != instance.build
                    || binding.root != instance.root
                    || binding.extra_volumes != instance.extra_volumes
                    || binding.host_mounts != instance.host_mounts
                    || binding.restore_host_mounts != instance.restore_host_mounts
                {
                    return Err(
                        "Launch request is already bound to another session-instance contract"
                            .into(),
                    );
                }
                return Ok(binding.clone());
            }
            if incarnation != self.incarnation {
                return Err(
                    "Launch incarnation changed; inspect the original request before choosing \
                     a new launch"
                        .into(),
                );
            }
            if self.records.len() >= MAX_RECORDS
                || self.records.iter().filter(|record| !terminal(record.instance.state)).count()
                    >= MAX_LIVE
            {
                return Err(
                    "Session-instance launch registry is full; this launch did not start".into()
                );
            }
            if self.records.iter().filter(|record| !terminal(record.instance.state)).count()
                >= MAX_PROCESSES
            {
                return Err("GUI process limit reached, including launch reservations; \
                    this launch did not start"
                    .into());
            }
            instance.id = Id::generate()?;
            instance.owner_generation = 0;
            instance.state = State::Reserved;
            if !valid(&instance) {
                return Err("Invalid session-instance launch contract".into());
            }
            let mut candidate = self.records.clone();
            if let Some(prior) = restored_from {
                let record = candidate
                    .iter_mut()
                    .find(|record| record.instance.id == prior)
                    .expect("validated restore source");
                record.saved = false;
            }
            candidate.push(Record {
                saved: instance.restore_host_mounts,
                restored_from,
                mount_revision: 0,
                instance: instance.clone(),
                created: incarnation,
                owner: None,
                context: None,
            });
            self.persist_candidate(candidate)?;
            Ok(instance)
        }
        pub fn claim(
            &mut self,
            instance: Id,
            stream: &UnixStream,
        ) -> Result<(SessionInstance, Context)> {
            self.available()?;
            let owner = Owner::capture(stream)?;
            let index = self
                .records
                .iter()
                .position(|record| record.instance.id == instance)
                .ok_or("Session instance is not reserved")?;
            let record = &self.records[index];
            if terminal(record.instance.state) || record.instance.state == State::Draining {
                return Err("Session instance has stopped accepting ownership".into());
            }
            if record.owner.as_ref().is_some_and(|identity| *identity != owner.identity) {
                return Err("Session instance already has another process owner".into());
            }
            if self.records.iter().enumerate().any(|(other, record)| {
                other != index
                    && !terminal(record.instance.state)
                    && record.owner.as_ref() == Some(&owner.identity)
            }) {
                return Err("This GUI process already owns a session instance".into());
            }
            if !self.owners.contains_key(&instance) && self.owners.len() >= MAX_PROCESSES {
                return Err("GUI process limit reached; claim did not start".into());
            }
            if let Some(context) = record.context {
                return Ok((record.instance.clone(), context));
            }
            let generation = next_generation(record.instance.owner_generation)
                .ok_or("Session-instance owner generation exhausted")?;
            let context = Context {
                instance,
                handle: Id::generate()?,
                incarnation: self.incarnation,
                owner_generation: generation,
            };
            let record = &mut self.records[index];
            record.instance.owner_generation = generation;
            record.instance.state = State::Owned;
            record.owner = Some(owner.identity.clone());
            record.context = Some(context);
            let descriptor = record.instance.clone();
            self.owners.insert(instance, owner);
            self.persist()?;
            Ok((descriptor, context))
        }
        pub fn authorize(&self, context: Context, stream: &UnixStream) -> Result<SessionInstance> {
            self.available()?;
            let owner = Owner::capture(stream)?;
            let record = self
                .records
                .iter()
                .find(|record| record.instance.id == context.instance)
                .ok_or("Unknown session-instance context")?;
            if live_context(record.instance.state, record.context, self.incarnation)
                != Some(context)
                || record.owner.as_ref() != Some(&owner.identity)
            {
                return Err("Session-instance context has lost ownership; \
                    accepted mutations must not be replayed"
                    .into());
            }
            Ok(record.instance.clone())
        }
        /// Validate every ownership/target precondition before a caller starts
        /// irreversible retirement. Reconstruction repeats the check at commit.
        pub fn validate_reconstruction(
            &self,
            prior: Context,
            incarnation: Id,
            build: &str,
            stream: &UnixStream,
        ) -> Result<()> {
            self.reconstruction(prior, incarnation, build, stream).map(|_| ())
        }
        fn reconstruction(
            &self,
            prior: Context,
            incarnation: Id,
            build: &str,
            stream: &UnixStream,
        ) -> Result<(usize, Owner, u64)> {
            self.available()?;
            if incarnation != self.incarnation || !crate::identifier::valid(build, 32) {
                return Err("Reconstruction target changed before admission".into());
            }
            let owner = Owner::capture(stream)?;
            let index = self
                .records
                .iter()
                .position(|record| record.instance.id == prior.instance)
                .ok_or("Unknown reconstruction instance")?;
            let record = &self.records[index];
            let restarted = prior.incarnation != self.incarnation;
            if record.owner.as_ref() != Some(&owner.identity)
                || record.instance.owner_generation != prior.owner_generation
                || !reconstruction_state(
                    record.instance.state,
                    restarted,
                    record.context.is_some(),
                    record.context == Some(prior),
                    record.instance.build != build,
                )
            {
                return Err("Reconstruction no longer owns the exact prior generation; \
                    uncertain actions must not be repeated"
                    .into());
            }
            let generation = next_generation(prior.owner_generation)
                .ok_or("Session-instance owner generation exhausted")?;
            Ok((index, owner, generation))
        }
        /// Explicit reconstruction consumes the old generation once. A lost
        /// reply cannot reuse that generation; no operation or VM is replayed.
        pub fn reconstruct(
            &mut self,
            prior: Context,
            incarnation: Id,
            build: &str,
            stream: &UnixStream,
            next_handle: Id,
        ) -> Result<(SessionInstance, Context)> {
            let (index, owner, generation) =
                self.reconstruction(prior, incarnation, build, stream)?;
            if next_handle == Id::from_bytes([0; 16])
                || self.records.iter().any(|record| {
                    record.context.is_some_and(|context| context.handle == next_handle)
                })
            {
                return Err("Reconstruction requires a fresh context handle".into());
            }
            let context = Context {
                instance: prior.instance,
                handle: next_handle,
                incarnation,
                owner_generation: generation,
            };
            let record = &mut self.records[index];
            record.instance.build = build.into();
            record.instance.state = State::Owned;
            record.instance.owner_generation = generation;
            record.context = Some(context);
            let instance = record.instance.clone();
            self.owners.insert(prior.instance, owner);
            self.persist()?;
            Ok((instance, context))
        }
        pub fn owns_context(&self, context: Context) -> bool {
            !self.failed
                && self.records.iter().any(|record| {
                    live_context(record.instance.state, record.context, self.incarnation)
                        == Some(context)
                })
        }
        pub fn owns_handle(&self, handle: Id) -> bool {
            !self.failed
                && self.records.iter().any(|record| {
                    live_context(record.instance.state, record.context, self.incarnation)
                        .is_some_and(|context| context.handle == handle)
                })
        }
        pub fn exit_descriptors(&self) -> impl Iterator<Item = (Id, &OwnedFd)> {
            self.owners.iter().map(|(id, owner)| (*id, &owner.exit))
        }
        pub fn owner_exited(&mut self, instance: Id) -> Result<()> {
            self.owners.remove(&instance);
            let record = self
                .records
                .iter_mut()
                .find(|record| record.instance.id == instance)
                .ok_or("Unknown exited session instance")?;
            record.context = None;
            record.instance.state = State::Exited;
            self.persist()
        }
        /// Binding validation rejected the first claim before guest execution.
        /// Never retire an owned instance on a later or competing claim failure.
        pub fn reject_unclaimed(&mut self, instance: Id) -> Result<()> {
            self.available()?;
            let record = self
                .records
                .iter_mut()
                .find(|record| record.instance.id == instance)
                .ok_or("Unknown launch reservation")?;
            if can_reject_claim(
                record.instance.state,
                record.owner.is_some(),
                record.context.is_some(),
            ) {
                record.instance.state = State::LaunchFailed;
                self.persist()?;
            }
            Ok(())
        }
        /// Only a definitive failure before process dispatch may call this.
        /// An uncertain launch reply is inspected; it is never marked failed.
        pub fn launch_failed(&mut self, instance: Id, incarnation: Id) -> Result<SessionInstance> {
            self.available()?;
            let record = self
                .records
                .iter_mut()
                .find(|record| record.instance.id == instance)
                .ok_or("Unknown launch reservation")?;
            if record.created != incarnation
                || record.owner.is_some()
                || !matches!(record.instance.state, State::Reserved | State::LaunchFailed)
            {
                return Err(
                    "Dispatched or owned instance requires kernel ownership recovery".into()
                );
            }
            record.instance.state = State::LaunchFailed;
            let descriptor = record.instance.clone();
            self.persist()?;
            Ok(descriptor)
        }
        pub fn active(&self) -> bool {
            self.records.iter().any(|record| !terminal(record.instance.state))
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, reason = "Tests assert durable registry fixture behavior.")]
    mod tests {
        use super::*;
        fn contract(request: Id) -> SessionInstance {
            SessionInstance {
                restore_host_mounts: false,
                id: Id::from_bytes([0; 16]),
                launch_request: request,
                account: Id::from_bytes([4; 16]),
                session: crate::identifier::encode(&[0xab; 32]),
                build: crate::identifier::encode(&[0xcd; 32]),
                root: Id::from_bytes([3; 16]),

                extra_volumes: vec![],
                host_mounts: Box::default(),
                owner_generation: 0,
                state: State::Reserved,
            }
        }
        #[test]
        fn reservations_count_toward_process_capacity_and_failure_is_terminal() {
            let dir = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(dir.path(), epoch).unwrap();
            let mut instances = Vec::new();
            for _ in 0..MAX_PROCESSES {
                instances.push(registry.reserve(epoch, contract(Id::generate().unwrap())).unwrap());
            }
            assert!(
                instances.iter().enumerate().all(|(i, instance)| instances[..i]
                    .iter()
                    .all(|other| other.id != instance.id))
            );
            let request = Id::generate().unwrap();
            assert!(registry.reserve(epoch, contract(request)).is_err());
            assert!(registry.inspect(request).unwrap().is_none());
            let first = &instances[0];
            let listed = registry.list(&first.session).unwrap();
            assert_eq!(listed.len(), MAX_PROCESSES);
            assert!(listed.iter().all(|owner| owner.process.is_none()));
            assert!(registry.list(&crate::identifier::encode(&[0xef; 32])).unwrap().is_empty());
            assert!(registry.list("invalid").is_err());
            let failed = registry.launch_failed(first.id, epoch).unwrap();
            assert_eq!(failed.state, State::LaunchFailed);
            assert_eq!(registry.list(&first.session).unwrap().len(), MAX_PROCESSES - 1);
            assert_eq!(registry.reserve(epoch, contract(first.launch_request)).unwrap(), failed);
            let (_client, server) = UnixStream::pair().unwrap();
            assert!(registry.claim(first.id, &server).is_err());
            registry.reserve(epoch, contract(request)).unwrap();
        }
        #[test]
        fn watches_begin_at_claim_and_preserve_unknown_survivors() {
            let dir = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(dir.path(), epoch).unwrap();
            let first = registry.reserve(epoch, contract(Id::generate().unwrap())).unwrap();
            let second = registry.reserve(epoch, contract(Id::generate().unwrap())).unwrap();
            assert!(registry.active_sessions().is_empty());
            for state in [State::Owned, State::Unknown, State::Draining] {
                registry.records[0].instance.state = state;
                assert_eq!(registry.active_sessions(), vec![first.session.clone()]);
            }
            registry.records[0].instance.state = State::Exited;
            assert!(registry.active_sessions().is_empty());
            let (_client, server) = UnixStream::pair().unwrap();
            registry.claim(second.id, &server).unwrap();
            assert_eq!(registry.active_sessions(), vec![second.session.clone()]);
            registry.owner_exited(second.id).unwrap();
            assert!(registry.active_sessions().is_empty());
        }
        #[test]
        fn unclaimed_launch_does_not_take_the_fourth_running_session_watch() {
            let dir = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(dir.path(), epoch).unwrap();
            for n in 1..=3 {
                let mut launch = contract(Id::generate().unwrap());
                launch.session = crate::identifier::encode(&{
                    let mut bytes = [0; 32];
                    bytes[16..].copy_from_slice(&(n as u128).to_be_bytes());
                    bytes
                });
                registry.reserve(epoch, launch).unwrap();
                registry.records.last_mut().unwrap().instance.state = State::Unknown;
            }
            let mut pending = contract(Id::generate().unwrap());
            pending.session = crate::identifier::encode(&{
                let mut bytes = [0; 32];
                bytes[31] = 4;
                bytes
            });
            let pending = registry.reserve(epoch, pending).unwrap();
            let mut launch = contract(Id::generate().unwrap());
            launch.session = crate::identifier::encode(&{
                let mut bytes = [0; 32];
                bytes[31] = 5;
                bytes
            });
            let running = registry.reserve(epoch, launch).unwrap();
            let (_client, server) = UnixStream::pair().unwrap();
            registry.claim(running.id, &server).unwrap();
            let active = registry.active_sessions();
            assert_eq!(active.len(), 4);
            assert!(active.contains(&running.session));
            assert!(!active.contains(&pending.session));
            assert_eq!(registry.descriptor(pending.id).unwrap().state, State::Reserved);
        }

        #[test]
        fn persistence_failure_cannot_export_a_successful_reservation() {
            let dir = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(dir.path(), epoch).unwrap();
            std::os::unix::fs::symlink(
                dir.path().join("unused"),
                dir.path().join("session-instances.pending"),
            )
            .unwrap();
            let request = Id::generate().unwrap();
            assert!(registry.reserve(epoch, contract(request)).is_err());
            assert!(registry.inspect(request).is_err());
            assert!(registry.reserve(epoch, contract(Id::generate().unwrap())).is_err());
            assert!(!dir.path().join("session-instances").exists());
        }
        #[test]
        fn launch_is_durable_deduplicated_and_never_expires_an_unknown_owner() {
            let dir = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let request = Id::generate().unwrap();
            let mut registry = Registry::open(dir.path(), epoch).unwrap();
            let reserved = registry.reserve(epoch, contract(request)).unwrap();
            assert_eq!(registry.reserve(epoch, contract(request)).unwrap(), reserved);
            let mut wrong = contract(request);
            wrong.build = crate::identifier::encode(&[0xef; 32]);
            assert!(registry.reserve(epoch, wrong).is_err());
            drop(registry);
            let mut restored = Registry::open(dir.path(), Id::generate().unwrap()).unwrap();
            assert_eq!(restored.inspect(request).unwrap(), Some(reserved));
            assert!(restored.reserve(epoch, contract(Id::generate().unwrap())).is_err());
        }
        #[test]
        fn mount_configuration_is_local_durable_and_part_of_launch_identity() {
            let dir = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let request = Id::generate().unwrap();
            let mut registry = Registry::open(dir.path(), epoch).unwrap();
            let mut config = contract(request);
            config.host_mounts = vec![
                crate::engine::host_mount::HostMount::resolve(dir.path(), Some("Documents"), false)
                    .unwrap(),
            ]
            .into_boxed_slice();
            let saved = registry.reserve(epoch, config.clone()).unwrap();
            assert_eq!(registry.reserve(epoch, config.clone()).unwrap(), saved);
            config.host_mounts[0].write = true;
            assert!(registry.reserve(epoch, config).is_err());
            drop(registry);
            let restored = Registry::open(dir.path(), Id::generate().unwrap()).unwrap();
            assert_eq!(restored.inspect(request).unwrap(), Some(saved));
        }

        #[test]
        fn oversized_configuration_rejection_preserves_live_ownership_and_saved_state() {
            let directory = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(directory.path(), epoch).unwrap();
            let mut empty = contract(Id::generate().unwrap());
            empty.restore_host_mounts = true;
            let live = registry.reserve(epoch, empty).unwrap();
            let (_client, server) = UnixStream::pair().unwrap();
            let (_, context) = registry.claim(live.id, &server).unwrap();
            let large = || {
                let mut config = contract(Id::generate().unwrap());
                config.restore_host_mounts = true;
                config.host_mounts = (0..super::super::super::host_mount::MAX_MOUNTS)
                    .map(|index| crate::engine::host_mount::HostMount {
                        id: Id::generate().unwrap(),
                        path: format!("/{}", "a".repeat(4094)).into_bytes(),
                        label: format!("Folder {index}"),
                        write: false,
                    })
                    .collect();
                config
            };
            let mut saved = Vec::new();
            for _ in 0..MAX_PROCESSES {
                match registry.reserve(epoch, large()) {
                    Ok(instance) => saved.push(instance),
                    Err(_) => break,
                }
            }
            assert!(!saved.is_empty());
            assert!(
                saved.len() < MAX_PROCESSES - 1,
                "aggregate bytes, not process count, reject the reservation"
            );
            assert!(registry.owns_context(context));
            assert!(registry.set_mounts(live.id, &[], large().host_mounts).is_err());
            assert!(registry.descriptor(live.id).unwrap().host_mounts.is_empty());
            assert!(registry.owns_context(context));
            let prior = &saved[0];
            registry.owner_exited(prior.id).unwrap();
            assert!(
                registry
                    .restore(epoch, Id::generate().unwrap(), prior.id, prior.build.clone(), vec![])
                    .is_err()
            );
            assert!(registry.configuration(prior.id, prior.account).is_ok());
            assert!(registry.owns_context(context));
            drop(registry);
            let registry = Registry::open(directory.path(), Id::generate().unwrap()).unwrap();
            assert!(registry.configuration(live.id, live.account).unwrap().host_mounts.is_empty());
            assert_eq!(
                registry.configuration(prior.id, prior.account).unwrap().host_mounts,
                prior.host_mounts
            );
        }
        #[test]
        fn saved_gui_instance_restores_once_after_exit_with_fresh_mounts() {
            let directory = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(directory.path(), epoch).unwrap();
            let mut config = contract(Id::generate().unwrap());
            config.restore_host_mounts = true;
            config.host_mounts = vec![
                crate::engine::host_mount::HostMount::resolve(
                    directory.path(),
                    Some("Documents"),
                    true,
                )
                .unwrap(),
            ]
            .into();
            let prior = registry.reserve(epoch, config).unwrap();
            let request = Id::generate().unwrap();
            assert!(
                registry.restore(epoch, request, prior.id, prior.build.clone(), vec![]).is_err()
            );
            registry.owner_exited(prior.id).unwrap();
            drop(registry);
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(directory.path(), epoch).unwrap();
            assert_eq!(
                registry.configurations(&prior.session, prior.account).unwrap(),
                vec![prior.id]
            );
            assert!(
                registry
                    .configurations(&prior.session, Id::generate().unwrap())
                    .unwrap()
                    .is_empty()
            );
            let restored =
                registry.restore(epoch, request, prior.id, prior.build.clone(), vec![]).unwrap();
            assert_ne!(restored.id, prior.id);
            assert_ne!(restored.host_mounts[0].id, prior.host_mounts[0].id);
            assert_eq!(restored.host_mounts[0].path, prior.host_mounts[0].path);
            assert!(restored.host_mounts[0].write);
            assert_eq!(
                registry.restore(epoch, request, prior.id, prior.build.clone(), vec![]).unwrap(),
                restored
            );
            assert!(
                registry
                    .restore(epoch, Id::generate().unwrap(), prior.id, prior.build.clone(), vec![])
                    .is_err()
            );
            assert_eq!(
                registry.configurations(&prior.session, prior.account).unwrap(),
                vec![restored.id]
            );
            assert!(registry.configuration(prior.id, prior.account).is_err());
            drop(registry);
            let registry = Registry::open(directory.path(), Id::generate().unwrap()).unwrap();
            assert_eq!(registry.configuration(restored.id, prior.account).unwrap(), restored);
        }
        #[test]
        fn mount_edits_reject_stale_drafts_and_never_reuse_removed_grants() {
            let directory = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(directory.path(), epoch).unwrap();
            let mut config = contract(Id::generate().unwrap());
            config.restore_host_mounts = true;
            config.host_mounts = vec![
                crate::engine::host_mount::HostMount::resolve(
                    directory.path(),
                    Some("Documents"),
                    true,
                )
                .unwrap(),
            ]
            .into();
            let prior = registry.reserve(epoch, config).unwrap();
            let mut rename = prior.host_mounts.clone();
            rename[0].label = "Work".into();
            let renamed = registry.set_mounts(prior.id, &prior.host_mounts, rename).unwrap();
            assert_eq!(renamed.host_mounts[0].id, prior.host_mounts[0].id);
            assert_eq!(registry.mount_revision(prior.id), 1);
            assert!(registry.set_mounts(prior.id, &prior.host_mounts, Box::default()).is_err());
            let mut unsafe_reuse = renamed.host_mounts.clone();
            unsafe_reuse[0].write = false;
            assert!(registry.set_mounts(prior.id, &renamed.host_mounts, unsafe_reuse).is_err());
            registry.set_mounts(prior.id, &renamed.host_mounts, Box::default()).unwrap();
            let readded = registry.set_mounts(prior.id, &[], renamed.host_mounts).unwrap();
            assert_ne!(readded.host_mounts[0].id, prior.host_mounts[0].id);
            assert_eq!(registry.mount_revision(prior.id), 3);
            drop(registry);
            let registry = Registry::open(directory.path(), Id::generate().unwrap()).unwrap();
            assert_eq!(registry.configuration(prior.id, prior.account).unwrap(), readded);
        }

        #[test]
        fn startup_exit_survives_other_progress_and_terminal_history_is_bounded() {
            // A crash before the first guest report must remain visible while
            // other startups advance. Reservations describe their next claim.
            let dir = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(dir.path(), epoch).unwrap();
            let instance = registry.reserve(epoch, contract(Id::generate().unwrap())).unwrap();
            let pending = registry.startup_progress().pop().unwrap();
            assert_eq!(pending.incarnation, epoch);
            assert_eq!(pending.owner_generation, instance.owner_generation + 1);
            assert_eq!(pending.phase, StartupPhase::Starting);
            let (_client, server) = UnixStream::pair().unwrap();
            let (_, context) = registry.claim(instance.id, &server).unwrap();
            registry.owner_exited(instance.id).unwrap();
            let next = registry.reserve(epoch, contract(Id::generate().unwrap())).unwrap();
            let (_, next_context) = registry.claim(next.id, &server).unwrap();
            registry.set_startup(next_context, Some(0), StartupPhase::OpeningWindow).unwrap();
            let failed = registry
                .startup_progress()
                .into_iter()
                .find(|progress| progress.instance == instance.id)
                .unwrap();
            assert_eq!(failed.owner_generation, context.owner_generation);
            assert_eq!(failed.phase, StartupPhase::Failed);
            assert!(registry.set_startup(context, Some(0), StartupPhase::Ready).is_err());
            registry.set_startup(next_context, Some(0), StartupPhase::Ready).unwrap();
            registry.owner_exited(next.id).unwrap();
            let live = registry.reserve(epoch, contract(Id::generate().unwrap())).unwrap();
            let (_, live_context) = registry.claim(live.id, &server).unwrap();
            registry.set_startup(live_context, Some(0), StartupPhase::OpeningWindow).unwrap();
            let completed = registry
                .startup_progress()
                .into_iter()
                .find(|progress| progress.instance == next.id)
                .unwrap();
            assert_eq!(completed.phase, StartupPhase::Ready);
            for _ in 0..MAX_PROCESSES + 1 {
                let rejected = registry.reserve(epoch, contract(Id::generate().unwrap())).unwrap();
                registry.launch_failed(rejected.id, epoch).unwrap();
                let failed = registry
                    .startup_progress()
                    .into_iter()
                    .find(|progress| progress.instance == rejected.id)
                    .unwrap();
                assert_eq!(failed.owner_generation, rejected.owner_generation + 1);
                assert_eq!(failed.phase, StartupPhase::Failed);
            }
            assert_eq!(registry.startup_progress().len(), MAX_PROCESSES + 1);
        }

        #[test]
        fn retained_show_rejects_in_flight_readiness_from_every_previous_presentation() {
            // Accept Show before initial setup finishes and before the previous
            // Ready reaches the engine. Only the current window observation wins.
            let dir = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(dir.path(), epoch).unwrap();
            let instance = registry.reserve(epoch, contract(Id::generate().unwrap())).unwrap();
            let (_client, server) = UnixStream::pair().unwrap();
            let (_, context) = registry.claim(instance.id, &server).unwrap();
            let first = registry.begin_show(instance.id, context.owner_generation).unwrap();
            assert_eq!(first, 1);
            registry.set_startup(context, None, StartupPhase::Instantiating).unwrap();
            registry.set_startup(context, None, StartupPhase::OpeningWindow).unwrap();
            assert!(registry.set_startup(context, None, StartupPhase::Ready).is_err());
            assert_eq!(registry.startup_progress()[0].phase, StartupPhase::OpeningWindow);
            assert!(registry.set_startup(context, Some(0), StartupPhase::Ready).is_err());
            let second = registry.begin_show(instance.id, context.owner_generation).unwrap();
            assert_eq!(second, first + 1);
            assert!(registry.set_startup(context, Some(first), StartupPhase::Ready).is_err());
            assert!(registry.set_startup(context, Some(first), StartupPhase::Failed).is_err());
            registry.set_startup(context, Some(second), StartupPhase::Ready).unwrap();
            assert_eq!(registry.list(&instance.session).unwrap()[0].presentation, second);
            let third = registry.begin_show(instance.id, context.owner_generation).unwrap();
            assert_eq!(third, second + 1);
            assert_eq!(registry.startup_progress()[0].presentation, third);
            assert_eq!(registry.startup_progress()[0].phase, StartupPhase::OpeningWindow);
            // VM failure belongs to this owner regardless of the active Show.
            registry.set_startup(context, None, StartupPhase::Failed).unwrap();
            assert_eq!(registry.startup_progress()[0].phase, StartupPhase::Failed);
            assert!(registry.begin_show(instance.id, context.owner_generation + 1).is_err());
            registry.owner_exited(instance.id).unwrap();
            assert!(registry.begin_show(instance.id, context.owner_generation).is_err());
        }

        #[test]
        fn claim_survives_operation_close_and_restart_invalidates_old_context() {
            let dir = tempfile::tempdir().unwrap();
            let epoch = Id::generate().unwrap();
            let mut registry = Registry::open(dir.path(), epoch).unwrap();
            let instance = registry.reserve(epoch, contract(Id::generate().unwrap())).unwrap();
            let (client, server) = UnixStream::pair().unwrap();
            let (_, context) = registry.claim(instance.id, &server).unwrap();
            drop(client);
            drop(server);
            let (_next_client, next) = UnixStream::pair().unwrap();
            assert_eq!(registry.authorize(context, &next).unwrap().id, instance.id);
            let listed = registry.list(&instance.session).unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].process.as_ref().unwrap().pid(), std::process::id());
            assert_eq!(listed[0].instance.state, State::Owned);
            assert_eq!(registry.claim(instance.id, &next).unwrap().1, context);
            registry.set_startup(context, Some(0), StartupPhase::OpeningWindow).unwrap();
            registry.set_startup(context, Some(0), StartupPhase::Ready).unwrap();
            assert_eq!(registry.startup_progress()[0].phase, StartupPhase::Ready);
            assert!(registry.set_startup(context, Some(0), StartupPhase::Loading).is_err());
            drop(registry);
            let mut registry = Registry::open(dir.path(), Id::generate().unwrap()).unwrap();
            let listed = registry.list(&instance.session).unwrap();
            assert_eq!(listed[0].instance.state, State::Unknown);
            assert_eq!(listed[0].process.as_ref().unwrap().pid(), std::process::id());
            assert!(registry.authorize(context, &next).is_err());
            let (descriptor, fresh) = registry.claim(instance.id, &next).unwrap();
            assert_eq!(fresh.owner_generation, context.owner_generation + 1);
            assert_eq!(descriptor.state, State::Owned);
            assert_ne!(fresh.handle, context.handle);
            assert!(registry.set_startup(context, Some(0), StartupPhase::Ready).is_err());
            assert_eq!(registry.startup_progress()[0].phase, StartupPhase::Loading);
            registry.set_startup(fresh, Some(0), StartupPhase::Ready).unwrap();
        }
    }
}
