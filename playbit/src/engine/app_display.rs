//! Disposable native artwork handoff. All filesystem/JSON work belongs on
//! background workers. Only the serial developer lane publishes these files.
use crate::engine::Result;
use crate::network::ap::operation::{BUILD_DISPLAY_LIMIT, BuildDisplay};
use std::{fs, io::Read, io::Write, os::unix::fs::OpenOptionsExt, path::Path};

pub(crate) fn publish(profile: &Path, display: &BuildDisplay) -> Result<()> {
    let _timing = crate::trace::Span::new("app.display.cache");
    display.validate(&display.build_id)?;
    let directory = crate::paths::private_directory(profile, crate::paths::APP_DISPLAYS)?;
    let mut entries = Vec::new();
    for (index, entry) in fs::read_dir(&directory)?.enumerate() {
        if index > 32 {
            return Err("display cache exceeds file limit".into());
        }
        let entry = entry?;
        if entry.file_name() == "candidate" {
            continue;
        }
        if !crate::identifier::valid(&entry.file_name().to_string_lossy(), 32) {
            return Err("unexpected display cache file".into());
        }
        if entries.len() == 32 {
            return Err("display cache exceeds entry limit".into());
        }
        entries.push((entry.metadata()?.modified()?, entry.path()));
    }
    let path = directory.join(&display.build_id);
    if entries.len() == 32 && !entries.iter().any(|(_, p)| *p == path) {
        entries.sort_unstable();
        fs::remove_file(&entries[0].1)?;
    }
    let bytes = serde_json::to_vec(display)?;
    if bytes.len() > BUILD_DISPLAY_LIMIT {
        return Err("display cache entry exceeds limit".into());
    }
    // A cache, not acknowledged app data: interrupted writes are discarded.
    // Atomic replacement allows readers to finish on their already-open file.
    let temporary = directory.join("candidate");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

pub fn read(profile: &Path, build: &str) -> Result<BuildDisplay> {
    let _timing = crate::trace::Span::new("app.display.cache_read");
    if !crate::identifier::valid(build, 32) {
        return Err("invalid display build ID".into());
    }
    let file = fs::File::open(profile.join(crate::paths::APP_DISPLAYS).join(build))?;
    let mut bytes = Vec::new();
    file.take(BUILD_DISPLAY_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > BUILD_DISPLAY_LIMIT {
        return Err("display cache entry exceeds limit".into());
    }
    let display: BuildDisplay = serde_json::from_slice(&bytes)?;
    display.validate(build)?;
    Ok(display)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    #[test]
    fn cache_bounds_entries_and_rejects_bad_identity_or_oversized_input() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..40 {
            let value = BuildDisplay {
                app_id: "test".into(),
                build_id: crate::identifier::encode(&{
                    let mut bytes = [0; 32];
                    bytes[16..].copy_from_slice(&(n as u128).to_be_bytes());
                    bytes
                }),
                name: "TODO".into(),
                icon: Some("aa".repeat(65536)),
                icon_error: None,
            };
            publish(dir.path(), &value).unwrap();
            assert_eq!(read(dir.path(), &value.build_id).unwrap(), value);
            assert!(
                fs::read_dir(dir.path().join(crate::paths::APP_DISPLAYS)).unwrap().count() <= 32
            );
        }
        assert!(read(dir.path(), "../bad").is_err());
        let build = crate::identifier::encode(&{
            let mut bytes = [0; 32];
            bytes[31] = 39;
            bytes
        });
        let path = dir.path().join(crate::paths::APP_DISPLAYS).join(&build);
        let mut value = read(dir.path(), &build).unwrap();
        value.build_id = crate::identifier::encode(&[0xab; 32]);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read(dir.path(), &build).is_err());
        fs::write(path, vec![b' '; BUILD_DISPLAY_LIMIT + 1]).unwrap();
        assert!(read(dir.path(), &build).is_err());
    }
    #[test]
    #[ignore = "reference-hardware benchmark; never CI"]
    fn benchmark_cache_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let mut value = BuildDisplay {
            app_id: "test".into(),
            build_id: String::new(),
            name: "n".repeat(256),
            icon: Some("ab".repeat(65536)),
            icon_error: None,
        };
        for n in 0..32 {
            value.build_id = crate::identifier::encode(&{
                let mut bytes = [0; 32];
                bytes[16..].copy_from_slice(&(n as u128).to_be_bytes());
                bytes
            });
            publish(dir.path(), &value).unwrap();
        }
        let mut writes = Vec::new();
        let mut reads = Vec::new();
        for n in 32..1032 {
            value.build_id = crate::identifier::encode(&{
                let mut bytes = [0; 32];
                bytes[16..].copy_from_slice(&(n as u128).to_be_bytes());
                bytes
            });
            let start = std::time::Instant::now();
            publish(dir.path(), &value).unwrap();
            writes.push(start.elapsed().as_nanos());
            let start = std::time::Instant::now();
            std::hint::black_box(read(dir.path(), &value.build_id).unwrap());
            reads.push(start.elapsed().as_nanos());
        }
        for (name, mut values) in [("publish_evict", writes), ("read_validate", reads)] {
            values.sort_unstable();
            println!(
                "display cache {name} entries=32 icon_bytes=65536 n=1000 p50_ns={} p99_ns={} max_ns={}",
                values[499], values[989], values[999]
            );
        }
    }
}
