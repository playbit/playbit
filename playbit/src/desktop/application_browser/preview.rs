//! Populate the native qualification window with a disconnected session and local instance.
use super::*;
use crate::engine::{host_mount::HostMount, session_instance::SessionInstance};
use crate::store::{BuildSelector, SeenParticipant, SessionActivity, UpdateAuthority};

impl Shell {
    pub(in crate::desktop) fn preview_app_browser(&mut self, empty: bool, cx: &mut Context<Self>) {
        self.account.id = Id::from_bytes([0x91; 16]);
        self.account_id = self.account.id.to_string();
        self.account_profile.display_name = Some("Robin Fernández".into());
        self.applications.scope_refreshing = true;
        self.applications.refreshing = true;
        // Keep the build IDs prepared by load_preview so both artwork sizes use its packages.
        let imported: Vec<_> = self
            .sessions
            .iter()
            .zip(["micro-kart", "pinball", "banana-bingo", "corkboard", "social-importer"])
            .map(|(row, id)| crate::store::ImportedApp {
                app: AppIdentity { publisher: self.account.id, app_id: id.into() },
                build_id: row.resolved_build_id.clone(),
                name: row.app_name.clone().expect("fixture app name"),
            })
            .collect();
        let selected = imported[0].clone();
        self.apply_imported_app_catalog(imported);
        self.sessions.clear();
        let mut sessions = Vec::new();
        let mut activity = BTreeMap::new();
        if !empty {
            for (i, started) in [1_793_289_600, 1_791_475_200].into_iter().enumerate() {
                let reference = crate::store::SessionReference::new([10 + i as u8; 16])
                    .expect("fixture session reference");
                let id = reference.internal_id().to_owned();
                self.sessions.push(SessionRow {
                    id: id.clone(),
                    reference: reference.code(),
                    resolved_build_id: selected.build_id.clone(),
                    app_name: Some(selected.name.clone()),
                    build_state: crate::engine::builds::BuildState::Ready,
                });
                sessions.push(Session {
                    id: id.clone(),
                    app: selected.app.clone(),
                    root_volume: Id::from_bytes([1; 16]),
                    shared_volume: None,
                    creator: self.account.id,
                    selector: BuildSelector::Exact { build_id: selected.build_id.clone() },
                    resolved_build_id: selected.build_id.clone(),
                    update_authority: UpdateAuthority::Creator,
                });
                activity.insert(
                    id,
                    SessionActivity {
                        started: Some(started),
                        participants: ["Sam", "Anne"]
                            .into_iter()
                            .take(2 - i)
                            .enumerate()
                            .map(|(i, name)| SeenParticipant {
                                account: Id::from_bytes([20 + i as u8; 16]),
                                name: name.into(),
                                last_seen: started,
                            })
                            .collect(),
                    },
                );
            }
        }
        self.recent_sessions = self.sessions.iter().map(|row| row.id.clone()).collect();
        self.applications.set_activity(
            activity,
            &sessions,
            self.account.id,
            self.account_profile.display_name.as_deref(),
        );
        self.apply_application_catalog(sessions, cx);
        self.applications.selected = Some(selected.app.clone());
        self.applications.slots_app = Some(selected.app.clone());
        self.applications.app_slots = [Role::Root, Role::User, Role::Local]
            .into_iter()
            .map(|role| AutomaticVolumeSlot {
                role,
                volume: Id::from_bytes([role as u8 + 1; 16]),
                state: AutomaticVolumeState::Lazy,
                overridden: false,
            })
            .collect();
        self.applications.details = details::Details::preview(
            self.account.id,
            &selected.app,
            &selected.build_id,
            Some(1_258_291),
            details::History {
                updated: Some(1_793_229_420),
                notes: vec![
                    details::ReleaseNote::new(
                        selected.build_id.clone(),
                        "Adds two new cars and improves physics".into(),
                        1_793_229_420,
                    ),
                    details::ReleaseNote::new(
                        crate::identifier::encode(&[18; 32]),
                        "Fixes a bug with high score recording".into(),
                        1_792_863_900,
                    ),
                ],
            },
        );
    }

    pub(in crate::desktop) fn preview_session_browser(
        &mut self,
        mode: &str,
        cx: &mut Context<Self>,
    ) {
        let mut sessions = Vec::new();
        for (i, row) in self.sessions.iter_mut().enumerate() {
            let app = AppIdentity { publisher: self.account.id, app_id: format!("preview-{i}") };
            let reference =
                crate::store::SessionReference::new([i as u8 + 10; 16]).expect("fixture reference");
            row.id = reference.internal_id().into();
            row.reference = reference.code();
            sessions.push(Session {
                app,
                id: row.id.clone(),
                root_volume: Id::from_bytes([i as u8 + 1; 16]),
                shared_volume: None,
                creator: self.account.id,
                selector: BuildSelector::Exact { build_id: row.resolved_build_id.clone() },
                resolved_build_id: row.resolved_build_id.clone(),
                update_authority: UpdateAuthority::Creator,
            });
        }
        let session = sessions[0].clone();
        let started = crate::store::now_unix_secs() - 720;
        self.sidebar_session = Some(session.id.clone());
        self.applications.scope_refreshing = true;
        self.applications.refreshing = true;
        self.applications.owners_session = Some(session.id.clone());
        self.account_profile.display_name = Some("Anne".into());
        self.applications.set_activity(
            BTreeMap::from([(
                session.id.clone(),
                SessionActivity {
                    started: Some(started),
                    participants: vec![
                        SeenParticipant {
                            account: Id::from_bytes([4; 16]),
                            name: "Robin Fernández".into(),
                            last_seen: started + 480,
                        },
                        SeenParticipant {
                            account: Id::from_bytes([5; 16]),
                            name: "Sam Ylletröja".into(),
                            last_seen: started - 86400,
                        },
                        SeenParticipant {
                            account: self.account.id,
                            name: "Anne".into(),
                            last_seen: started,
                        },
                    ],
                },
            )]),
            &sessions,
            self.account.id,
            Some("Anne"),
        );
        self.apply_application_catalog(sessions, cx);
        self.applications.refreshing = false;
        self.applications.slots_app = Some(session.app.clone());
        self.applications.slots_session = Some(session.id.clone());
        let slots = |roles: &[Role]| {
            roles
                .iter()
                .map(|&role| AutomaticVolumeSlot {
                    role,
                    volume: Id::from_bytes([role as u8 + 1; 16]),
                    state: AutomaticVolumeState::Lazy,
                    overridden: false,
                })
                .collect()
        };
        self.applications.app_slots = slots(&[Role::Root, Role::User, Role::Local]);
        self.applications.session_slots =
            slots(&[Role::Session, Role::UserSession, Role::LocalSession]);
        if mode != "stopped" {
            let id = Id::generate().expect("fixture instance");
            let owner = LaunchOwner {
                instance: SessionInstance {
                    id,
                    launch_request: id,
                    account: self.account.id,
                    session: session.id,
                    build: session.resolved_build_id,
                    root: session.root_volume,
                    extra_volumes: vec![],
                    host_mounts: vec![HostMount {
                        id,
                        path: std::env::home_dir()
                            .expect("preview home")
                            .join("Downloads")
                            .as_os_str()
                            .as_encoded_bytes()
                            .to_vec(),
                        label: "Downloads".into(),
                        write: false,
                    }]
                    .into_boxed_slice(),
                    restore_host_mounts: false,
                    owner_generation: 1,
                    state: InstanceState::Owned,
                },
                process: Some(
                    crate::engine::process_owner::Owner::capture(
                        &std::os::unix::net::UnixStream::pair().expect("preview socket pair").0,
                    )
                    .expect("preview process identity")
                    .identity,
                ),
                presentation: 1,
            };
            self.applications.owners = vec![owner];
            self.applications.instance = (mode == "instance").then_some(id);
        }
    }
}
