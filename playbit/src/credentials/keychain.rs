//! Account recovery keys use SecItem. No other local secrets enter Keychain.
mod query;
use std::io;

fn failure(error: security_framework::base::Error) -> io::Error {
    let reason = match error.code() {
        -25293 => "access denied",
        -25308 => "locked or access requires user approval",
        -25291 => "unavailable",
        _ => "operation failed",
    };
    io::Error::other(format!(
        "Playbit Keychain {reason} ({}); unlock the device or restore this app’s Keychain access and provisioning, then retry",
        error.code(),
    ))
}

pub(crate) fn available() -> bool {
    #[cfg(test)]
    {
        true
    } // Unit tests use only the synthetic backend below.
    #[cfg(not(test))]
    {
        query::available()
    }
}

#[cfg(all(not(test), feature = "connected-engine"))]
pub(crate) use query::account as find_account;
#[cfg(not(test))]
pub(crate) use query::{read_confirmed as read_account, save as save_account};
#[cfg(test)]
pub(crate) use testing::{find_account, read_account, save_account};

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::{
        collections::BTreeMap,
        sync::{LazyLock, Mutex},
    };
    static FAILURES: LazyLock<Mutex<BTreeMap<String, bool>>> = LazyLock::new(Default::default);
    // Process fixtures share synthetic credentials; production never uses these files.
    fn directory(service: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!(
                "playbit-test-accounts-{}",
                blake3::hash(env!("CARGO_MANIFEST_DIR").as_bytes()).to_hex()
            ))
            .join(blake3::hash(service.as_bytes()).to_hex().as_str())
    }
    pub(crate) fn fail(account: &str, failure: Option<bool>) {
        let mut failures = FAILURES.lock().expect("failures");
        if let Some(value) = failure {
            failures.insert(account.into(), value);
        } else {
            failures.remove(account);
        }
    }
    pub(crate) fn read_account(service: &str, account: &str) -> io::Result<Option<Vec<u8>>> {
        if FAILURES.lock().expect("failures").get(account) == Some(&false) {
            return Err(io::Error::other("injected Keychain read failure"));
        }
        match std::fs::read(directory(service).join(account)) {
            Ok(bytes) => {
                let (_, secret, confirmed): (String, Vec<u8>, bool) =
                    serde_json::from_slice(&bytes)?;
                Ok(confirmed.then_some(secret))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    pub(crate) fn save_account(
        service: &str,
        account: &str,
        email: Option<&str>,
        secret: &[u8],
        confirmed: bool,
    ) -> io::Result<()> {
        if FAILURES.lock().expect("failures").contains_key(account) {
            return Err(io::Error::other("injected Keychain write failure"));
        }
        if let Some(saved) = read_account(service, account)?
            && saved != secret
        {
            return Err(io::Error::other("A different account key is already saved"));
        }
        let dir = directory(service);
        std::fs::create_dir_all(&dir)?;
        let verified = confirmed;
        let email = match email {
            Some(email) => email.to_owned(),
            None => std::fs::read(dir.join(account))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<(String, Vec<u8>, bool)>(&bytes).ok())
                .map(|(email, _, _)| email)
                .unwrap_or_default(),
        };
        std::fs::write(dir.join(account), serde_json::to_vec(&(email, secret, verified))?)
    }
    pub(crate) fn find_account(
        service: &str,
    ) -> io::Result<Option<super::super::AccountCredential>> {
        let entries = match std::fs::read_dir(directory(service)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut found = None;
        for entry in entries {
            let entry = entry?;
            let (email, secret, verified): (String, Vec<u8>, bool) =
                serde_json::from_slice(&std::fs::read(entry.path())?)?;
            if !verified {
                continue;
            }
            if found.is_some() {
                return Ok(None);
            }
            found =
                Some((entry.file_name().into_string().expect("test account ID"), email, secret));
        }
        let Some((account, email, secret)) = found else {
            return Ok(None);
        };
        super::super::AccountCredential::parse(&account, email, &secret).map(Some)
    }
}
