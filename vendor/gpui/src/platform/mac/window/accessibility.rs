//! Main-thread AppKit projection. Native peers never own the window state.
use super::MacWindowState;
use crate::{AccessibilityNode, ForegroundExecutor};
use accesskit::{
    ActionHandler, ActionRequest, ActivationHandler, Node, NodeId, Role, TreeId, TreeInfo,
    TreeUpdate,
};
use accesskit_macos::SubclassingAdapter;
use parking_lot::Mutex;
use std::{
    cell::RefCell,
    collections::HashSet,
    ffi::c_void,
    rc::Rc,
    sync::{Arc, Weak},
};

#[derive(Clone, Default)]
struct Projection {
    nodes: Vec<(NodeId, Arc<Node>)>,
    focus: Option<NodeId>,
}
impl Projection {
    fn from_frame(entries: &[AccessibilityNode], scale_factor: f32) -> Self {
        let children: HashSet<_> =
            entries.iter().flat_map(|e| e.node.children().iter().copied()).collect();
        let mut root = Node::new(Role::Window);
        // GPUI bounds and text metrics are logical pixels. AccessKit's AppKit
        // adapter consumes physical pixels, then converts them back to points.
        // Scale once at the root so descendants and text hit testing agree.
        root.set_transform(accesskit::Affine::scale(f64::from(scale_factor)));
        root.set_children(
            entries.iter().filter(|e| !children.contains(&e.id)).map(|e| e.id).collect::<Vec<_>>(),
        );
        let mut nodes = Vec::with_capacity(entries.len() + 1);
        nodes.push((NodeId(0), Arc::new(root)));
        nodes.extend(entries.iter().map(|e| (e.id, e.node.clone())));
        nodes.sort_unstable_by_key(|(id, _)| *id);
        assert!(nodes.windows(2).all(|n| n[0].0 != n[1].0), "duplicate accessibility identity");
        Self { nodes, focus: entries.iter().find(|e| e.focused).map(|e| e.id) }
    }

    fn update(&self, previous: &Self) -> TreeUpdate {
        TreeUpdate {
            nodes: self
                .nodes
                .iter()
                .filter(|(id, node)| {
                    previous
                        .nodes
                        .binary_search_by_key(id, |(id, _)| *id)
                        .map_or(true, |i| previous.nodes[i].1 != *node)
                })
                .map(|(id, node)| (*id, (**node).clone()))
                .collect(),
            tree: previous.nodes.is_empty().then(|| TreeInfo::new(NodeId(0))),
            tree_id: TreeId::ROOT,
            focus: self.focus.unwrap_or(NodeId(0)),
        }
    }
}

#[derive(Default)]
struct Snapshot {
    current: Projection,
    published: Projection,
}
struct Activate(Rc<RefCell<Snapshot>>);
impl ActivationHandler for Activate {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        let mut snapshot = self.0.borrow_mut();
        if snapshot.current.nodes.is_empty() {
            return None;
        }
        let tree = snapshot.current.update(&Projection::default());
        snapshot.published = snapshot.current.clone();
        Some(tree)
    }
}
struct Act(Weak<Mutex<MacWindowState>>);
impl ActionHandler for Act {
    fn do_action(&mut self, request: ActionRequest) {
        let Some(state) = self.0.upgrade() else {
            return;
        };
        // AppKit may reenter while actions update focus. No window lock crosses
        // the callback; the GPUI frame checks that this target is still alive.
        let callback = state.lock().accessibility_callback.take();
        if let Some(mut callback) = callback {
            callback(request);
            state.lock().accessibility_callback = Some(callback);
        }
    }
}

pub(super) struct Accessibility {
    adapter: SubclassingAdapter,
    snapshot: Rc<RefCell<Snapshot>>,
    scheduled: bool,
    focused: bool,
}
impl Accessibility {
    pub(super) unsafe fn new(
        view: *mut c_void,
        state: Weak<Mutex<MacWindowState>>,
    ) -> Rc<RefCell<Self>> {
        let snapshot = Rc::new(RefCell::new(Snapshot::default()));
        // Install before showing/focusing the view. Its retained NSView is owned
        // here, outside MacWindowState, to avoid the view's state-ivar cycle.
        let adapter =
            unsafe { SubclassingAdapter::new(view, Activate(snapshot.clone()), Act(state)) };
        Rc::new(RefCell::new(Self { adapter, snapshot, scheduled: false, focused: false }))
    }

    pub(super) fn update(
        this: &Rc<RefCell<Self>>,
        entries: &[AccessibilityNode],
        error: Option<&'static str>,
        focused: bool,
        scale_factor: f32,
        executor: &ForegroundExecutor,
    ) {
        let _span = playbit_trace::Span::new("gpui.accessibility.project");
        let mut bridge = this.borrow_mut();
        bridge.snapshot.borrow_mut().current = if let Some(error) = error {
            let mut alert = Node::new(Role::Alert);
            alert.set_label(error);
            Projection::from_frame(
                &[AccessibilityNode {
                    id: NodeId(1),
                    node: Arc::new(alert),
                    focused: false,
                    action: None,
                }],
                scale_factor,
            )
        } else {
            Projection::from_frame(entries, scale_factor)
        };
        bridge.focused = focused;
        if bridge.scheduled {
            return;
        }
        bridge.scheduled = true;
        let weak = Rc::downgrade(this);
        let delivery = playbit_trace::Interval::new("gpui.accessibility.delivery");
        // One coalesced completed-frame publication per window. Native events
        // run after the App render borrow, with no adapter/window borrow held.
        executor
            .spawn(async move {
                drop(delivery);
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let _span = playbit_trace::Span::new("gpui.accessibility.publish");
                let (events, focus_events) = {
                    let mut bridge = this.borrow_mut();
                    bridge.scheduled = false;
                    let snapshot = bridge.snapshot.clone();
                    let events = {
                        let _span = playbit_trace::Span::new("gpui.accessibility.adapter");
                        bridge.adapter.update_if_active(|| {
                            let _span = playbit_trace::Span::new("gpui.accessibility.diff");
                            let mut snapshot = snapshot.borrow_mut();
                            let update = snapshot.current.update(&snapshot.published);
                            snapshot.published = snapshot.current.clone();
                            update
                        })
                    };
                    let focused = bridge.focused;
                    let focus_events = {
                        let _span = playbit_trace::Span::new("gpui.accessibility.focus");
                        bridge.adapter.update_view_focus_state(focused)
                    };
                    (events, focus_events)
                };
                // Includes native-peer lookup and AppKit notification delivery.
                // Nested durations are not additive; diff is inside adapter.
                let _span = playbit_trace::Span::new("gpui.accessibility.raise");
                if let Some(events) = events {
                    events.raise();
                }
                if let Some(events) = focus_events {
                    events.raise();
                }
            })
            .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: u64, label: &str, focused: bool) -> AccessibilityNode {
        let mut node = Node::new(Role::Button);
        node.set_label(label);
        AccessibilityNode { id: NodeId(id), node: Arc::new(node), focused, action: None }
    }

    #[test]
    fn playbit_projection_preserves_identity_and_removes_retired_nodes() {
        let before = Projection::from_frame(&[entry(2, "A", true), entry(4, "B", false)], 1.);
        let after = Projection::from_frame(&[entry(4, "B", true), entry(6, "C", false)], 1.);
        let update = after.update(&before);
        assert_eq!(update.focus, NodeId(4));
        assert_eq!(update.nodes.iter().map(|(id, _)| id.0).collect::<Vec<_>>(), [0, 6]);
        assert_eq!(update.nodes[0].1.children(), [NodeId(4), NodeId(6)]);
        assert!(update.tree.is_none());
        assert!(after.update(&after).nodes.is_empty());
    }

    #[test]
    fn playbit_text_runs_have_one_parent_and_survive_activation_before_first_frame() {
        let snapshot = Rc::new(RefCell::new(Snapshot::default()));
        let mut activate = Activate(snapshot.clone());
        assert!(activate.request_initial_tree().is_none());
        let mut field = entry(2, "Task", true);
        Arc::make_mut(&mut field.node).set_children(vec![NodeId(3)]);
        snapshot.borrow_mut().current =
            Projection::from_frame(&[field, entry(3, "Text", false)], 1.);
        let tree = activate.request_initial_tree().unwrap();
        assert_eq!(tree.nodes[0].1.children(), [NodeId(2)]);
        assert_eq!(tree.nodes.len(), 3);
        assert_eq!(tree.focus, NodeId(2));
        assert!(tree.tree.is_some());
        let snapshot = snapshot.borrow();
        assert!(snapshot.current.update(&snapshot.published).nodes.is_empty());
    }

    #[test]
    fn playbit_diff_includes_disabled_and_mixed_changes() {
        let mut control = entry(2, "Task", false);
        let node = Arc::make_mut(&mut control.node);
        node.set_role(Role::CheckBox);
        node.set_toggled(accesskit::Toggled::False);
        let before = Projection::from_frame(&[control.clone()], 1.);
        let node = Arc::make_mut(&mut control.node);
        node.set_toggled(accesskit::Toggled::Mixed);
        node.set_disabled();
        let after = Projection::from_frame(&[control], 1.);
        let update = after.update(&before);
        assert_eq!(update.nodes.len(), 1);
        assert_eq!(update.nodes[0].0, NodeId(2));
        assert!(update.nodes[0].1.is_disabled());
        assert_eq!(update.nodes[0].1.toggled(), Some(accesskit::Toggled::Mixed));
    }

    #[test]
    fn playbit_backing_scale_updates_control_and_nested_text_geometry() {
        use accesskit::{Affine, Point, Rect};
        use accesskit_consumer::{NodeRef, Tree, TreeChangeHandler};
        struct Changes;
        impl TreeChangeHandler for Changes {
            fn node_added(&mut self, _: &NodeRef) {}
            fn node_updated(&mut self, _: &NodeRef, _: &NodeRef) {}
            fn focus_moved(&mut self, _: Option<&NodeRef>, _: Option<&NodeRef>) {}
            fn node_removed(&mut self, _: &NodeRef) {}
        }
        let mut button = entry(2, "Disabled", false);
        let node = Arc::make_mut(&mut button.node);
        node.set_disabled();
        node.set_bounds(Rect::new(20., 390., 80., 418.));
        let mut field = entry(3, "Field", true);
        let node = Arc::make_mut(&mut field.node);
        node.set_role(Role::TextInput);
        node.set_bounds(Rect::new(20., -12., 180., 16.));
        node.set_children(vec![NodeId(4)]);
        let mut text = entry(4, "", false);
        let node = Arc::make_mut(&mut text.node);
        node.set_role(Role::TextRun);
        node.set_value("é");
        node.set_bounds(Rect::new(24., -8., 32., 12.));
        let entries = [button, field, text];
        let mut previous = Projection::from_frame(&entries, 1.);
        let mut tree = Tree::new(previous.update(&Projection::default()), true);
        for scale in [2., 1.] {
            let next = Projection::from_frame(&entries, scale as f32);
            let update = next.update(&previous);
            assert_eq!(update.nodes.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [NodeId(0)]);
            tree.update_and_process_changes(update, &mut Changes);
            let state = tree.state();
            for entry in &entries {
                let node = state.node_by_tree_local_id(entry.id, TreeId::ROOT).unwrap();
                assert_eq!(
                    node.bounding_box(),
                    Some(Affine::scale(scale).transform_rect_bbox(entry.node.bounds().unwrap()))
                );
            }
            let button = state.node_by_tree_local_id(NodeId(2), TreeId::ROOT).unwrap();
            assert!(button.is_disabled());
            let text = state.node_by_tree_local_id(NodeId(4), TreeId::ROOT).unwrap();
            // The adapter converts a physical selection point into text-local
            // coordinates using this inverse, including after scrolling.
            assert_eq!(
                text.transform().inverse() * Point::new(28. * scale, -4. * scale),
                Point::new(28., -4.)
            );
            assert_eq!(state.focus_in_tree().locate().0, NodeId(3));
            previous = next;
        }
    }
}
