//! Shared navigation rows keep their label, glyph and display hint separate.
use super::*;

impl Control {
    /// Only selectable, enabled rows participate in management list navigation.
    pub fn navigation_selection(&self) -> Option<bool> {
        if self.disabled || !self.is_navigation_row() { None } else { self.navigation }
    }

    /// A busy row can still leave its column while its next read is pending.
    pub fn has_navigation_focus(&self, window: &Window) -> bool {
        self.is_navigation_row() && self.focus.is_focused(window)
    }

    /// Arrow keys move logical row focus without entering Tab's focus-ring mode.
    pub fn focus_from_arrow(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.navigation_selection().is_none() {
            return false;
        }
        self.cancel_delayed_rename();
        self.press.cancel();
        self.focus_intent = FocusIntent::WithoutRing;
        self.show_focus_ring = false;
        self.accessibility_scroll = true;
        self.focus.focus(window);
        cx.notify();
        true
    }

    pub fn set_launch_progress(&mut self, progress: Option<&'static str>, cx: &mut Context<Self>) {
        if self.launch_progress.map(|(label, _)| label) != progress {
            let start =
                self.launch_progress.map_or_else(std::time::Instant::now, |(_, start)| start);
            self.launch_progress = progress.map(|label| (label, start));
            cx.notify();
        }
    }

    pub fn set_navigation_glyph(&mut self, asset: &'static str, cx: &mut Context<Self>) {
        if self.navigation_glyph != Some(asset) {
            self.navigation_glyph = Some(asset);
            cx.notify();
        }
    }

    /// Keep selection while another column owns the active content.
    pub fn set_navigation_selection_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.navigation_selection_active != active {
            self.navigation_selection_active = active;
            cx.notify();
        }
    }

    #[cfg(all(test, feature = "desktop"))]
    pub(crate) fn navigation_surface_for_test(&self, window: &Window) -> gpui::Div {
        self.navigation_surface(
            self.navigation == Some(true),
            self.keyboard_focus_visible(window),
            window,
        )
    }

    pub fn set_navigation_detail(&mut self, value: &str, cx: &mut Context<Self>) {
        let detail = (!value.is_empty()).then_some(value);
        if self.navigation_detail.as_ref().map(|value| value.as_ref()) != detail {
            self.navigation_detail = detail.map(|text| text.to_owned().into());
            cx.notify();
        }
    }

    /// Folder rows open a separate column and show its direction at the trailing edge.
    pub fn set_navigation_child(&mut self, child: bool, cx: &mut Context<Self>) {
        if self.navigation_child != child {
            self.navigation_child = child;
            cx.notify();
        }
    }

    /// A trailing action has its own hit target and does not change row selection.
    pub fn set_navigation_accessory(
        &mut self,
        accessory: Option<gpui::Entity<Control>>,
        cx: &mut Context<Self>,
    ) {
        if self.navigation_accessory != accessory {
            self.navigation_accessory = accessory;
            cx.notify();
        }
    }

    pub(super) fn navigation_accessory_slot(
        &self,
        foreground: gpui::Hsla,
        opacity: f32,
        window: &Window,
    ) -> Option<gpui::Div> {
        Some(
            div()
                .debug_selector(|| "navigation-accessory".into())
                .size(zoom::pixels(window, 24.))
                .flex_none()
                .occlude()
                .text_color(foreground)
                .opacity(opacity)
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_up(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(self.navigation_accessory.clone()?),
        )
    }

    pub(super) fn navigation_surface(
        &self,
        selected: bool,
        focused: bool,
        window: &Window,
    ) -> gpui::Div {
        let sp = |value| zoom::pixels(window, value);
        let color = |role| gpui::Hsla::from(rgba(self.theme.color(role).0));
        let secondary = selected && !self.navigation_selection_active;
        let inactive = !window.is_window_active() || secondary;
        let icon_opacity = if inactive && !secondary { 0.6 } else { 1. };
        let foreground = if secondary {
            gpui::black()
        } else if selected && inactive {
            color(Role::ColumnSelectionInactiveText)
        } else if selected {
            color(Role::MenuSelectionText)
        } else {
            color(Role::Text).alpha(0.9)
        };
        let selection =
            color(if inactive { Role::ColumnSelectionInactive } else { Role::MenuSelection });
        div()
            .relative()
            .w_full()
            .h(sp(32.))
            .flex()
            .items_center()
            .gap(sp(6.))
            .px(sp(8.))
            .when(self.sidebar_row, |v| v.pr(sp(12.)))
            .when(self.navigation_child, |v| v.pr(sp(4.)))
            .rounded(sp(4.))
            .text_size(sp(14.))
            .line_height(sp(20.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .letter_spacing(zoom::scaled(window, 0.14))
            .text_color(foreground)
            .when(selected, |v| v.bg(selection))
            .when(
                self.sidebar_row && self.navigation_glyph.is_none() && self.app_icon.is_none(),
                |v| {
                    v.child(
                        svg()
                            .path(if self.app_icon_loading {
                                "app-placeholder"
                            } else {
                                "app-default"
                            })
                            .size(sp(if self.compact_app_icon { 16. } else { 24. }))
                            .flex_none()
                            .opacity(icon_opacity)
                            .text_color(foreground.alpha(if self.app_icon_loading {
                                0.3
                            } else {
                                0.8
                            })),
                    )
                },
            )
            .when_some(self.app_icon.clone(), |v, icon| {
                v.child(
                    gpui::img(icon)
                        .size(sp(if self.compact_app_icon { 16. } else { 24. }))
                        .flex_none()
                        .opacity(icon_opacity),
                )
            })
            .when_some(self.navigation_glyph, |v, glyph| {
                v.child(
                    svg()
                        .path(glyph)
                        .size(sp(16.))
                        .flex_none()
                        .text_color(foreground)
                        .opacity(icon_opacity),
                )
            })
            .when(self.toolbar_icon.is_none(), |v| {
                if let Some(name) = &self.session_name {
                    v.child(self.session_placeholder(name, foreground, window))
                } else {
                    v.child(self.navigation_label(foreground))
                }
            })
            .when_some(self.navigation_detail.clone(), |v, hint| {
                v.child(
                    div()
                        .max_w(gpui::relative(0.45))
                        .truncate()
                        .text_size(sp(11.))
                        .text_color(foreground.alpha(if self.sidebar_row { 0.4 } else { 0.6 }))
                        .child(hint),
                )
                .when(!self.sidebar_row, |v| {
                    v.child(
                        svg()
                            .path("menu-submenu")
                            .w(sp(8.))
                            .h(sp(16.))
                            .flex_none()
                            .text_color(foreground.alpha(0.6)),
                    )
                })
            })
            .when(self.navigation_child, |view| {
                view.child(
                    svg().path("entry-child").size(sp(16.)).flex_none().text_color(
                        foreground.alpha(if selected && !inactive { 0.6 } else { 0.3 }),
                    ),
                )
            })
            .when_some(self.launch_progress, |view, (_, started)| {
                window.request_animation_frame();
                view.child(
                    svg()
                        .path("progress-spinner")
                        .size(sp(16.))
                        .flex_none()
                        .text_color(foreground)
                        .with_transformation(gpui::Transformation::rotate(gpui::Radians(
                            started.elapsed().as_secs_f32() * std::f32::consts::TAU,
                        ))),
                )
            })
            .when_some(self.toolbar_icon, |v, icon| {
                v.w(sp(28.))
                    .px(sp(6.))
                    .child(svg().path(icon).size(sp(16.)).text_color(color(Role::TextSecondary)))
            })
            .children(self.navigation_accessory_slot(foreground, icon_opacity, window))
            .when(self.popup_open && !selected, |v| {
                v.child(
                    div()
                        .debug_selector(|| "navigation-context-outline".into())
                        .absolute()
                        .inset_0()
                        .rounded(sp(4.))
                        .border(sp(2.))
                        .border_color(rgba(0x000000b3)),
                )
            })
            .when(focused, |v| v.child(self.navigation_focus_ring(window)))
    }

    pub(super) fn navigation_focus_ring(&self, window: &Window) -> gpui::Div {
        div()
            .debug_selector(|| "navigation-focus-ring".into())
            .absolute()
            .inset_0()
            .rounded(zoom::pixels(window, 4.))
            .border(zoom::pixels(window, 2.))
            .border_color(self.color(Role::FocusRing))
    }

    pub(super) fn sidebar_surface(
        &self,
        icon: &SidebarIcon,
        window: &Window,
        focused: bool,
        pressed: bool,
    ) -> gpui::Div {
        let sp = |value| zoom::pixels(window, value);
        let active =
            self.sidebar_selected || self.popup_open || matches!(icon, SidebarIcon::Update);
        let inactive = !window.is_window_active();
        let icon_opacity = if inactive && !self.disabled { 0.6 } else { 1. };
        let hovered = self.press.hovered && !self.disabled && !self.sidebar_muted && !inactive;
        let foreground = rgba(if self.disabled {
            0x0000004d
        } else if active && inactive {
            0x000000cc
        } else if active && !self.sidebar_muted {
            0xffffffff
        } else if pressed || hovered {
            0x000000ff
        } else if focused {
            0x000000e6
        } else {
            0x000000cc
        });
        let background = rgba(if self.disabled {
            0
        } else if active {
            if self.sidebar_muted || inactive { 0x00000033 } else { 0x000000b3 }
        } else if pressed {
            0x00000033
        } else if hovered {
            0x00000014
        } else {
            0
        });
        let mut surface = div()
            .h(sp(24.))
            .w(sp(24.))
            .flex()
            .items_center()
            .justify_center()
            .gap(sp(6.))
            .rounded(sp(6.))
            .bg(background)
            .text_color(foreground)
            .when(focused, |v| v.border_2().border_color(self.color(Role::FocusRing)));
        surface = match icon {
            SidebarIcon::Glyph(asset) => surface.child(
                svg().path(*asset).size(sp(16.)).text_color(foreground).opacity(icon_opacity),
            ),
            SidebarIcon::Update => surface
                .rounded(sp(12.))
                .bg(gpui::Hsla::from(rgb(UPDATE_GREEN)).blend(gpui::black().alpha(
                    if self.popup_open {
                        0.
                    } else if pressed {
                        0.2
                    } else if hovered {
                        0.1
                    } else {
                        0.
                    },
                )))
                .child(
                    svg()
                        .path("upgrade")
                        .size(sp(16.))
                        .text_color(gpui::white())
                        .opacity(icon_opacity),
                ),
            SidebarIcon::Account(initial) => surface.child(
                div()
                    .size(sp(16.))
                    .opacity(icon_opacity)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(sp(8.))
                    .border(sp(1.5))
                    .border_color(foreground)
                    .when(active && !inactive, |v| v.bg(gpui::white()).text_color(rgba(0x000000b3)))
                    .text_size(sp(11.))
                    .letter_spacing(zoom::scaled(window, 0.11))
                    .line_height(sp(16.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_center()
                    .child(initial.clone()),
            ),
        };
        surface
    }

    fn navigation_label(&self, foreground: gpui::Hsla) -> gpui::Div {
        div().flex_1().min_w_0().text_ellipsis().child(
            gpui::StyledText::new(self.label.clone()).with_highlights([(
                0..self.label.len(),
                gpui::HighlightStyle { color: Some(foreground), ..Default::default() },
            )]),
        )
    }
}
