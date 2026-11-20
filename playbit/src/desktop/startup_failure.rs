//! Windowless startup failures use AppKit, with explicit local-profile replacement.
use super::*;

impl Shell {
    pub(super) fn show_startup_failure(host: &Entity<Self>, cx: &mut App) {
        if host.read(cx).startup_prompt {
            return;
        }
        host.update(cx, |host, cx| {
            host.startup_prompt = true;
            crate::diagnostic!("Playbit startup failed: {}", host.error);
            cx.notify();
        });
        // Tests inspect admission without entering AppKit's modal event loop.
        #[cfg(not(test))]
        {
            let corrupt = host.read(cx).error == crate::engine::ipc::CORRUPT_STORE_ERROR;
            let message = startup_message(corrupt, &profile_path());
            let host = host.clone();
            cx.spawn(async move |cx| {
                gpui::Timer::after(std::time::Duration::ZERO).await;
                // AppKit can dispatch callbacks while modal. Hold no App/entity borrow.
                let sign_in = macos::startup_failure(&message, corrupt);
                let _ = cx.update(|cx| {
                    if sign_in {
                        host.update(cx, |host, cx| host.reset_sign_in(None, cx));
                    } else {
                        cx.quit();
                    }
                });
            })
            .detach();
        }
    }
}

fn startup_message(corrupt: bool, profile: &std::path::Path) -> String {
    let profile = profile.display();
    if corrupt {
        format!(
            "Playbit couldn’t read your local data at {profile}.\n\nSign in again to reset the local profile on this device. Local changes that haven’t synced will be removed. Your online account is unchanged."
        )
    } else {
        format!(
            "Playbit couldn’t open your local profile at {profile}.\n\nYour local data has not been reset. See the profile’s logs for details."
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn alert_explains_the_scope_of_explicit_profile_reset() {
        // Both failure kinds identify the selected profile, including custom paths.
        let profile = std::path::Path::new("/tmp/example/Playbit Profile");
        let message = super::startup_message(true, profile);
        assert!(message.contains("at /tmp/example/Playbit Profile."));
        assert!(message.contains("Local changes that haven’t synced will be removed."));
        assert!(message.contains("Your online account is unchanged."));
        let message = super::startup_message(false, profile);
        assert!(message.contains("at /tmp/example/Playbit Profile."));
        assert!(message.contains("Your local data has not been reset."));
    }
}
