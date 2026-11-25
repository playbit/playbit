//! Reusable native visual fixture, compiled only with GUI qualification tools.
use super::*;
use crate::engine::reply::TransferProgress;

gpui::actions!(
    status_bar_preview,
    [
        Volumes,
        Restart,
        Connection,
        Failure,
        CrashReport,
        Idle,
        Narrow,
        Compact,
        Wide,
        LogOutput,
        LogStopped,
        AppInfoReady,
        AppInfoFailed,
        Feedback,
        LongFeedback,
        ApLive,
        ApConnecting,
        ApOffline
    ]
);

fn set(
    host: &Entity<Shell>,
    engine_recovering: bool,
    kind: TransferKind,
    progress: Option<TransferProgress>,
    cx: &mut App,
) {
    host.update(cx, |host, cx| {
        host.engine_recovering = engine_recovering;
        host.sending_crash_reports.update(cx, |sending, cx| {
            *sending = false;
            cx.notify();
        });
        host.last_transfer = TransferStatus {
            kind,
            progress,
            message: (kind == TransferKind::Failed).then(|| {
                concat!(
                    "Timed out waiting for initial database synchronization\n",
                    "Profile: \"/fixture/profile\"\n",
                    "Hint: set PLAYBIT_PROFILE=/path/to/new-profile to use a separate store",
                )
                .into()
            }),
            ..Default::default()
        };
        cx.notify();
    });
}

fn initialize(cx: &mut App) {
    #[cfg(target_os = "macos")]
    crate::ui::desktop::appearance::set_light_appearance();
    cx.text_system()
        .add_fonts(vec![
            std::borrow::Cow::Borrowed(crate::ui::desktop::UI_FONT_DATA),
            std::borrow::Cow::Borrowed(include_bytes!("../../assets/JetBrainsMonoNL-Medium.ttf")),
        ])
        .expect("bundled control font");
    text_editor::install_keybindings(cx);
    menus::install(cx);
}

pub(super) fn main() {
    run(false, None, None);
}

pub(super) fn updates() {
    run(true, None, None);
}

pub(super) fn sidebar(icons: Option<std::path::PathBuf>, session: Option<String>) {
    run(false, Some(icons), session);
}

fn run(updates: bool, sidebar: Option<Option<std::path::PathBuf>>, session: Option<String>) {
    crate::engine::profile::initialize().expect("isolated preview profile");
    Application::new().with_assets(controls::Assets).run(move |cx| {
        initialize(cx);
        let (send, requests, errors) = worker_queue::channel();
        let (_responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        // Explicitly disconnected fixture: controls cannot operate on a real profile.
        drop(requests);
        host.update(cx, |host, _| {
            host.enrolled = Some(true);
            host.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: Some("Status preview".into()),
                unlocked: true,
                initialized: true,
            };
            host.recovery = Some(RecoveryStatus::Ready {
                revision: 1,
                pending_save: false,
                restored_sessions: None,
                restored_volumes: None,
            });
            host.sessions = (1..=4)
                .map(|n| SessionRow {
                    reference: String::new(),
                    id: format!("status-preview-{n}"),
                    resolved_build_id: String::new(),
                    build_state: crate::engine::builds::BuildState::Ready,
                    app_name: Some(format!("TODO {n}")),
                })
                .collect();
        });
        cx.bind_keys([
            gpui::KeyBinding::new("cmd-ctrl-1", ApLive, None),
            gpui::KeyBinding::new("cmd-ctrl-2", ApConnecting, None),
            gpui::KeyBinding::new("cmd-ctrl-3", ApOffline, None),
        ]);
        for state in [
            crate::engine::reply::ApConnectionState::Live,
            crate::engine::reply::ApConnectionState::Connecting,
            crate::engine::reply::ApConnectionState::Offline,
        ] {
            let target = host.clone();
            let apply = move |cx: &mut App| {
                target.update(cx, |host, cx| {
                    host.ap_status.update(cx, |indicator, cx| {
                        indicator.set(
                            Some(crate::engine::reply::ApConnectionStatus {
                                state,
                                hostname: "ap.playbit.app".into(),
                            }),
                            cx,
                        )
                    });
                })
            };
            match state {
                crate::engine::reply::ApConnectionState::Live => {
                    cx.on_action(move |_: &ApLive, cx| apply(cx))
                }
                crate::engine::reply::ApConnectionState::Connecting => {
                    cx.on_action(move |_: &ApConnecting, cx| apply(cx))
                }
                crate::engine::reply::ApConnectionState::Offline
                | crate::engine::reply::ApConnectionState::OfflineMode => {
                    cx.on_action(move |_: &ApOffline, cx| apply(cx))
                }
            }
        }
        host.update(cx, |host, cx| {
            host.ap_status.update(cx, |indicator, cx| {
                indicator.set(
                    Some(crate::engine::reply::ApConnectionStatus {
                        state: crate::engine::reply::ApConnectionState::Live,
                        hostname: "ap.playbit.app".into(),
                    }),
                    cx,
                )
            })
        });
        set(
            &host,
            false,
            TransferKind::Running,
            Some(TransferProgress::Volumes { current: 3, total: 4 }),
            cx,
        );
        cx.bind_keys([
            gpui::KeyBinding::new("cmd-alt-1", Volumes, None),
            gpui::KeyBinding::new("cmd-alt-2", Restart, None),
            gpui::KeyBinding::new("cmd-alt-3", Connection, None),
            gpui::KeyBinding::new("cmd-alt-4", Failure, None),
            gpui::KeyBinding::new("cmd-alt-5", CrashReport, None),
            gpui::KeyBinding::new("cmd-alt-0", Idle, None),
            gpui::KeyBinding::new("cmd-alt-6", Narrow, None),
            gpui::KeyBinding::new("cmd-alt-7", Compact, None),
            gpui::KeyBinding::new("cmd-alt-8", Wide, None),
        ]);
        let target = host.clone();
        cx.on_action(move |_: &Volumes, cx| {
            set(
                &target,
                false,
                TransferKind::Running,
                Some(TransferProgress::Volumes { current: 3, total: 4 }),
                cx,
            )
        });
        let target = host.clone();
        cx.on_action(move |_: &Restart, cx| set(&target, true, TransferKind::Idle, None, cx));
        let target = host.clone();
        cx.on_action(move |_: &Connection, cx| {
            set(
                &target,
                false,
                TransferKind::Running,
                Some(TransferProgress::Connecting { hostname: "ap-1.playbit.app".into() }),
                cx,
            )
        });
        let target = host.clone();
        cx.on_action(move |_: &Failure, cx| set(&target, false, TransferKind::Failed, None, cx));
        let target = host.clone();
        cx.on_action(move |_: &CrashReport, cx| {
            target.update(cx, |host, cx| {
                host.engine_recovering = false;
                host.sending_crash_reports.update(cx, |sending, cx| {
                    *sending = true;
                    cx.notify();
                });
                cx.notify();
            });
        });
        let target = host.clone();
        cx.on_action(move |_: &Idle, cx| set(&target, false, TransferKind::Idle, None, cx));
        if updates {
            super::update::preview(host.downgrade(), cx);
        }
        if let Some(icons) = &sidebar {
            host.update(cx, |host, _| {
                host.account_profile.display_name = Some("Anne Hammersmith".into());
                host.account_id = "SJs5Gs9dijSjMs34ds0d".into();
                host.sidebar_volumes = true;
                host.last_transfer = TransferStatus::default();
                host.volumes = ["Archive", "Family"]
                    .into_iter()
                    .map(|name| crate::store::Volume {
                        id: crate::store::Id::generate().expect("fixture id"),
                        name: name.into(),
                        owner: crate::store::Id::generate().expect("fixture owner"),
                        personal: true,
                        share_key_with_service: false,
                    })
                    .collect();
                host.sessions =
                    ["Micro Kart", "Pinball", "Banana Bingo", "CorkBoard", "Social Importer"]
                        .into_iter()
                        .enumerate()
                        .map(|(n, name)| SessionRow {
                            reference: String::new(),
                            id: crate::identifier::encode(&[0x31 + n as u8; 32]),
                            resolved_build_id: crate::identifier::encode(&{
                                let mut bytes = [0; 32];
                                bytes[16..].copy_from_slice(&(n as u128).to_be_bytes());
                                bytes
                            }),
                            build_state: crate::engine::builds::BuildState::Ready,
                            app_name: Some(name.into()),
                        })
                        .collect();
                if let Some(directory) = icons {
                    host.session_icons.load_preview(directory, &mut host.sessions);
                }
                if session.as_deref() == Some("loading") {
                    host.sidebar_volumes = false;
                    host.sessions[3].build_state = crate::engine::builds::BuildState::Downloading;
                    host.sessions[4].build_state = crate::engine::builds::BuildState::Checking;
                    host.sessions[4].app_name = None;
                    host.session_icons.loading_preview(&host.sessions);
                }
            });
        }
        if session.as_deref() == Some("loading") {
            cx.bind_keys([
                gpui::KeyBinding::new("cmd-alt-r", AppInfoReady, None),
                gpui::KeyBinding::new("cmd-alt-e", AppInfoFailed, None),
            ]);
            let ready = host.clone();
            cx.on_action(move |_: &AppInfoReady, cx| {
                ready.update(cx, |host, cx| {
                    host.sessions[4].app_name = Some("Social Importer".into());
                    host.sessions[4].build_state = crate::engine::builds::BuildState::Ready;
                    host.session_icons.loading_preview(&host.sessions);
                    cx.notify();
                });
                let window = ready.read(cx).sessions_window.expect("preview window");
                cx.defer(move |cx| {
                    window
                        .update(cx, |_, window, _| window.activate_window())
                        .expect("activate preview");
                    cx.activate(true);
                });
            });
            let failed = host.clone();
            cx.on_action(move |_: &AppInfoFailed, cx| {
                failed.update(cx, |host, cx| {
                    host.sessions[4].app_name = None;
                    host.sessions[4].build_state =
                        crate::engine::builds::BuildState::DownloadFailed {
                            error: "The network connection was lost".into(),
                        };
                    host.session_icons.loading_preview(&host.sessions);
                    cx.notify();
                });
                let window = failed.read(cx).sessions_window.expect("preview window");
                cx.defer(move |cx| {
                    window
                        .update(cx, |_, window, _| window.activate_window())
                        .expect("activate preview");
                    cx.activate(true);
                });
            });
        }
        if let Some(mode) = &session
            && mode != "loading"
        {
            host.update(cx, |host, cx| {
                host.sidebar_volumes = false;
                if mode == "new-empty" {
                    host.sessions.clear();
                    return;
                }
                host.recent_sessions = host.sessions.iter().map(|row| row.id.clone()).collect();
                if mode == "builds" {
                    use crate::engine::builds::BuildState;
                    for (row, state) in host.sessions.iter_mut().zip([
                        BuildState::NotDownloaded,
                        BuildState::Downloading,
                        BuildState::Unreadable { error: "Permission denied".into() },
                        BuildState::Invalid { error: "build content hash mismatch".into() },
                        BuildState::DownloadFailed { error: "blob not found".into() },
                    ]) {
                        row.build_state = state;
                    }
                    host.sidebar_session = Some(host.sessions[4].id.clone());
                    return;
                }
                if mode == "new" {
                    return;
                }
                if matches!(mode.as_str(), "active" | "empty" | "stopped" | "instance") {
                    host.preview_session_browser(mode, cx);
                } else {
                    host.sidebar_session = Some(host.sessions[1].id.clone());
                }
                let id = host.sidebar_session.clone().expect("selected preview session");
                let owner = host.applications.highlighted_instance().map(|owner| owner.instance.id);
                let instance = host.applications.instance;
                host.logs.update(cx, |logs, cx| {
                    use crate::ui::app_control::{LogBatch, LogCursor, LogEntry, LogSeverity};
                    logs.select(&id, instance, cx);
                    if let Some(owner) = owner {
                        logs.receive(&id, log::Update::Running(owner), cx);
                    }
                    if mode == "empty" {
                        return;
                    }
                    let line_count = if mode == "benchmark" { 256 } else { 12 };
                    let lines = (0..line_count)
                        .map(|i| {
                            LogEntry::recorded(
                                if i == 3 {
                                    "Example owner failure".into()
                                } else if i == 6 && mode == "instance" {
                                    "Notification sent to user regarding updates with a really long message that causes it to wrap".into()
                                } else {
                                    format!("Example log message {i}")
                                },
                                if i == 3 { LogSeverity::Error } else { LogSeverity::Info },
                            )
                        })
                        .collect();
                    logs.receive(
                        &id,
                        log::Update::Batch(LogBatch {
                            cursor: LogCursor {
                                instance: owner
                                    .unwrap_or_else(|| crate::store::Id::from_bytes([1; 16])),
                                generation: 1,
                                next: line_count,
                            },
                            lines,
                            dropped: 0,
                            reset: false,
                        }),
                        cx,
                    );
                    if mode == "stopped" {
                        logs.receive(&id, log::Update::Stopped, cx);
                    }
                });
            });
        }
        Shell::show_sessions(&host, cx);
        host.read(cx)
            .sessions_window
            .expect("preview window")
            .update(cx, |_, window, _| {
                window.set_window_title(if session.is_some() {
                    "Playbit — Session preview"
                } else if sidebar.is_some() {
                    "Playbit — Sidebar preview"
                } else {
                    "Playbit — Status bar preview"
                });
            })
            .expect("preview window");
        cx.activate(true);
        let target = host.clone();
        cx.on_action(move |_: &Narrow, cx| resize_preview(&target, 650., cx));
        let target = host.clone();
        cx.on_action(move |_: &Compact, cx| resize_preview(&target, 540., cx));
        let target = host.clone();
        cx.on_action(move |_: &Wide, cx| resize_preview(&target, 1024., cx));
        if session.is_some() {
            cx.bind_keys([
                gpui::KeyBinding::new("cmd-alt-9", LogOutput, None),
                gpui::KeyBinding::new("cmd-alt-s", LogStopped, None),
                gpui::KeyBinding::new("cmd-alt-b", Feedback, None),
                gpui::KeyBinding::new("cmd-alt-shift-b", LongFeedback, None),
            ]);
            let target = host.clone();
            cx.on_action(move |_: &Feedback, cx| {
                target.update(cx, |host, cx| {
                    host.show_visual_bell("Account info copied to clipboard", "session-share", cx)
                });
            });
            let target = host.clone();
            cx.on_action(move |_: &LongFeedback, cx| {
                target.update(cx, |host, cx| {
                    host.show_visual_bell(
                        &"Copied account info.\nPaste it into your message.\t".repeat(12),
                        "menu-check",
                        cx,
                    )
                });
            });
            let target = host.clone();
            cx.on_action(move |_: &LogOutput, cx| {
                target.update(cx, |host, cx| {
                    let Some(id) = host.sidebar_session.clone() else {
                        return;
                    };
                    host.logs.update(cx, |logs, cx| {
                        use crate::ui::app_control::{LogBatch, LogCursor, LogEntry, LogSeverity};
                        let next = logs.log_cursor.map_or(0, |cursor| cursor.next);
                        logs.receive(
                            &id,
                            log::Update::Batch(LogBatch {
                                cursor: LogCursor {
                                    instance: crate::store::Id::from_bytes([1; 16]),
                                    generation: 1,
                                    next: next + 32,
                                },
                                lines: (next..next + 32)
                                    .map(|i| {
                                        LogEntry::recorded(
                                            format!("Appended log message {i}"),
                                            LogSeverity::Info,
                                        )
                                    })
                                    .collect(),
                                dropped: 0,
                                reset: false,
                            }),
                            cx,
                        );
                    });
                    cx.notify();
                });
            });
            let target = host.clone();
            cx.on_action(move |_: &LogStopped, cx| {
                target.update(cx, |host, cx| {
                    let Some(id) = host.sidebar_session.clone() else {
                        return;
                    };
                    host.logs.update(cx, |logs, cx| logs.receive(&id, log::Update::Stopped, cx));
                    cx.notify();
                });
            });
        }
        if session.as_deref() == Some("benchmark") {
            measure_sessions(host, cx);
        }
    });
}

fn resize_preview(host: &Entity<Shell>, width: f32, cx: &mut App) {
    let handle = host.read(cx).sessions_window.expect("preview window");
    cx.defer(move |cx| {
        handle
            .update(cx, |_, window, _| window.resize(size(px(width), px(560.))))
            .expect("resize preview");
    });
}

// Semantic selection and native wheel dispatch through the actual GPUI renderer.
// Synchronous draw ends at CPU scene construction, not physical presentation.
fn measure_sessions(host: Entity<Shell>, cx: &mut App) {
    let handle = host.read(cx).sessions_window.expect("fixture window");
    cx.spawn(async move |cx| {
        cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
        let mut status_samples = Vec::with_capacity(1000);
        for i in 0..1020 {
            let start = std::time::Instant::now();
            host.update(cx, |host, cx| {
                let state = [
                    crate::engine::reply::ApConnectionState::Live,
                    crate::engine::reply::ApConnectionState::Connecting,
                    crate::engine::reply::ApConnectionState::Offline,
                ][i % 3];
                host.ap_status.update(cx, |indicator, cx| {
                    indicator.set(
                        Some(crate::engine::reply::ApConnectionStatus {
                            state,
                            hostname: "ap.playbit.app".into(),
                        }),
                        cx,
                    )
                });
            })
            .expect("fixture AP status");
            gpui::AnyWindowHandle::from(handle)
                .update(cx, |_, window, cx| {
                    let _ = window.draw(cx);
                })
                .expect("fixture AP status draw");
            if i >= 20 {
                status_samples.push(start.elapsed().as_secs_f64() * 1000.);
            }
        }
        status_samples.sort_by(f64::total_cmp);
        println!(
            "AP status + CPU scene: n=1000 p50={:.3} p95={:.3} p99={:.3} max={:.3} ms",
            status_samples[499], status_samples[949], status_samples[989], status_samples[999]
        );
        host.update(cx, |host, cx| {
            host.ap_status.update(cx, |indicator, cx| {
                indicator.set(
                    Some(crate::engine::reply::ApConnectionStatus {
                        state: crate::engine::reply::ApConnectionState::Connecting,
                        hostname: "ap.playbit.app".into(),
                    }),
                    cx,
                )
            })
        })
        .expect("fixture concurrent spinner");
        for count in [0, 4, 128, 256] {
            host.update(cx, |host, cx| {
                host.sessions = (0..count)
                    .map(|i| SessionRow {
                        reference: String::new(),
                        build_state: crate::engine::builds::BuildState::Ready,
                        id: format!("fixture-{i}"),
                        app_name: Some(format!("Example {i}")),
                        resolved_build_id: crate::identifier::encode(&{
                            let mut bytes = [0; 32];
                            bytes[16..].copy_from_slice(&(i as u128).to_be_bytes());
                            bytes
                        }),
                    })
                    .collect();
                host.logs.update(cx, |logs, cx| {
                    let lines = vec![
                        crate::ui::app_control::LogEntry::recorded(
                            "x".repeat(256),
                            crate::ui::app_control::LogSeverity::Info
                        );
                        256
                    ];
                    logs.select("fixture-0", None, cx);
                    logs.set("fixture-0", &lines, cx);
                    logs.select("fixture-1", None, cx);
                    logs.set("fixture-1", &lines, cx);
                });
                cx.notify();
            })
            .expect("fixture host");
            let mut callbacks = Vec::with_capacity(1000);
            let mut scenes = Vec::with_capacity(1000);
            for i in 0..1020 {
                let start = std::time::Instant::now();
                host.update(cx, |host, cx| {
                    let id = format!("fixture-{}", i % 2);
                    host.sidebar_session = Some(id.clone());
                    host.logs.update(cx, |logs, cx| logs.select(&id, None, cx));
                    cx.notify();
                })
                .expect("fixture selection");
                let callback = start.elapsed().as_secs_f64() * 1000.;
                gpui::AnyWindowHandle::from(handle)
                    .update(cx, |_, window, cx| {
                        let _ = window.draw(cx);
                    })
                    .expect("fixture draw");
                if i >= 20 {
                    callbacks.push(callback);
                    scenes.push(start.elapsed().as_secs_f64() * 1000.);
                }
            }
            for (name, mut samples) in
                [("selection callback", callbacks), ("selection + CPU scene", scenes)]
            {
                samples.sort_by(f64::total_cmp);
                println!(
                    "{name}: sessions={count} rows=256 bytes=65536 n=1000 p50={:.3} p99={:.3} \
                     max={:.3} ms",
                    samples[499], samples[989], samples[999]
                );
            }
            let mut samples = Vec::with_capacity(1000);
            for i in 0..1000 {
                let start = std::time::Instant::now();
                gpui::AnyWindowHandle::from(handle)
                    .update(cx, |_, window, cx| {
                        window.dispatch_event(
                            gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                                position: gpui::point(px(500.), px(200.)),
                                delta: gpui::ScrollDelta::Pixels(gpui::point(
                                    px(0.),
                                    px(if i % 2 == 0 { -10. } else { 10. }),
                                )),
                                ..Default::default()
                            }),
                            cx,
                        );
                        let _ = window.draw(cx);
                    })
                    .expect("fixture wheel and draw");
                samples.push(start.elapsed().as_secs_f64() * 1000.);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "wheel + CPU scene: sessions={count} n=1000 p50={:.3} p99={:.3} max={:.3} ms",
                samples[499], samples[989], samples[999]
            );
            host.update(cx, |host, _| {
                for (index, row) in host.sessions.iter_mut().enumerate() {
                    row.id =
                        crate::identifier::encode(blake3::hash(&index.to_le_bytes()).as_bytes());
                    row.app_name = None;
                    row.build_state = crate::engine::builds::BuildState::Downloading;
                }
            })
            .expect("loading fixture");
            let mut samples = Vec::with_capacity(1000);
            for index in 0..1020 {
                let start = std::time::Instant::now();
                host.update(cx, |host, cx| {
                    let _ = host.sidebar_view(true, cx);
                })
                .expect("loading rows");
                if index >= 20 {
                    samples.push(start.elapsed().as_secs_f64() * 1000.);
                }
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "loading sidebar construction: sessions={count} n=1000 p50={:.3} p99={:.3} \
                 max={:.3} ms",
                samples[499], samples[989], samples[999]
            );
        }
        host.update(cx, |host, cx| host.logs.update(cx, |logs, cx| logs.clear(cx)))
            .expect("fixture clear");
        let mut samples = Vec::with_capacity(1000);
        for next in 0..1280 {
            let start = std::time::Instant::now();
            host.update(cx, |host, cx| {
                host.logs.update(cx, |logs, cx| {
                    use crate::ui::app_control::{LogBatch, LogCursor, LogEntry, LogSeverity};
                    let id = logs.session.clone().expect("fixture session");
                    logs.receive(
                        &id,
                        log::Update::Batch(LogBatch {
                            cursor: LogCursor {
                                instance: crate::store::Id::from_bytes([1; 16]),
                                generation: 1,
                                next,
                            },
                            lines: vec![LogEntry::recorded("x".repeat(256), LogSeverity::Info)],
                            dropped: 0,
                            reset: false,
                        }),
                        cx,
                    );
                    // Maximum marker occupancy: one completed run per retained message.
                    logs.receive(&id, log::Update::Stopped, cx);
                });
            })
            .expect("fixture log update");
            gpui::AnyWindowHandle::from(handle)
                .update(cx, |_, window, cx| {
                    let _ = window.draw(cx);
                })
                .expect("fixture log draw");
            if next >= 280 {
                samples.push(start.elapsed().as_secs_f64() * 1000.);
            }
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "log append/stop + CPU scene: rows<=512 bytes=65536 n=1000 p50={:.3} \
             p99={:.3} max={:.3} ms",
            samples[499], samples[989], samples[999]
        );
        let mut samples = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = std::time::Instant::now();
            host.update(cx, |host, cx| {
                host.show_visual_bell("Account info copied to clipboard", "session-share", cx)
            })
            .expect("fixture feedback");
            gpui::AnyWindowHandle::from(handle)
                .update(cx, |_, window, cx| {
                    let _ = window.draw(cx);
                })
                .expect("fixture feedback draw");
            samples.push(start.elapsed().as_secs_f64() * 1000.);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "Visual Bell + CPU scene: sessions=256 log_rows<=512 n=1000 p50={:.3} \
             p99={:.3} max={:.3} ms",
            samples[499], samples[989], samples[999]
        );
        cx.update(|cx| cx.quit()).expect("fixture quit");
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Focusable;

    #[gpui::test]
    fn preview_initialization_supports_field_focus_and_editing(cx: &mut gpui::TestAppContext) {
        // Use the actual preview initialization before exercising focus and editing.
        cx.update(initialize);
        let window = cx.add_window(|_, cx| text_editor::TextEditor::new("Original", cx));
        cx.run_until_parked();
        window
            .update(cx, |editor, window, cx| window.focus(&editor.focus_handle(cx)))
            .expect("editor window");
        cx.run_until_parked();
        cx.simulate_keystrokes(window.into(), "cmd-a");
        cx.simulate_input(window.into(), "Edited");
        window
            .read_with(cx, |editor, _| assert_eq!(editor.text(), "Edited"))
            .expect("editor window");
        cx.simulate_keystrokes(window.into(), "backspace");
        window
            .read_with(cx, |editor, _| assert_eq!(editor.text(), "Edite"))
            .expect("editor window");
    }
}
