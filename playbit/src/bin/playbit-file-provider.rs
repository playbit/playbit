//! Private bridge for the macOS provider's on-demand XPC service.
#[cfg(target_os = "macos")]
use playbit::engine::{self, Result};
#[cfg(target_os = "macos")]
use std::io::Read;
use std::io::Write;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let watching = match args.next() {
        None => false,
        Some(mode) if mode == "--domain" => return domain_request(args),
        Some(mode) if mode == "--watch" && args.next().is_none() => true,
        _ => return Err("Unexpected File Provider arguments".into()),
    };
    engine::profile::initialize()?;
    let profile = engine::profile_path();
    let channel = engine::file_provider::require_system_profile(&profile)?;
    let executable = engine::launch::executable_for(&std::env::current_exe()?)?;
    let _child = engine::launch::recover(&profile, &executable)?;
    if watching {
        let mut after = None;
        let mut revision = serde_json::Value::Null;
        let mut connected = serde_json::Value::Null;
        let mut account = serde_json::Value::Null;
        let mut ready = serde_json::Value::Null;
        let mut catalog = serde_json::Value::Null;
        let profile_text = serde_json::to_value(std::fs::canonicalize(&profile)?)?;
        loop {
            let request =
                serde_json::to_vec(&serde_json::json!({"op":"volumes", "after":revision}))?;
            let reply = engine::file_provider::call(&profile, &request)?;
            let mut value: serde_json::Value = serde_json::from_slice(&reply)?;
            if let Some(error) = value["error"].as_str() {
                return Err(error.to_owned().into());
            }
            let refresh = match &value["volumes"] {
                serde_json::Value::Array(volumes)
                    if volumes.len() <= playbit::store::file_provider::PROVIDER_VOLUME_LIMIT =>
                {
                    true
                }
                serde_json::Value::Null if catalog.is_array() => false,
                _ => return Err("invalid File Provider catalog reply".into()),
            };
            revision = value["revision"].clone();
            let emit = refresh
                || value["connected"] != connected
                || value["account"] != account
                || value["provider_ready"] != ready;
            connected = value["connected"].clone();
            account = value["account"].clone();
            ready = value["provider_ready"].clone();
            if refresh {
                catalog = value["volumes"].take();
            }
            if emit {
                value["volumes"] = catalog.clone();
                value["profile"] = profile_text.clone();
                value["channel"] = channel.name().into();
                std::io::stdout().write_all(&serde_json::to_vec(&value)?)?;
                std::io::stdout().write_all(b"\n")?;
                std::io::stdout().flush()?;
            }
            after = Some(engine::ipc::ChangeWait::connect(&profile, after)?.receive()?.token);
        }
    }
    request(&profile)
}

#[cfg(target_os = "macos")]
fn domain_request(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<()> {
    let channel = args.next().ok_or("Missing File Provider channel")?;
    let profile = PathBuf::from(args.next().ok_or("Missing File Provider profile")?);
    if !profile.is_absolute() || args.next().is_some() {
        return Err("Invalid File Provider domain arguments".into());
    }
    if channel != engine::file_provider::require_system_profile(&profile)?.name() {
        return Err("Finder domain and profile belong to different environments".into());
    }
    engine::profile::initialize_at(&profile)?;
    let bridge = std::env::current_exe()?.canonicalize()?;
    let app = bridge.ancestors().nth(3).ok_or("File Provider requires a packaged bridge")?;
    match engine::launch_agent::owner(&profile)?.as_deref() {
        Some(owner) if owner != app => {
            // macOS may load the extension from either copy. Only launchd may
            // start the foreign installation. The path authenticates its peer.
            let engine = engine::launch::executable_for(&owner.join("Contents/MacOS/Playbit"))?;
            engine::launch::recover_provider_owner(&profile, &engine)?;
        }
        None if engine::profile::resolve_environment()?.0 != profile => {
            // Before the first upgraded engine launch, an existing Public
            // domain can still reach its profile's authenticated live socket.
            return request(&profile).map_err(|error| {
                if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    )
                }) {
                    "Open the Playbit app for this environment to reconnect its Finder drive".into()
                } else {
                    error
                }
            });
        }
        _ => {
            let engine = engine::launch::executable_for(&bridge)?;
            engine::launch::recover(&profile, &engine)?;
        }
    }
    request(&profile)
}

#[cfg(target_os = "macos")]
fn request(profile: &Path) -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin().take(65537).read_to_end(&mut bytes)?;
    let reply = engine::file_provider::call(profile, &bytes)?;
    std::io::stdout().write_all(&reply)?;
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn run() -> playbit::engine::Result<()> {
    Err("File Provider is supported on macOS only".into())
}
fn main() {
    if let Err(error) = run() {
        let reply = playbit::engine::file_provider::error_response(error.as_ref());
        let _ = std::io::stdout().write_all(reply.to_string().as_bytes());
    }
}
