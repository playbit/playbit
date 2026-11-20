//! Accessibility projection of the existing rendered frame.
use crate::{App, Window};
use accesskit::{ActionRequest, Node, NodeId};
use std::{rc::Rc, sync::Arc};

/// Maximum accessibility nodes in one window, including its root.
pub const MAX_ACCESSIBILITY_NODES: usize = 1024;
/// Maximum projected UTF-8 label/value/placeholder bytes per window.
pub const MAX_ACCESSIBILITY_TEXT_BYTES: usize = 1024 * 1024;

/// Dispatches a native action through the current control implementation.
pub type AccessibilityActionHandler = Rc<dyn Fn(&ActionRequest, &mut Window, &mut App)>;

#[derive(Clone)]
pub(crate) struct AccessibilityNode {
    pub id: NodeId,
    pub node: Arc<Node>,
    pub focused: bool,
    pub action: Option<AccessibilityActionHandler>,
}

impl AccessibilityNode {
    pub fn text_bytes(&self) -> usize {
        [self.node.label(), self.node.value(), self.node.placeholder()]
            .into_iter()
            .flatten()
            .map(str::len)
            .sum()
    }
}

impl Window {
    /// Add one semantic node during prepaint. IDs must remain attached to the
    /// same control for its lifetime; zero and one are reserved for the window.
    /// Actions are dispatched only against the latest completed frame.
    pub fn insert_accessibility_node(
        &mut self,
        id: NodeId,
        node: Node,
        focused: bool,
        action: Option<AccessibilityActionHandler>,
    ) {
        self.invalidator.debug_assert_prepaint();
        if self.input_disabled {
            return;
        }
        assert!(id.0 > 1, "accessibility IDs zero and one are reserved");
        if self.next_frame.accessibility_error.is_some() {
            return;
        }
        let entry = AccessibilityNode { id, node: Arc::new(node), focused, action };
        let bytes = self.next_frame.accessibility_text_bytes + entry.text_bytes();
        if let Some(error) =
            accessibility_budget_error(self.next_frame.accessibility_nodes.len() + 1, bytes)
        {
            self.next_frame.accessibility_error = Some(error);
            return;
        }
        self.next_frame.accessibility_text_bytes = bytes;
        self.next_frame.accessibility_nodes.push(entry);
    }

    /// A projection failure for the host to display. Native accessibility also
    /// exposes this error as an alert instead of an incomplete control tree.
    pub fn accessibility_error(&self) -> Option<&'static str> {
        self.rendered_frame.accessibility_error
    }

    /// Inspect the completed semantic frame in renderer integration tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn accessibility_nodes_for_test(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.rendered_frame.accessibility_nodes.iter().map(|entry| (entry.id, entry.node.as_ref()))
    }

    pub(crate) fn dispatch_accessibility_action(&mut self, request: ActionRequest, cx: &mut App) {
        let _timing = playbit_trace::Span::new("gpui.accessibility.action");
        if self.rendered_frame.accessibility_error.is_some()
            || request.target_tree != accesskit::TreeId::ROOT
        {
            return;
        }
        let action = self
            .rendered_frame
            .accessibility_nodes
            .iter()
            .find(|entry| entry.id == request.target_node && !entry.node.is_disabled())
            .and_then(|entry| entry.action.clone());
        if let Some(action) = action {
            action(&request, self, cx);
        }
    }
}

pub(crate) fn accessibility_budget_error(nodes: usize, bytes: usize) -> Option<&'static str> {
    // Reserve the window root and its explicit failure alert.
    if nodes + 2 > MAX_ACCESSIBILITY_NODES {
        Some(
            "This app exceeds the 1,024-node accessibility limit. Its accessible controls are unavailable.",
        )
    } else if bytes > MAX_ACCESSIBILITY_TEXT_BYTES {
        Some(
            "This app exceeds the 1 MiB accessibility text limit. Its accessible controls are unavailable.",
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DrawPhase, TestAppContext};
    use std::cell::Cell;

    #[crate::test]
    fn playbit_oversized_projection_fails_without_retaining_the_excess(cx: &mut TestAppContext) {
        cx.add_empty_window().update(|window, _| {
            window.invalidator.set_phase(DrawPhase::Prepaint);
            let mut node = Node::new(accesskit::Role::Label);
            node.set_value("a".repeat(MAX_ACCESSIBILITY_TEXT_BYTES));
            window.insert_accessibility_node(NodeId(2), node, false, None);
            let mut excess = Node::new(accesskit::Role::Label);
            excess.set_value("b");
            window.insert_accessibility_node(NodeId(4), excess, false, None);
            assert!(window.next_frame.accessibility_error.unwrap().contains("1 MiB"));
            assert_eq!(window.next_frame.accessibility_nodes.len(), 1);
            assert_eq!(window.next_frame.accessibility_text_bytes, MAX_ACCESSIBILITY_TEXT_BYTES);
            window.next_frame.clear();
            window.insert_accessibility_node(
                NodeId(2),
                Node::new(accesskit::Role::Button),
                false,
                None,
            );
            assert!(window.next_frame.accessibility_error.is_none());
            window.next_frame.clear();
            window.invalidator.set_phase(DrawPhase::None);
        });
    }

    #[crate::test]
    fn playbit_retired_disabled_and_failed_frame_actions_do_not_run(cx: &mut TestAppContext) {
        cx.add_empty_window().update(|window, cx| {
            let called = Rc::new(Cell::new(0));
            let counter = called.clone();
            window.rendered_frame.accessibility_nodes.push(AccessibilityNode {
                id: NodeId(2),
                node: Arc::new(Node::new(accesskit::Role::Button)),
                focused: false,
                action: Some(Rc::new(move |_, _, _| counter.set(counter.get() + 1))),
            });
            let request = || ActionRequest {
                action: accesskit::Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: NodeId(2),
                data: None,
            };
            window.dispatch_accessibility_action(request(), cx);
            assert_eq!(called.get(), 1);
            Arc::make_mut(&mut window.rendered_frame.accessibility_nodes[0].node).set_disabled();
            window.dispatch_accessibility_action(request(), cx);
            assert_eq!(called.get(), 1);
            Arc::make_mut(&mut window.rendered_frame.accessibility_nodes[0].node).clear_disabled();
            window.rendered_frame.accessibility_error = Some("Unsupported projection");
            window.dispatch_accessibility_action(request(), cx);
            assert_eq!(called.get(), 1);
            window.rendered_frame.clear();
            window.dispatch_accessibility_action(request(), cx);
            assert_eq!(called.get(), 1);
        });
    }
}
