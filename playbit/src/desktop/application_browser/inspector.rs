//! The inspector follows the active column. Session activity uses only known account observations.
use super::super::session_log_view::formatted_time;
use super::{scrolling::vertical_viewport, *};
use crate::ui::desktop::typography::tabular_numerals;
use crate::ui::desktop::zoom::length as ui;

#[derive(Clone, Default)]
pub(super) struct Activity {
    pub(super) date: Option<String>,
    started: Option<String>,
    pub(super) peers: String,
    people: Vec<Person>,
}

#[derive(Clone)]
struct Person {
    account: Id,
    name: String,
    last_seen: Option<u64>,
}

impl State {
    pub(in crate::desktop) fn set_activity(
        &mut self,
        activity: BTreeMap<String, crate::store::SessionActivity>,
        sessions: &[Session],
        account: Id,
        display_name: Option<&str>,
    ) {
        let mut observed_names: BTreeMap<Id, (u64, &str)> = BTreeMap::new();
        for visitor in activity.values().flat_map(|activity| &activity.participants) {
            let observed = observed_names.entry(visitor.account).or_insert((0, ""));
            if visitor.last_seen >= observed.0 {
                *observed = (visitor.last_seen, &visitor.name);
            }
        }
        let names: BTreeMap<_, _> = observed_names
            .into_iter()
            .filter(|(_, (_, name))| !name.is_empty())
            .map(|(id, (_, name))| (id, name.to_owned()))
            .collect();
        if *self.participant_names != names {
            self.participant_names = Arc::new(names);
        }
        self.activity = sessions
            .iter()
            .map(|session| {
                let observed = activity.get(&session.id);
                let started = observed.and_then(|row| row.started);
                let mut people = observed
                    .into_iter()
                    .flat_map(|row| &row.participants)
                    .map(|p| Person {
                        account: p.account,
                        name: if p.name.is_empty() {
                            p.account.to_string()
                        } else {
                            p.name.clone()
                        },
                        last_seen: Some(p.last_seen),
                    })
                    .collect::<Vec<_>>();
                for id in [session.creator, account] {
                    if !people.iter().any(|p| p.account == id) {
                        people.push(Person { account: id, name: id.to_string(), last_seen: None });
                    }
                }
                if let Some(me) = people.iter_mut().find(|p| p.account == account) {
                    me.name = display_name.filter(|name| !name.is_empty()).unwrap_or("You").into();
                }
                people.sort_by(|a, b| {
                    (a.account == account)
                        .cmp(&(b.account == account))
                        .then_with(|| crate::ui::collation::compare(&a.name, &b.name))
                        .then(a.account.cmp(&b.account))
                });
                let peers = people
                    .iter()
                    .filter(|p| p.account != account)
                    .filter_map(|p| p.name.split_whitespace().next())
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    session.id.clone(),
                    Activity {
                        date: started.map(|s| formatted_time(s as _, c"%b %e")),
                        started: started.map(|s| formatted_time(s as _, c"%b %e, %Y at %H:%M")),
                        peers,
                        people,
                    },
                )
            })
            .collect();
    }
}

impl Shell {
    pub(super) fn application_inspector(
        &mut self,
        session: &Session,
        owner: Option<&LaunchOwner>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let title = if owner.is_some() { "Session Instance" } else { "Session" };
        let mut heading = self.application_heading(
            "application-inspector-title",
            title,
            None,
            None,
            window.is_window_active(),
            cx,
        );
        let action = if let Some(owner) = owner {
            let id = owner.instance.id;
            let session = session.id.clone();
            self.host_callback(
                "application-instance-stop",
                "Stop",
                self.applications.action_busy || !is_running(owner),
                move |host, cx| host.stop_application_instance(session.clone(), id, cx),
                cx,
            )
        } else {
            let share = self.host_callback(
                "application-share",
                "Share",
                self.session_action_busy,
                |host, cx| host.share_session(cx),
                cx,
            );
            share.update(cx, |control, cx| {
                control.set_session_style(controls::SessionStyle::Share, cx)
            });
            share
        };
        heading = heading.pr(ui(10.)).child(div().flex_none().flex().occlude().child(action));
        let activity = self.applications.activity.get(&session.id);
        let id = owner.map_or_else(
            || {
                self.sessions
                    .iter()
                    .find(|r| r.id == session.id)
                    .map(|r| r.reference.clone())
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "Unavailable".into())
            },
            |owner| owner.instance.id.to_string(),
        );
        let started = owner
            .map(|owner| {
                formatted_time(
                    crate::engine::session_instance::reserved_unix_secs(owner.instance.id).into(),
                    c"%b %e, %Y at %H:%M",
                )
            })
            .or_else(|| activity.and_then(|a| a.started.clone()))
            .unwrap_or_else(|| "—".into());
        let mut body = div()
            .id("application-inspector-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .child(self.application_metadata("application-id", "ID", &id, cx))
            .child(self.application_metadata("application-started", "Started", &started, cx));
        body = if let Some(owner) = owner {
            body.child(self.instance_folders(owner, cx))
        } else {
            body.child(self.session_participants(session, cx))
        };
        div()
            .flex_1()
            .min_w(ui(260.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(0xececec))
            .child(heading)
            .child(vertical_viewport(body, &mut self.applications.inspector_scrolling, cx))
    }

    fn session_participants(&mut self, session: &Session, cx: &mut Context<Self>) -> gpui::Div {
        let mut table = div()
            .mt(ui(8.))
            .flex_none()
            .child(self.inspector_table_header(&["Participant", "Last seen"], cx));
        let people = self
            .applications
            .activity
            .get(&session.id)
            .map(|a| a.people.clone())
            .unwrap_or_default();
        let now = crate::store::now_unix_secs();
        for person in people {
            let me = person.account == self.account.id;
            let name = if me && person.name != "You" {
                format!("{} (You)", person.name)
            } else {
                person.name.clone()
            };
            let seen = match person.last_seen {
                Some(time) if now.saturating_sub(time) < 60 => "Just now".into(),
                Some(time) if now.saturating_sub(time) < 3600 => {
                    format!("{} min ago", (now - time) / 60)
                }
                Some(time) => formatted_time(time as _, c"%b %e, %Y at %H:%M"),
                None => "—".into(),
            };
            let row = table_row()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(ui(6.))
                        .child(initial(&person.name, me))
                        .child(
                            self.application_text(
                                &format!("participant-{}", person.account),
                                name,
                                cx,
                            )
                            .truncate(),
                        ),
                )
                .child(
                    tabular_numerals(self.application_text(
                        &format!("participant-seen-{}", person.account),
                        seen,
                        cx,
                    ))
                    .w(ui(119.))
                    .truncate()
                    .text_color(rgba(0x000000b3)),
                );
            table = table.child(row);
        }
        table
    }

    fn instance_folders(&mut self, owner: &LaunchOwner, cx: &mut Context<Self>) -> gpui::Div {
        let mut table = div()
            .mt(ui(8.))
            .flex_none()
            .child(self.inspector_table_header(&["Local folder", "Name", "Permissions"], cx));
        for mount in &owner.instance.host_mounts {
            let path = mount.path();
            let path = std::env::home_dir()
                .and_then(|home| {
                    path.strip_prefix(home).ok().map(|rest| format!("~/{}", rest.display()))
                })
                .unwrap_or_else(|| path.display().to_string());
            table = table.child(
                table_row()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(ui(6.))
                            .child(initial(&mount.label, false))
                            .child(
                                self.application_text(
                                    &format!("mount-path-{}", mount.id),
                                    path,
                                    cx,
                                )
                                .truncate(),
                            ),
                    )
                    .child(
                        self.application_text(
                            &format!("mount-name-{}", mount.id),
                            mount.label.clone(),
                            cx,
                        )
                        .w(ui(71.))
                        .truncate(),
                    )
                    .child(
                        self.application_text(
                            &format!("mount-permissions-{}", mount.id),
                            if mount.write { "Read + Write" } else { "Read-only" },
                            cx,
                        )
                        .w(ui(83.))
                        .truncate(),
                    ),
            );
        }
        if owner.instance.host_mounts.is_empty() {
            table = table.child(
                self.application_text("application-no-mounts", "No local folders granted", cx)
                    .px(ui(12.))
                    .text_size(ui(11.))
                    .text_color(rgba(0x00000080)),
            );
        }
        table
    }

    fn inspector_table_header(
        &mut self,
        columns: &[&'static str],
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut header = table_row()
            .h(ui(32.))
            .mb(ui(8.))
            .border_b_1()
            .border_color(theme::column_divider_color());
        for (i, &label) in columns.iter().enumerate() {
            let mut cell = div()
                .h(ui(24.))
                .flex()
                .items_center()
                .child(self.application_text(label, label, cx).flex_1().min_w_0().truncate());
            if i == 0 {
                cell = cell
                    .flex_1()
                    .min_w_0()
                    .pr(ui(4.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(
                        gpui::svg()
                            .path("table-sort-ascending")
                            .size(ui(16.))
                            .flex_none()
                            .text_color(rgba(0x000000b3)),
                    );
            } else {
                cell = cell
                    .w(ui(if columns.len() == 2 {
                        119.
                    } else if i == 1 {
                        71.
                    } else {
                        83.
                    }))
                    .text_color(rgba(0x000000b3));
            }
            if i + 1 < columns.len() {
                cell = cell.border_r_1().border_color(theme::column_divider_color());
            }
            header = header.child(cell);
        }
        header
    }
}

fn table_row() -> gpui::Div {
    div()
        .w_full()
        .flex_none()
        .h(ui(24.))
        .flex()
        .items_center()
        .px(ui(12.))
        .gap(ui(8.))
        .text_size(ui(11.))
        .line_height(ui(16.))
        .text_color(rgba(0x000000e6))
}

pub(super) fn initial(name: &str, filled: bool) -> gpui::Div {
    div()
        .size(ui(16.))
        .flex_none()
        .rounded_full()
        .border_1()
        .border_color(gpui::black())
        .flex()
        .justify_center()
        .items_center()
        .text_size(ui(10.))
        .line_height(ui(14.))
        .when(filled, |v| v.bg(gpui::black()).text_color(gpui::white()))
        .child(name.chars().next().unwrap_or('?').to_uppercase().to_string())
}
