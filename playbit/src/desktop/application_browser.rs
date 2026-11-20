//! App, retained session and local instance columns share the existing data browser.
use super::*;
use crate::{
    engine::session_instance::{LaunchOwner, State as InstanceState},
    package::{AppIdentity, AutomaticVolumeRole as Role},
    store::{AutomaticVolumeSlot, AutomaticVolumeState, Id, Session},
};
use std::collections::{BTreeMap, BTreeSet};

mod actions;
mod navigation;
mod queries;
mod scrolling;
mod view;

const MAX_SESSIONS: usize = 256;

#[derive(Clone)]
struct Application {
    identity: AppIdentity,
    name: String,
    build: String,
    sessions: Vec<String>,
}

#[derive(Clone)]
enum Target {
    Application(AppIdentity),
    Session(String),
    Instance(Id),
    Scope { role: Role, volume: Id, session: bool },
    Log,
}

#[derive(Clone)]
struct Row {
    key: String,
    target: Target,
    selected: bool,
}

#[derive(Default)]
pub(super) struct State {
    catalog: Vec<Session>,
    imported: Vec<crate::store::ImportedApp>,
    apps: Vec<Application>,
    selected: Option<AppIdentity>,
    instance: Option<Id>,
    owners: Vec<LaunchOwner>,
    owners_session: Option<String>,
    refreshing: bool,
    pub(super) refresh_needed: bool,
    instance_error: Option<String>,
    action_busy: bool,
    pending_scope: Option<(Role, Id, bool)>,
    uncertain_app: Option<AppIdentity>,
    root_open: bool,
    selected_role: Option<Role>,
    scope_in_session: bool,
    log_open: bool,
    app_slots: Vec<AutomaticVolumeSlot>,
    session_slots: Vec<AutomaticVolumeSlot>,
    slots_app: Option<AppIdentity>,
    slots_session: Option<String>,
    scope_refreshing: bool,
    scope_refresh_needed: bool,
    scope_error: Option<String>,
    columns: Vec<Vec<Row>>,
    scrolling: Option<Entity<crate::ui::desktop::scrollbar::Scrollbars>>,
    sessions_scrolling: Option<Entity<crate::ui::desktop::scrollbar::Scrollbars>>,
    instances_scrolling: Option<Entity<crate::ui::desktop::scrollbar::Scrollbars>>,
    inspector_scrolling: Option<Entity<crate::ui::desktop::scrollbar::Scrollbars>>,
    controls: BTreeSet<String>,
    texts: BTreeMap<String, Entity<()>>,
    rendered_texts: BTreeSet<String>,
    identifiers: BTreeMap<&'static str, Entity<text_editor::TextEditor>>,
}

impl State {
    pub(super) fn imported_apps(&self) -> &[crate::store::ImportedApp] {
        &self.imported
    }
    pub(super) fn available(&self) -> bool {
        !self.apps.is_empty()
    }

    pub(super) fn has_selection(&self) -> bool {
        self.selected.is_some()
    }

    pub(super) fn clear_selection(&mut self) {
        self.selected = None;
        self.instance = None;
        self.pending_scope = None;
        self.root_open = false;
        self.selected_role = None;
        self.scope_in_session = false;
        self.log_open = false;
    }

    fn rebuild(&mut self, rows: &[SessionRow], recent: &[String]) {
        let mut apps: Vec<Application> = self
            .imported
            .iter()
            .map(|app| Application {
                identity: app.app.clone(),
                name: app.name.clone(),
                build: app.build_id.clone(),
                sessions: Vec::new(),
            })
            .collect();
        for session in &self.catalog {
            let row = rows.iter().find(|row| row.id == session.id);
            if let Some(app) = apps.iter_mut().find(|app| app.identity == session.app) {
                app.sessions.push(session.id.clone());
            } else {
                apps.push(Application {
                    identity: session.app.clone(),
                    name: row
                        .and_then(|row| row.app_name.clone())
                        .unwrap_or_else(|| session.app.app_id.clone()),
                    build: session.resolved_build_id.clone(),
                    sessions: vec![session.id.clone()],
                });
            }
        }
        for app in &mut apps {
            app.sessions.sort_by_key(|id| {
                recent.iter().position(|recent| recent == id).unwrap_or(usize::MAX)
            });
            if !self.imported.iter().any(|imported| imported.app == app.identity)
                && let Some(row) =
                    app.sessions.first().and_then(|id| rows.iter().find(|row| &row.id == id))
            {
                app.build = row.resolved_build_id.clone();
                app.name = row.app_name.clone().unwrap_or_else(|| app.identity.app_id.clone());
            }
        }
        apps.sort_by(|a, b| {
            crate::ui::collation::compare(&a.name, &b.name)
                .then_with(|| a.identity.publisher.cmp(&b.identity.publisher))
                .then_with(|| a.identity.app_id.cmp(&b.identity.app_id))
        });
        self.apps = apps;
        if self.uncertain_app.as_ref().is_some_and(|identity| {
            self.apps.iter().any(|app| &app.identity == identity && !app.sessions.is_empty())
        }) {
            self.uncertain_app = None;
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| !self.apps.iter().any(|app| &app.identity == selected))
        {
            self.selected = None;
            self.instance = None;
            self.root_open = false;
            self.log_open = false;
        }
    }
}

fn application_key(identity: &AppIdentity) -> String {
    format!("application-app-{}-{}", identity.publisher, identity.app_id)
}

impl Shell {
    pub(super) fn apply_imported_app_catalog(&mut self, apps: Vec<crate::store::ImportedApp>) {
        if apps.len() > MAX_SESSIONS
            || apps.iter().any(|app| {
                app.app.validate().is_err()
                    || !crate::identifier::valid(&app.build_id, 32)
                    || app.name.is_empty()
                    || app.name.len() > 256
            })
        {
            self.error = "The imported app catalog is invalid or exceeds its display limit.".into();
            return;
        }
        self.applications.imported = apps;
        self.applications.rebuild(&self.sessions, &self.recent_sessions);
    }
    pub(super) fn begin_application_render(&mut self) {
        self.applications.columns.clear();
        self.applications.controls.clear();
        self.applications.rendered_texts.clear();
    }

    pub(super) fn finish_application_render(&mut self, cx: &mut Context<Self>) {
        self.navigation_controls.retain(|key, control| {
            let keep = !key.starts_with("application-") || self.applications.controls.contains(key);
            if !keep {
                control.update(cx, |control, _| control.retire());
            }
            keep
        });
        self.applications.texts.retain(|key, _| self.applications.rendered_texts.contains(key));
    }

    pub(super) fn apply_application_catalog(
        &mut self,
        sessions: Vec<Session>,
        cx: &mut Context<Self>,
    ) {
        if sessions.len() > MAX_SESSIONS {
            self.error = "The application catalog exceeds its 256-session display limit.".into();
            return;
        }
        self.applications.catalog = sessions;
        self.applications.rebuild(&self.sessions, &self.recent_sessions);
        if let Some(session) = self
            .selected_session_id()
            .and_then(|id| self.applications.catalog.iter().find(|session| session.id == id))
        {
            self.applications.selected = Some(session.app.clone());
        }
        self.applications.refresh_needed = true;
        self.refresh_application_instances(cx);
        self.applications.scope_refresh_needed = true;
        self.refresh_application_scopes(cx);
    }

    pub(super) fn select_application_session(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self
            .selected_session_id()
            .and_then(|id| self.applications.catalog.iter().find(|session| session.id == id))
            .cloned()
        else {
            return;
        };
        self.applications.pending_scope = None;
        self.applications.selected = Some(session.app);
        self.applications.instance = None;
        self.applications.root_open = false;
        self.applications.selected_role = None;
        self.applications.log_open = false;
        self.applications.refresh_needed = true;
        self.refresh_application_instances(cx);
        self.applications.scope_refresh_needed = true;
        self.refresh_application_scopes(cx);
    }
}

fn short_id(id: &str, sessions: &[String], rows: &[SessionRow]) -> String {
    let Some(code) = rows.iter().find(|row| row.id == id).map(|row| row.reference.as_str()) else {
        return "Session".into();
    };
    if code.is_empty() {
        return "Session".into();
    }
    let mut end = 6.min(code.len());
    while end < code.len()
        && rows.iter().any(|other| {
            other.id != id
                && sessions.contains(&other.id)
                && other.reference.starts_with(&code[..end])
        })
    {
        end += 1;
    }
    format!("Session {}", &code[..end])
}

fn is_running(owner: &LaunchOwner) -> bool {
    owner.process.is_some()
        && matches!(owner.instance.state, InstanceState::Owned | InstanceState::Unknown)
}

fn scope_key(role: Role, session: bool) -> String {
    format!("application-scope-{}-{}", if session { "session" } else { "app" }, role as u8)
}

fn instance_state(
    owner: &LaunchOwner,
    startup: &[crate::engine::session_instance::StartupProgress],
) -> &'static str {
    if owner.instance.state == InstanceState::Owned
        && let Some(progress) = startup.iter().find(|progress| {
            progress.instance == owner.instance.id
                && progress.owner_generation == owner.instance.owner_generation
                && progress.presentation == owner.presentation
                && progress.session == owner.instance.session
        })
        && progress.phase != crate::engine::session_instance::StartupPhase::Ready
    {
        return progress.phase.label();
    }
    match owner.instance.state {
        InstanceState::Reserved => "Opening…",
        InstanceState::Owned => "Running",
        InstanceState::Unknown => "Not responding",
        InstanceState::Draining => "Stopping…",
        InstanceState::Exited => "Stopped",
        InstanceState::LaunchFailed => "Failed to open",
    }
}

#[cfg(test)]
mod tests;
