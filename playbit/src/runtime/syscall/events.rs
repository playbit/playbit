//! Bounded app input, event delivery and wake/update notifications.
use super::{
    Event, EventKind, HostError, HostResult, MAX_QUEUED_EVENT_BYTES, MAX_QUEUED_EVENTS, State, abi,
};
use crate::runtime::{MenuEntry, ViewNode};
use std::time::Instant;

fn editable_field_accepts(disabled: bool, read_only: bool, kind: EventKind) -> bool {
    !disabled && !read_only && matches!(kind, EventKind::Submit | EventKind::TextChanged)
}

fn segment_input_accepts(
    disabled: bool,
    segment_disabled: bool,
    id: u32,
    target: u32,
    kind: EventKind,
) -> bool {
    !disabled && !segment_disabled && id == target && kind == EventKind::Click
}

pub(crate) fn menu_contains(items: &[MenuEntry], id: u32) -> bool {
    items.iter().any(|item| match item {
        MenuEntry::Action { id: item, disabled, .. } => *item == id && !disabled,
        MenuEntry::Submenu { items, disabled, .. } => !disabled && menu_contains(items, id),
        MenuEntry::Separator => false,
    })
}
fn contains(view: &ViewNode, event: &Event) -> bool {
    match view {
        ViewNode::Theme { child, .. } | ViewNode::Scroll { child, .. } => contains(child, event),
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            children.iter().any(|child| contains(child, event))
        }
        ViewNode::ContextMenu { child, items } => {
            contains(child, event)
                || (event.kind == EventKind::Click && menu_contains(items, event.target))
        }
        ViewNode::PopupButton { items, disabled, .. } => {
            !disabled && event.kind == EventKind::Click && menu_contains(items, event.target)
        }
        ViewNode::SegmentedControl { segments, disabled, .. } => segments.iter().any(|segment| {
            segment_input_accepts(*disabled, segment.disabled, segment.id, event.target, event.kind)
        }),
        ViewNode::Button { id, disabled, .. }
        | ViewNode::Checkbox { id, disabled, .. }
        | ViewNode::Radio { id, disabled, .. }
        | ViewNode::ListRow { id, disabled, .. } => {
            !disabled && *id == event.target && event.kind == EventKind::Click
        }
        ViewNode::TextField { id, disabled, read_only, .. } => {
            *id == event.target && editable_field_accepts(*disabled, *read_only, event.kind)
        }
        ViewNode::AutocompleteField { id, disabled, .. } => {
            !disabled
                && *id == event.target
                && matches!(event.kind, EventKind::Submit | EventKind::TextChanged)
        }
        ViewNode::Canvas { id, width, height, frames, .. } => {
            *id != 0
                && *id == event.target
                && match event.kind {
                    EventKind::Pointer => abi::PointerInput::decode(&event.data)
                        .is_ok_and(|input| input.x <= *width && input.y <= *height),
                    EventKind::Key => true,
                    EventKind::Frame => *frames == playbit_api::ui::CanvasFrames::Continuous,
                    _ => false,
                }
        }
        ViewNode::FileImage { .. } | ViewNode::JpegImage { .. } | ViewNode::Text { .. } => false,
    }
}

/// Host-originated UI input must address an enabled control in the current
/// semantic scene. System events use their dedicated host lifecycle methods.
pub fn validate_input(view: &crate::runtime::ViewNode, event: &Event) -> HostResult<()> {
    if event.data.len() > abi::MAX_EVENT_BYTES
        || (event.kind == EventKind::Submit
            && event.data.len() > playbit_api::abi::text::MAX_TEXT_BYTES)
    {
        return Err(HostError::LimitExceeded);
    }
    if event.kind == EventKind::SurfaceChanged {
        if event.target != 0 {
            return Err(HostError::InvalidArgument);
        }
        return playbit_api::abi::environment::Surface::decode(&event.data).map(|_| ());
    }
    match event.kind {
        EventKind::Click if event.data.is_empty() => {}
        EventKind::TextChanged => {
            playbit_api::abi::text::TextInput::decode(&event.data)?;
        }
        EventKind::Submit if std::str::from_utf8(&event.data).is_ok() => {}
        EventKind::Pointer if abi::PointerInput::decode(&event.data).is_ok() => {}
        EventKind::Key if abi::KeyInput::decode(&event.data).is_ok() => {}
        EventKind::Frame if abi::FrameInput::decode(&event.data).is_ok() => {}
        _ => return Err(HostError::InvalidArgument),
    }
    if contains(view, event) { Ok(()) } else { Err(HostError::InvalidArgument) }
}

pub(in crate::runtime) fn push_event(state: &mut State, event: Event) -> HostResult<()> {
    state.reap_work();
    state.work.admit_root()?;
    push_work_event(state, event, None)
}

pub(in crate::runtime) fn push_work_event(
    state: &mut State,
    event: Event,
    root: Option<u64>,
) -> HostResult<()> {
    assert!(state.events.len() <= MAX_QUEUED_EVENTS);
    assert!(state.queued_bytes <= MAX_QUEUED_EVENT_BYTES);
    let before_count = state.events.len();
    let before_bytes = state.queued_bytes;
    let event_bytes = 20 + event.data.len();
    if event.data.len() > abi::MAX_EVENT_BYTES
        || state.events.len() == MAX_QUEUED_EVENTS
        || state.queued_bytes + event.data.len() + 20 > MAX_QUEUED_EVENT_BYTES
    {
        return Err(HostError::LimitExceeded);
    }
    let root = match root {
        Some(root) => {
            assert!(state.work.remaining.contains_key(&root), "completion root remains accepted");
            root
        }
        None => state.work.new_root()?,
    };
    state.queued_bytes += event.data.len() + 20;
    state.events.push_back(crate::runtime::instance::QueuedEvent::new(event, root));
    assert_eq!(state.events.len(), before_count + 1);
    assert_eq!(state.queued_bytes, before_bytes + event_bytes);
    assert!(state.events.len() <= MAX_QUEUED_EVENTS);
    assert!(state.queued_bytes <= MAX_QUEUED_EVENT_BYTES);
    Ok(())
}

pub(in crate::runtime) fn queue_due_wake(state: &mut State, now: Instant) {
    if state.work.draining {
        return;
    }
    if state.wake_deadline.is_none_or(|deadline| deadline > now) {
        return;
    }
    let _timing = crate::trace::Span::new("guest.wake");
    if push_event(state, Event { kind: EventKind::Wake, target: 0, data: Vec::new() }).is_ok() {
        state.wake_deadline = None;
    }
    // A full event queue keeps the deadline. The host drains a turn and retries
    // on its next due-work check; a wake never displaces another event.
}

pub(in crate::runtime) fn peek_event(state: &State) -> HostResult<&Event> {
    super::require_supported(state, super::Opcode::EventPoll)?;
    if state.trace_config_pending {
        return Ok(&state.trace_config);
    }
    state.events.front().map(|queued| &queued.event).ok_or(HostError::WouldBlock)
}

/// The trusted adapter admits all output before consuming the event. The
/// exclusive instance owner prevents a producer from replacing the peeked item.
pub(in crate::runtime) fn take_event(state: &mut State) -> HostResult<Event> {
    if state.trace_config_pending {
        state.trace_config_pending = false;
        return Ok(state.trace_config.clone());
    }
    let front = peek_event(state)?;
    if front.kind == EventKind::ChannelMessage {
        if front.data.len() < 104 {
            return Err(HostError::Internal);
        }
        let account = front.data[8..24].try_into().expect("channel account");
        let sender = front.data[40..56].try_into().expect("channel instance");
        let message = front.data[88..104].try_into().expect("channel message");
        state.host.channel_consumed(
            state.context.identity.instance_id,
            account,
            sender,
            message,
        )?;
    }
    let event_bytes = 20 + peek_event(state)?.data.len();
    let count = state.events.len();
    let bytes = state.queued_bytes;
    assert!(bytes <= MAX_QUEUED_EVENT_BYTES);
    assert!(bytes >= event_bytes, "the front event is included in queue accounting");
    let event = state.events.pop_front().expect("the event was checked before removing it");
    state.work.active = Some(event.root);
    state.work.progress = true;
    state.reap_work();
    state.queued_bytes -= event_bytes;
    assert_eq!(state.events.len(), count - 1);
    assert_eq!(state.queued_bytes, bytes - event_bytes);
    let _timing = crate::trace::Span::new("guest.event.consume");
    super::control::queue_volume_change(state);
    state.queue_name_changes();
    Ok(event.event)
}
