//! Profile-owned, bounded panic reports. Unattributed OS reports stay local.
use crate::{
    engine::{Result, profile},
    release_channel::ReleaseChannel,
    store::Id,
};
use serde::{Deserialize, Serialize};
#[cfg(any(feature = "desktop", test))]
use std::io::Read;
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const MAX_REPORTS: usize = 16;
pub(crate) const MAX_BYTES: u64 = 64 * 1024;
const DIRECTORY: &str = "crash-reports";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Report {
    schema: u8,
    pub channel: ReleaseChannel,
    pub build: String,
    process: String,
    pid: u32,
    timestamp_secs: u64,
    pub account_id: Option<Id>,
    pub report: String,
}

pub(crate) struct Queue {
    directory: PathBuf,
    channel: ReleaseChannel,
    names: Vec<Id>,
    lock: File,
}

impl Drop for Queue {
    fn drop(&mut self) {
        // A concurrent fork must not extend ownership until exec.
        if let Err(error) = self.lock.unlock() {
            crate::diagnostic!("Failed to release crash report queue lock: {error}");
        }
    }
}

#[cfg(feature = "desktop")]
pub(crate) fn sender_lock(profile: &Path) -> Result<File> {
    let control = crate::paths::private_directory(profile, "control")?;
    let lock = profile::private_file(&control.join("crash-report-sender.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err("Crash reports are already being sent".into()),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

impl Queue {
    pub(crate) fn open(profile: &Path, channel: ReleaseChannel) -> Result<Self> {
        let control = crate::paths::private_directory(profile, "control")?;
        let lock = profile::private_file(&control.join("crash-reports.lock"))?;
        match lock.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => {
                return Err("Crash reports are already being handled".into());
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let directory = profile.join(DIRECTORY);
        profile::private_directory(&directory)?;
        let mut queue = Self { directory, channel, names: Vec::new(), lock };
        queue.inspect()?;
        Ok(queue)
    }

    fn inspect(&mut self) -> Result<()> {
        let entries = fs::read_dir(&self.directory)?
            .take(MAX_REPORTS + 1)
            .collect::<std::io::Result<Vec<_>>>()?;
        if entries.len() > MAX_REPORTS {
            return Err("Crash report queue exceeds its entry limit; reports are retained".into());
        }
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_str().ok_or("Invalid crash report name")?;
            if let Some(id) = name.strip_prefix(".pending-") {
                Id::parse(id)?;
                let file = open_record(&entry.path())?;
                drop(file);
                // A writer retains this same queue lock until publication.
                // With exclusive ownership, this is an interrupted write.
                fs::remove_file(entry.path())?;
            } else {
                let id = name
                    .strip_prefix("report-")
                    .and_then(|name| name.strip_suffix(".json"))
                    .ok_or("Unexpected entry in crash report queue")?;
                self.names.push(Id::parse(id)?);
            }
        }
        Ok(())
    }

    fn path(&self, id: Id) -> PathBuf {
        self.directory.join(format!("report-{id}.json"))
    }

    #[cfg(any(feature = "desktop", test))]
    pub(crate) fn read(&self, id: Id) -> Result<Report> {
        let file = open_record(&self.path(id))?;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("Crash report exceeds its byte limit".into());
        }
        let report: Report = serde_json::from_slice(&bytes)
            .map_err(|_| "Invalid local crash report; it has been retained")?;
        if report.schema != 1
            || report.channel != self.channel
            || report.account_id.is_some()
            || report.report.len() > crate::trace::diagnostic::REPORT_BYTES
            || report.build.len() > 128
            || report.process.len() > 64
        {
            return Err("Crash report does not belong to this environment".into());
        }
        Ok(report)
    }

    pub(crate) fn names(&self) -> &[Id] {
        &self.names
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn remove(&self, id: Id) -> Result<()> {
        fs::remove_file(self.path(id))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
}

fn open_record(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.len() > MAX_BYTES
        || metadata.uid() != profile::current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err("Unsafe local crash report".into());
    }
    Ok(file)
}

#[cfg(any(feature = "desktop", test))]
pub(crate) fn snapshot(profile: &Path, channel: ReleaseChannel) -> Result<Vec<Id>> {
    if !profile.join(DIRECTORY).try_exists()? {
        return Ok(Vec::new());
    }
    let queue = Queue::open(profile, channel)?;
    for id in queue.names() {
        queue.read(*id)?;
    }
    Ok(queue.names().to_vec())
}

pub(crate) fn write(profile: &Path, channel: ReleaseChannel, report: &str) -> Result<()> {
    if report.len() > crate::trace::diagnostic::REPORT_BYTES {
        return Err("Panic report exceeds its byte limit".into());
    }
    let queue = Queue::open(profile, channel)?;
    if queue.names().len() >= MAX_REPORTS {
        return Err("Crash report queue is full; existing reports are retained".into());
    }
    let mut process = std::env::current_exe()?
        .file_name()
        .ok_or("Missing process name")?
        .to_string_lossy()
        .into_owned();
    let mut end = process.len().min(64);
    while !process.is_char_boundary(end) {
        end -= 1;
    }
    process.truncate(end);
    let record = Report {
        schema: 1,
        channel: queue.channel,
        build: crate::VERSION.into(),
        process,
        pid: std::process::id(),
        timestamp_secs: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        account_id: None,
        report: report.into(),
    };
    let bytes = serde_json::to_vec(&record)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("Encoded panic report exceeds its byte limit".into());
    }
    let id = Id::generate()?;
    let pending = queue.directory.join(format!(".pending-{id}"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&pending)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(pending, queue.path(id))?;
    File::open(&queue.directory)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profiles_partition_reports_and_queue_mutations_are_exclusive() {
        let root = tempfile::tempdir().expect("profiles");
        let public = root.path().join("public");
        let testing = root.path().join("testing");
        fs::create_dir(&public).expect("public profile");
        fs::create_dir(&testing).expect("testing profile");
        write(&public, ReleaseChannel::Public, "public panic").expect("public report");
        write(&testing, ReleaseChannel::Testing, "testing panic").expect("testing report");
        let queue = Queue::open(&public, ReleaseChannel::Public).expect("queue ownership");
        assert!(Queue::open(&public, ReleaseChannel::Public).is_err());
        let report = queue.read(queue.names()[0]).expect("report");
        assert_eq!(report.report, "public panic");
        assert!(report.account_id.is_none());
        assert!(snapshot(&testing, ReleaseChannel::Public).is_err());
        assert_eq!(snapshot(&testing, ReleaseChannel::Testing).expect("testing reports").len(), 1);
    }
    #[test]
    fn interrupted_reports_recover_without_exceeding_the_queue_bound() {
        let root = tempfile::tempdir().expect("profile");
        let queue = Queue::open(root.path(), ReleaseChannel::Testing).expect("queue");
        let pending = queue.directory.join(format!(".pending-{}", Id::generate().expect("id")));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pending)
            .expect("interrupted write");
        // A concurrent fork can retain this file description until exec.
        let inherited = queue.lock.try_clone().expect("inherited queue lock");
        drop(queue);
        for _ in 0..MAX_REPORTS {
            write(root.path(), ReleaseChannel::Testing, "fixture panic").expect("bounded report");
        }
        assert!(write(root.path(), ReleaseChannel::Testing, "one more").is_err());
        assert_eq!(
            snapshot(root.path(), ReleaseChannel::Testing).expect("retained reports").len(),
            MAX_REPORTS
        );
        assert!(!pending.exists());
        drop(inherited);
    }
}
