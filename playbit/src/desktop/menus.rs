//! Native application menu commands.
//! Editing uses the same actions as keyboard input, preserving native focus/IME.
#![allow(unexpected_cfgs)] // objc's macros check their optional cargo-clippy feature.
use crate::ui::desktop::text_editor;
use crate::ui::desktop::zoom;
use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType};

gpui::actions!(
    playbit_shell,
    [
        About,
        Hide,
        HideOthers,
        ShowAll,
        Quit,
        SignOut,
        CloseWindow,
        Minimize,
        Zoom,
        BringAllToFront,
        ShowSessions,
        Help
    ]
);

pub fn install(cx: &mut App) {
    let _timing = crate::trace::Span::new("ui.install_menus");
    let registrations = crate::trace::Span::new("ui.menu.actions_and_keys");
    zoom::install(cx);
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    #[cfg(target_os = "macos")]
    cx.on_action(show_about);
    cx.on_action(|_: &Help, cx| cx.open_url("https://playbit.app/docs/"));
    cx.on_action(|_: &CloseWindow, cx| {
        if cx.active_window().is_some_and(|handle| {
            handle.downcast::<crate::ui::desktop::sign_in::SignIn>().is_some()
        }) {
            return;
        }
        defer_active_window_action(cx, |_, window, _| window.remove_window());
    });
    cx.on_action(|_: &Minimize, cx| {
        defer_active_window_action(cx, |_, window, _| window.minimize_window());
    });
    cx.on_action(|_: &Zoom, cx| {
        defer_active_window_action(cx, |_, window, _| window.zoom_window());
    });
    cx.on_action(|_: &BringAllToFront, cx| {
        cx.defer(|cx| {
            for handle in cx.windows() {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            }
            cx.activate(true);
        });
    });
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-?", Help, None),
    ]);
    drop(registrations);
    let definitions_timing = crate::trace::Span::new("ui.menu.definitions");
    let menus = definitions();
    drop(definitions_timing);
    let _native = crate::trace::Span::new("ui.menu.native_install");
    cx.set_menus(menus);
}

fn defer_active_window_action(
    cx: &mut App,
    action: fn(gpui::AnyView, &mut gpui::Window, &mut App),
) {
    let _dispatch = crate::trace::Span::new("ui.window.command.dispatch");
    if let Some(handle) = cx.active_window() {
        let admitted = crate::trace::clock_ns();
        // Keyboard dispatch already borrows this window. Retain its identity,
        // then update after dispatch releases it (also safe if it closes).
        cx.defer(move |cx| {
            crate::trace::mark_related_at(
                "ui.window.command.admitted",
                Default::default(),
                admitted,
            );
            let lookup = crate::trace::Span::new("ui.window.command.lookup");
            let _ = handle.update(cx, |root, window, cx| {
                drop(lookup);
                let _apply = crate::trace::Span::new("ui.window.command.apply");
                action(root, window, cx);
            });
        });
    }
}

fn definitions() -> Vec<Menu> {
    vec![
        Menu {
            name: "Playbit".into(),
            items: vec![
                MenuItem::action("About Playbit", About),
                #[cfg(target_os = "macos")]
                MenuItem::action("Check for Updates...", super::update::CheckForUpdates),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide Playbit", Hide),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Sign out...", SignOut),
                MenuItem::separator(),
                MenuItem::action("Quit Playbit", Quit),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Undo", text_editor::Undo, OsAction::Undo),
                MenuItem::os_action("Redo", text_editor::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", text_editor::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", text_editor::Copy, OsAction::Copy),
                MenuItem::os_action("Paste", text_editor::Paste, OsAction::Paste),
                MenuItem::action("Delete", text_editor::Backspace),
                MenuItem::os_action("Select All", text_editor::SelectAll, OsAction::SelectAll),
            ],
        },
        zoom::menu(),
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::os_action("Minimize", Minimize, OsAction::Minimize),
                MenuItem::os_action("Zoom", Zoom, OsAction::Zoom),
                MenuItem::os_action("Close Window", CloseWindow, OsAction::CloseWindow),
                MenuItem::separator(),
                MenuItem::action("Bring All to Front", BringAllToFront),
                MenuItem::action("Sessions", ShowSessions),
            ],
        },
        Menu { name: "Help".into(), items: vec![MenuItem::action("Playbit Help", Help)] },
    ]
}

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "AppKit owns this main-thread callback and the standard About panel."
)]
fn show_about(_: &About, _: &mut App) {
    use cocoa::foundation::NSString;
    use objc::{class, msg_send, runtime::Object, sel, sel_impl};
    // AppKit owns this callback's main thread and its standard About panel.
    // Override the numeric bundle fields with the same version the CLI reports.
    unsafe {
        let version = NSString::alloc(std::ptr::null_mut()).init_str(crate::VERSION);
        let empty = NSString::alloc(std::ptr::null_mut()).init_str("");
        let application_key = NSString::alloc(std::ptr::null_mut()).init_str("ApplicationVersion");
        let build_key = NSString::alloc(std::ptr::null_mut()).init_str("Version");
        let values = [version, empty];
        let keys = [application_key, build_key];
        let options: *mut Object = msg_send![class!(NSDictionary),
            dictionaryWithObjects: values.as_ptr() forKeys: keys.as_ptr() count: keys.len()];
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, orderFrontStandardAboutPanelWithOptions: options];
        for string in [version, empty, application_key, build_key] {
            let _: () = msg_send![string, release];
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[gpui::test]
    fn close_command_from_window_dispatch_closes_only_its_target(cx: &mut gpui::TestAppContext) {
        use gpui::Focusable;
        use std::borrow::Cow;
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../../assets/InterVariable.ttf"))])
            .unwrap();
        cx.update(text_editor::install_keybindings);
        cx.update(install);
        let other = cx.add_window(|_, cx| text_editor::TextEditor::new("Other draft", cx));
        let target = cx.add_window(|_, cx| text_editor::TextEditor::new("Target draft", cx));
        target
            .update(cx, |editor, window, cx| {
                window.activate_window();
                editor.focus_handle(cx).focus(window);
            })
            .unwrap();
        cx.run_until_parked();
        target
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(CloseWindow), cx);
                cx.defer(move |cx| {
                    other.update(cx, |_, window, _| window.activate_window()).unwrap();
                });
            })
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(cx.windows() == vec![other.into()], "only the other window should remain");
        });
        assert_eq!(
            other.read_with(cx, |editor, _| editor.text().to_owned()).unwrap(),
            "Other draft"
        );

        let closing = cx.add_window(|_, cx| text_editor::TextEditor::new("Closing", cx));
        closing
            .update(cx, |editor, window, cx| {
                window.activate_window();
                editor.focus_handle(cx).focus(window);
            })
            .unwrap();
        cx.run_until_parked();
        closing
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(CloseWindow), cx);
                cx.defer(move |cx| {
                    closing.update(cx, |_, window, _| window.remove_window()).unwrap();
                    other.update(cx, |_, window, _| window.activate_window()).unwrap();
                });
            })
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert!(cx.windows() == vec![other.into()]));
    }

    #[gpui::test]
    fn read_only_editor_exposes_selection_and_copy_without_mutating_commands(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::Focusable;
        use std::borrow::Cow;
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../../assets/InterVariable.ttf"))])
            .unwrap();
        cx.update(text_editor::install_keybindings);
        let window =
            cx.add_window(|_, cx| text_editor::TextEditor::new("Recovery key", cx).read_only());
        window.update(cx, |editor, window, cx| editor.focus_handle(cx).focus(window)).unwrap();
        cx.run_until_parked();
        for item in &definitions()[1].items {
            if let MenuItem::Action { name, action, .. } = item {
                let expected =
                    !matches!(name.as_ref(), "Cut" | "Paste" | "Delete" | "Undo" | "Redo");
                window
                    .update(cx, |_, window, cx| {
                        assert_eq!(
                            window.is_action_available(action.as_ref(), cx),
                            expected,
                            "{name}"
                        );
                    })
                    .unwrap();
            }
        }
    }

    #[gpui::test]
    fn menu_edit_actions_follow_focused_editor(cx: &mut gpui::TestAppContext) {
        use gpui::Focusable;
        use std::borrow::Cow;
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../../assets/InterVariable.ttf"))])
            .unwrap();
        cx.update(text_editor::install_keybindings);
        let menus = definitions();
        assert!(menus.iter().map(|menu| menu.items.len()).sum::<usize>() <= 34);
        let edit = &menus[1];
        let action = |label: &str| {
            edit.items
                .iter()
                .find_map(|item| match item {
                    MenuItem::Action { name, action, .. } if name.as_ref() == label => {
                        Some(action.boxed_clone())
                    }
                    _ => None,
                })
                .unwrap()
        };
        assert!(!cx.update(|cx| cx.is_action_available(action("Cut").as_ref())));
        let window = cx.add_window(|_, cx| text_editor::TextEditor::new("Menu café 日本語", cx));
        let editor = window.root(cx).unwrap();
        window.update(cx, |editor, window, cx| editor.focus_handle(cx).focus(window)).unwrap();
        cx.run_until_parked();
        for (command, expected) in [
            ("Select All", "Menu café 日本語"),
            ("Copy", "Menu café 日本語"),
            ("Cut", ""),
            ("Undo", "Menu café 日本語"),
            ("Redo", ""),
            ("Paste", "Menu café 日本語"),
        ] {
            window
                .update(cx, |_, window, cx| {
                    assert!(window.is_action_available(action(command).as_ref(), cx));
                    window.dispatch_action(action(command), cx);
                })
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                editor.read_with(cx, |editor, _| editor.text().to_owned()),
                expected,
                "{command}"
            );
        }
    }
}
