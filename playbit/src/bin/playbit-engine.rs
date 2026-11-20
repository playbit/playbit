use playbit::engine::{Engine, failure::write_startup_failure, profile_path};
fn main() {
    #[cfg(target_os = "macos")]
    if let Err(error) = playbit::engine::launch_agent::prepare_engine() {
        playbit::diagnostic!("playbit-engine: {error}");
        std::process::exit(1);
    }
    if let Err(error) = playbit::engine::profile::initialize() {
        playbit::diagnostic!("playbit-engine: {error}");
        std::process::exit(1);
    }
    if let Err(error) = playbit::trace::initialize(1, &[]) {
        playbit::diagnostic!("Playbit tracing unavailable: {error}");
    }
    let profile = profile_path();
    playbit::platform::failure::startup(&profile);
    playbit::platform::failure::install_native(&profile);
    #[cfg(target_os = "macos")]
    let installation_startup = prepare_installation(&profile);
    #[cfg(target_os = "macos")]
    let (installation_startup, _installation_engine) = match installation_startup {
        Ok(lease) => lease,
        Err(error) => {
            let _ = write_startup_failure(&profile, error.as_ref(), std::io::stdout().lock());
            playbit::diagnostic!("playbit-engine: {error}");
            std::process::exit(1);
        }
    };
    let open = || {
        playbit::engine::testing_profile::from_environment().and_then(|credentials| {
            match credentials {
                Some(credentials) => Engine::open_testing(&profile, credentials),
                None => Engine::open(&profile),
            }
        })
    };
    if let Err(error) = playbit::engine::process::serve_with_ready(&profile, open, || {
        #[cfg(target_os = "macos")]
        {
            // Another startup may have pruned this profile before its socket
            // existed. Register the now-live endpoint before releasing the gate.
            if let Some(installation) = playbit::engine::installation::Installation::for_executable(
                &std::env::current_exe()?,
            )? {
                installation.register_profile(&profile)?;
            }
            drop(installation_startup);
        }
        Ok(())
    }) {
        // The service has released its lifetime lease before publishing this
        // failure. A launcher may retry as soon as it receives the frame.
        if let Err(write_error) =
            write_startup_failure(&profile, error.as_ref(), std::io::stdout().lock())
        {
            playbit::diagnostic!("playbit-engine: could not report startup failure: {write_error}");
        }
        playbit::platform::failure::critical(
            &profile,
            "engine service stopped",
            &format!("{error:?}"),
        );
        playbit::diagnostic!("playbit-engine: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn prepare_installation(
    profile: &std::path::Path,
) -> playbit::engine::Result<(
    Option<playbit::engine::installation::Lease>,
    Option<playbit::engine::installation::Lease>,
)> {
    let executable = std::env::current_exe()?;
    let lease = playbit::engine::installation::client_lease(&executable)?;
    let installation = playbit::engine::installation::Installation::for_executable(&executable)?;
    let engine =
        installation.as_ref().map(|installation| installation.engine_lease()).transpose()?;
    if let Some(installation) = installation {
        installation.register_profile(profile)?;
    }
    Ok((lease, engine))
}
