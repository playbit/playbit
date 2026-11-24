//! Shared UI button creation, callback binding, and control reuse.
use super::*;
use playbit_api::ui::ButtonStyle;

impl Shell {
    pub(super) fn host_button(
        &mut self,
        id: &'static str,
        title: &'static str,
        disabled: bool,
        activate: controls::Activate,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let theme = playbit_api::ui::Theme::light();
        let properties = controls::Properties {
            kind: controls::Kind::Button(playbit_api::ui::ButtonStyle::Default),
            label: title,
            disabled,
            theme: &theme,
        };
        self.host_control(id, properties, activate, cx)
    }

    pub(super) fn host_control(
        &mut self,
        id: &'static str,
        properties: controls::Properties<'_>,
        activate: controls::Activate,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        self.rendered_host_controls.push(id);
        let control = self.host_controls.entry(id).or_insert_with(|| {
            cx.new(|cx| controls::Control::new(0, properties, activate.clone(), cx))
        });
        control.update(cx, |control, cx| {
            control.set(properties, cx);
            control.set_activation(activate);
        });
        control.clone()
    }

    pub(super) fn action(
        &mut self,
        id: &'static str,
        title: &'static str,
        request: Request,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let disabled =
            worker_recovery::Action::of(&request).is_some() && self.recovery_busy.is_some();
        let weak = cx.weak_entity();
        self.host_button(
            id,
            title,
            disabled,
            Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| this.send_action(request.clone(), cx));
            }),
            cx,
        )
    }

    pub(super) fn host_callback(
        &mut self,
        id: &'static str,
        title: &'static str,
        disabled: bool,
        activate: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let weak = cx.weak_entity();
        self.host_button(
            id,
            title,
            disabled,
            Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    let _timing = crate::trace::Span::new("ui.host.action");
                    this.cancel_invitation_restoration();
                    activate(this, cx);
                    cx.notify();
                });
            }),
            cx,
        )
    }

    pub(super) fn navigation_button(
        &mut self,
        key: String,
        label: &str,
        selected: bool,
        activate: controls::Activate,
        icon: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let theme = theme();
        let properties = controls::Properties {
            kind: controls::Kind::Button(ButtonStyle::Default),
            label,
            disabled: self.enrolled.is_none(),
            theme: &theme,
        };
        let control = self.navigation_controls.entry(key).or_insert_with(|| {
            cx.new(|cx| {
                let control =
                    controls::Control::new(0, properties, activate.clone(), cx).navigation();
                if let Some(icon) = icon { control.toolbar_icon(icon) } else { control }
            })
        });
        control.update(cx, |control, cx| {
            control.set(properties, cx);
            control.set_activation(activate);
            control.select_navigation(selected, cx);
        });
        control.clone()
    }
}
