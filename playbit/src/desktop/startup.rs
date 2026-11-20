//! Application setup, URL delivery, and local engine startup.
use super::*;
use crate::engine::launch;

pub(super) fn main_with_focus(focus_window: bool) {
    match guest_app::main(focus_window) {
        Ok(true) => return,
        Ok(false) => (),
        Err(error) => {
            super::update::launch_error(&format!("Cannot open this app.\n\n{error}"));
            return;
        }
    }
    if let Err(error) = crate::engine::profile::initialize_gui() {
        super::update::launch_error(&error.to_string());
        return;
    }
    if let Err(error) = process_log::initialize(&crate::engine::profile_path()) {
        super::update::launch_error(&format!("Cannot open Playbit GUI log: {error}"));
        return;
    }
    crate::platform::failure::startup(&crate::engine::profile_path());
    crate::platform::failure::install_native(&crate::engine::profile_path());
    crate::trace::mark("app.main");
    // The engine always recovers from durable state automatically. A prior
    // panic report is diagnostic evidence, never a user-acknowledgment gate.
    let worker = worker::start(focus_window);
    let (send_url, receive_urls) = async_channel::bounded(4);
    let url_overflow = receive_urls.clone();
    let platform_timing = crate::trace::Span::new("ui.initialize_platform");
    let application = Application::new().with_assets(controls::Assets);
    drop(platform_timing);
    application.on_open_urls(move |urls| {
        crate::trace::mark("ui.open_urls_received");
        for invitation in urls {
            let url = if invitation.len() <= 64 * 1024 {
                Ok(invitation)
            } else {
                Err("Open request exceeds 64 KiB")
            };
            if send_url.try_send(url).is_err() {
                // Keep the queue bounded and reserve one visible rejection.
                let _ = url_overflow.try_recv();
                let _ = send_url.try_send(Err("Too many open requests; reopen the intended item"));
            }
        }
    });
    application.on_reopen(|cx| cx.dispatch_action(&menus::ShowSessions));
    application.run(move |cx: &mut App| {
        finish_launch(worker, receive_urls, cx.should_open_initial_window(), focus_window, cx);
    });
}

fn finish_launch(
    worker: (
        RequestSender,
        async_channel::Receiver<WorkerResponse>,
        async_channel::Receiver<&'static str>,
    ),
    receive_urls: async_channel::Receiver<Result<String, &'static str>>,
    open_initial: bool,
    focus_window: bool,
    cx: &mut App,
) {
    let _launch = crate::trace::Span::new("ui.finish_launching");
    cx.background_executor().spawn(async { agent_skill::install() }).detach();
    #[cfg(target_os = "macos")]
    crate::ui::desktop::appearance::set_light_appearance();
    {
        let _font = crate::trace::Span::new("ui.register_control_font");
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(crate::ui::desktop::UI_FONT_DATA),
                std::borrow::Cow::Borrowed(include_bytes!(
                    "../../assets/JetBrainsMonoNL-Medium.ttf"
                )),
            ])
            .expect("bundled control font must load");
    }
    cx.on_window_closed(|cx| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    text_editor::install_keybindings(cx);
    menus::install(cx);
    let window_timing = crate::trace::Span::new("ui.open_window");
    let host = if focus_window {
        Shell::launch(worker, receive_urls, open_initial, cx)
    } else {
        Shell::launch_with_focus(worker, receive_urls, open_initial, false, cx)
    };
    host.update(cx, |host, _| {
        if focus_window {
            host.crash_report_profile = Some(profile_path());
        }
    });
    let weak = host.downgrade();
    #[cfg(target_os = "macos")]
    update::setup(host.downgrade(), cx);
    cx.on_window_closed(move |cx| {
        let _ = weak.update(cx, |host, cx| {
            if host
                .sessions_window
                .is_some_and(|handle| !cx.windows().contains(&gpui::AnyWindowHandle::from(handle)))
            {
                host.sessions_window = None;
                host.session_tab = SessionTab::Log;
                host.tab_change_pending = None;
                cx.notify();
            }
        });
    })
    .detach();
    drop(window_timing);
    if focus_window {
        cx.activate(true);
        host.update(cx, |host, cx| {
            maybe_send_crash_reports(cx, host);
        });
    }
}

pub(super) fn start_local_engine(
    profile: &std::path::Path,
    recovering: bool,
    child: &mut Option<std::process::Child>,
) -> std::result::Result<(), String> {
    let _timing = crate::trace::Span::new("ui.start_local_engine");
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let engine = launch::executable_for(&executable).map_err(|e| e.to_string())?;
    launch::retain(launch::start_for_desktop(profile, &engine, recovering), child)
        .inspect(|_| {
            if let Err(error) = crate::trace::attach(profile) {
                crate::platform::failure::log(profile, "trace attachment", &format!("{error:?}"));
            }
            // A successful automatic reconnect acknowledges the prior crash.
            match std::fs::remove_file(profile.join("control/engine-crash")) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => crate::platform::failure::log(
                    profile,
                    "remove acknowledged crash marker",
                    &format!("{error:?}"),
                ),
            }
        })
        .map_err(|error| error.to_string())
}
