use super::*;
/// Reuses production native dialogs and the actual sidebar in an isolated shell.
pub(in crate::desktop) fn setup(host: gpui::WeakEntity<crate::desktop::Shell>, cx: &mut App) {
    gpui::actions!(update_preview, [Progress, Current, Available, Failure, Disabled]);
    let (choices, receive) = async_channel::bounded(1);
    cx.set_global(Updates {
        installation: None,
        state: State::Ready("0.1.0-beta.10".into()),
        busy: false,
        refresh_pending: false,
        _observer: None,
        prepared: Some("0.1.0-beta.10".into()),
        explicit: false,
        reopen: false,
        quitting: false,
        dialog: None,
        choices,
    });
    cx.observe_global::<Updates>(move |cx| {
        let _ = host.update(cx, |_, cx| cx.notify());
    })
    .detach();
    fn show(state: State, cx: &mut App) {
        let started = std::time::Instant::now();
        let updates = cx.global_mut::<Updates>();
        updates.prepared = match &state {
            State::Ready(version) => Some(version.clone()),
            _ => None,
        };
        updates.state = state;
        updates.explicit = true;
        present(cx);
        crate::diagnostic!("native_dialog_ready_ms={:.3}", started.elapsed().as_secs_f64() * 1000.);
    }
    cx.on_action(|_: &CheckForUpdates, cx| show(State::Ready("0.1.0-beta.10".into()), cx));
    cx.on_action(|_: &Upgrade, cx| show(State::Ready("0.1.0-beta.10".into()), cx));
    cx.on_action(|_: &Progress, cx| show(State::Checking, cx));
    cx.on_action(|_: &Current, cx| show(State::Idle, cx));
    cx.on_action(|_: &Available, cx| show(State::Ready("0.1.0-beta.10".into()), cx));
    cx.on_action(|_: &Disabled, cx| show(State::Disabled(DisabledReason::Development), cx));
    cx.on_action(|_: &Failure, cx| {
        let started = std::time::Instant::now();
        let updates = cx.global_mut::<Updates>();
        updates.state = State::Preparing("0.1.0-beta.10".into());
        updates.explicit = false;
        complete(Err(installation::LOCATION_ERROR.into()), cx);
        crate::diagnostic!("native_dialog_ready_ms={:.3}", started.elapsed().as_secs_f64() * 1000.);
    });
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-alt-p", Progress, None),
        gpui::KeyBinding::new("cmd-alt-u", Current, None),
        gpui::KeyBinding::new("cmd-alt-a", Available, None),
        gpui::KeyBinding::new("cmd-alt-e", Failure, None),
        gpui::KeyBinding::new("cmd-alt-d", Disabled, None),
    ]);
    cx.spawn(async move |cx| {
        while let Ok(choice) = receive.recv().await {
            crate::diagnostic!("native_dialog_choice={choice:?}");
            if cx
                .update(|cx| {
                    let updates = cx.global_mut::<Updates>();
                    updates.dialog.take();
                    updates.explicit = false;
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}
