//! Keyboard focus follows the visible app, session and data columns.
use super::*;

impl Shell {
    pub(in crate::desktop) fn navigate_application_columns(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.applications.available() || self.sidebar_volumes || self.new_session.is_some() {
            return false;
        }
        if self.navigate_application_browser(key, window, cx) {
            return true;
        }
        let focused = self.applications.columns.iter().enumerate().find_map(|(column, rows)| {
            rows.iter()
                .position(|row| {
                    self.navigation_controls
                        .get(&row.key)
                        .is_some_and(|control| control.read(cx).focused(window))
                })
                .map(|row| (column, row))
        });
        let Some((column, index)) = focused else {
            return false;
        };
        if key == "right"
            && column == if self.applications.scope_in_session { 2 } else { 1 }
            && self.applications.root_open
            && let Some(browser) = &self.browser
        {
            browser.update(cx, |browser, cx| browser.focus_entry_list(window, cx));
            cx.notify();
            cx.stop_propagation();
            return true;
        }
        let rows = &self.applications.columns;
        let target = match key {
            "up" => Some((column, index.saturating_sub(1))),
            "down" => Some((column, (index + 1).min(rows[column].len() - 1))),
            "left" if column > 0 => Some((
                column - 1,
                rows[column - 1].iter().position(|row| row.selected).unwrap_or(0),
            )),
            "right" if column + 1 < rows.len() && !rows[column + 1].is_empty() => Some((
                column + 1,
                rows[column + 1].iter().position(|row| row.selected).unwrap_or(0),
            )),
            _ => None,
        };
        if let Some((column, index)) = target {
            let row = &rows[column][index];
            let target = row.target.clone();
            let key = row.key.clone();
            let selected = row.selected;
            if (selected || self.select_application_target(target, cx))
                && let Some(control) = self.navigation_controls.get(&key)
            {
                control.update(cx, |control, cx| control.focus_from_arrow(window, cx));
            }
        }
        cx.stop_propagation();
        true
    }

    fn navigate_application_browser(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(browser) = self.browser.as_ref().filter(|_| self.applications.root_open) else {
            return false;
        };
        match browser.update(cx, |browser, cx| browser.navigate_list(key, window, cx)) {
            browser::ListNavigation::Unhandled => return false,
            browser::ListNavigation::Handled => {}
            browser::ListNavigation::PreviousColumn => {
                let Some(role) = self.applications.selected_role else { return false };
                let key = scope_key(role, self.applications.scope_in_session);
                if let Some(control) = self.navigation_controls.get(&key) {
                    control.update(cx, |control, cx| control.focus_from_arrow(window, cx));
                }
            }
        }
        cx.notify();
        cx.stop_propagation();
        true
    }
}
