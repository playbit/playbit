//! One source-side intent, retained until Git succeeds. The store remains the
//! sole owner of automatic-volume transactions and receipt validation. No source file is
//! replaced during recovery; only missing, planned files may be published.
use super::*;
use serde::{Deserialize, Serialize};
use std::{io::Write, path::Component};

const INTENT: &str = ".pb-init.json";
// JSON byte arrays need at most four bytes per template byte, plus bounded paths.
const MAX_INTENT_BYTES: usize = 3 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    profile: PathBuf,
    prepared: bool,
    files: BTreeMap<PathBuf, Vec<u8>>,
}

pub(crate) struct Initialization<'a> {
    directory: &'a Path,
    intent: Intent,
    // Lock the directory inode, not the replaceable intent. It survives rename
    // and is released by the OS on process exit; no stale lock-file recovery.
    _lock: DirectoryLock,
}

struct DirectoryLock(fs::File);
impl Drop for DirectoryLock {
    fn drop(&mut self) {
        // Another thread may fork while we hold this descriptor. Explicitly
        // release the shared lock even if that child has not reached exec yet.
        let _ = self.0.unlock();
    }
}

impl<'a> Initialization<'a> {
    #[cfg(test)]
    pub(crate) fn begin(
        directory: &'a Path,
        name: Option<&str>,
        executable: &Path,
        profile: &Path,
    ) -> Result<Self> {
        Self::begin_with_language(directory, name, None, None, executable, profile)
    }

    pub(crate) fn begin_with_language(
        directory: &'a Path,
        name: Option<&str>,
        app_id: Option<&str>,
        language: Option<playbit::package::Language>,
        executable: &Path,
        profile: &Path,
    ) -> Result<Self> {
        let lock = fs::File::open(directory)?;
        lock.try_lock().map_err(|e| {
            format!("another pb init may be running here: {e}; retry after it exits")
        })?;
        let lock = DirectoryLock(lock);
        let profile = std::path::absolute(profile)?;
        let path = directory.join(INTENT);
        let retained = regular_file(&path)?;
        let intent = if retained {
            let bytes = read_bounded(&path, MAX_INTENT_BYTES)?;
            serde_json::from_slice(&bytes).map_err(|e| {
                format!(
                    "cannot read retained {INTENT}: {e}. Preserve this directory and restore the \
                     intent from backup; do not start another initialization."
                )
            })?
        } else {
            if fs::read_dir(directory)?.next().is_some() {
                return Err("pb init requires an empty source directory or its retained \
                    .pb-init.json; this directory was not changed. \
                    Use the completed project with pb build/run, \
                    or choose an empty directory for a new project."
                    .into());
            }
            let resolved = directory.canonicalize()?;
            let directory_name = resolved.file_name().and_then(|name| name.to_str());
            let name = name
                .or(directory_name)
                .ok_or("directory name is not valid UTF-8; provide --name")?;
            let app_id = match app_id {
                Some(app_id) => app_id.to_owned(),
                None => directory_app_id(
                    directory_name.ok_or("directory name is not valid UTF-8; provide --app-id")?,
                ),
            };
            let files = template(
                name,
                &app_id,
                executable,
                language.unwrap_or(playbit::package::Language::Rust),
            )?;
            Intent { profile: profile.clone(), prepared: false, files }
        };
        let value = Self { directory, intent, _lock: lock };
        value.check_intent().map_err(|error| {
            if retained {
                value.recovery_error(error)
            } else {
                format!(
                    "invalid initialization template or profile: {error}. \
                     No project files or root were created; \
                     fix the reported problem, then retry pb init."
                )
            }
        })?;
        if value.intent.profile != profile {
            return Err(format!(
                "Retained initialization uses profile {}. \
                 Retry pb init with that PLAYBIT_PROFILE; no files or volumes were changed.",
                value.intent.profile.display()
            )
            .into());
        }
        if name.is_some_and(|name| value.name().is_ok_and(|original| name != original)) {
            return Err(format!(
                "Retained initialization is named {:?}. \
                 Retry `pb init` without a name to resume it; \
                 rename the project after completion.",
                value.name()?
            )
            .into());
        }
        if app_id.is_some_and(|app_id| {
            value.original_manifest().is_ok_and(|manifest| app_id != manifest.app_id)
        }) {
            return Err(
                "retained initialization uses a different app_id; retry without --app-id".into()
            );
        }
        if language.is_some_and(|lang| {
            value
                .original_manifest()
                .is_ok_and(|m| m.lang.unwrap_or(playbit::package::Language::Rust) != lang)
        }) {
            return Err(
                "retained initialization uses a different language; retry without --lang".into()
            );
        }
        // An atomic, synced intent is published before any project file or IPC.
        if !retained {
            publish(&path, &serde_json::to_vec(&value.intent)?, false).map_err(|e| {
                format!(
                    "saving initialization identity failed: {e}. No root was requested. \
                     Retain this directory and retry pb init."
                )
            })?;
        }
        #[cfg(test)]
        checkpoint(INTENT).map_err(|e| value.recovery_error(e))?;
        Ok(value)
    }

    fn check_intent(&self) -> Result<()> {
        let files = &self.intent.files;
        if files.is_empty()
            || files.len() > MAX_TEMPLATE_ENTRIES
            || files.values().map(Vec::len).sum::<usize>() > MAX_TEMPLATE_BYTES
            || !self.intent.profile.is_absolute()
            || self.intent.profile.as_os_str().len() > MAX_PATH_BYTES
        {
            return Err("invalid initialization intent bounds or profile".into());
        }
        for path in files.keys() {
            let depth = path.components().count();
            if depth == 0
                || depth > MAX_TEMPLATE_DEPTH + 1
                || path.as_os_str().len() > MAX_PATH_BYTES
                || path.components().any(|c| !matches!(c, Component::Normal(_)))
                || [INTENT, ".git", "target"].iter().any(|prefix| path.starts_with(prefix))
            {
                return Err("invalid initialization file path".into());
            }
        }
        self.bytes(".gitignore")?;
        let manifest = self.original_manifest()?;
        self.bytes(manifest.icon.as_deref().ok_or("project template is missing its icon path")?)?;
        if manifest.lang == Some(playbit::package::Language::Ocaml) {
            self.bytes("src/app.ml")?;
        } else {
            for required in ["Cargo.toml", "src/app.rs", ".cargo/config.toml"] {
                self.bytes(required)?;
            }
        }
        playbit::package::VolumeInitializers::from_json(
            self.bytes(playbit::package::VOLUME_INITIALIZERS_FILE)?,
        )?;
        Ok(())
    }

    fn bytes(&self, path: &str) -> Result<&[u8]> {
        self.intent
            .files
            .get(Path::new(path))
            .map(Vec::as_slice)
            .ok_or_else(|| format!("retained initialization is missing {path}").into())
    }

    fn original_manifest(&self) -> Result<Manifest> {
        Manifest::from_source(std::str::from_utf8(self.bytes("playbit.toml")?)?)
    }

    pub(crate) fn name(&self) -> Result<String> {
        Ok(self.original_manifest()?.name)
    }

    pub(crate) fn scaffold(&mut self) -> Result<()> {
        if self.intent.prepared {
            return Ok(());
        }
        for (relative, bytes) in &self.intent.files {
            let path = self.directory.join(relative);
            prepare_parents(self.directory, relative)
                .map_err(|error| format!("preparing {}: {error}", relative.display()))?;
            if regular_file(&path)? {
                if read_bounded(&path, MAX_TEMPLATE_BYTES)? != *bytes {
                    return Err(format!(
                        "{} conflicts with the retained template. \
                         Move that file aside, retry pb init, then reapply your edits; \
                         it has not been overwritten.",
                        relative.display()
                    )
                    .into());
                }
                // A previous publish may have failed at fsync, or the user may
                // have restored this file. Establish durability before ready.
                fs::File::open(&path)?.sync_all()?;
                sync_directory(path.parent().ok_or("missing file parent")?)?;
            } else {
                publish(&path, bytes, false)
                    .map_err(|error| format!("creating {}: {error}", relative.display()))?;
            }
            #[cfg(test)]
            checkpoint(relative.to_str().expect("template path is UTF-8"))?;
        }
        // After every planned file is durable, edits are allowed. Root identity
        // and declaration still have to agree before every initialization request.
        self.intent.prepared = true;
        publish(&self.directory.join(INTENT), &serde_json::to_vec(&self.intent)?, true)?;
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(Manifest, playbit::store::RootInitialization)> {
        for relative in self.intent.files.keys() {
            check_parents(self.directory, relative)?;
            if !regular_file(&self.directory.join(relative))? {
                return Err(format!(
                    "{} is missing from the prepared project; \
                     restore it from backup or .pb-init.json before retrying pb init",
                    relative.display()
                )
                .into());
            }
        }
        let manifest = Manifest::read(self.directory)?;
        if manifest.app_id != self.original_manifest()?.app_id {
            return Err("playbit.toml app_id differs from the retained identity; \
                restore its original app_id before retrying"
                .into());
        }
        let original = playbit::package::VolumeInitializers::from_json(
            self.bytes(playbit::package::VOLUME_INITIALIZERS_FILE)?,
        )?;
        let current = playbit::package::read_volume_initializers(self.directory)?;
        if serde_json::to_vec(&current)? != serde_json::to_vec(&original)? {
            return Err("playbit-volumes.json differs from the retained declaration; \
                move your edited declaration aside and restore the original from \
                .pb-init.json before retrying"
                .into());
        }
        // Generated metadata is disposable, but must never traverse user links.
        check_parents(self.directory, Path::new("target/playbit/metadata.rs"))?;
        regular_file(&self.directory.join("target/playbit/metadata.rs"))?;
        let git = self.directory.join(".git");
        if let Ok(info) = fs::symlink_metadata(&git)
            && !info.is_dir()
        {
            return Err(
                ".git must be a directory, not a link or file; move it aside before retrying"
                    .into(),
            );
        }
        Ok((manifest, current.for_role(playbit::package::AutomaticVolumeRole::Root)))
    }

    pub(crate) fn finish(&self) -> Result<()> {
        (|| -> Result<()> {
            fs::remove_file(self.directory.join(INTENT))?;
            sync_directory(self.directory)
        })()
        .map_err(|error| {
            format!(
                "Root and Git initialization succeeded, but finalizing {INTENT} failed: \
                 {error}. If the record remains, retry pb init here with the original \
                 profile; otherwise the project is complete, use pb build/run."
            )
            .into()
        })
    }

    pub(crate) fn recovery_error(&self, error: impl std::fmt::Display) -> String {
        let app = self
            .original_manifest()
            .map(|manifest| manifest.app_id)
            .unwrap_or_else(|_| "unknown".into());
        format!(
            "{error}\nInitialization retained in {} (app {app}). Keep .pb-init.json and your files; fix the reported problem, then run `pb init` in this same directory with the original profile. Its publisher root may already exist; retry retains that data.",
            self.directory.display()
        )
    }
}

fn directory_app_id(name: &str) -> String {
    let mut app_id = String::new();
    for byte in name.bytes().map(|byte| byte.to_ascii_lowercase()) {
        let separator = matches!(byte, b'.' | b'-')
            && app_id.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric);
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || separator {
            app_id.push(char::from(byte));
        }
    }
    if app_id.ends_with(['.', '-']) {
        app_id.pop();
    }
    app_id
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(format!("{} exceeds {limit} bytes", path.display()).into());
    }
    Ok(bytes)
}

fn regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_file() => Ok(true),
        Ok(_) => Err(format!(
            "{} must be a regular file, not a link or directory; \
             move the conflicting entry aside before retrying",
            path.display()
        )
        .into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn check_parents(directory: &Path, relative: &Path) -> Result<()> {
    let mut path = directory.to_path_buf();
    let parent = relative.parent().ok_or("missing file parent")?;
    for component in parent.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(info) if info.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "{} must be a directory, not a link or file; move it aside before retrying",
                    path.display()
                )
                .into());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn prepare_parents(directory: &Path, relative: &Path) -> Result<()> {
    check_parents(directory, relative)?;
    let mut path = directory.to_path_buf();
    for component in relative.parent().ok_or("missing file parent")?.components() {
        path.push(component);
        if !path.exists() {
            fs::create_dir(&path)?;
        }
        sync_directory(path.parent().ok_or("missing directory parent")?)?;
    }
    Ok(())
}

fn sync_directory(directory: &Path) -> Result<()> {
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

fn publish(path: &Path, bytes: &[u8], replace_intent: bool) -> Result<()> {
    let parent = path.parent().ok_or("missing destination parent")?;
    // Staging outside the source means a crash before publishing the first
    // intent leaves the empty source retryable.
    let staging_directory = if path.file_name() == Some(std::ffi::OsStr::new(INTENT)) {
        parent.parent().ok_or("initialization needs a writable source parent directory")?
    } else {
        parent
    };
    let staging = staging_directory.join(format!(".pb-init-{}.tmp", Id::generate()?));
    #[cfg(test)]
    if INTERRUPT.with_borrow(|point| point == "occupied staging") {
        fs::write(&staging, b"preexisting user file")?;
    }
    // Only enter cleanup after exclusive creation establishes ownership.
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&staging)?;
    let result = (|| -> Result<()> {
        #[cfg(test)]
        if let Err(error) = checkpoint(&format!(
            "writing:{}",
            path.file_name().expect("file name").to_string_lossy()
        )) {
            file.write_all(&bytes[..bytes.len() / 2])?;
            return Err(error);
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        if replace_intent {
            assert_eq!(path.file_name(), Some(std::ffi::OsStr::new(INTENT)));
            fs::rename(&staging, path)?;
        } else {
            fs::hard_link(&staging, path)?;
        }
        #[cfg(test)]
        checkpoint(&format!(
            "published:{}",
            path.file_name().expect("file name").to_string_lossy()
        ))?;
        sync_directory(parent)
    })();
    let _ = fs::remove_file(staging);
    result
}

#[cfg(test)]
thread_local! { static INTERRUPT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) }; }
#[cfg(test)]
fn checkpoint(path: &str) -> Result<()> {
    if INTERRUPT.with_borrow(|interrupt| interrupt == path) {
        return Err("injected file publication failure".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
