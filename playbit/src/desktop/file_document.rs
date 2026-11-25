//! File opens hydrate through Foundation coordination on the background executor.
//! Reads and package imports finish within their security-scoped access interval.
#![allow(
    unsafe_code,
    reason = "Foundation objects and synchronous accessor blocks obey Cocoa ownership."
)]
use crate::store::{DatabaseReference, MAX_DATABASE_REFERENCE_BYTES};
use block::ConcreteBlock;
use cocoa::{
    base::{id, nil},
    foundation::{NSAutoreleasePool, NSString},
};
use objc::{class, msg_send, sel, sel_impl};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{cell::RefCell, ffi::CStr, io::Read, os::unix::ffi::OsStrExt, path::Path, rc::Rc};

static READING: AtomicBool = AtomicBool::new(false);

pub(super) struct ReadGuard;

impl Drop for ReadGuard {
    fn drop(&mut self) {
        READING.store(false, Ordering::Release);
    }
}

pub(super) fn begin() -> Result<ReadGuard, String> {
    READING
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .map(|_| ReadGuard)
        .map_err(|_| {
            "A database reference is already being downloaded or opened. \
             Try again when it finishes."
                .into()
        })
}

pub(super) fn read(text: &str, _guard: ReadGuard) -> Result<DatabaseReference, String> {
    with_path(text, read_file)
}

pub(super) fn with_path<T: 'static>(
    text: &str,
    access: impl Fn(&Path) -> Result<T, String> + 'static,
) -> Result<T, String> {
    // SAFETY: all objects are local to this worker and released before its
    // autorelease pool drains. File coordination invokes the accessor synchronously.
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let result = coordinated(text, access);
        pool.drain();
        result
    }
}

pub(super) fn is_app(text: &str) -> bool {
    // SAFETY: URL parsing is bounded by the incoming URL limit and performs no I/O.
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let string = NSString::alloc(nil).init_str(text);
        let url: id = msg_send![class!(NSURL), URLWithString: string];
        let extension: id = if url == nil { nil } else { msg_send![url, pathExtension] };
        let bytes: *const std::ffi::c_char =
            if extension == nil { std::ptr::null() } else { msg_send![extension, UTF8String] };
        let app =
            !bytes.is_null() && CStr::from_ptr(bytes).to_bytes().eq_ignore_ascii_case(b"pbapp");
        let _: () = msg_send![string, release];
        pool.drain();
        app
    }
}

unsafe fn coordinated<T: 'static>(
    text: &str,
    access: impl Fn(&Path) -> Result<T, String> + 'static,
) -> Result<T, String> {
    // SAFETY: Foundation retains the URL through the synchronous coordination.
    unsafe {
        let string = NSString::alloc(nil).init_str(text);
        let url: id = msg_send![class!(NSURL), URLWithString: string];
        let _: () = msg_send![string, release];
        if url == nil
            || !{
                let file: bool = msg_send![url, isFileURL];
                file
            }
        {
            return Err("Open a local Playbit document file.".into());
        }
        let scoped: bool = msg_send![url, startAccessingSecurityScopedResource];
        let output = Rc::new(RefCell::new(None));
        let destination = output.clone();
        let accessor = ConcreteBlock::new(move |coordinated: id| {
            let path: *const std::ffi::c_char = msg_send![coordinated, fileSystemRepresentation];
            let result = if path.is_null() {
                Err("The document has no local path.".into())
            } else {
                let path = Path::new(std::ffi::OsStr::from_bytes(CStr::from_ptr(path).to_bytes()));
                if path.as_os_str().len() > crate::engine::host_mount::MAX_PATH_BYTES {
                    Err("The document path is too long.".into())
                } else {
                    access(path)
                }
            };
            *destination.borrow_mut() = Some(result);
        })
        .copy();
        let coordinator: id = msg_send![class!(NSFileCoordinator), alloc];
        let coordinator: id = msg_send![coordinator, initWithFilePresenter: nil];
        let mut error: id = nil;
        let _: () = msg_send![coordinator, coordinateReadingItemAtURL: url options: 0usize error: &mut error byAccessor: &*accessor];
        let _: () = msg_send![coordinator, release];
        if scoped {
            let _: () = msg_send![url, stopAccessingSecurityScopedResource];
        }
        if error != nil {
            return Err("The document could not be downloaded or opened. \
                Try again when its drive is available."
                .into());
        }
        output
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Err("The document could not be opened.".into()))
    }
}

pub(super) fn open_regular(path: &Path) -> Result<std::fs::File, String> {
    // Opening an untrusted reference path must not wait on a FIFO before we
    // can reject its file type. NONBLOCK has no effect on ordinary files.
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| error.to_string())?;
    let file = std::fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > crate::package::MAX_BUNDLE_BYTES as u64 {
        return Err("This is not a valid Playbit document.".into());
    }
    Ok(file)
}

fn read_file(path: &Path) -> Result<DatabaseReference, String> {
    let mut file = open_regular(path)?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_DATABASE_REFERENCE_BYTES as u64 {
        return Err("This is not a valid Playbit database reference.".into());
    }
    let mut bytes = [0; MAX_DATABASE_REFERENCE_BYTES];
    let len = metadata.len() as usize;
    file.read_exact(&mut bytes[..len]).map_err(|error| error.to_string())?;
    if file.read(&mut [0; 1]).map_err(|error| error.to_string())? != 0 {
        return Err("The database reference changed while opening.".into());
    }
    DatabaseReference::parse(&bytes[..len]).map_err(|error| error.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert coordinated reads of temporary references.")]
mod tests {
    use super::*;

    #[test]
    fn app_urls_decode_spaces_unicode_and_extensions_through_foundation() {
        // Coordinated access sees the actual file and keeps errors on the worker.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("My café #1.pbapp");
        std::fs::write(&path, b"package fixture").unwrap();
        let url = format!(
            "file://{}",
            path.to_str().unwrap().replace(' ', "%20").replace('#', "%23").replace('é', "%C3%A9")
        );
        assert!(is_app(&url));
        assert!(is_app("file:///tmp/App.PBAPP"));
        assert!(!is_app("file:///tmp/App.pbdb"));
        assert!(!is_app("file:///tmp/App.pbapp.txt"));
        let actual = with_path(&url, |path| {
            let mut bytes = String::new();
            open_regular(path)?.read_to_string(&mut bytes).map_err(|error| error.to_string())?;
            Ok(bytes)
        })
        .unwrap();
        assert_eq!(actual, "package fixture");
        assert!(with_path("https://example.com/App.pbapp", |_| Ok(())).is_err());
        assert!(
            with_path("file:///missing/App.pbapp", |path| open_regular(path).map(drop)).is_err()
        );
    }

    #[test]
    fn coordinated_open_reads_only_the_canonical_pointer() {
        // Use Foundation's actual read coordinator, including a path requiring URL escaping.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("My database.pbdb");
        let reference = DatabaseReference {
            volume: crate::store::Id::from_bytes([1; 16]),
            entry: crate::store::EntryId::from_bytes([2; 15]).unwrap(),
        };
        std::fs::write(&path, reference.to_string()).unwrap();
        let url = format!("file://{}", path.to_str().unwrap().replace(' ', "%20"));
        let guard = begin().unwrap();
        assert!(begin().is_err());
        assert_eq!(read(&url, guard).unwrap(), reference);
        std::fs::write(&path, b"not a database reference").unwrap();
        assert!(read(&url, begin().unwrap()).is_err());
        let fifo = directory.path().join("pipe.pbdb");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the temporary path is terminated and remains live for mkfifo.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(read_file(&fifo).is_err());
    }
}
