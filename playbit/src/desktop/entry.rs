//! The macOS application.
pub fn app_main() {
    crate::desktop::record_process_entry();
    // The installer holds the installation's exclusive client/engine leases.
    // This offline operation must run new code without acquiring a client lease
    // or opening a profile database. Normal app startup follows below.
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--refresh-guest-apps") {
        let mut args = std::env::args_os().skip(2);
        let result = match (args.next(), args.next()) {
            (Some(profile), None) => {
                super::guest_app::refresh_installation(std::path::Path::new(&profile))
            }
            _ => Err("Invalid guest app refresh invocation".into()),
        };
        if let Err(error) = result {
            crate::diagnostic!("Cannot refresh guest apps: {error}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(target_os = "macos")]
    if crate::desktop::file_provider::requested() {
        if let Err(error) = crate::desktop::file_provider::run() {
            crate::diagnostic!("File Provider: {error}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(target_os = "macos")]
    let _installation = {
        if crate::desktop::update::installer_requested() {
            if let Err(error) = crate::engine::profile::initialize() {
                crate::desktop::update::launch_error(&error.to_string());
                return;
            }
            let profile = crate::engine::profile_path();
            if let Err(error) = crate::desktop::process_log::initialize(&profile) {
                crate::desktop::update::launch_error(&format!(
                    "Cannot open Playbit GUI log: {error}"
                ));
                return;
            }
            crate::platform::failure::startup(&profile);
            crate::platform::failure::install_native(&profile);
            let mut admitted = false;
            if let Err(error) = crate::desktop::update::run_installer(&mut admitted) {
                crate::platform::failure::log(&profile, "macOS update", &error.to_string());
                if admitted {
                    crate::desktop::update::update_error(&format!(
                        "The update couldn't be installed automatically.\n\n{error}\n\nDownload and install the update manually, or try Check for Updates again."
                    ));
                }
                std::process::exit(1);
            }
            return;
        }
        let lease = std::env::current_exe()
            .map_err(Into::into)
            .and_then(|path| crate::engine::installation::client_lease(&path));
        match lease {
            Ok(lease) => lease,
            Err(error) => {
                crate::desktop::update::launch_error(&error.to_string());
                return;
            }
        }
    };
    if let Err(error) = crate::trace::initialize(2, &[]) {
        crate::diagnostic!("Playbit tracing unavailable: {error}");
    }
    crate::desktop::main()
}
