//! Offline test profile preparation for scripts/macos_provisioned_keychain.py.
//! Only synthetic fixture items may be created/removed by the provisioned mode.
use playbit::{
    network::ap::ClientIdentity,
    store::{Id, LocalStore, RecoveryState},
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const MARKER: &str = "keychain-test-fixture";

#[cfg(target_os = "macos")]
#[path = "keychain-fixture/protected.rs"]
mod protected;

fn prepare(path: &Path) -> Result<()> {
    // Exclusive creation prevents this helper from adopting real app data.
    std::fs::create_dir(path)?;
    let store = LocalStore::open(path)?;
    let account = store.account().id;
    let mut secret = [0; 16];
    getrandom::fill(&mut secret)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path.join(MARKER))?
        .write_all(account.to_string().as_bytes())?;
    // The fresh synthetic account signs copied built-in templates offline.
    // Keep this test authority out of production profile preparation.
    let package = playbit::package::Package::new(
        "keychain-fixture".into(),
        "Keychain signer fixture".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        BTreeMap::new(),
    )?;
    playbit::package::signature::testing::signed(&store, package)?;
    let mut identity = ClientIdentity::from_seed(
        "offline@example.invalid",
        "Offline Keychain fixture",
        store.account_seed()?,
    )?;
    // Synthetic local completion, not a valid AP login. Run its engine offline.
    identity.auth_token = account.as_bytes().iter().copied().chain([0; 56]).collect();
    identity.instance_id = Id::from_bytes([1; 16]).to_string().into_bytes();
    identity.ca_certificate = vec![1];
    identity.instance_certificate = vec![1];
    identity.save(&path.join("instance.json"))?;
    store.save_recovery_state(&RecoveryState {
        secret,
        acknowledged: true,
        revision: 1,
        synchronized_digest: None,
        pending: None,
    })?;
    println!(
        "{}",
        serde_json::json!({
            "account": account.to_string(),
            "service": "playbit.account_key",
            "secret": secret,
        })
    );
    Ok(())
}

fn probe(path: &Path, complete: bool) -> Result<()> {
    let mut marker = String::new();
    std::fs::File::open(path.join(MARKER))?.take(64).read_to_string(&mut marker)?;
    let account = Id::parse(&marker)?;
    let store = LocalStore::open(path)?;
    if store.account().id != account {
        return Err("fixture account does not match marker".into());
    }
    if !store.recovery_state()?.is_some_and(|state| state.acknowledged) {
        return Err("fixture recovery state is missing".into());
    }
    if complete {
        store.set_display_name("Offline Keychain fixture")?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() == 2 && args[1] == "group" {
        println!("{}", env!("PLAYBIT_KEYCHAIN_ACCESS_GROUP"));
        return Ok(());
    }
    if !cfg!(target_os = "macos") || args.len() != 3 {
        return Err("macOS only: keychain-fixture prepare|complete|probe|create-key|delete-\
            key|probe-key PROFILE"
            .into());
    }
    match args[1].to_str() {
        #[cfg(target_os = "macos")]
        Some(action @ ("create-key" | "delete-key" | "probe-key")) => {
            protected::run(action, Path::new(&args[2]))
        }
        Some("prepare") => prepare(Path::new(&args[2])),
        Some("complete") => probe(Path::new(&args[2]), true),
        Some("probe") => probe(Path::new(&args[2]), false),
        _ => Err("unknown fixture action".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_profile_retains_its_account_and_explicit_offline_signer() {
        // Seed only during exclusive fresh setup and preserve the credential fixture's account binding.
        let root = tempfile::tempdir().expect("private fixture parent");
        let path = root.path().join("profile");
        prepare(&path).expect("fresh synthetic fixture opens");
        let marker = std::fs::read(path.join(MARKER)).expect("owned account marker");
        let store = LocalStore::open(&path).expect("fixture reopens");
        assert_eq!(marker, store.account().id.to_string().as_bytes());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("fixture clock")
            .as_secs();
        store.publisher_signer(now).expect("the same account has its offline signer");
        assert!(store.recovery_state().expect("fixture recovery state").is_some());
        drop(store);
        assert!(prepare(&path).is_err(), "existing profiles cannot acquire fixture authority");
        assert_eq!(std::fs::read(path.join(MARKER)).expect("original marker"), marker);
        probe(&path, false).expect("original credential binding survives");
    }
}
