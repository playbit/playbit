//! Private storage admission. POSIX mode alone does not exclude inherited ACLs.
#![allow(
    unsafe_code,
    reason = "Descriptor-based ACL and mode checks protect owned temporary storage."
)]
use std::{
    fs::File,
    io,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
};

/// Validate placement without changing a caller's directory or permissions.
pub fn directory(file: &File) -> io::Result<()> {
    let status = file.metadata()?;
    // SAFETY: geteuid has no arguments or memory effects.
    let owner = unsafe { libc::geteuid() };
    if !status.is_dir() || status.uid() != owner || status.mode() & 0o7777 != 0o700 {
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    if !acl::empty(file)? {
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    Ok(())
}

/// Existing control records live at fixed names inside the runtime-owned
/// private directory. Reject aliases and inherited access before reading or
/// removing an interrupted atomic-write control record.
pub(crate) fn control_file(file: &File) -> io::Result<()> {
    let status = file.metadata()?;
    // SAFETY: geteuid has no arguments or memory effects.
    let owner = unsafe { libc::geteuid() };
    if !status.is_file()
        || status.uid() != owner
        || status.nlink() != 1
        || status.mode() & 0o7777 != 0o600
        || !acl::empty(file)?
    {
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    Ok(())
}

/// Only newly created runtime-owned staging objects enter this function. They
/// must start with restrictive creation mode and contain no private bytes yet.
pub fn restrict_new(file: &File) -> io::Result<()> {
    let status = file.metadata()?;
    // SAFETY: geteuid has no arguments or memory effects.
    let owner = unsafe { libc::geteuid() };
    if status.uid() != owner || (!status.is_file() && !status.is_dir()) {
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    let mode = if status.is_dir() { 0o700 } else { 0o600 };
    if status.mode() & 0o7777 & !mode != 0 || status.is_file() && status.len() != 0 {
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    // iOS can deny setting an ACL even when the object already has none.
    // Only remove inherited entries; an unreadable ACL still rejects admission.
    if !acl::empty(file)? {
        acl::clear(file)?;
    }
    // SAFETY: retained owned descriptor, ordinary permission bits only.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if !acl::empty(file)? || file.metadata()?.mode() & 0o7777 != mode {
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
pub(crate) mod acl {
    use super::*;
    use std::ffi::{c_int, c_void};
    // sys/acl.h: Darwin extended ACLs are distinct from unsupported POSIX types.
    const EXTENDED: c_int = 0x100;
    unsafe extern "C" {
        fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
        fn acl_set_fd_np(fd: c_int, acl: *mut c_void, kind: c_int) -> c_int;
        fn acl_get_entry(acl: *mut c_void, entry: c_int, output: *mut *mut c_void) -> c_int;
        fn acl_valid(acl: *mut c_void) -> c_int;
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_free(value: *mut c_void) -> c_int;
    }
    struct Acl(*mut c_void);
    impl Acl {
        fn new(value: *mut c_void) -> io::Result<Self> {
            if value.is_null() { Err(io::Error::last_os_error()) } else { Ok(Self(value)) }
        }
    }
    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: this object owns the successful ACL allocation.
            unsafe {
                acl_free(self.0);
            }
        }
    }
    pub(crate) fn empty(file: &File) -> io::Result<bool> {
        // SAFETY: retained descriptor; returned allocation is owned by Acl.
        let raw = unsafe { acl_get_fd_np(file.as_raw_fd(), EXTENDED) };
        if raw.is_null() && io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            // Darwin returns ENOENT for an absent extended ACL. Confirm this
            // retained object still exists; other retrieval failures stay errors.
            return Ok(file.metadata()?.nlink() != 0);
        }
        let acl = Acl::new(raw)?;
        // SAFETY: ACL is live. Validation distinguishes an invalid ACL from an
        // empty valid ACL, whose first-entry lookup returns EINVAL on Darwin.
        if unsafe { acl_valid(acl.0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut entry = std::ptr::null_mut();
        // SAFETY: live validated ACL and exclusive output; 0 = ACL_FIRST_ENTRY.
        if unsafe { acl_get_entry(acl.0, 0, &mut entry) } == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINVAL) { Ok(true) } else { Err(error) }
    }
    pub(super) fn clear(file: &File) -> io::Result<()> {
        // SAFETY: allocate an empty ACL, owned until the native setter returns.
        let acl = Acl::new(unsafe { acl_init(0) })?;
        // SAFETY: retained descriptor and live empty ACL; no pointed guest data.
        if unsafe { acl_set_fd_np(file.as_raw_fd(), acl.0, EXTENDED) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
#[cfg(target_os = "linux")]
mod acl {
    use super::*;
    const NAMES: [&std::ffi::CStr; 2] = [c"system.posix_acl_access", c"system.posix_acl_default"];
    pub(super) fn empty(file: &File) -> io::Result<bool> {
        for name in NAMES {
            // SAFETY: retained descriptor, constant name, zero-size probe.
            if unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) }
                >= 0
            {
                return Ok(false);
            }
            if !matches!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ENODATA | libc::ENOTSUP)
            ) {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(true)
    }
    pub(super) fn clear(file: &File) -> io::Result<()> {
        for name in NAMES {
            // SAFETY: remove only access/default ACLs from new private storage.
            if unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) } < 0
                && !matches!(
                    io::Error::last_os_error().raw_os_error(),
                    Some(libc::ENODATA | libc::ENOTSUP)
                )
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert private temporary storage admission.")]
mod tests {
    use super::*;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    #[test]
    fn private_placement_requires_owner_mode_and_no_inherited_acl() {
        let fixture = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let dir = File::open(fixture.path()).unwrap();
        directory(&dir).unwrap();
        std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(directory(&dir).is_err());
        assert!(restrict_new(&dir).is_err(), "never narrow an existing broad grant implicitly");
        std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        #[cfg(target_vendor = "apple")]
        {
            let result = std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone allow read,search,file_inherit,directory_inherit"])
                .arg(fixture.path())
                .status()
                .unwrap();
            assert!(result.success());
            assert!(directory(&dir).is_err(), "mode 0700 does not mask a Darwin allow ACL");
        }
        restrict_new(&dir).unwrap();
        directory(&dir).unwrap();
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(fixture.path().join("staging"))
            .unwrap();
        restrict_new(&file).unwrap();
        assert_eq!(file.metadata().unwrap().mode() & 0o7777, 0o600);
        file.set_len(1).unwrap();
        assert!(restrict_new(&file).is_err(), "private bytes cannot precede ACL admission");
    }
}
