//! Portable app setup before callbacks borrow account-bound services. The host
//! retains one selected session; the compiled package is its exact code identity.
use super::{
    SessionLaunch,
    worker::setup::{Prompt, Setup},
};
use crate::{engine::connected::Connected, package::Package};
use playbit_api::abi::operations::App;
mod selection;

pub struct Portable {
    pub path: std::path::PathBuf,
    pub expected: [u8; 32],
    pub app: App,
}
// SAFETY: run_portable's caller retains callback code and user data through both
// worker joins. This descriptor is transported; callbacks run only on the app owner.
#[allow(unsafe_code, reason = "The embedding host owns trusted callback lifetimes.")]
unsafe impl Send for Portable {}

impl Portable {
    pub(super) fn load(&self) -> Result<Package, String> {
        let package = Package::open(&self.path).map_err(|error| {
            format!("Cannot open native bundle {}: {error}", self.path.display())
        })?;
        if package.digest() != self.expected {
            return Err("Native executable and portable package differ; rebuild the app".into());
        }
        Ok(package)
    }
    pub(super) fn prepare(
        self,
        owner: &mut Option<Connected>,
        change_profile: &mut super::network::ChangeProfile<'_>,
        ui: &mut Setup,
    ) -> Result<Vec<SessionLaunch>, String> {
        let _timing = crate::trace::Span::new("ui.native.setup");
        let connected = owner.as_mut().ok_or("Account profile unavailable")?;
        let profile = connected.account_profile().map_err(display)?;
        if !connected.enrolled()
            || !profile.unlocked
            || profile.agreement_required
            || profile
                .display_name
                .as_deref()
                .is_none_or(|name| !crate::ui::desktop::sign_in::validation::display_name(name))
        {
            ui.sign_in(owner, change_profile)?;
        }
        let connected = owner.as_mut().ok_or("Account profile unavailable")?;
        ui.busy("Opening app…")?;
        let package = connected.authenticate_native_package(self.load()?).map_err(display)?;
        let id = match selection::read(connected.services.profile_path()).map_err(display)? {
            Some(selection) => selection.resume(connected, &package).map_err(display)?.id,
            None => select(connected, ui, &package)?,
        };
        let reference = connected.services.store.session_reference(&id).map_err(display)?;
        println!("Session: {}", reference.code());
        if connected.services.store.session(&id).map_err(display)?.shared_volume.is_some() {
            println!("Invitation: {}", reference.link(connected.channel()));
        } else if !connected.services.store.is_local_session(&id).map_err(display)? {
            let invitation =
                connected.services.store.invitation(&id, connected.channel()).map_err(display)?;
            println!("Invitation: {invitation}");
        }
        Ok(vec![SessionLaunch { session: id, package, app: self.app }])
    }
}
fn display(error: impl std::fmt::Display) -> String {
    error.to_string().chars().take(1024).collect()
}
fn select(connected: &mut Connected, ui: &mut Setup, package: &Package) -> Result<String, String> {
    let mut error = String::new();
    let mut invitation = String::new();
    loop {
        let answer = ui.ask(Prompt {
            id: 600,
            title: package.name(),
            explanation: "Enter a session ID or invitation, or create a new local session.",
            field: Some((&invitation, "Session ID or invitation")),
            action: "Open session",
            secondary: Some("Create session"),
            error: &error,
        })?;
        invitation = answer.value;
        ui.busy(if answer.secondary { "Creating session…" } else { "Joining session…" })?;
        let result = if answer.secondary {
            selection::create(connected, package)
        } else {
            connected.join_native(invitation.trim(), package)
        };
        match result {
            Ok(session) => {
                if !answer.secondary {
                    selection::Selection::Session { id: session.id.clone() }
                        .write(connected.services.profile_path())
                        .map_err(display)?;
                }
                return Ok(session.id);
            }
            Err(failure) => error = display(failure),
        }
    }
}
