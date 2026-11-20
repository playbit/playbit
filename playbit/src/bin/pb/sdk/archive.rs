//! libarchive only decodes the verified archive. File creation and bounds are ours.
#![allow(
    unsafe_code,
    reason = "Narrow system libarchive reader FFI; all output paths and sizes are admitted in Rust."
)]
use playbit::engine::Result;
use std::{
    collections::BTreeSet,
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs,
    io::Write,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Component, Path},
};

#[link(name = "archive")]
unsafe extern "C" {
    fn archive_read_new() -> *mut c_void;
    fn archive_read_support_filter_xz(a: *mut c_void) -> c_int;
    fn archive_read_support_format_tar(a: *mut c_void) -> c_int;
    fn archive_read_open_filename(a: *mut c_void, name: *const c_char, block: usize) -> c_int;
    fn archive_read_next_header(a: *mut c_void, entry: *mut *mut c_void) -> c_int;
    fn archive_read_data(a: *mut c_void, bytes: *mut c_void, count: usize) -> isize;
    fn archive_read_free(a: *mut c_void) -> c_int;
    fn archive_entry_pathname(entry: *mut c_void) -> *const c_char;
    fn archive_entry_filetype(entry: *mut c_void) -> libc::mode_t;
    fn archive_entry_size(entry: *mut c_void) -> i64;
    fn archive_entry_perm(entry: *mut c_void) -> libc::mode_t;
    fn archive_entry_symlink(entry: *mut c_void) -> *const c_char;
    fn archive_entry_hardlink(entry: *mut c_void) -> *const c_char;
}
struct Reader(*mut c_void);
impl Drop for Reader {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                archive_read_free(self.0);
            }
        }
    }
}

pub(super) fn extract(source: &Path, destination: &Path, expanded: u64) -> Result<()> {
    let reader = Reader(unsafe { archive_read_new() });
    if reader.0.is_null() {
        return Err("cannot allocate SDK archive reader".into());
    }
    let filename = CString::new(source.as_os_str().as_bytes())?;
    if unsafe { archive_read_support_filter_xz(reader.0) } != 0
        || unsafe { archive_read_support_format_tar(reader.0) } != 0
        || unsafe { archive_read_open_filename(reader.0, filename.as_ptr(), 65536) } != 0
    {
        return Err("cannot open SDK tar.xz archive".into());
    }
    fs::create_dir(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    let mut entries = BTreeSet::new();
    let mut spellings = std::collections::BTreeMap::new();
    let mut total = 0u64;
    let mut path_bytes = 0usize;
    let mut buffer = [0u8; 65536];
    loop {
        let mut entry = std::ptr::null_mut();
        let status = unsafe { archive_read_next_header(reader.0, &mut entry) };
        if status == 1 {
            break;
        }
        if status != 0 || entry.is_null() {
            return Err("invalid SDK archive header".into());
        }
        let pointer = unsafe { archive_entry_pathname(entry) };
        if pointer.is_null() {
            return Err("SDK archive entry has no path".into());
        }
        let name = unsafe { CStr::from_ptr(pointer) }.to_str()?.trim_end_matches('/');
        path_bytes = path_bytes.checked_add(name.len()).ok_or("SDK path size overflow")?;
        if path_bytes > 8 * 1024 * 1024 {
            return Err("SDK path metadata exceeds 8 MiB".into());
        }
        let path = Path::new(name);
        let kind = unsafe { archive_entry_filetype(entry) };
        let size = unsafe { archive_entry_size(entry) };
        if name.len() > 1024
            || !name.is_ascii()
            || name.contains('\\')
            || path.components().count() > 16
            || !path.starts_with("sdk")
            || !path.components().all(|p| matches!(p, Component::Normal(_)))
            || !matches!(kind, 0o040000 | 0o100000)
            || size < 0
            || (kind == 0o040000 && size != 0)
            || !unsafe { archive_entry_symlink(entry) }.is_null()
            || !unsafe { archive_entry_hardlink(entry) }.is_null()
            || !entries.insert(name.to_ascii_lowercase())
            || entries.len() > 50_000
        {
            return Err("unsafe, duplicate or oversized SDK archive entry".into());
        }
        let mut prefix = String::new();
        for component in name.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if !spellings.contains_key(&prefix.to_ascii_lowercase()) {
                path_bytes += prefix.len();
            }
            if spellings.len() >= 50_000 || path_bytes > 8 * 1024 * 1024 {
                return Err("SDK path metadata exceeds limit".into());
            }
            if component.is_empty()
                || spellings
                    .insert(prefix.to_ascii_lowercase(), prefix.clone())
                    .is_some_and(|old| old != prefix)
            {
                return Err("colliding SDK archive paths".into());
            }
        }
        let target = destination.join(path);
        if kind == 0o040000 {
            fs::create_dir_all(&target)?;
            continue;
        }
        total = total.checked_add(size as u64).ok_or("SDK size overflow")?;
        if total > expanded {
            return Err("expanded SDK exceeds manifest size".into());
        }
        fs::create_dir_all(target.parent().ok_or("SDK entry has no parent")?)?;
        let mut file = fs::File::create_new(&target)?;
        let mut copied = 0u64;
        loop {
            let n =
                unsafe { archive_read_data(reader.0, buffer.as_mut_ptr().cast(), buffer.len()) };
            if n < 0 {
                return Err("invalid SDK archive data".into());
            }
            if n == 0 {
                break;
            }
            copied += n as u64;
            if copied > size as u64 {
                return Err("SDK entry exceeds declared size".into());
            }
            file.write_all(&buffer[..n as usize])?;
        }
        if copied != size as u64 {
            return Err("truncated SDK entry".into());
        }
        let executable = unsafe { archive_entry_perm(entry) } & 0o111 != 0;
        file.set_permissions(fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }))?;
        file.sync_all()?;
    }
    if total != expanded {
        return Err("expanded SDK size differs from manifest".into());
    }
    Ok(())
}
