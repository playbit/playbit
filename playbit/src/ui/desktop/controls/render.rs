//! Control rendering keeps appearance, input wiring and accessibility separate.
//! Surface overrides retain their order; all variants share the same input semantics.
use super::*;
use crate::ui::desktop::typography;

struct Appearance {
    scale: f32,
    display: f32,
    focused: bool,
    pressed: bool,
}
impl Appearance {
    fn pixels(&self, logical: f32) -> Pixels {
        px((logical * self.scale * self.display).round() / self.display)
    }
}

impl Render for Control {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scale = zoom::factor(cx);
        let display = window.scale_factor();
        #[cfg(test)]
        {
            self.renders += 1;
        }
        self.observe_focus(window, cx);
        if let FocusIntent::PendingSelection { rename } = self.focus_intent {
            // The owner projects accepted selection before rendering this row.
            // Consume the intent even after a veto, so it cannot focus a later selection.
            self.focus_intent = FocusIntent::None;
            if !self.disabled
                && self.navigation == Some(true)
                // The owner's new frame may revoke the previous frame's rename candidate.
                && (!rename || self.delayed_rename.is_some())
                // A rename candidate can focus its selected ancestor column while muted.
                && (self.navigation_selection_active || rename)
            {
                self.focus_intent = FocusIntent::WithoutRing;
                self.show_focus_ring = false;
                self.focus.focus(window);
            }
        }
        let appearance = Appearance {
            scale,
            display,
            focused: !self.disabled
                && window.is_window_active()
                && self.show_focus_ring
                && self.focus.is_focused(window),
            pressed: !self.disabled && (self.press.active() || self.popup_open),
        };
        let surface = self.render_surface(&appearance, window);
        let mut root = self.render_root(surface, &appearance, window);
        if !self.disabled {
            root = self.pointer_handlers(root, cx);
            root = self.keyboard_handlers(root, cx);
        } else {
            self.clear_native_tooltip(window);
        }
        let entity = cx.entity().downgrade();
        root.child(
            gpui::canvas(
                move |bounds, window, cx| {
                    if let Some(entity) = entity.upgrade() {
                        entity
                            .update(cx, |this, cx| this.publish_accessibility(bounds, window, cx));
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }
}

impl Control {
    pub(super) fn color(&self, role: Role) -> gpui::Hsla {
        rgba(self.theme.color(role).0).into()
    }

    fn observe_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.subscriptions.is_none() {
            self.subscriptions = Some([
                cx.on_focus(&self.focus, window, |this, _, cx| {
                    this.show_focus_ring = this.focus_intent != FocusIntent::WithoutRing;
                    cx.notify();
                }),
                cx.on_blur(&self.focus, window, |this, window, cx| {
                    this.press.cancel();
                    this.cancel_delayed_rename();
                    this.clear_copy_feedback();
                    this.show_focus_ring = false;
                    if !this.focus.is_focused(window) {
                        this.focus_intent = FocusIntent::None;
                    }
                    cx.notify();
                }),
            ]);
        }
        if (self.copy_value.is_some() || self.delayed_rename.is_some())
            && self.activation_subscription.is_none()
        {
            self.activation_subscription =
                Some(cx.observe_window_activation(window, |this, window, cx| {
                    if !window.is_window_active() {
                        this.press.cancel();
                        this.cancel_delayed_rename();
                        this.press.hovered = false;
                        this.clear_copy_feedback();
                        cx.notify();
                    }
                }));
        }
    }

    fn background(&self, pressed: bool) -> gpui::Hsla {
        match self.kind {
            Kind::Checkbox(_) => {
                if self.disabled {
                    self.color(Role::ControlBackground).alpha(0.1)
                } else if pressed {
                    self.color(Role::ControlBackground).alpha(0.5)
                } else {
                    self.color(Role::ControlBackground)
                }
            }
            Kind::Button(ButtonStyle::Primary) => {
                if self.disabled {
                    self.color(Role::Text).alpha(0.3)
                } else {
                    self.color(if pressed { Role::PrimaryPressed } else { Role::PrimaryBackground })
                }
            }
            Kind::Button(ButtonStyle::Destructive) => self
                .color(if pressed { Role::DestructivePressed } else { Role::DestructiveBackground })
                .alpha(if self.disabled {
                    0.3
                } else if pressed {
                    0.7
                } else {
                    0.8
                }),
            Kind::Button(ButtonStyle::Default) if self.toolbar_icon.is_some() => {
                self.color(Role::Text).alpha(if pressed {
                    0.15
                } else if self.press.hovered {
                    0.08
                } else {
                    0.
                })
            }
            Kind::Button(ButtonStyle::Default) => self.color(Role::Text).alpha(if self.disabled {
                0.05
            } else if pressed {
                0.15
            } else {
                0.08
            }),
            Kind::Popup => self.color(Role::Text).alpha(if self.disabled {
                0.05
            } else if pressed {
                0.15
            } else {
                0.08
            }),
        }
    }

    fn foreground(&self, pressed: bool) -> gpui::Hsla {
        let check = matches!(self.kind, Kind::Checkbox(_));
        let destructive = matches!(self.kind, Kind::Button(ButtonStyle::Destructive));
        let prominent = matches!(self.kind, Kind::Button(ButtonStyle::Primary));
        if prominent || destructive {
            self.color(if destructive { Role::DestructiveText } else { Role::PrimaryText }).alpha(
                if self.disabled {
                    0.5
                } else if pressed {
                    0.8
                } else {
                    0.9
                },
            )
        } else {
            self.color(Role::Text).alpha(if self.disabled {
                if check { 0.3 } else { 0.25 }
            } else if pressed {
                0.7
            } else {
                0.9
            })
        }
    }

    fn border(&self, appearance: &Appearance) -> (Pixels, gpui::Hsla) {
        let sp = |logical| appearance.pixels(logical);
        let Appearance { focused, pressed, .. } = *appearance;
        let check = matches!(self.kind, Kind::Checkbox(_));
        let border_width = sp(if focused {
            2.
        } else if check {
            1.
        } else {
            0.
        });
        let border = if focused {
            self.color(Role::FocusRing).alpha(0.9)
        } else if check {
            self.color(Role::Text).alpha(if self.disabled {
                0.2
            } else if pressed {
                0.7
            } else {
                0.6
            })
        } else {
            self.color(Role::Border).alpha(0.)
        };
        (border_width, border)
    }

    fn standard_surface(&self, appearance: &Appearance, window: &Window) -> gpui::Div {
        let sp = |logical| appearance.pixels(logical);
        let Appearance { focused, pressed, .. } = *appearance;
        let check = matches!(self.kind, Kind::Checkbox(_));
        let strong =
            matches!(self.kind, Kind::Button(ButtonStyle::Primary | ButtonStyle::Destructive));
        let foreground = if self.navigation_accessory_icon.is_some() {
            window.text_style().color
        } else {
            self.foreground(pressed)
        };
        let background = if self.navigation_accessory_icon.is_some() {
            foreground.alpha(if self.disabled {
                0.
            } else if pressed {
                0.15
            } else if self.press.hovered {
                0.08
            } else {
                0.
            })
        } else {
            self.background(pressed)
        };
        let (border_width, border) = self.border(appearance);
        let radius =
            sp(if self.inspector_icon.is_some() || self.navigation_accessory_icon.is_some() {
                6.
            } else if let Some((width, height)) = self.button_size
                && width == height
            {
                height / 2.
            } else if self.toolbar_icon.is_some() {
                12.
            } else if check {
                2.
            } else {
                6.
            });
        let mut surface =
            div().relative().flex_none().rounded(radius).bg(background).text_color(foreground);
        let outline_inset = if check && !focused { px(0.) } else { -border_width };
        // Checkbox strokes are inside the box; its focus ring remains outside.
        surface = surface.child(
            div()
                .absolute()
                .top(outline_inset)
                .right(outline_inset)
                .bottom(outline_inset)
                .left(outline_inset)
                .rounded(if check && !focused { radius } else { radius + border_width })
                .border(border_width)
                .border_color(border),
        );
        if focused && strong {
            surface = surface.child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .left_0()
                    .rounded(radius)
                    .border(sp(1.))
                    .border_color(self.color(Role::PrimaryText).alpha(0.9)),
            );
        }
        self.surface_content(surface, appearance, foreground)
    }

    fn checkbox_surface(
        &self,
        mut surface: gpui::Div,
        state: CheckState,
        appearance: &Appearance,
    ) -> gpui::Div {
        let sp = |logical| appearance.pixels(logical);
        let pressed = appearance.pressed;
        // The focused 14dp box retains the same centered 16dp icon slot.
        let icon_inset = sp(if appearance.focused { -1. } else { 0. });
        surface = surface.size(sp(if appearance.focused { 14. } else { 16. }));
        let path = match state {
            CheckState::Mixed => Some("check-mixed"),
            CheckState::On => Some("check-on"),
            CheckState::Off if pressed => Some("check-on"),
            CheckState::Off => None,
        };
        let opacity = if self.disabled {
            0.2
        } else if pressed {
            0.3
        } else {
            0.8
        };
        surface = surface.children(path.map(|path| {
            // The full-slot Figma checkmark export includes its 80% opacity.
            let opacity = if path == "check-on" { opacity / 0.8 } else { opacity };
            svg()
                .path(path)
                .size(sp(16.))
                .absolute()
                .left(icon_inset)
                .top(icon_inset)
                .text_color(self.color(Role::Text).alpha(opacity))
        }));
        surface
    }

    fn surface_content(
        &self,
        mut surface: gpui::Div,
        appearance: &Appearance,
        foreground: gpui::Hsla,
    ) -> gpui::Div {
        let sp = |logical| appearance.pixels(logical);
        let Appearance { focused, pressed, .. } = *appearance;
        if let Kind::Checkbox(state) = self.kind {
            surface = self.checkbox_surface(surface, state, appearance);
        } else if let Some(icon) = self.circle_icon {
            surface = surface.flex().items_center().justify_center().child(
                svg().path(icon).size(sp(24.)).text_color(self.color(Role::Text).alpha(0.7)),
            );
        } else if let Some(icon) = self.toolbar_icon {
            surface = surface.w(sp(34.)).h(sp(24.)).flex().items_center().justify_center().child(
                svg().path(icon).size(sp(16.)).text_color(
                    self.color(Role::Text).alpha(if pressed || self.press.hovered || focused {
                        0.9
                    } else {
                        0.4
                    }),
                ),
            );
        } else if let Some(icon) = self.navigation_accessory_icon {
            surface = surface.size(sp(24.)).flex().items_center().justify_center().child(
                svg()
                    .debug_selector(|| "navigation-accessory-icon".into())
                    .path(icon)
                    .size(sp(16.))
                    .flex_none()
                    .text_color(foreground.alpha(if self.disabled { 0.25 } else { 0.5 })),
            );
        } else if let Some(icon) = self.inspector_icon {
            surface = surface.size(sp(24.)).flex().items_center().justify_center().child(
                svg()
                    .debug_selector(|| "inspector-button-icon".into())
                    .path(icon)
                    .size(sp(16.))
                    .flex_none()
                    .text_color(self.foreground(pressed)),
            );
        } else if self.kind == Kind::Popup {
            surface = self.popup_surface(surface, appearance);
        } else {
            surface = surface
                .h(sp(24.))
                .px(sp(12.))
                .flex()
                .items_center()
                .font_weight(gpui::FontWeight::MEDIUM)
                .letter_spacing(px(0.))
                .child(self.label.clone());
        }
        surface
    }

    fn popup_surface(&self, surface: gpui::Div, appearance: &Appearance) -> gpui::Div {
        let sp = |logical| appearance.pixels(logical);
        surface
            .h(sp(24.))
            .min_w(sp(86.))
            .px(sp(12.))
            .gap(sp(6.))
            .flex()
            .items_center()
            .font_weight(gpui::FontWeight::MEDIUM)
            .when_some(self.popup_context.as_ref(), |surface, context| {
                surface.w_full().min_w_0().overflow_hidden().when_some(context.icon, |v, icon| {
                    v.child(
                        div()
                            .debug_selector(|| "popup-context-icon-slot".into())
                            .relative()
                            .w(sp(12.))
                            .h(sp(16.))
                            .flex_none()
                            .child(
                                svg()
                                    .debug_selector(|| "popup-context-icon".into())
                                    .path(icon)
                                    .absolute()
                                    .left(-sp(4.))
                                    .size(sp(16.))
                                    .text_color(self.foreground(appearance.pressed)),
                            ),
                    )
                })
            })
            .child(div().min_w_0().flex_1().text_ellipsis().child(self.label.clone()))
            .when_some(
                self.popup_context.as_ref().filter(|context| !context.detail.is_empty()),
                |v, context| {
                    v.child(
                        typography::tabular_numerals(div())
                            .debug_selector(|| "popup-context-detail".into())
                            .min_w_0()
                            .max_w(gpui::relative(0.4))
                            .text_ellipsis()
                            .text_color(self.color(Role::Text).alpha(if self.disabled {
                                0.2
                            } else {
                                0.4
                            }))
                            .child(context.detail.clone()),
                    )
                },
            )
            .child(self.popup_glyph(appearance))
    }

    fn popup_glyph(&self, appearance: &Appearance) -> gpui::Div {
        let sp = |logical| appearance.pixels(logical);
        let inspector = self.popup_context.is_some();
        div()
            .debug_selector(|| "popup-glyph-slot".into())
            .relative()
            .w(sp(if inspector { 8. } else { 16. }))
            .h(sp(16.))
            .flex_none()
            .child(
                svg()
                    .debug_selector(|| "popup-control-glyph".into())
                    .path(if inspector { "permission-popup-glyph" } else { "popup-glyph" })
                    .absolute()
                    .left_0()
                    .w(px(if inspector { 11.3536 } else { 16. } * appearance.scale))
                    .h(sp(16.))
                    .text_color(self.color(Role::Text).alpha(if self.disabled { 0.5 } else { 1. })),
            )
    }

    fn copy_surface(&self, copy: &CopyValue, appearance: &Appearance) -> gpui::Div {
        let sp = |logical| appearance.pixels(logical);
        let Appearance { focused, pressed, .. } = *appearance;
        let active = self.press.hovered || focused;
        let label = if active && copy.copied {
            format!("✓ {} copied:", self.label)
        } else if active {
            format!(
                "{}:",
                self.accessibility_label
                    .as_ref()
                    .map_or_else(|| format!("Copy {}", self.label), ToString::to_string)
            )
        } else {
            format!("{}:", self.label)
        };
        let mut surface = div()
            .w(sp(self.button_size.map_or(224., |size| size.0)))
            .h(sp(40.))
            .px(sp(8.))
            .py(sp(4.))
            .rounded(sp(8.))
            .bg(rgba(if pressed {
                0x00000033
            } else if active {
                0x00000014
            } else {
                0
            }))
            .when(focused, |v| v.border_2().border_color(self.color(Role::FocusRing)))
            .child(
                div().text_color(rgba(if active { 0x00000052 } else { 0x00000080 })).child(label),
            )
            .child(
                div()
                    .whitespace_nowrap()
                    .text_color(rgba(if active { 0x000000cc } else { 0x00000080 }))
                    .child(copy.value.clone()),
            );
        surface.text_style().get_or_insert_with(Default::default).font_features =
            Some(typography::identifier_features());
        surface
    }

    pub(super) fn is_navigation_row(&self) -> bool {
        self.navigation.is_some()
            && matches!(self.kind, Kind::Button(_))
            && self.sidebar_icon.is_none()
            && self.toolbar_icon.is_none()
            && self.session_style.is_none()
    }

    fn render_surface(&self, appearance: &Appearance, window: &mut Window) -> gpui::Div {
        let mut surface = self.standard_surface(appearance, window);
        // Preserve override precedence, including combinations of specialized properties.

        if self.table_cells.is_none()
            && let Some(selected) = self.navigation
        {
            // Navigation reflects the owner's selection, including while a pointer is held.
            let selected =
                selected || (!self.is_navigation_row() && appearance.pressed && !self.popup_open);
            surface = self.navigation_surface(selected, appearance.focused, window);
        }
        if let Some(cells) = &self.table_cells {
            let selected = self.navigation == Some(true);
            let inactive = !window.is_window_active() || !self.navigation_selection_active;
            let foreground = if selected && inactive {
                self.color(Role::ColumnSelectionInactiveText)
            } else {
                rgba(if selected { 0xffffffff } else { 0x000000e6 }).into()
            };
            let background = selected.then(|| {
                if inactive {
                    self.color(Role::ColumnSelectionInactive)
                } else {
                    rgba(0x000000b3).into()
                }
            });
            surface = super::super::table::row(
                cells.values.clone(),
                cells.binary_value,
                foreground,
                background,
                appearance.focused.then(|| self.color(Role::FocusRing)),
                self.navigation_accessory_slot(foreground, if inactive { 0.6 } else { 1. }, window)
                    .map(|slot| slot.into_any_element()),
                window,
            );
        }

        if let Some((width, height)) = self.button_size {
            surface = surface
                .w(appearance.pixels(width))
                .h(appearance.pixels(height))
                .flex_none()
                .when(self.popup_context.is_none(), |v| v.px_0())
                .justify_center();
        }
        if self.switch
            && let Kind::Checkbox(state) = self.kind
        {
            surface = self.switch_surface(state, appearance);
        }
        if let Some(copy) = &self.copy_value {
            surface = self.copy_surface(copy, appearance);
        }
        if let Some(icon) = &self.sidebar_icon {
            surface = self.sidebar_surface(icon, window, appearance.focused, appearance.pressed);
        }
        if let Some(style) = self.session_style {
            surface = self.session_surface(style, appearance);
        }
        let dim = self.session_style.is_some();
        surface.when(dim && !self.disabled && !window.is_window_active(), |v| v.opacity(0.6))
    }

    fn switch_surface(&self, state: CheckState, appearance: &Appearance) -> gpui::Div {
        let sp = |value| appearance.pixels(value);
        let on = state != CheckState::Off;
        div()
            .relative()
            .w(sp(36.))
            .h(sp(24.))
            .flex_none()
            .when(self.disabled, |view| view.opacity(0.5))
            .child(
                div()
                    .debug_selector(|| "switch-socket".into())
                    .absolute()
                    .left_0()
                    .top(sp(1.))
                    .w(sp(36.))
                    .h(sp(22.))
                    .rounded(sp(12.))
                    .bg(rgba(if on { 0x000000b3 } else { 0x00000014 })),
            )
            .child(
                div()
                    .debug_selector(|| "switch-knob".into())
                    .absolute()
                    .left(sp(if on { 16. } else { 2. }))
                    .top(sp(3.))
                    .size(sp(18.))
                    .rounded(sp(9.))
                    .bg(rgba(0xffffffff))
                    .shadow(vec![gpui::BoxShadow {
                        color: rgba(0x00000033).into(),
                        offset: gpui::point(px(0.), sp(1.)),
                        blur_radius: sp(1.),
                        spread_radius: px(0.),
                    }]),
            )
            .when(appearance.focused, |view| {
                view.child(
                    div()
                        .absolute()
                        .top(-sp(2.))
                        .right(-sp(2.))
                        .bottom(-sp(2.))
                        .left(-sp(2.))
                        .rounded(sp(14.))
                        .border(sp(2.))
                        .border_color(self.color(Role::FocusRing)),
                )
            })
    }

    fn session_surface(&self, style: SessionStyle, appearance: &Appearance) -> gpui::Div {
        let sp = |value| appearance.pixels(value);
        let surface = div()
            .flex()
            .items_center()
            .justify_center()
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(rgba(if self.disabled { 0x00000055 } else { 0x000000e6 }))
            .when(appearance.focused, |v| v.border_2().border_color(self.color(Role::FocusRing)));
        match style {
            SessionStyle::Template => surface
                .size_full()
                .rounded(sp(5.))
                .bg(rgb(if appearance.pressed { 0xaaaaaa } else { 0xbebebe }))
                .text_size(sp(16.))
                .line_height(sp(20.))
                .child(self.label.clone()),
            SessionStyle::AppTile { bundled_icon } => {
                let source = bundled_icon
                    .map(gpui::ImageSource::from)
                    .or_else(|| self.app_icon.clone().map(Into::into));
                let icon = match source {
                    Some(source) => {
                        gpui::img(source).size(sp(APP_TILE_ICON_POINTS)).into_any_element()
                    }
                    None => svg()
                        .path("app-default")
                        .size(sp(APP_TILE_ICON_POINTS))
                        .text_color(rgba(if self.disabled { 0x00000055 } else { 0x000000cc }))
                        .into_any_element(),
                };
                surface
                    .size_full()
                    .flex_col()
                    .justify_start()
                    .pt(sp(8.))
                    .px(sp(if bundled_icon.is_some() { 8. } else { 4. }))
                    // The 16-point line box supplies the rest of the gap to the caption's ink.
                    .gap(sp(4.))
                    .rounded(sp(6.))
                    .when(!self.disabled, |v| v.text_color(rgb(0)))
                    .child(icon)
                    .child(
                        div()
                            .w_full()
                            .text_center()
                            .truncate()
                            .text_size(sp(11.))
                            .line_height(sp(16.))
                            .letter_spacing(px(0.))
                            .child(self.label.clone()),
                    )
            }
            SessionStyle::Share => surface
                .text_size(sp(11.))
                .line_height(sp(16.))
                .letter_spacing(px(0.11 * appearance.scale))
                .h(sp(24.))
                .px(sp(12.))
                .gap(sp(6.))
                .rounded(sp(6.))
                .bg(rgba(if appearance.pressed { 0x00000026 } else { 0x00000014 }))
                .child(
                    div().w(sp(12.)).h(sp(16.)).relative().child(
                        svg()
                            .path("session-share")
                            .absolute()
                            .top_0()
                            .left(sp(-4.))
                            .size(sp(16.))
                            .text_color(self.foreground(appearance.pressed)),
                    ),
                )
                .child(self.label.clone()),
        }
    }

    fn render_root(
        &self,
        surface: gpui::Div,
        appearance: &Appearance,
        window: &Window,
    ) -> gpui::Stateful<gpui::Div> {
        let sp = |logical| appearance.pixels(logical);
        let check = matches!(self.kind, Kind::Checkbox(_)) && !self.switch;
        let mut root = div()
            .id("control")
            .relative()
            .flex()
            .items_center()
            .max_w_full()
            .h(sp(self.button_size.map_or(
                if self.table_cells.is_some() {
                    super::super::table::ROW_HEIGHT
                } else if self.navigation.is_some() {
                    32.
                } else {
                    24.
                },
                |(_, height)| height,
            )))
            .pl(sp(if check && appearance.focused { 1. } else { 0. }))
            .gap(sp(if check && appearance.focused { 9. } else { 8. }))
            .child(surface);
        if self.kind == Kind::Popup && self.popup_context.is_some() {
            root = root.w_full().min_w_0();
        }
        if self.navigation.is_some() && self.sidebar_icon.is_none() && self.session_style.is_none()
        {
            root = root.px_0().when(self.toolbar_icon.is_none(), |v| v.w_full());
        }
        if let Some((width, height)) = self.button_size {
            root = root.w(sp(width)).h(sp(height)).flex_none();
        }
        if matches!(self.session_style, Some(SessionStyle::AppTile { .. })) && !self.disabled {
            // Resolve pointer feedback against the hitbox in this frame, including during a drag.
            root = root.rounded(sp(6.));
            if self.press.key.is_some() {
                root = root.bg(rgba(0x00000033));
            } else {
                root = root.hover(|v| {
                    v.bg(rgba(if self.press.pointer { 0x00000033 } else { 0x00000014 }))
                });
            }
        }
        if self.toolbar_icon.is_some()
            || self.sidebar_icon.is_some()
            || self.session_style.is_some()
            || self.navigation_accessory_icon.is_some()
        {
            // A titlebar control must exclude its parent's native drag region.
            root = root.occlude();
        }
        *root.text_style() = Some(label_style(window));
        root = root.text_color(self.color(if self.disabled {
            Role::TextDisabled
        } else {
            Role::Text
        }));
        if check {
            // The fixed checkbox surface remains visible while the label can
            // shrink. Avoid asking flex layout for an automatic content minimum.
            root = root
                .min_w(sp(24.))
                .child(div().min_w_0().text_ellipsis().child(self.label.clone()));
        }
        root.style().align_self =
            Some(if self.toolbar_icon.is_some() || self.sidebar_icon.is_some() || self.switch {
                gpui::AlignSelf::Center
            } else {
                gpui::AlignSelf::FlexStart
            });
        // Keep the dispatch ancestry when a focused control becomes disabled,
        // so Tab can still reach the containing view's navigation handler.
        root = root.track_focus(&self.focus);
        root
    }

    fn pointer_handlers(
        &self,
        root: gpui::Stateful<gpui::Div>,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        root.on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event, window, cx| {
                this.cancel_delayed_rename();
                if !this.disabled
                    && let Some(menu) = this.context_menu.clone()
                {
                    this.press.cancel();
                    cx.stop_propagation();
                    menu(event, window, cx);
                }
            }),
        )
        .on_any_mouse_down(|_, window, _| window.prevent_default())
        .on_hover(cx.listener(|this, hovered, window, cx| {
            this.press.hovered = *hovered;
            if !hovered {
                this.clear_copy_feedback();
            }
            if this.toolbar_icon.is_some() || this.navigation_accessory_icon.is_some() {
                if *hovered && !this.disabled {
                    set_native_tooltip(window, Some(this.label.as_ref()));
                    this.native_tooltip_visible = true;
                } else {
                    this.clear_native_tooltip(window);
                }
                cx.notify();
            }
            if this.press.pointer || this.copy_value.is_some() || this.sidebar_icon.is_some() {
                cx.notify();
            }
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                window.prevent_default();
                this.prepare_delayed_rename(event, window);
                if this.disabled {
                    return;
                }
                if event.modifiers.control
                    && let Some(menu) = this.context_menu.clone()
                {
                    this.press.cancel();
                    cx.stop_propagation();
                    menu(event, window, cx);
                    return;
                }
                this.show_focus_ring = false;
                // Selected rows still need pointer focus when renaming is unavailable.
                if this.is_navigation_row() {
                    this.focus_intent =
                        FocusIntent::PendingSelection { rename: this.rename_press.is_some() };
                }
                let _timing = crate::trace::Span::new("ui.control.press_update");
                crate::trace::mark("ui.control.press");
                this.press.pointer = true;
                this.press.hovered = true;
                if this.kind == Kind::Popup {
                    this.open_popup(window, cx);
                } else if let Some(select) = this.single_click.clone() {
                    select(this.id, cx);
                } else if this.is_navigation_row() && this.rename_press.is_none() {
                    this.activate(cx);
                }
                cx.notify();
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, event: &gpui::MouseUpEvent, window, cx| {
                // This listener runs only for a release inside the control.
                // Our press state also accounts for Escape, blur and disabling.
                if std::mem::take(&mut this.press.pointer) && this.kind != Kind::Popup {
                    if !this.arm_delayed_rename(event, window, cx)
                        && (if this.single_click.is_some() {
                            event.click_count >= 2
                        } else {
                            !this.is_navigation_row()
                        })
                    {
                        this.activate(cx);
                    }
                    cx.notify();
                }
            }),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.cancel_delayed_rename();
                if this.press.pointer {
                    this.press.pointer = false;
                    cx.notify();
                }
            }),
        )
    }

    fn keyboard_handlers(
        &self,
        root: gpui::Stateful<gpui::Div>,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        root.on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
            this.cancel_delayed_rename();
            if !this.is_navigation_row() || event.keystroke.key == "tab" {
                this.focus_intent = FocusIntent::None;
                this.show_focus_ring = true;
            }
            cx.notify();
            let _timing = crate::trace::Span::new("ui.control.key_update");
            if event.keystroke.key == "escape" {
                this.press.cancel();
                cx.notify();
                cx.stop_propagation();
            } else if this.kind == Kind::Popup
                && (event.keystroke.key == "down" || activation_key(&event.keystroke).is_some())
            {
                this.open_popup(window, cx);
                cx.stop_propagation();
            } else if let Some(key) = activation_key(&event.keystroke) {
                if !event.is_held {
                    this.press.key = Some(key);
                    cx.notify();
                }
                cx.stop_propagation();
            }
        }))
        .on_key_up(cx.listener(|this, event: &gpui::KeyUpEvent, _, cx| {
            let was_pressed = this.press.key.is_some();
            if this.press.release_key(activation_key(&event.keystroke)) && this.kind != Kind::Popup
            {
                this.activate(cx);
            }
            if was_pressed {
                cx.notify();
            }
        }))
    }

    fn accessibility_node(&self, bounds: Bounds<Pixels>) -> gpui::accesskit::Node {
        use gpui::accesskit::{Action, Node, Rect, Role, Toggled};
        let mut node = Node::new(match self.kind {
            Kind::Checkbox(_) => Role::CheckBox,
            Kind::Button(_) => Role::Button,
            Kind::Popup => Role::ComboBox,
        });
        node.set_label(self.accessibility_label.as_ref().unwrap_or(&self.label).to_string());
        match &self.session_name {
            Some(SessionName::Loading(_)) => {
                node.set_description("Loading app information".to_owned())
            }
            Some(SessionName::Unavailable(_)) => {
                node.set_description("App information unavailable".to_owned())
            }
            None => {}
        }
        if let Some(copy) = &self.copy_value {
            if self.accessibility_label.is_none() {
                node.set_label(format!("Copy {}", self.label));
            }
            node.set_description(if copy.copied {
                format!("{} copied: {}", self.label, copy.value)
            } else {
                copy.value.to_string()
            });
            node.set_live(gpui::accesskit::Live::Polite);
        }
        if let Some((label, _)) = self.launch_progress {
            node.set_description(label);
            node.set_live(gpui::accesskit::Live::Polite);
        } else if let Some(error) = &self.session_info_error {
            node.set_description(error.to_string());
        } else if self.app_icon_failed {
            node.set_description("App artwork unavailable".to_owned());
        }
        if self.sidebar_icon.is_some() {
            node.set_selected(self.sidebar_selected || self.popup_open);
            if matches!(self.sidebar_icon, Some(SidebarIcon::Update)) {
                node.set_description("Update ready to install");
            }
        } else if let Some(selected) = self.navigation {
            node.set_selected(selected);
        }

        self.accessibility_details(&mut node);
        node.set_bounds(Rect::new(
            bounds.left().to_f64(),
            bounds.top().to_f64(),
            bounds.right().to_f64(),
            bounds.bottom().to_f64(),
        ));
        if let Kind::Checkbox(state) = self.kind {
            node.set_toggled(match state {
                CheckState::Off => Toggled::False,
                CheckState::On => Toggled::True,
                CheckState::Mixed => Toggled::Mixed,
            });
        }
        if self.disabled {
            node.set_disabled();
        } else {
            node.add_action(Action::Click);
            node.add_action(Action::Focus);
        }
        node
    }

    fn accessibility_details(&self, node: &mut gpui::accesskit::Node) {
        if let Some(detail) = &self.navigation_detail {
            node.set_description(if self.sidebar_row {
                detail.to_string()
            } else {
                format!("Record {detail}")
            });
        }
        if let Some(context) = &self.popup_context
            && !context.detail.is_empty()
        {
            node.set_description(context.detail.to_string());
        }
        if let Some(cells) = &self.table_cells {
            node.set_label(
                self.accessibility_label.as_ref().unwrap_or(&cells.values[0]).to_string(),
            );
            node.set_description(format!("Value: {}", cells.values[1]));
        }
    }

    fn publish_accessibility(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui::accesskit::NodeId;
        self.bounds = Some(bounds);
        let node = self.accessibility_node(bounds);
        let focused = !self.disabled && self.focus.is_focused(window);
        if std::mem::take(&mut self.accessibility_scroll) {
            window.request_autoscroll(bounds.dilate(zoom::pixels(window, 8.)));
        }
        window.insert_accessibility_node(
            NodeId(
                cx.entity_id().as_u64().checked_mul(2).expect("accessibility identity exhausted"),
            ),
            node,
            focused,
            Some(Rc::new(cx.listener(Self::accessibility_action))),
        );
    }

    fn accessibility_action(
        &mut self,
        request: &gpui::accesskit::ActionRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui::accesskit::Action;
        if self.disabled {
            return;
        }
        #[cfg(target_os = "macos")]
        if popup::is_modal_active() {
            return;
        }
        match request.action {
            Action::Click if self.kind == Kind::Popup => self.open_popup(window, cx),
            Action::Click => self.activate(cx),
            Action::Focus => {
                self.focus.focus(window);
                self.accessibility_scroll = true;
                cx.notify();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn compact_popups_keep_full_identity_and_fixed_glyph_geometry(cx: &mut gpui::TestAppContext) {
        // Narrow controls truncate long text without shrinking either glyph or losing the AX ID.
        const DETAIL: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
        cx.update(zoom::install);
        for scale in [10_000, 20_000] {
            cx.update(|cx| zoom::set_scale(scale, cx).expect("supported scale"));
            for presentation in 0..3 {
                let window = cx.add_window(|window, cx| {
                    zoom::apply(window, cx);
                    let mut control = Control::new(
                        1,
                        Properties {
                            kind: Kind::Popup,
                            label: "A long subject or permission label that must truncate",
                            disabled: false,
                            theme: &Theme::light(),
                        },
                        Rc::new(|_, _| {}),
                        cx,
                    );
                    control.set_button_size(180., 24., cx);
                    match presentation {
                        1 => control.set_popup_context(Some("permission-person-popup"), DETAIL, cx),
                        2 => control.set_popup_context(None, "", cx),
                        _ => {}
                    }
                    control
                });
                cx.run_until_parked();
                window
                    .update(cx, |control, _, _| {
                        let node = control.accessibility_node(Bounds::default());
                        assert_eq!(node.role(), gpui::accesskit::Role::ComboBox);
                        assert_eq!(node.label(), Some(control.label.as_ref()));
                        assert_eq!(node.description(), (presentation == 1).then_some(DETAIL));
                    })
                    .expect("popup identity");
                let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
                let factor = scale as f32 / 10_000.;
                let glyph = visual.debug_bounds("popup-control-glyph").expect("popup glyph");
                let slot = visual.debug_bounds("popup-glyph-slot").expect("fixed glyph slot");
                assert_eq!(
                    slot.size,
                    gpui::size(
                        px(if presentation == 0 { 16. } else { 8. } * factor),
                        px(16. * factor)
                    )
                );
                let width = if presentation == 0 { 16. } else { 11.3536 };
                // GPUI rounds Taffy dimensions in device pixels, then converts back to points.
                let expected_width = window
                    .update(cx, |_, window, _| zoom::pixels(window, width))
                    .expect("popup scale");
                assert_eq!(
                    glyph.size.width, expected_width,
                    "presentation {presentation}, zoom {factor}, glyph {glyph:?}"
                );
                assert_eq!(glyph.size.height, px(16. * factor));
                assert_eq!(glyph.left(), slot.left());
                if presentation == 1 {
                    let icon = visual.debug_bounds("popup-context-icon").expect("subject glyph");
                    let icon_slot =
                        visual.debug_bounds("popup-context-icon-slot").expect("subject slot");
                    assert_eq!(icon.size, gpui::size(px(16. * factor), px(16. * factor)));
                    assert_eq!(icon_slot.size, gpui::size(px(12. * factor), px(16. * factor)));
                    assert_eq!(icon.left(), icon_slot.left() - px(4. * factor));
                    assert!(icon.right() <= slot.left());
                }
                window
                    .update(cx, |_, window, _| window.remove_window())
                    .expect("close popup fixture");
            }
        }
    }

    #[gpui::test]
    fn inspector_icon_button_keeps_standard_activation_and_square_geometry(
        cx: &mut gpui::TestAppContext,
    ) {
        // The compact preset changes geometry, while the ordinary button still activates on release.
        cx.update(zoom::install);
        for scale in [10_000, 20_000] {
            cx.update(|cx| zoom::set_scale(scale, cx).expect("supported scale"));
            let count = Rc::new(std::cell::Cell::new(0));
            let activated = count.clone();
            let window = cx.add_window(|window, cx| {
                zoom::apply(window, cx);
                let mut control = Control::new(
                    1,
                    Properties {
                        kind: Kind::Button(ButtonStyle::Default),
                        label: "Remove permission",
                        disabled: false,
                        theme: &Theme::light(),
                    },
                    Rc::new(move |_, _| activated.set(activated.get() + 1)),
                    cx,
                );
                control.set_inspector_icon("permission-remove", cx);
                control
            });
            cx.run_until_parked();
            window
                .update(cx, |control, window, _| {
                    let appearance = Appearance {
                        scale: scale as f32 / 10_000.,
                        display: window.scale_factor(),
                        focused: false,
                        pressed: false,
                    };
                    let mut surface = control.render_surface(&appearance, window);
                    assert_eq!(
                        surface.style().corner_radii.top_left,
                        Some(appearance.pixels(6.).into())
                    );
                    assert_eq!(
                        surface.style().background,
                        Some(control.color(Role::Text).alpha(0.08).into())
                    );
                    let node = control.accessibility_node(Bounds::default());
                    assert_eq!(node.role(), gpui::accesskit::Role::Button);
                    assert_eq!(node.label(), Some("Remove permission"));
                })
                .expect("icon button appearance");
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            let factor = scale as f32 / 10_000.;
            let icon = visual.debug_bounds("inspector-button-icon").expect("button glyph");
            assert_eq!(
                icon,
                Bounds::new(
                    point(px(4. * factor), px(4. * factor)),
                    gpui::size(px(16. * factor), px(16. * factor))
                )
            );
            let center = point(px(12. * factor), px(12. * factor));
            visual.simulate_mouse_down(center, MouseButton::Left, Default::default());
            assert_eq!(count.get(), 0);
            visual.simulate_mouse_up(center, MouseButton::Left, Default::default());
            assert_eq!(count.get(), 1);
            window.update(cx, |_, window, _| window.remove_window()).expect("close icon fixture");
        }
    }

    #[gpui::test]
    fn navigation_accessory_activates_without_selecting_or_renaming_its_row(
        cx: &mut gpui::TestAppContext,
    ) {
        struct AccessoryRow {
            row: gpui::Entity<Control>,
            accessory: gpui::Entity<Control>,
        }
        impl Render for AccessoryRow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(240.)).child(self.row.clone()).on_key_down(|event, window, cx| {
                    if event.keystroke.key == "tab" {
                        window.focus_next();
                        cx.stop_propagation();
                    }
                })
            }
        }
        cx.update(zoom::install);
        for table in [false, true] {
            let selected = Rc::new(std::cell::Cell::new(0));
            let activated = Rc::new(std::cell::Cell::new(0));
            let renamed = Rc::new(std::cell::Cell::new(0));
            let selection_count = selected.clone();
            let action_count = activated.clone();
            let rename_count = renamed.clone();
            let window = cx.add_window(|_, cx| {
                let accessory = cx.new(|cx| {
                    let mut control = Control::new(
                        2,
                        Properties {
                            kind: Kind::Button(ButtonStyle::Default),
                            label: "Contains draft changes",
                            disabled: false,
                            theme: &Theme::light(),
                        },
                        Rc::new(move |_, _| action_count.set(action_count.get() + 1)),
                        cx,
                    );
                    control.set_navigation_accessory_icon("draft-changes", cx);
                    control
                });
                let row = cx.new(|cx| {
                    let mut row = Control::new(
                        1,
                        Properties {
                            kind: Kind::Button(ButtonStyle::Default),
                            label: "Edited item",
                            disabled: false,
                            theme: &Theme::light(),
                        },
                        Rc::new(move |_, _| selection_count.set(selection_count.get() + 1)),
                        cx,
                    )
                    .navigation();
                    row.select_navigation(true, cx);
                    row.set_delayed_rename(Some(Rc::new(move |_, _| {
                        rename_count.set(rename_count.get() + 1)
                    })));
                    row.set_navigation_accessory(Some(accessory.clone()), cx);
                    if table {
                        row.set_table_cells("key", "value", false, cx);
                    }
                    row
                });
                AccessoryRow { row, accessory }
            });
            cx.run_until_parked();
            window
                .update(cx, |fixture, window, cx| {
                    window.activate_window();
                    fixture.row.update(cx, |row, cx| row.focus_from_arrow(window, cx));
                })
                .expect("focus row");
            cx.run_until_parked();
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            let slot = visual.debug_bounds("navigation-accessory").expect("accessory hit target");
            let icon = visual.debug_bounds("navigation-accessory-icon").expect("accessory glyph");
            assert_eq!(slot.size, gpui::size(px(24.), px(24.)));
            assert_eq!(icon.size, gpui::size(px(16.), px(16.)));
            assert_eq!(icon.center(), slot.center());
            visual.simulate_mouse_down(slot.center(), MouseButton::Left, Default::default());
            assert_eq!(activated.get(), 0, "ordinary button activates on release");
            visual.simulate_mouse_up(slot.center(), MouseButton::Left, Default::default());
            assert_eq!(activated.get(), 1);
            assert_eq!(selected.get(), 0);
            assert_eq!(renamed.get(), 0);
            window
                .update(&mut visual, |fixture, window, cx| {
                    let row = fixture.row.read(cx);
                    assert!(row.focused(window), "pointer accessory preserves row arrow focus");
                    assert!(!row.keyboard_focus_visible(window));
                    assert!(!row.press.pointer);
                    assert!(row.rename_task.is_none());
                })
                .expect("nested activation");
            visual.simulate_keystrokes("tab");
            window
                .update(&mut visual, |fixture, window, cx| {
                    assert!(fixture.accessory.read(cx).keyboard_focus_visible(window));
                })
                .expect("Tab reaches accessory");
            visual.simulate_keystrokes("space");
            visual.simulate_event(gpui::KeyUpEvent {
                keystroke: gpui::Keystroke::parse("space").expect("valid Space key"),
            });
            assert_eq!(activated.get(), 2);
            assert_eq!(selected.get(), 0, "accessory keyboard activation cannot select its row");
            window.update(&mut visual, |_, window, _| window.remove_window()).expect("close");
        }
    }

    struct NavigationRows {
        rows: [gpui::Entity<Control>; 2],
        selected: usize,
        dispatches: usize,
        accepts_selection: bool,
    }

    impl Render for NavigationRows {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            for (index, row) in self.rows.iter().enumerate() {
                row.update(cx, |row, cx| row.select_navigation(index == self.selected, cx));
            }
            div()
                .w(px(240.))
                .flex()
                .flex_col()
                .on_key_down(|event, window, cx| {
                    if event.keystroke.key == "tab" {
                        window.focus_next();
                        cx.stop_propagation();
                    }
                })
                .children(self.rows.iter().cloned())
        }
    }

    fn assert_navigation_selection(
        window: gpui::WindowHandle<NavigationRows>,
        selected: usize,
        dispatches: usize,
        cx: &mut gpui::TestAppContext,
    ) {
        window
            .update(cx, |rows, window, cx| {
                assert_eq!(rows.selected, selected);
                assert_eq!(rows.dispatches, dispatches);
                for (index, row) in rows.rows.iter().enumerate() {
                    let row = row.read(cx);
                    assert_eq!(row.navigation, Some(index == selected));
                    let appearance = Appearance {
                        scale: 1.,
                        display: window.scale_factor(),
                        focused: false,
                        pressed: row.press.active(),
                    };
                    let mut surface = row.render_surface(&appearance, window);
                    let selection = if window.is_window_active() && row.navigation_selection_active
                    {
                        Role::MenuSelection
                    } else {
                        Role::ColumnSelectionInactive
                    };
                    assert_eq!(
                        surface.style().background,
                        (index == selected).then(|| row.color(selection).into()),
                        "only the accepted selection has a filled row"
                    );
                }
                assert_eq!(
                    window
                        .accessibility_nodes_for_test()
                        .filter(|(_, node)| node.is_selected() == Some(true))
                        .count(),
                    1
                );
            })
            .expect("navigation selection");
    }

    #[gpui::test]
    fn navigation_selects_on_down_and_retains_selection_after_an_outside_release(
        cx: &mut gpui::TestAppContext,
    ) {
        // The parent owns selection. A rejected change cannot gain pressed-row fill,
        // and an accepted selection survives dragging away without another dispatch.
        let window = cx.add_window(|_, cx| {
            let owner: gpui::WeakEntity<NavigationRows> = cx.weak_entity();
            let rows = std::array::from_fn(|index| {
                let owner = owner.clone();
                cx.new(|cx| {
                    Control::new(
                        index as u32,
                        Properties {
                            kind: Kind::Button(ButtonStyle::Default),
                            label: ["Old row", "New row"][index],
                            disabled: false,
                            theme: &Theme::light(),
                        },
                        Rc::new(move |_, cx| {
                            let _ = owner.update(cx, |rows, cx| {
                                rows.dispatches += 1;
                                if rows.accepts_selection {
                                    rows.selected = index;
                                    cx.notify();
                                }
                            });
                        }),
                        cx,
                    )
                    .navigation()
                })
            });
            NavigationRows { rows, selected: 0, dispatches: 0, accepts_selection: true }
        });
        cx.run_until_parked();
        assert_navigation_selection(window, 0, 0, cx);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_mouse_down(point(px(12.), px(48.)), MouseButton::Left, Default::default());
        assert_navigation_selection(window, 1, 1, cx);
        window
            .update(cx, |rows, window, cx| {
                assert!(
                    rows.rows[1].read(cx).focused(window),
                    "first down focuses the accepted row"
                );
                assert!(!window.is_window_active(), "logical focus must not activate the app");
                assert!(!rows.rows[1].read(cx).keyboard_focus_visible(window));
                assert!(!rows.rows[0].read(cx).focused(window));
            })
            .expect("accepted pointer focus");
        window.update(cx, |_, window, _| window.activate_window()).expect("activate");
        visual.run_until_parked();
        window
            .update(cx, |rows, window, cx| {
                assert!(!rows.rows[1].read(cx).keyboard_focus_visible(window));
            })
            .expect("activation keeps pointer focus ring hidden");
        visual.simulate_mouse_move(
            point(px(500.), px(500.)),
            MouseButton::Left,
            Default::default(),
        );
        visual.simulate_mouse_up(point(px(500.), px(500.)), MouseButton::Left, Default::default());
        assert_navigation_selection(window, 1, 1, cx);
        visual.simulate_keystrokes("down right up left");
        window
            .update(cx, |rows, window, cx| {
                assert!(rows.rows[1].read(cx).focused(window));
                assert!(!rows.rows[1].read(cx).keyboard_focus_visible(window));
            })
            .expect("list arrows do not reveal a pointer-focused row's ring");

        window.update(cx, |rows, _, _| rows.accepts_selection = false).expect("reject navigation");
        visual.simulate_mouse_down(point(px(12.), px(16.)), MouseButton::Left, Default::default());
        assert_navigation_selection(window, 1, 2, cx);
        window
            .update(cx, |rows, window, cx| {
                assert!(rows.rows[1].read(cx).focused(window), "a veto keeps the old focus");
                assert!(!rows.rows[0].read(cx).focused(window));
            })
            .expect("rejected pointer focus");
        visual.simulate_mouse_up(point(px(12.), px(16.)), MouseButton::Left, Default::default());
        assert_navigation_selection(window, 1, 2, cx);
        visual.simulate_keystrokes("tab");
        window
            .update(cx, |rows, window, cx| {
                assert!(rows.rows[0].read(cx).keyboard_focus_visible(window));
                assert_eq!(rows.selected, 1, "Tab changes focus without changing selection");
            })
            .expect("keyboard focus");
        window
            .update(cx, |rows, _, cx| {
                rows.rows[0].update(cx, |row, cx| {
                    row.set(
                        Properties {
                            kind: Kind::Button(ButtonStyle::Default),
                            label: "Old row",
                            disabled: true,
                            theme: &Theme::light(),
                        },
                        cx,
                    );
                });
            })
            .expect("disable navigation row");
        visual.run_until_parked();
        visual.simulate_click(point(px(12.), px(16.)), Default::default());
        assert_navigation_selection(window, 1, 2, cx);
    }

    #[gpui::test]
    fn selected_row_focus_waits_for_an_accepted_active_column(cx: &mut gpui::TestAppContext) {
        // A secondary row must wait for activation; the active selected row can take focus.
        let accepted = Rc::new(std::cell::Cell::new(false));
        let revoke_rename = Rc::new(std::cell::Cell::new(false));
        let window = cx.add_window(|_, cx| {
            let accepted = accepted.clone();
            let revoke_rename = revoke_rename.clone();
            let weak: gpui::WeakEntity<Control> = cx.weak_entity();
            let select: Activate = Rc::new(move |_, cx| {
                let accepted = accepted.get();
                let revoke_rename = revoke_rename.get();
                if accepted || revoke_rename {
                    let weak = weak.clone();
                    cx.defer(move |cx| {
                        let _ = weak.update(cx, |row, cx| {
                            if revoke_rename {
                                row.set_delayed_rename(None);
                            }
                            if accepted {
                                row.set_navigation_selection_active(true, cx);
                            }
                            cx.notify();
                        });
                    });
                }
            });
            let mut row = Control::new(
                1,
                Properties {
                    kind: Kind::Button(ButtonStyle::Default),
                    label: "Selected volume",
                    disabled: false,
                    theme: &Theme::light(),
                },
                select.clone(),
                cx,
            )
            .navigation();
            row.select_navigation(true, cx);
            row.set_single_click(select);
            row
        });
        let editor_focus = window
            .update(cx, |_, window, cx| {
                let focus = cx.focus_handle();
                window.activate_window();
                window.focus(&focus);
                focus
            })
            .expect("editor focus");
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        for active in [true, false] {
            window
                .update(cx, |row, window, cx| {
                    window.focus(&editor_focus);
                    row.set_navigation_selection_active(active, cx);
                })
                .expect("column activity");
            visual.run_until_parked();
            visual.simulate_click(point(px(12.), px(12.)), Default::default());
            window
                .update(cx, |row, window, _| {
                    if active {
                        assert!(row.focused(window), "clicking the active row restores focus");
                        assert!(!row.keyboard_focus_visible(window));
                    } else {
                        assert!(editor_focus.is_focused(window), "a veto keeps secondary focus");
                        assert_eq!(row.focus_intent, FocusIntent::None);
                    }
                })
                .expect("vetoed selected row");
        }
        // The previous frame may still offer rename when a newly edited draft vetoes selection.
        // Revoke it during the callback, before the control consumes its pointer-focus intent.
        revoke_rename.set(true);
        for active in [true, false] {
            window
                .update(cx, |row, window, cx| {
                    window.focus(&editor_focus);
                    row.set_navigation_selection_active(active, cx);
                    row.set_delayed_rename(Some(Rc::new(|_, _| {})));
                    cx.notify();
                })
                .expect("previous frame's rename eligibility");
            visual.run_until_parked();
            visual.simulate_mouse_down(
                point(px(12.), px(12.)),
                MouseButton::Left,
                Default::default(),
            );
            window
                .update(cx, |row, window, _| {
                    assert!(
                        editor_focus.is_focused(window),
                        "revoked rename must preserve editor focus"
                    );
                    assert_eq!(row.focus_intent, FocusIntent::None);
                    assert!(row.delayed_rename.is_none());
                    assert!(row.rename_press.is_none());
                    assert!(row.rename_task.is_none());
                })
                .expect("revoked rename before render");
            visual.simulate_mouse_up(
                point(px(12.), px(12.)),
                MouseButton::Left,
                Default::default(),
            );
        }
        revoke_rename.set(false);
        window
            .update(cx, |row, _, cx| row.set_navigation_selection_active(true, cx))
            .expect("unrelated later activity");
        visual.run_until_parked();
        window
            .update(cx, |_, window, _| assert!(editor_focus.is_focused(window)))
            .expect("discarded focus intent");

        accepted.set(true);
        window
            .update(cx, |row, _, cx| row.set_navigation_selection_active(false, cx))
            .expect("inactive selected volume");
        visual.run_until_parked();
        visual.simulate_mouse_down(point(px(12.), px(12.)), MouseButton::Left, Default::default());
        window
            .update(cx, |row, window, _| {
                assert!(row.focused(window), "accepted active column focuses before release");
                assert!(!row.keyboard_focus_visible(window));
            })
            .expect("accepted selected volume");
    }

    #[gpui::test]
    fn table_cells_keep_pointer_selection_focus_and_accessibility(cx: &mut gpui::TestAppContext) {
        let actions = Rc::new(std::cell::Cell::new(0));
        let activated = actions.clone();
        let window = cx.add_window(|_, cx| {
            let weak: gpui::WeakEntity<Control> = cx.weak_entity();
            let mut row = Control::new(
                1,
                Properties {
                    kind: Kind::Button(ButtonStyle::Default),
                    label: "seed",
                    disabled: false,
                    theme: &Theme::light(),
                },
                Rc::new(move |_, cx| {
                    activated.set(activated.get() + 1);
                    let weak = weak.clone();
                    cx.defer(move |cx| {
                        let _ = weak.update(cx, |row, cx| row.select_navigation(true, cx));
                    });
                }),
                cx,
            )
            .navigation();
            row.set_table_cells("seed", "0x9aa0c7f3", true, cx);
            row
        });
        window.update(cx, |_, window, _| window.activate_window()).expect("activate table");
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_mouse_down(point(px(16.), px(12.)), MouseButton::Left, Default::default());
        assert_eq!(actions.get(), 1, "table selection dispatches on down");
        window
            .update(cx, |row, window, _| {
                assert!(row.focused(window));
                assert!(!row.keyboard_focus_visible(window));
                assert_eq!(row.bounds.expect("table row").size.height, px(24.));
                let (_, node) = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some("seed"))
                    .expect("table control remains accessible");
                assert_eq!(node.is_selected(), Some(true));
                assert_eq!(node.description(), Some("Value: 0x9aa0c7f3"));
            })
            .expect("selected table row");
        let key = visual.debug_bounds("table-key-cell").expect("key cell");
        let value = visual.debug_bounds("table-value-cell").expect("value cell");
        visual.simulate_mouse_up(point(px(16.), px(12.)), MouseButton::Left, Default::default());
        assert_eq!(actions.get(), 1, "release does not repeat the table selection");
        visual.simulate_keystrokes("down");
        window
            .update(cx, |row, window, _| {
                assert!(row.focused(window));
                assert!(!row.keyboard_focus_visible(window));
            })
            .expect("table arrows retain logical focus without revealing its ring");
        assert_eq!(visual.debug_bounds("table-key-cell"), Some(key));
        assert_eq!(visual.debug_bounds("table-value-cell"), Some(value));
        visual.simulate_keystrokes("tab");
        window
            .update(cx, |row, window, _| assert!(row.keyboard_focus_visible(window)))
            .expect("Tab reveals table keyboard focus");
        assert_eq!(visual.debug_bounds("table-key-cell"), Some(key));
        assert_eq!(visual.debug_bounds("table-value-cell"), Some(value));
        assert_eq!(
            visual.debug_bounds("table-focus-ring"),
            visual.debug_bounds("table-row-content")
        );
    }

    #[gpui::test]
    fn switch_geometry_keeps_checkbox_pointer_keyboard_and_accessibility_semantics(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(zoom::install);
        for scale in [10_000, 20_000] {
            cx.update(|cx| zoom::set_scale(scale, cx).expect("supported scale"));
            let actions = Rc::new(std::cell::Cell::new(0));
            let activated = actions.clone();
            let window = cx.add_window(|window, cx| {
                zoom::apply(window, cx);
                let weak: gpui::WeakEntity<Control> = cx.weak_entity();
                let mut control = Control::new(
                    1,
                    Properties {
                        kind: Kind::Checkbox(CheckState::Off),
                        label: "Custom Permissions",
                        disabled: false,
                        theme: &Theme::light(),
                    },
                    Rc::new(move |_, cx| {
                        activated.set(activated.get() + 1);
                        let weak = weak.clone();
                        cx.defer(move |cx| {
                            let _ = weak.update(cx, |control, cx| {
                                let on = matches!(control.kind, Kind::Checkbox(CheckState::Off));
                                control.set(
                                    Properties {
                                        kind: Kind::Checkbox(if on {
                                            CheckState::On
                                        } else {
                                            CheckState::Off
                                        }),
                                        label: "Custom Permissions",
                                        disabled: false,
                                        theme: &Theme::light(),
                                    },
                                    cx,
                                );
                            });
                        });
                    }),
                    cx,
                );
                control.set_switch(cx);
                control
            });
            window.update(cx, |_, window, _| window.activate_window()).expect("activate switch");
            cx.run_until_parked();
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            let factor = scale as f32 / 10_000.;
            let socket = visual.debug_bounds("switch-socket").expect("socket");
            let knob = visual.debug_bounds("switch-knob").expect("off knob");
            assert_eq!(
                socket,
                Bounds::new(
                    point(px(0.), px(factor)),
                    gpui::size(px(36. * factor), px(22. * factor))
                )
            );
            assert_eq!(
                knob,
                Bounds::new(
                    point(px(2. * factor), px(3. * factor)),
                    gpui::size(px(18. * factor), px(18. * factor))
                )
            );
            let position = point(px(8. * factor), px(12. * factor));
            visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
            assert_eq!(actions.get(), 0, "switch keeps checkbox release activation");
            visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
            assert_eq!(actions.get(), 1);
            assert_eq!(
                visual.debug_bounds("switch-knob").expect("on knob").left(),
                px(16. * factor)
            );
            window
                .update(cx, |control, window, _| {
                    assert_eq!(
                        control.bounds.expect("switch hit target").size,
                        gpui::size(px(36. * factor), px(24. * factor))
                    );
                    let (_, node) = window
                        .accessibility_nodes_for_test()
                        .find(|(_, node)| node.label() == Some("Custom Permissions"))
                        .expect("accessible switch label");
                    assert_eq!(node.role(), gpui::accesskit::Role::CheckBox);
                    assert_eq!(node.toggled(), Some(gpui::accesskit::Toggled::True));
                    control.focus(window);
                })
                .expect("checkbox accessibility");
            visual.simulate_keystrokes("space");
            visual.simulate_event(gpui::KeyUpEvent {
                keystroke: gpui::Keystroke::parse("space").expect("space key"),
            });
            assert_eq!(actions.get(), 2, "Space retains checkbox activation");
            assert_eq!(visual.debug_bounds("switch-knob"), Some(knob));
            assert_eq!(visual.debug_bounds("switch-socket"), Some(socket));
            window.update(cx, |_, window, _| window.remove_window()).expect("close switch");
        }
    }

    #[gpui::test]
    fn switch_is_centered_in_a_section_heading(cx: &mut gpui::TestAppContext) {
        // Center the 24 dp control in a 44 dp section at both supported test scales.
        struct Heading(gpui::Entity<Control>);
        impl Render for Heading {
            fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .w(zoom::pixels(window, 360.))
                    .h(zoom::pixels(window, 44.))
                    .flex()
                    .items_center()
                    .child(div().flex_1())
                    .child(self.0.clone())
            }
        }
        cx.update(zoom::install);
        for scale in [10_000, 20_000] {
            cx.update(|cx| zoom::set_scale(scale, cx).expect("supported scale"));
            let window = cx.add_window(|window, cx| {
                zoom::apply(window, cx);
                Heading(cx.new(|cx| {
                    let mut control = Control::new(
                        1,
                        Properties {
                            kind: Kind::Checkbox(CheckState::Off),
                            label: "Custom Permissions",
                            disabled: false,
                            theme: &Theme::light(),
                        },
                        Rc::new(|_, _| {}),
                        cx,
                    );
                    control.set_switch(cx);
                    control
                }))
            });
            cx.run_until_parked();
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            let factor = scale as f32 / 10_000.;
            let socket = visual.debug_bounds("switch-socket").expect("section switch");
            assert_eq!(socket.top(), px(11. * factor));
            assert_eq!(socket.size.height, px(22. * factor));
            window.update(cx, |_, window, _| window.remove_window()).expect("close heading");
        }
    }

    #[gpui::test]
    fn row_focus_overlay_keeps_content_geometry_at_each_zoom(cx: &mut gpui::TestAppContext) {
        struct RowGeometry {
            row: gpui::Entity<Control>,
            focused: bool,
            children: Rc<std::cell::RefCell<Vec<Bounds<Pixels>>>>,
        }
        impl Render for RowGeometry {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let children = self.children.clone();
                let surface = self
                    .row
                    .read(cx)
                    .navigation_surface(true, self.focused, window)
                    .on_children_prepainted(move |bounds, _, _| *children.borrow_mut() = bounds);
                div().w(zoom::pixels(window, 240.)).child(surface)
            }
        }
        cx.update(zoom::install);
        for scale in [10_000, 20_000] {
            cx.update(|cx| zoom::set_scale(scale, cx).expect("supported scale"));
            for sidebar in [false, true] {
                let children = Rc::new(std::cell::RefCell::new(Vec::new()));
                let window = cx.add_window(|window, cx| {
                    zoom::apply(window, cx);
                    let row = cx.new(|cx| {
                        let mut row = Control::new(
                            1,
                            Properties {
                                kind: Kind::Button(ButtonStyle::Default),
                                label: "Row label",
                                disabled: false,
                                theme: &Theme::light(),
                            },
                            Rc::new(|_, _| {}),
                            cx,
                        )
                        .navigation();
                        row.sidebar_row = sidebar;
                        row
                    });
                    RowGeometry { row, focused: false, children: children.clone() }
                });
                cx.run_until_parked();
                let content = children.borrow().clone();
                window
                    .update(cx, |geometry, window, cx| {
                        let row = geometry.row.read(cx);
                        let mut surface = row.navigation_surface(true, true, window);
                        assert_eq!(surface.style().border_widths.top, None);
                        assert_eq!(surface.style().border_widths.left, None);
                        let mut unselected = row.navigation_surface(false, false, window);
                        assert_eq!(
                            unselected.style().text.as_ref().and_then(|text| text.color),
                            Some(row.color(Role::Text).alpha(0.9))
                        );
                        let mut ring = row.navigation_focus_ring(window);
                        assert_eq!(ring.style().position, Some(gpui::Position::Absolute));
                        for border in [
                            ring.style().border_widths.top,
                            ring.style().border_widths.right,
                            ring.style().border_widths.bottom,
                            ring.style().border_widths.left,
                        ] {
                            assert_eq!(border, Some(zoom::pixels(window, 2.).into()));
                        }
                        geometry.focused = true;
                        cx.notify();
                    })
                    .expect("focus overlay");
                cx.run_until_parked();
                let observed = children.borrow();
                assert_eq!(
                    &observed[..content.len()],
                    content.as_slice(),
                    "keyboard focus cannot move or shrink the row's glyph or label"
                );
                assert_eq!(observed.len(), content.len() + 1);
                let factor = scale as f32 / 10_000.;
                assert_eq!(
                    observed[content.len()],
                    Bounds::new(
                        point(px(0.), px(0.)),
                        gpui::size(px(240. * factor), px(32. * factor))
                    )
                );
                window.update(cx, |_, window, _| window.remove_window()).expect("close geometry");
            }
        }
    }

    #[gpui::test]
    fn navigation_metadata_keeps_icon_buttons_and_app_cards_on_release(
        cx: &mut gpui::TestAppContext,
    ) {
        // These controls share navigation metadata but are actions rather than selectable rows.
        for presentation in 0..3 {
            let count = Rc::new(std::cell::Cell::new(0));
            let activated = count.clone();
            let window = cx.add_window(|_, cx| {
                let mut control = Control::new(
                    1,
                    Properties {
                        kind: Kind::Button(ButtonStyle::Default),
                        label: "Action",
                        disabled: false,
                        theme: &Theme::light(),
                    },
                    Rc::new(move |_, _| activated.set(activated.get() + 1)),
                    cx,
                )
                .navigation();
                match presentation {
                    0 => control = control.toolbar_icon("add"),
                    1 => control.set_sidebar_icon(SidebarIcon::Glyph("add"), false, false, cx),
                    _ => {
                        control.set_session_style(SessionStyle::AppTile { bundled_icon: None }, cx)
                    }
                }
                control
            });
            cx.run_until_parked();
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            visual.simulate_mouse_down(
                point(px(12.), px(12.)),
                MouseButton::Left,
                Default::default(),
            );
            assert_eq!(count.get(), 0);
            visual.simulate_mouse_up(
                point(px(12.), px(12.)),
                MouseButton::Left,
                Default::default(),
            );
            assert_eq!(count.get(), 1);
            window.update(cx, |_, window, _| window.remove_window()).expect("close action fixture");
        }
    }

    #[gpui::test]
    fn sidebar_selection_follows_native_window_activation(cx: &mut gpui::TestAppContext) {
        // Switch native activity without changing selection or notifying the control.
        let window = cx.add_window(|_, cx| {
            let mut control = Control::new(
                1,
                Properties {
                    kind: Kind::Button(ButtonStyle::Default),
                    label: "Pinball",
                    disabled: false,
                    theme: &Theme::light(),
                },
                Rc::new(|_, _| {}),
                cx,
            )
            .navigation();
            control.set_sidebar_row();
            control.select_navigation(true, cx);
            control
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let mut renders = 0;
        for active in [true, false, true] {
            if active {
                window.update(cx, |_, window, _| window.activate_window()).expect("activate");
                cx.run_until_parked();
            } else {
                visual.deactivate_window();
            }
            window
                .update(cx, |control, window, _| {
                    assert_eq!(window.is_window_active(), active);
                    assert!(control.renders > renders, "activation must redraw retained controls");
                    renders = control.renders;
                    let mut selected = control.navigation_surface(true, false, window);
                    let foreground = if active { rgba(0xffffffff) } else { rgba(0x000000cc) };
                    let background = if active {
                        control.color(Role::MenuSelection)
                    } else {
                        control.color(Role::ColumnSelectionInactive)
                    };
                    assert_eq!(
                        selected.style().text.as_ref().and_then(|text| text.color),
                        Some(foreground.into())
                    );
                    assert_eq!(selected.style().background, Some(background.into()));
                    let mut unselected = control.navigation_surface(false, false, window);
                    assert_eq!(
                        unselected.style().text.as_ref().and_then(|text| text.color),
                        Some(control.color(Role::Text).alpha(0.9))
                    );
                })
                .expect("sidebar appearance");
        }
        window
            .update(cx, |control, window, cx| {
                for active in [false, true] {
                    control.set_navigation_selection_active(active, cx);
                    let appearance = Appearance {
                        scale: 1.,
                        display: window.scale_factor(),
                        focused: false,
                        pressed: false,
                    };
                    let mut surface = control.render_surface(&appearance, window);
                    assert_eq!(
                        surface.style().background,
                        Some(
                            control
                                .color(if active {
                                    Role::MenuSelection
                                } else {
                                    Role::ColumnSelectionInactive
                                })
                                .into()
                        )
                    );
                    assert_eq!(
                        surface.style().text.as_ref().and_then(|text| text.color),
                        Some(if active {
                            control.color(Role::MenuSelectionText)
                        } else {
                            gpui::black()
                        })
                    );
                    let mut unselected = control.navigation_surface(false, false, window);
                    assert_eq!(
                        unselected.style().text.as_ref().and_then(|text| text.color),
                        Some(control.color(Role::Text).alpha(0.9))
                    );
                    assert_eq!(control.navigation, Some(true));
                    assert_eq!(
                        control.accessibility_node(Bounds::default()).is_selected(),
                        Some(true)
                    );
                }
            })
            .expect("selection follows active content column");
    }

    #[gpui::test]
    fn context_target_outline_tracks_menu_lifecycle_without_selecting_the_row(
        cx: &mut gpui::TestAppContext,
    ) {
        // Use the pointer handler and rendered frame so transient pressed styling
        // cannot turn a context target into a filled navigation selection.
        for selected in [false, true] {
            let window = cx.add_window(|_, cx| {
                let mut control = Control::new(
                    1,
                    Properties {
                        kind: Kind::Button(ButtonStyle::Default),
                        label: "Volume",
                        disabled: false,
                        theme: &Theme::light(),
                    },
                    Rc::new(|_, _| {}),
                    cx,
                )
                .navigation();
                control.set_sidebar_row();
                control.select_navigation(selected, cx);
                let target = cx.weak_entity();
                control.set_context_menu(Rc::new(move |_, _, cx| {
                    let target = target.clone();
                    cx.defer(move |cx| {
                        let _ = target.update(cx, |control, cx| control.set_popup_open(true, cx));
                    });
                }));
                control
            });
            window.update(cx, |_, window, _| window.activate_window()).expect("activate");
            cx.run_until_parked();
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            assert_eq!(visual.debug_bounds("navigation-context-outline"), None);
            visual.simulate_mouse_down(
                point(px(12.), px(12.)),
                MouseButton::Right,
                Default::default(),
            );
            visual.simulate_mouse_up(
                point(px(12.), px(12.)),
                MouseButton::Right,
                Default::default(),
            );
            for open in [true, false] {
                if !open {
                    // Menu finish uses this same setter after dismissing its native window.
                    window
                        .update(cx, |control, _, cx| control.set_popup_open(false, cx))
                        .expect("dismiss context menu");
                    visual.run_until_parked();
                }
                let outline = visual.debug_bounds("navigation-context-outline");
                window
                    .update(cx, |control, window, _| {
                        let bounds = control.bounds.expect("rendered navigation bounds");
                        assert_eq!(control.navigation, Some(selected));
                        assert_eq!(control.popup_open, open);
                        // GPUI retains debug bounds across frames; inspect the first opening,
                        // then validate dismissal through current state and production styling.
                        if open {
                            assert_eq!(outline, (!selected).then_some(bounds));
                        }
                        let appearance = Appearance {
                            scale: 1.,
                            display: window.scale_factor(),
                            focused: false,
                            pressed: control.press.active() || control.popup_open,
                        };
                        let mut surface = control.render_surface(&appearance, window);
                        assert_eq!(
                            surface.style().background,
                            selected.then(|| control.color(Role::MenuSelection).into())
                        );
                        assert_eq!(
                            control.accessibility_node(bounds).is_selected(),
                            Some(selected)
                        );
                    })
                    .expect("rendered context target");
            }
            visual.simulate_mouse_down(
                point(px(12.), px(12.)),
                MouseButton::Left,
                Default::default(),
            );
            window
                .update(cx, |control, window, _| {
                    let appearance = Appearance {
                        scale: 1.,
                        display: window.scale_factor(),
                        focused: false,
                        pressed: control.press.active(),
                    };
                    let mut surface = control.render_surface(&appearance, window);
                    assert_eq!(
                        surface.style().background,
                        selected.then(|| control.color(Role::MenuSelection).into()),
                        "holding an unselected row cannot create a synthetic selection"
                    );
                })
                .expect("ordinary navigation press");
            visual.simulate_mouse_up(
                point(px(12.), px(12.)),
                MouseButton::Left,
                Default::default(),
            );
            window.update(cx, |_, window, _| window.remove_window()).expect("close fixture");
        }
    }
}
