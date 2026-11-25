//! A profile-bound process endpoint survives deletion of the profile itself.
//! It serves no requests: the GUI uses kernel peer credentials to retire an
//! orphan before reopening. A lifetime lease prevents overlapping engines.
use super::{Result, process_identity::Peer, profile};
use crate::release_channel::ReleaseChannel;
use std::{
    fs::{self, File, TryLockError},
    io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::Duration,
};

pub(super) struct Endpoint {
    lease: File,
    listener: Option<UnixListener>,
    path: PathBuf,
    identity: Option<(u64, u64)>,
}

fn path(profile: &Path) -> Result<PathBuf> {
    let profile = profile::canonical_profile(profile)?;
    let hash = blake3::hash(profile.as_os_str().as_encoded_bytes());
    let name = crate::identifier::encode(hash.as_bytes());
    // Unix socket paths are short. A fixed, owner-only runtime directory also
    // works for long home paths and profiles on another filesystem.
    Ok(PathBuf::from(format!("/tmp/playbit-{}", profile::current_uid())).join(name))
}

impl Endpoint {
    pub(super) fn reserve(profile: &Path) -> Result<Self> {
        let path = path(profile)?;
        profile::private_directory(path.parent().expect("runtime directory"))?;
        let lease = profile::private_file(&path.with_extension("lock"))?;
        match lease.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => {
                return Err("A Playbit engine is still running for this profile. \
                    Reopen Playbit to finish recovery."
                    .into());
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        Ok(Self { lease, listener: None, path, identity: None })
    }

    pub(super) fn listen(&mut self) -> Result<()> {
        assert!(self.listener.is_none(), "recovery endpoint is published once");
        match fs::symlink_metadata(&self.path) {
            Ok(metadata)
                if metadata.file_type().is_socket() && metadata.uid() == profile::current_uid() =>
            {
                fs::remove_file(&self.path)?;
            }
            Ok(_) => return Err("Unsafe engine recovery endpoint".into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&self.path)?;
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        let metadata = fs::symlink_metadata(&self.path)?;
        self.identity = Some((metadata.dev(), metadata.ino()));
        self.listener = Some(listener);
        Ok(())
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && Some((metadata.dev(), metadata.ino())) == self.identity
            && let Err(error) = fs::remove_file(&self.path)
        {
            crate::diagnostic!("Failed to remove engine recovery endpoint: {error}");
        }

        // A concurrent fork must not extend ownership until its descriptor closes at exec.
        if let Err(error) = self.lease.unlock() {
            crate::diagnostic!("Failed to release engine recovery lease: {error}");
        }
    }
}

/// Only a broken endpoint or channel binding requires retirement. Healthy
/// engines continue running across ordinary GUI launches.
fn stale(profile: &Path, channel: ReleaseChannel) -> Result<bool> {
    let socket = profile.join(crate::paths::ENGINE_SOCKET);
    match fs::symlink_metadata(&socket) {
        Ok(metadata) => {
            Ok(!metadata.file_type().is_socket() || profile::for_profile(profile)? != channel)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn retire_if_stale(profile: &Path, channel: ReleaseChannel) -> Result<()> {
    if !stale(profile, channel)? {
        return Ok(());
    }
    let path = path(profile)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata)
            if metadata.file_type().is_socket() && metadata.uid() == profile::current_uid() => {}
        Ok(_) => return Err("Unsafe engine recovery endpoint".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    profile::private_directory(path.parent().expect("runtime directory"))?;
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    match socket.connect(&socket2::SockAddr::unix(&path)?) {
        Ok(()) => (),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }
    let stream: UnixStream = socket.into();
    let peer = match Peer::capture(&stream) {
        Ok(peer) => peer,
        Err(error) if super::launch::peer_vanished(error.as_ref()) => return Ok(()),
        Err(error) => return Err(error),
    };
    match peer.require_engine() {
        Ok(()) => (),
        Err(error) if super::launch::peer_vanished(error.as_ref()) => return Ok(()),
        Err(error) => return Err(error),
    }
    // Another launcher may have finished recovery since our first check.
    // The lifetime lease prevents replacement of this authenticated owner
    // until it exits; never retire it using an earlier owner's stale state.
    if !stale(profile, channel)? {
        return Ok(());
    }
    drop(stream);
    match peer.stop(Duration::from_secs(3)) {
        Err(error) if super::launch::peer_vanished(error.as_ref()) => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests;
