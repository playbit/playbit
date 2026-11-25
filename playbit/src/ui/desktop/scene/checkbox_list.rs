//! Virtualize the visual part of a scroll view's fixed-height checkbox column.
use super::Scene;
use crate::{
    runtime::ViewNode,
    ui::desktop::{controls, zoom},
};
use gpui::{AnyElement, App, Window, prelude::*};

const MIN_VIRTUAL_ROWS: usize = 32;

impl Scene {
    pub(super) fn virtual_checkbox_column(
        &mut self,
        child: &ViewNode,
        theme: &playbit_api::ui::Theme,
        window: &Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let ViewNode::Column(nodes) = child else { return None };
        if nodes.len() < MIN_VIRTUAL_ROWS
            || !nodes.iter().all(|node| matches!(node, ViewNode::Checkbox { .. }))
        {
            return None;
        }
        let rows = nodes
            .iter()
            .filter_map(|node| {
                let ViewNode::Checkbox { id, label, state, disabled } = node else {
                    return None;
                };
                let properties = controls::Properties {
                    kind: controls::Kind::Checkbox(*state),
                    label,
                    disabled: *disabled || !self.active,
                    theme,
                };
                Some(self.control_entity(*id, properties, cx).clone())
            })
            .collect();
        Some(
            controls::virtual_rows(
                rows,
                controls::checkbox_height(window),
                zoom::pixels(window, 12.),
            )
            .into_any_element(),
        )
    }
}
