//! Private bridge for the macOS provider's on-demand XPC service.
#[cfg(target_os = "macos")]
use playbit::engine::{self, Result};
#[cfg(target_os = "macos")]
use std::io::Read;
use std::io::Write;
#[cfg(target_os = "macos")]
fn run() -> Result<()> {
    engine::profile::initialize()?;
    let profile = engine::profile_path();
    engine::file_provider::require_system_profile(&profile)?;
    let executable = engine::launch::executable_for(&std::env::current_exe()?)?;
    let _child = engine::launch::recover(&profile, &executable)?;
    if std::env::args().nth(1).as_deref() == Some("--watch") {
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
                std::io::stdout().write_all(&serde_json::to_vec(&value)?)?;
                std::io::stdout().write_all(b"\n")?;
                std::io::stdout().flush()?;
            }
            after = Some(engine::ipc::ChangeWait::connect(&profile, after)?.receive()?.token);
        }
    }
    let mut bytes = Vec::new();
    std::io::stdin().take(65537).read_to_end(&mut bytes)?;
    let reply = engine::file_provider::call(&profile, &bytes)?;
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
