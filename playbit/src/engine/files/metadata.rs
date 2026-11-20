//! Worker-only, bounded preparation of metadata before atomic host publication.
//! Unknown access policy or annotations fail before the destination is changed.
#![allow(
    unsafe_code,
    reason = "Retained descriptors bind native ACL, xattr, ownership and flag operations."
)]
use std::{
    ffi::{CStr, CString},
    fs::File,
    io,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
};

const MAX_ATTRIBUTE_BYTES: usize = 64 * 1024;
const MAX_ATTRIBUTES: usize = 128;

pub struct Prepared {
    source: (u64, u64),
    uid: u32,
    gid: u32,
    mode: u32,
    flags: u32,
    attributes: Vec<(CString, Vec<u8>)>,
    #[cfg(target_vendor = "apple")]
    acl: access::Acl,
}
impl Prepared {
    /// The caller retains the destination binding and revalidates it at rename.
    pub fn capture(source: &File) -> io::Result<Self> {
        Self::capture_policy(source, false)
    }
    /// The caller exclusively creates this empty probe with mode 0666 in the
    /// destination directory. It captures umask and inherited ACLs without ever
    /// placing private contents in that broadly accessible temporary object.
    pub fn creation_policy(probe: &File) -> io::Result<Self> {
        let status = probe.metadata()?;
        // SAFETY: geteuid reads the effective owner of the newly created probe.
        if status.len() != 0 || status.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        Self::capture_policy(probe, true)
    }
    fn capture_policy(source: &File, creation: bool) -> io::Result<Self> {
        let status = source.metadata()?;
        if !status.is_file() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let flags = platform_flags::preserved(source, &status)?;
        let attributes = attributes(source, creation)?;
        #[cfg(target_vendor = "apple")]
        let acl = access::Acl::capture(source)?;
        Ok(Self {
            source: (status.dev(), status.ino()),
            uid: status.uid(),
            gid: status.gid(),
            mode: status.mode() & 0o777,
            flags,
            attributes,
            #[cfg(target_vendor = "apple")]
            acl,
        })
    }
    /// The staging object must remain in a private directory until publication:
    /// final permissions may allow other users to read its completed contents.
    pub fn apply(&self, staging: &File) -> io::Result<()> {
        let status = staging.metadata()?;
        // SAFETY: geteuid reads the current effective user identity.
        if !status.is_file()
            || status.uid() != unsafe { libc::geteuid() }
            || status.mode() & 0o7777 != 0o600
            || (status.dev(), status.ino()) == self.source
        {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        // Keep attributes the backend assigned to this fresh staging object.
        // They describe these new contents and are not copied from the source.
        // A copied annotation must not overwrite a different staging restriction.
        let assigned = attribute_names(staging)?;
        for (name, value) in &self.attributes {
            let present = assigned.split(|byte| *byte == 0).any(|part| part == name.as_bytes());
            if os_assigned(name) && !present {
                return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
            }
            if present {
                require_same_attribute(staging, name, value)?;
            }
        }
        if status.uid() != self.uid || status.gid() != self.gid {
            // SAFETY: only the private staging descriptor changes ownership.
            if unsafe { libc::fchown(staging.as_raw_fd(), self.uid, self.gid) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: only ordinary permissions are applied; setuid/setgid are omitted.
        if unsafe { libc::fchmod(staging.as_raw_fd(), self.mode as libc::mode_t) } < 0 {
            return Err(io::Error::last_os_error());
        }
        #[cfg(target_vendor = "apple")]
        self.acl.apply(staging)?;
        for (name, value) in &self.attributes {
            if !os_assigned(name) {
                xattr::set(staging, name, value)?;
            }
        }
        platform_flags::apply(staging, self.flags)?;
        // Metadata preparation itself can trigger OS policy. Verify provenance
        // again; never synthesize, remove, or overwrite the OS-assigned value.
        for (name, value) in &self.attributes {
            if os_assigned(name) {
                require_same_attribute(staging, name, value)?;
            }
        }
        let final_status = staging.metadata()?;
        if final_status.uid() != self.uid
            || final_status.gid() != self.gid
            || final_status.mode() & 0o7777 != self.mode
        {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        Ok(())
    }
}
fn os_assigned(name: &CStr) -> bool {
    cfg!(target_vendor = "apple") && name.to_bytes() == b"com.apple.provenance"
}
fn require_same_attribute(file: &File, name: &CStr, expected: &[u8]) -> io::Result<()> {
    let size = xattr::get(file, name, &mut [])?;
    if size != expected.len() {
        return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
    }
    let mut actual = vec![0; size];
    if xattr::get(file, name, &mut actual)? != size || actual != expected {
        return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
    }
    Ok(())
}
fn attribute_names(file: &File) -> io::Result<Vec<u8>> {
    let size = xattr::list(file, &mut [])?;
    if size > MAX_ATTRIBUTE_BYTES {
        return Err(io::Error::from_raw_os_error(libc::E2BIG));
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut names = vec![0; size];
    let count = xattr::list(file, &mut names)?;
    if count > names.len() {
        return Err(io::Error::from_raw_os_error(libc::E2BIG));
    }
    names.truncate(count);
    if names.last() != Some(&0) {
        return Err(io::Error::from_raw_os_error(libc::EIO));
    }
    Ok(names)
}
fn attributes(file: &File, creation: bool) -> io::Result<Vec<(CString, Vec<u8>)>> {
    let names = attribute_names(file)?;
    let mut used = names.len();
    let mut attributes = Vec::new();
    if names.is_empty() {
        return Ok(attributes);
    }
    for (index, name) in names[..names.len() - 1].split(|byte| *byte == 0).enumerate() {
        if index == MAX_ATTRIBUTES || name.is_empty() || name.len() > 255 {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        // This is provenance of our empty creation probe, not destination
        // creation policy. Fresh staging retains its own OS-assigned provenance.
        // Existing destinations always use strict replacement classification.
        if creation && cfg!(target_vendor = "apple") && name == b"com.apple.provenance" {
            continue;
        }
        let preserve = preserve_attribute(name)?;
        if !preserve {
            continue;
        }
        let name = CString::new(name)?;
        let size = xattr::get(file, &name, &mut [])?;
        if size > MAX_ATTRIBUTE_BYTES - used {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        let mut value = vec![0; size];
        if size != 0 {
            let count = xattr::get(file, &name, &mut value)?;
            if count > value.len() {
                return Err(io::Error::from_raw_os_error(libc::E2BIG));
            }
            value.truncate(count);
        }
        // Charge capacity even when the attribute shrank between native calls.
        used += size;
        attributes.push((name, value));
    }
    Ok(attributes)
}
#[cfg(target_vendor = "apple")]
fn preserve_attribute(name: &[u8]) -> io::Result<bool> {
    match name {
        b"com.apple.metadata:_kMDItemUserTags"
        | b"com.apple.metadata:kMDItemFinderComment"
        | b"com.apple.quarantine"
        | b"com.apple.provenance" => Ok(true),
        b"com.apple.TextEncoding" => Ok(false),
        _ => Err(io::Error::from_raw_os_error(libc::ENOTSUP)),
    }
}
#[cfg(target_os = "linux")]
fn preserve_attribute(name: &[u8]) -> io::Result<bool> {
    match name {
        // The kernel ACL API validates the exact binary ACL and its mask on
        // fsetxattr. Do not interpret, reconstruct, or normalize ACL entries.
        b"system.posix_acl_access" => Ok(true),
        b"security.capability" => Ok(false),
        // SELinux requires a dedicated handler; labels are never silently lost.
        _ => Err(io::Error::from_raw_os_error(libc::ENOTSUP)),
    }
}
mod xattr {
    use super::*;
    fn count(result: libc::ssize_t) -> io::Result<usize> {
        if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result as usize) }
    }
    pub(super) fn list(file: &File, bytes: &mut [u8]) -> io::Result<usize> {
        // SAFETY: a live descriptor and exclusive output, whose size is exact.
        #[cfg(target_vendor = "apple")]
        let result = unsafe {
            libc::flistxattr(file.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len(), 0)
        };
        #[cfg(target_os = "linux")]
        let result =
            unsafe { libc::flistxattr(file.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        if result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTSUP) {
            return Ok(0);
        }
        count(result)
    }
    pub(super) fn get(file: &File, name: &CStr, bytes: &mut [u8]) -> io::Result<usize> {
        // SAFETY: a live descriptor, terminated name and exclusive bounded output.
        #[cfg(target_vendor = "apple")]
        let result = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                0,
                0,
            )
        };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), bytes.as_mut_ptr().cast(), bytes.len())
        };
        count(result)
    }
    pub(super) fn set(file: &File, name: &CStr, bytes: &[u8]) -> io::Result<()> {
        // SAFETY: a live private descriptor, terminated name and bounded input.
        #[cfg(target_vendor = "apple")]
        let result = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
                0,
            )
        };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::fsetxattr(file.as_raw_fd(), name.as_ptr(), bytes.as_ptr().cast(), bytes.len(), 0)
        };
        count(result as libc::ssize_t).map(|_| ())
    }
}
#[cfg(target_vendor = "apple")]
mod platform_flags {
    use super::*;
    use std::os::darwin::fs::MetadataExt;
    const PRESERVE: u32 = libc::UF_HIDDEN | libc::UF_NODUMP;
    // sys/stat.h: SF_DATALESS is a read-only synthetic materialization flag.
    const INTERNAL: u32 = libc::UF_COMPRESSED | 0x40000000;
    pub(super) fn preserved(_: &File, status: &std::fs::Metadata) -> io::Result<u32> {
        let flags = status.st_flags();
        if flags & !(PRESERVE | INTERNAL) != 0 {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
        }
        Ok(flags & PRESERVE)
    }
    pub(super) fn apply(file: &File, flags: u32) -> io::Result<()> {
        let existing = file.metadata()?.st_flags();
        if existing & !(PRESERVE | INTERNAL) != 0 {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
        }
        let desired = (existing & INTERNAL) | flags;
        if desired == existing {
            return Ok(());
        }
        // SAFETY: retain backend state and apply only classified user-intent flags.
        if unsafe { libc::fchflags(file.as_raw_fd(), desired) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
#[cfg(target_os = "linux")]
mod platform_flags {
    use super::*;
    const PRESERVE: u32 = 0x00000008 | 0x00000040 | 0x00000080; // SYNC, NODUMP, NOATIME
    const INTERNAL: u32 = 0x00040000 | 0x00080000 | 0x10000000; // HUGE_FILE, EXTENTS, INLINE_DATA
    fn read(file: &File) -> io::Result<Option<u32>> {
        let mut flags: libc::c_int = 0;
        // SAFETY: FS_IOC_GETFLAGS writes one int (the request encodes long).
        if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) } < 0 {
            let error = io::Error::last_os_error();
            return if matches!(error.raw_os_error(), Some(libc::ENOTTY | libc::ENOTSUP)) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some(flags as u32))
    }
    pub(super) fn preserved(file: &File, _: &std::fs::Metadata) -> io::Result<u32> {
        let flags = read(file)?.unwrap_or(0);
        if flags & !(PRESERVE | INTERNAL) != 0 {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
        }
        Ok(flags & PRESERVE)
    }
    pub(super) fn apply(file: &File, flags: u32) -> io::Result<()> {
        let Some(existing) = read(file)? else {
            return if flags == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(libc::ENOTSUP))
            };
        };
        if existing & !(PRESERVE | INTERNAL) != 0 {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
        }
        let desired = (existing & INTERNAL) | flags;
        if desired == existing {
            return Ok(());
        }
        let desired = desired as libc::c_int;
        // SAFETY: only staging changes; retain backend layout and classified policy.
        if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS, &desired) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
#[cfg(target_vendor = "apple")]
mod access {
    use super::*;
    use std::ffi::{c_int, c_void};
    const EXTENDED: c_int = 0x100;
    unsafe extern "C" {
        fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
        fn acl_set_fd_np(fd: c_int, acl: *mut c_void, kind: c_int) -> c_int;
        fn acl_get_entry(acl: *mut c_void, entry: c_int, output: *mut *mut c_void) -> c_int;
        fn acl_valid(acl: *mut c_void) -> c_int;
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_free(value: *mut c_void) -> c_int;
    }
    pub(super) struct Acl(*mut c_void);
    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: this object owns one successful native allocation.
            unsafe {
                acl_free(self.0);
            }
        }
    }
    impl Acl {
        pub(super) fn capture(file: &File) -> io::Result<Self> {
            // SAFETY: retained descriptor; successful allocation is owned below.
            let mut pointer = unsafe { acl_get_fd_np(file.as_raw_fd(), EXTENDED) };
            if pointer.is_null() {
                if io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: allocate a valid empty ACL for an absent source ACL.
                pointer = unsafe { acl_init(0) };
                if pointer.is_null() {
                    return Err(io::Error::last_os_error());
                }
            }
            let acl = Self(pointer);
            // SAFETY: live allocation; validate before interpreting end-of-list.
            if unsafe { acl_valid(acl.0) } < 0 {
                return Err(io::Error::last_os_error());
            }
            for index in 0..=MAX_ATTRIBUTES {
                let mut entry = std::ptr::null_mut();
                // SAFETY: valid ACL and exclusive output; FIRST=0, NEXT=-1.
                if unsafe { acl_get_entry(acl.0, if index == 0 { 0 } else { -1 }, &mut entry) } < 0
                {
                    let error = io::Error::last_os_error();
                    return if error.raw_os_error() == Some(libc::EINVAL) {
                        Ok(acl)
                    } else {
                        Err(error)
                    };
                }
            }
            Err(io::Error::from_raw_os_error(libc::E2BIG))
        }
        pub(super) fn apply(&self, file: &File) -> io::Result<()> {
            // SAFETY: retained staging descriptor and validated captured ACL.
            if unsafe { acl_set_fd_np(file.as_raw_fd(), self.0, EXTENDED) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests verify metadata on isolated host fixtures.")]
mod tests {
    use super::*;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    #[test]
    fn creation_policy_preserves_access_without_mutating_probe_or_transferring_privilege() {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        std::fs::write(&source_path, []).unwrap();
        // macOS retains setgid only when the fixture uses one of our groups.
        // SAFETY: getegid only reads this test process's effective group identity.
        let group = unsafe { libc::getegid() };
        std::os::unix::fs::chown(&source_path, None, Some(group)).unwrap();
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o6751)).unwrap();
        let source = File::open(&source_path).unwrap();
        let prepared = Prepared::creation_policy(&source).unwrap();
        let staging_path = fixture.path().join("staging");
        let staging = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging_path)
            .unwrap();
        super::super::privacy::restrict_new(&staging).unwrap();
        std::fs::write(&staging_path, b"replacement").unwrap();
        prepared.apply(&staging).unwrap();
        assert_eq!(staging.metadata().unwrap().mode() & 0o7777, 0o751);
        assert_eq!(staging.metadata().unwrap().uid(), source.metadata().unwrap().uid());
        assert_eq!(staging.metadata().unwrap().gid(), source.metadata().unwrap().gid());
        assert_eq!(source.metadata().unwrap().mode() & 0o7777, 0o6751);
        assert!(std::fs::read(&source_path).unwrap().is_empty());
        assert!(prepared.apply(&source).is_err());
    }
    #[cfg(target_vendor = "apple")]
    #[test]
    fn replacement_requires_existing_identical_os_provenance() {
        let fixture = tempfile::tempdir().unwrap();
        let source = File::create(fixture.path().join("source")).unwrap();
        let mut prepared = Prepared::capture(&source).unwrap();
        let staging = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(fixture.path().join("staging"))
            .unwrap();
        super::super::privacy::restrict_new(&staging).unwrap();
        // Synthetic captured metadata exercises absent and different staging
        // provenance without trying to forge or change the protected OS xattr.
        prepared.attributes.retain(|(name, _)| !os_assigned(name));
        let name = c"com.apple.provenance";
        let expected = match xattr::get(&staging, name, &mut []) {
            Ok(size) => {
                let mut value = vec![0; size];
                assert_eq!(xattr::get(&staging, name, &mut value).unwrap(), size);
                value.push(0);
                value
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOATTR) => vec![1],
            Err(error) => panic!("could not inspect staging provenance: {error}"),
        };
        prepared.attributes.push((name.to_owned(), expected));
        assert_eq!(prepared.apply(&staging).unwrap_err().raw_os_error(), Some(libc::ENOTSUP));
        assert_eq!(staging.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(source.metadata().unwrap().len(), 0);
    }
    #[cfg(target_vendor = "apple")]
    #[test]
    fn annotations_acl_and_restrictive_metadata_have_explicit_policies() {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        std::fs::write(&source_path, []).unwrap();
        let source = File::open(&source_path).unwrap();
        let result = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone deny write"])
            .arg(&source_path)
            .status()
            .unwrap();
        assert!(result.success());
        xattr::set(&source, c"com.apple.quarantine", b"0081;fixture").unwrap();
        xattr::set(&source, c"com.apple.metadata:_kMDItemUserTags", b"tags").unwrap();
        xattr::set(&source, c"com.apple.TextEncoding", b"UTF-8;134217984").unwrap();
        let prepared = Prepared::capture(&source).unwrap();
        let staging = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(fixture.path().join("staging"))
            .unwrap();
        super::super::privacy::restrict_new(&staging).unwrap();
        prepared.apply(&staging).unwrap();
        for (name, value) in &prepared.attributes {
            let mut actual = vec![0; value.len()];
            assert_eq!(xattr::get(&staging, name, &mut actual).unwrap(), value.len());
            assert_eq!(&actual, value);
        }
        assert!(
            !attribute_names(&staging)
                .unwrap()
                .windows(b"TextEncoding".len())
                .any(|part| part == b"TextEncoding")
        );
        // Applying the preserved ACL prevents a fresh write open by this user.
        assert!(File::options().write(true).open(fixture.path().join("staging")).is_err());
        xattr::set(&source, c"com.playbit.unknown", b"policy").unwrap();
        assert!(Prepared::capture(&source).is_err());
        assert!(Prepared::creation_policy(&source).is_err());
        assert!(std::fs::read(source_path).unwrap().is_empty());
    }
}
