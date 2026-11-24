//! Parsing and validation of device-local mount configuration, without grants.
use super::{Result, failure::usage_error};
use std::{
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

pub const MAX_MOUNTS: usize = 32;
pub const MAX_PATH_BYTES: usize = 4096;
pub const MAX_LABEL_BYTES: usize = 255;

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
#[serde(deny_unknown_fields)]
pub struct HostMount {
    pub id: crate::store::Id,
    pub path: Vec<u8>,
    pub label: String,
    pub write: bool,
}

impl HostMount {
    /// Field two is always a label, even `rw`. Resolution is launch-time I/O
    /// for a management worker. This configuration does not grant guest access.
    pub fn from_cli(argument: &str, cwd: &Path) -> Result<Self> {
        if argument.len() > 2 * (MAX_PATH_BYTES + MAX_LABEL_BYTES) + 4 {
            return Err(usage_error("Host mount argument is too long"));
        }
        let mut fields = vec![String::new()];
        let mut chars = argument.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => match chars.next() {
                    Some(ch @ (':' | '\\')) => fields.last_mut().expect("one mount field").push(ch),
                    _ => {
                        return Err(usage_error("Host mount escapes must be \\: or \\\\"));
                    }
                },
                ':' if fields.len() < 3 => fields.push(String::new()),
                ':' => {
                    return Err(usage_error("Host mount expects PATH[:LABEL[:ro|rw]]"));
                }
                ch => fields.last_mut().expect("one mount field").push(ch),
            }
        }
        if fields[0].is_empty() {
            return Err(usage_error("Host mount path is empty"));
        }
        let write = match fields.get(2).map(String::as_str) {
            None | Some("ro") => false,
            Some("rw") => true,
            _ => return Err(usage_error("Host mount mode must be ro or rw")),
        };
        Self::resolve(&cwd.join(&fields[0]), fields.get(1).map(String::as_str), write)
    }

    pub fn resolve(path: &Path, label: Option<&str>, write: bool) -> Result<Self> {
        let path = path
            .canonicalize()
            .map_err(|error| format!("Cannot open host mount {}: {error}", path.display()))?;
        if !path.is_dir() {
            return Err(format!("Host mount is not a directory: {}", path.display()).into());
        }
        let label = match label.filter(|label| !label.is_empty()) {
            Some(label) => label.to_owned(),
            None => match path.file_name() {
                Some(name) => {
                    name.to_str().ok_or("Host mount needs an explicit UTF-8 label")?.to_owned()
                }
                None => "host".to_owned(),
            },
        };
        let mount = Self {
            id: crate::store::Id::generate()?,
            path: path.as_os_str().as_bytes().to_vec(),
            label,
            write,
        };
        mount.validate()?;
        Ok(mount)
    }

    pub fn path(&self) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(self.path.clone()).into()
    }

    pub fn validate(&self) -> Result<()> {
        if self.id == crate::store::Id::from_bytes([0; 16])
            || self.path.is_empty()
            || self.path.len() > MAX_PATH_BYTES
            || self.path[0] != b'/'
            || self.path.contains(&0)
            || (self.path.len() > 1
                && self.path[1..]
                    .split(|byte| *byte == b'/')
                    .any(|part| matches!(part, b"" | b"." | b"..")))
            || self.label.is_empty()
            || self.label.len() > MAX_LABEL_BYTES
            || self.label.contains('\0')
        {
            return Err("Invalid host mount path or label".into());
        }
        Ok(())
    }
}

pub fn validate(mounts: &[HostMount]) -> Result<()> {
    if mounts.len() > MAX_MOUNTS {
        return Err("Too many host filesystem mounts".into());
    }
    for (index, mount) in mounts.iter().enumerate() {
        mount.validate()?;
        if mounts[..index].iter().any(|prior| prior.id == mount.id) {
            return Err("Duplicate host mount identity".into());
        }
        if mounts[..index].iter().any(|prior| prior.label == mount.label) {
            return Err(format!(
                "Duplicate host mount label {:?}; specify a unique label",
                mount.label
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert mount parser fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn positional_modes_escapes_and_resolved_labels() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        for name in ["photos", "photos:originals", "back\\slash"] {
            std::fs::create_dir(cwd.join(name)).unwrap();
        }
        for (text, path, label, write) in [
            ("photos", "photos", "photos", false),
            ("photos/", "photos", "photos", false),
            ("photos:rw", "photos", "rw", false),
            ("photos::rw", "photos", "photos", true),
            ("photos:Photos:ro", "photos", "Photos", false),
            (r"photos\:originals:Originals:rw", "photos:originals", "Originals", true),
            (r"back\\slash", "back\\slash", "back\\slash", false),
        ] {
            let mount = HostMount::from_cli(text, cwd).unwrap();
            assert_eq!(mount.path(), cwd.join(path).canonicalize().unwrap());
            assert_eq!(mount.label, label);
            assert_eq!(mount.write, write);
        }
        assert_eq!(HostMount::from_cli("/", cwd).unwrap().label, "host");
        for bad in [
            "",
            ":label",
            "photos:x:",
            "photos:x:write",
            "photos:x:rw:extra",
            r"photos\q",
            "photos\\",
            "absent",
        ] {
            assert!(HostMount::from_cli(bad, cwd).is_err(), "{bad}");
        }
        let a = HostMount::from_cli("photos:Same", cwd).unwrap();
        let b = HostMount::from_cli("photos:Same:rw", cwd).unwrap();
        assert!(validate(&[a.clone(), b]).is_err());
        assert!(validate(&[a, HostMount::from_cli("photos:same", cwd).unwrap()]).is_ok());
    }
}
