//! Native source notifications. The callback only coalesces bounded work; builds
//! and service calls stay on the CLI thread, independent of the AppKit runloop.
use notify::{Event, EventKind, RecursiveMode, Watcher};
use playbit::engine::{Result, installation::Installation};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions, TryLockError},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

#[path = "watch_wake.rs"]
mod wake;
use wake::Wake;

const SOURCE: u8 = 1;
const ARTIFACT: u8 = 2;
const FAILED: u8 = 4;
const MAX_CHANGED_PATHS: usize = 256;
const MAX_PATH_BYTES: usize = 4096;
const MAX_CHANGED_BYTES: u64 = 64 * 1024 * 1024;

// A true value means only metadata notifications arrived for this path.
type Paths = BTreeMap<PathBuf, bool>;
struct Work {
    flags: u8,
    received: Instant,
    // None requires a conservative rebuild after a rescan or queue overflow.
    paths: Option<Paths>,
}

struct Pending {
    work: Mutex<Option<Work>>,
    wake: Wake,
}
impl Pending {
    fn signal(&self, flags: u8, paths: Option<Paths>) {
        if flags == 0 {
            return;
        }
        // One wake and at most 256 paths of 4 KiB each represent pending work.
        // No framework-owned event object is retained past this callback.
        let mut work = self.work.lock().expect("mutex is not poisoned");
        match work.as_mut() {
            Some(pending) => {
                pending.flags |= flags;
                if let (Some(current), Some(paths)) = (&mut pending.paths, paths) {
                    for (path, metadata) in paths {
                        current.entry(path).and_modify(|old| *old &= metadata).or_insert(metadata);
                        if current.len() > MAX_CHANGED_PATHS {
                            pending.paths = None;
                            break;
                        }
                    }
                } else {
                    pending.paths = None;
                }
            }
            None => {
                *work = Some(Work { flags, received: Instant::now(), paths });
                self.wake.source_ready();
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileState {
    Missing,
    Directory,
    Contents(blake3::Hash),
}

fn file_state(path: &Path, remaining: &mut u64) -> Result<Option<FileState>> {
    let file = match OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(FileState::Missing));
        }
        Err(error) => {
            return Err(format!("cannot inspect watched file {}: {error}", path.display()).into());
        }
    };
    let metadata = file.metadata()?;
    if metadata.is_dir() {
        return Ok(Some(FileState::Directory));
    }
    if !metadata.is_file() || metadata.len() > *remaining {
        return Ok(None);
    }
    let mut hash = blake3::Hasher::new();
    hash.update(&metadata.mode().to_le_bytes());
    let mut input = file.take(*remaining + 1);
    let mut buffer = [0; 65536];
    loop {
        let size = input.read(&mut buffer)?;
        if size == 0 {
            return Ok(Some(FileState::Contents(hash.finalize())));
        }
        if size as u64 > *remaining {
            return Ok(None);
        }
        *remaining -= size as u64;
        hash.update(&buffer[..size]);
    }
}

#[derive(Default)]
struct SourceState {
    files: BTreeMap<PathBuf, FileState>,
}
impl SourceState {
    fn changed(&mut self, paths: Paths, remaining: &mut u64) -> Result<bool> {
        let mut changed = false;
        for (path, metadata_only) in paths {
            let Some(state) = file_state(&path, remaining)? else {
                changed = true;
                continue;
            };
            // Directory timestamps also change when ignored build output is written.
            if !(metadata_only && state == FileState::Directory) {
                changed |= self.files.get(&path) != Some(&state);
            }
            if !self.files.contains_key(&path) && self.files.len() == MAX_CHANGED_PATHS {
                self.files.pop_first();
            }
            self.files.insert(path, state);
        }
        Ok(changed)
    }
}

pub(super) struct Change {
    pub source: bool,
    pub scheduling_us: u128,
}

pub(super) struct SourceWatch {
    _watcher: notify::RecommendedWatcher,
    _lease: Lease,
    pending: Arc<Pending>,
    source_state: SourceState,
    artifact: PathBuf,
    artifact_state: Option<FileState>,
}

#[derive(Debug, thiserror::Error)]
#[error("Playbit is being updated")]
pub(super) struct UpdateRequested;

struct Lease(File);
impl Drop for Lease {
    fn drop(&mut self) {
        // A concurrently spawned child may retain the same open file description
        // until exec. Closing just our descriptor would leave its lock held.
        let _ = self.0.unlock();
    }
}

fn lease(profile: &Path) -> Result<Lease> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = profile.parent() {
        std::fs::create_dir_all(parent)?;
    }
    for slot in 0..4 {
        let mut name = profile.as_os_str().to_owned();
        name.push(format!(".development-{slot}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(Path::new(&name))?;
        match file.try_lock() {
            Ok(()) => return Ok(Lease(file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    Err("Four development sessions are being watched. End one pb run before starting another."
        .into())
}

fn path_flags(path: &Path, source: &Path, artifact: &Path, target: &Path) -> u8 {
    if path == artifact {
        return ARTIFACT;
    }
    if target != source && path.starts_with(target) {
        return 0;
    }
    let Ok(relative) = path.strip_prefix(source) else {
        return 0;
    };
    if relative.components().any(|part| {
        matches!(part, std::path::Component::Normal(name)
            if name == ".git" || name == "target" || name == "_build" || name == ".DS_Store")
    }) {
        return 0;
    }
    SOURCE
}

fn classify(event: &Event, source: &Path, artifact: &Path, target: &Path) -> u8 {
    if event.need_rescan() {
        return SOURCE | ARTIFACT;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return 0;
    }
    event.paths.iter().fold(0, |flags, path| flags | path_flags(path, source, artifact, target))
}

impl SourceWatch {
    pub fn start(
        source: &Path,
        artifact: &Path,
        output_directory: &Path,
        profile: &Path,
        installation: Option<&Installation>,
    ) -> Result<Self> {
        let source = source.canonicalize()?;
        let artifact = std::path::absolute(artifact)?;
        if source.as_os_str().len() > 4096 || artifact.as_os_str().len() > 4096 {
            return Err("source or artifact path exceeds 4 KiB".into());
        }
        let target = std::path::absolute(output_directory)?;
        let relative_artifact = artifact.strip_prefix(&target)?.to_path_buf();
        std::fs::create_dir_all(&target)?;
        let target = target.canonicalize()?;
        let artifact = target.join(relative_artifact);
        if source.to_str().is_none() || target.to_str().is_none() {
            return Err("source watching requires UTF-8 source and target paths".into());
        }
        let watch_source = source.clone();
        let watch_target = target.clone();
        let watch_artifact = artifact.clone();
        let lease = lease(profile)?;
        let interrupt = installation.map(Installation::run_interrupt).transpose()?;
        let pending = Arc::new(Pending { work: Mutex::new(None), wake: Wake::new(interrupt)? });
        let signal = pending.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<Event>| match event {
                Ok(event) => {
                    let flags = classify(&event, &source, &artifact, &target);
                    let metadata = matches!(
                        event.kind,
                        EventKind::Modify(notify::event::ModifyKind::Metadata(_))
                    );
                    let paths: Paths = event
                        .paths
                        .iter()
                        .filter(|p| path_flags(p, &source, &artifact, &target) & SOURCE != 0)
                        .take(MAX_CHANGED_PATHS + 1)
                        .map(|p| (p.clone(), metadata))
                        .collect();
                    let bounded = paths.len() <= MAX_CHANGED_PATHS
                        && paths.keys().all(|p| p.as_os_str().len() <= MAX_PATH_BYTES);
                    signal.signal(flags, (!event.need_rescan() && bounded).then_some(paths));
                }
                Err(_) => signal.signal(FAILED, None),
            })?;
        watcher.watch(&watch_source, RecursiveMode::Recursive)?;
        if !watch_target.starts_with(&watch_source) {
            watcher.watch(&watch_target, RecursiveMode::Recursive)?;
        }
        let watch = Self {
            _watcher: watcher,
            _lease: lease,
            pending,
            source_state: SourceState::default(),
            artifact: watch_artifact,
            artifact_state: None,
        };
        // Check after subscribing so a concurrent installer cannot be missed.
        watch.check_update()?;
        Ok(watch)
    }

    fn check_update(&self) -> Result<()> {
        if self.pending.wake.update_requested()? {
            return Err(UpdateRequested.into());
        }
        Ok(())
    }

    pub fn record_artifact(&mut self) -> Result<()> {
        let mut remaining = MAX_CHANGED_BYTES;
        self.artifact_state = file_state(&self.artifact, &mut remaining)?;
        Ok(())
    }

    pub fn next(&mut self) -> Result<Change> {
        loop {
            self.check_update()?;
            let work = self.pending.work.lock().expect("mutex is not poisoned").take();
            let Some(Work { flags, received, paths }) = work else {
                self.pending.wake.wait(None)?;
                continue;
            };
            if flags & FAILED != 0 {
                return Err("Source watching failed; the running app is retained. \
                    Run pb run again to resume watching."
                    .into());
            }
            let mut remaining = MAX_CHANGED_BYTES;
            let source = flags & SOURCE != 0
                && match paths {
                    Some(paths) => self.source_state.changed(paths, &mut remaining)?,
                    None => true,
                };
            let mut artifact = false;
            if flags & ARTIFACT != 0 {
                let state = file_state(&self.artifact, &mut remaining)?;
                artifact = state.is_none() || state != self.artifact_state;
                self.artifact_state = state;
            }
            if source || artifact {
                return Ok(Change { source, scheduling_us: received.elapsed().as_micros() });
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn duplicate_notifications_preserve_real_edits_and_atomic_saves() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("app.ml");
        let mut state = SourceState::default();
        let mut changed = |path: &Path, metadata| {
            let mut remaining = MAX_CHANGED_BYTES;
            state.changed(Paths::from([(path.to_owned(), metadata)]), &mut remaining).unwrap()
        };
        std::fs::write(&source, "first").unwrap();
        assert!(changed(&source, false));
        assert!(!changed(&source, true));
        std::fs::write(&source, "first").unwrap();
        assert!(!changed(&source, false));
        let temporary = root.path().join("save.tmp");
        std::fs::write(&temporary, "second").unwrap();
        std::fs::rename(&temporary, &source).unwrap();
        assert!(changed(&source, false));
        assert!(!changed(&source, true));
        assert!(!changed(root.path(), true));
        // Executable-bit changes remain relevant to custom build scripts.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(changed(&source, true));
        assert!(!changed(&source, true));
        std::fs::remove_file(&source).unwrap();
        assert!(changed(&source, false));
        assert!(!changed(&source, false));
    }

    #[test]
    fn coalescing_retains_edits_during_a_build_and_overflow_requires_rescan() {
        let pending = Pending { work: Mutex::new(None), wake: Wake::new(None).unwrap() };
        let paths = || Paths::from([(PathBuf::from("app.ml"), false)]);
        pending.signal(SOURCE, Some(paths()));
        let first = pending.work.lock().unwrap().take().unwrap();
        assert_eq!(first.flags, SOURCE);
        // Taking the first change starts a build; a later edit remains queued.
        pending.signal(SOURCE, Some(paths()));
        pending.signal(ARTIFACT, Some(Paths::new()));
        let next = pending.work.lock().unwrap().take().unwrap();
        assert_eq!(next.flags, SOURCE | ARTIFACT);
        assert_eq!(next.paths.unwrap(), paths());
        pending.signal(SOURCE, Some(paths()));
        let paths =
            (0..MAX_CHANGED_PATHS).map(|n| (PathBuf::from(format!("file-{n}")), false)).collect();
        pending.signal(SOURCE, Some(paths));
        assert!(pending.work.lock().unwrap().take().unwrap().paths.is_none());
    }
    #[test]
    fn generated_output_is_ignored_and_atomic_source_saves_are_changes() {
        let source = Path::new("/source");
        let target = Path::new("/source/target");
        let artifact = target.join("wasm32-unknown-unknown/debug/app.wasm");
        let root_output = source.join("app.wasm");
        let source_event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(source.join("app.ml"));
        assert_eq!(classify(&source_event, source, &root_output, source), SOURCE);
        for (path, expected) in [
            ("src/app.rs", SOURCE),
            ("src/widgets.rs", SOURCE),
            (".git/index", 0),
            (".DS_Store", 0),
            ("src/.DS_Store", 0),
            ("_build/generated/bindings.ml", 0),
            ("bindgen/target/debug/bindgen", 0),
            ("target/debug/Example.pbapp", 0),
            ("target/debug/Example.app/Contents/MacOS/app", 0),
            ("target/debug/temporary", 0),
        ] {
            let event = Event::new(EventKind::Any).add_path(source.join(path));
            assert_eq!(classify(&event, source, &artifact, target), expected);
        }
        // Project-local generated metadata stays ignored with a custom Cargo target.
        let event = Event::new(EventKind::Any).add_path(source.join("target/playbit/metadata.rs"));
        assert_eq!(classify(&event, source, &artifact, Path::new("/custom-target")), 0);
        let event = Event::new(EventKind::Any).add_path(source.join("playbit.toml"));
        assert_eq!(classify(&event, source, &artifact, Path::new("/custom-target")), SOURCE);
        let event = Event::new(EventKind::Any).add_path(artifact.clone());
        assert_eq!(classify(&event, source, &artifact, target), ARTIFACT);
    }

    #[test]
    fn edits_coalesce_and_watch_capacity_is_released_by_lease_drop() {
        let pending = Pending { work: Mutex::new(None), wake: Wake::new(None).unwrap() };
        for _ in 0..100 {
            pending.signal(SOURCE, None);
            pending.signal(ARTIFACT, Some(Paths::new()));
        }
        assert_eq!(pending.work.lock().unwrap().take().unwrap().flags, SOURCE | ARTIFACT);
        assert!(pending.work.lock().unwrap().is_none());
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        let mut slots = (0..4).map(|_| lease(&profile).unwrap()).collect::<Vec<_>>();
        assert!(lease(&profile).is_err());
        slots.pop();
        assert!(lease(&profile).is_ok());
        assert!(!profile.exists(), "watch leases must not make an uninitialized profile nonempty");
    }

    #[test]
    fn retiring_watch_releases_capacity_while_a_child_descriptor_is_still_open() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        let mut slots = (0..4).map(|_| lease(&profile).unwrap()).collect::<Vec<_>>();
        // fork duplicates the open file description until exec closes CLOEXEC
        // descriptors. Model that window without forking a threaded test runner.
        let inherited = slots[3].0.try_clone().unwrap();
        drop(slots.pop());
        assert!(lease(&profile).is_ok(), "retired watcher still owns capacity");
        drop(inherited);
    }

    #[test]
    #[ignore = "reference-device lease retirement benchmark; never run in CI"]
    fn benchmark_lease_retirement() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        let mut samples = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let slot = lease(&profile).unwrap();
            let inherited = slot.0.try_clone().unwrap();
            let started = Instant::now();
            drop(slot);
            samples.push(started.elapsed().as_nanos());
            let replacement = lease(&profile).expect("capacity must be immediately reusable");
            drop(inherited);
            drop(replacement);
        }
        samples.sort_unstable();
        playbit::diagnostic!(
            "watch lease retirement count={} p50_ns={} p99_ns={} max_ns={}",
            samples.len(),
            samples[499],
            samples[989],
            samples[999]
        );
        assert!(samples[989] < 100_000, "lease retirement exceeds its100µs allocation");
    }

    #[test]
    fn native_events_deliver_atomic_saves_and_external_artifacts() {
        use std::{fs, time::Duration};
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        let artifact = directory.path().join("target/wasm32-unknown-unknown/debug/app.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        let watcher = SourceWatch::start(
            &source,
            &artifact,
            &directory.path().join("target"),
            &directory.path().join("profile"),
            None,
        )
        .unwrap();
        let started = Instant::now();
        fs::write(source.join("edit.tmp"), "new source").unwrap();
        fs::rename(source.join("edit.tmp"), source.join("app.rs")).unwrap();
        let wait_for_work = |flags| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let work = watcher.pending.work.lock().unwrap().take();
                if work.is_some_and(|work| work.flags & flags != 0) {
                    break;
                }
                assert_eq!(
                    watcher.pending.wake.wait(Some(deadline)).unwrap(),
                    Some(libc::EVFILT_USER)
                );
            }
        };
        wait_for_work(SOURCE);
        playbit::diagnostic!("native atomic-save delivery_us={}", started.elapsed().as_micros());
        fs::write(&artifact, "external build").unwrap();
        wait_for_work(ARTIFACT);
    }

    #[test]
    fn installer_interrupts_an_idle_source_watch_and_allows_it_to_restart() {
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = root.join("source");
        std::fs::create_dir(&source).unwrap();
        let installation =
            Installation { app: root.join("Playbit.app"), state: root.join("state") };
        let start = || {
            SourceWatch::start(
                &source,
                &source.join("app.wasm"),
                &source,
                &root.join("profile"),
                Some(&installation),
            )
        };
        let mut watcher = start().unwrap();
        let request = installation.interrupt_runs().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match watcher.pending.wake.wait(Some(deadline)).unwrap() {
                Some(libc::EVFILT_VNODE) => break,
                Some(libc::EVFILT_USER) => continue,
                _ => panic!("installer did not wake the idle watcher"),
            }
        }
        let error = watcher.next().err().expect("watcher should stop for the update");
        assert!(error.is::<UpdateRequested>());
        assert_eq!(error.to_string(), "Playbit is being updated");
        drop(watcher);

        let error = start().err().expect("a watcher started during the update");
        assert!(error.is::<UpdateRequested>());
        drop(request);
        // A left-behind pulse carries no authority after the installer releases
        // the lock, including if the installer process stopped unexpectedly.
        assert!(start().is_ok());
    }
}
