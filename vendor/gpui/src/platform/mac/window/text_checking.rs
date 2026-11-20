//! AppKit owns checking and suggestion UI; the focused editor owns text and undo.
//! A separate client object for each editor prevents retired native callbacks from
//! reaching a new responder. All AppKit calls run on the foreground executor,
//! outside the window mutex and editor borrows.

mod suggestions;

use super::*;
use crate::TextCheckingState;
use objc::rc::StrongPtr;
use std::cell::RefCell;
use std::sync::OnceLock;

const CLIENT_STATE_IVAR: &str = "textCheckingState";
type Geometry = (Bounds<Pixels>, [f64; 4]);

struct Client {
    window: Weak<Mutex<MacWindowState>>,
    active: Cell<bool>,
    expected: RefCell<TextCheckingState>,
    annotated: RefCell<Option<AnnotatedText>>,
    suggestions: RefCell<suggestions::State>,
}

struct AnnotatedText {
    range: Range<usize>,
    text: String,
    string: StrongPtr,
}

pub(super) struct Session {
    client: StrongPtr,
    controller: StrongPtr,
    state: TextCheckingState,
    geometry: Option<Geometry>,
    pending_consider: Option<Range<usize>>,
}

impl Session {
    fn new(window: &Arc<Mutex<MacWindowState>>, state: TextCheckingState) -> Self {
        // AppKit does not retain the client. Retain both objects until invalidate
        // has disconnected asynchronous checking from this editor.
        unsafe {
            let object: id = msg_send![client_class(), new];
            let client = Box::new(Client {
                window: Arc::downgrade(window),
                active: Cell::new(true),
                expected: RefCell::new(state.clone()),
                annotated: RefCell::new(None),
                suggestions: RefCell::new(suggestions::State::default()),
            });
            (*object).set_ivar(CLIENT_STATE_IVAR, Box::into_raw(client).cast::<c_void>());
            let controller: id = msg_send![class!(NSTextCheckingController), alloc];
            let controller: id = msg_send![controller, initWithClient: object];
            Self {
                client: StrongPtr::new(object),
                controller: StrongPtr::new(controller),
                state,
                geometry: None,
                pending_consider: None,
            }
        }
    }

    fn prepare(
        &mut self,
        next: TextCheckingState,
        initial: bool,
        geometry: Option<Geometry>,
        after_layout: bool,
    ) -> Notifications {
        let old = &self.state;
        let committed_composition = old.marked_range.is_some() && next.marked_range.is_none();
        let text_changed = old.revision != next.revision
            || old.model_generation != next.model_generation
            || committed_composition
            || (initial && next.inserted_range.is_some());
        let selection_changed =
            initial || old.selection != next.selection || old.marked_range != next.marked_range;
        let client = client(unsafe { &**self.client });
        *client.expected.borrow_mut() = next.clone();
        if old.model_generation != next.model_generation {
            client.annotated.borrow_mut().take();
        }
        let changed =
            if text_changed { refresh_text(unsafe { &**self.client }, None) } else { None };
        suggestions::observe(unsafe { &**self.client }, &next, changed.as_ref());
        if text_changed || next.marked_range.is_some() {
            self.pending_consider =
                next.inserted_range.clone().filter(|_| next.marked_range.is_none());
        }
        let consider = if after_layout { self.pending_consider.take() } else { None };
        self.state = next.clone();
        let geometry_changed = !initial
            && after_layout
            && !text_changed
            && !selection_changed
            && self.geometry.is_some()
            && self.geometry != geometry;
        self.geometry = geometry;
        Notifications {
            client: self.client.clone(),
            controller: self.controller.clone(),
            state: next,
            changed,
            selection_changed,
            text_changed,
            geometry_changed,
            consider,
            after_layout,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        client(unsafe { &**self.client }).active.set(false);
        suggestions::dismiss(unsafe { &**self.client }, None);
        unsafe {
            let _: () = msg_send![*self.controller, invalidate];
        }
    }
}

struct Notifications {
    client: StrongPtr,
    controller: StrongPtr,
    state: TextCheckingState,
    changed: Option<Range<usize>>,
    selection_changed: bool,
    text_changed: bool,
    geometry_changed: bool,
    consider: Option<Range<usize>>,
    after_layout: bool,
}

impl Notifications {
    fn current(&self) -> bool {
        let object = unsafe { &**self.client };
        *client(object).expected.borrow() == self.state && with_handler(object, |_| ()).is_some()
    }

    fn send(self) {
        let object = unsafe { &**self.client };
        if !self.current() {
            return;
        }
        if self.text_changed || self.selection_changed {
            suggestions::dismiss(object, None);
        }
        unsafe {
            if self.state.marked_range.is_some() || self.geometry_changed {
                suggestions::dismiss(object, None);
                if self.state.marked_range.is_some() {
                    return;
                }
            }
            if let Some(range) = &self.changed {
                let _: () =
                    msg_send![*self.controller, didChangeTextInRange: NSRange::from(range.clone())];
            }
            if self.text_changed && self.current() {
                if let Some(range) = &self.state.inserted_range {
                    let _: () = msg_send![*self.controller, insertedTextInRange: NSRange::from(range.clone())];
                }
            }
            if self.selection_changed && self.current() {
                let _: () = msg_send![*self.controller, didChangeSelectedRange];
            }
            if let Some(range) = &self.consider
                && self.current()
            {
                // Smart punctuation needs consideration as well as change
                // notifications. Replacement/spelling traits are disabled on
                // this controller; their proposals are handled below.
                let _: () = msg_send![*self.controller, considerTextCheckingForRange: NSRange::from(range.clone())];
            }
            if self.current() {
                if self.consider.is_some() {
                    suggestions::request(object);
                } else if self.after_layout && self.selection_changed && !self.text_changed {
                    suggestions::show_reversion(object);
                }
            }
        }
    }
}

/// Coalesce frame/edit notifications and let GPUI finish restoring its handler
/// before AppKit can synchronously reenter the editor.
pub(super) fn schedule(window: &Arc<Mutex<MacWindowState>>) {
    let executor = {
        let mut state = window.lock();
        if state.text_checking_sync_pending {
            return;
        }
        state.text_checking_sync_pending = true;
        state.executor.clone()
    };
    let window = Arc::downgrade(window);
    executor
        .spawn(async move {
            if let Some(window) = window.upgrade() {
                window.lock().text_checking_sync_pending = false;
                synchronize(&window, true);
            }
        })
        .detach();
}

/// NSTextInputContext accepts a pending correction after returning from
/// insertText. Bring its client snapshot current before that callback arrives.
/// Suggestion presentation waits for the next completed field layout.
pub(super) fn did_edit(window: &Arc<Mutex<MacWindowState>>) {
    synchronize(window, false);
}

/// Accept a visible native proposal before GPUI submits or inserts a delimiter.
pub(super) fn before_key(window: &Arc<Mutex<MacWindowState>>, key: &Keystroke) -> bool {
    suggestions::before_key(window, key)
}

fn synchronize(window: &Arc<Mutex<MacWindowState>>, after_layout: bool) {
    super::input_policy::synchronize(window);
    let (mut handler, mut session, active, frame) = {
        let mut state = window.lock();
        let active = unsafe { state.native_window.isKeyWindow() == YES };
        let frame = unsafe { NSWindow::frame(state.native_window) };
        (
            state.input_handler.take(),
            state.text_checking.take(),
            active,
            [frame.origin.x, frame.origin.y, frame.size.width, frame.size.height],
        )
    };
    let snapshot = active.then(|| handler.as_mut()?.text_checking_state()).flatten();
    let geometry = after_layout
        .then(|| {
            snapshot.as_ref().and_then(|state| {
                let caret = state.selection.end;
                handler
                    .as_mut()?
                    .bounds_for_range_with_actual(caret..caret)
                    .map(|(bounds, _)| (bounds, frame))
            })
        })
        .flatten();
    window.lock().input_handler = handler;
    let Some(snapshot) = snapshot else {
        drop(session);
        return;
    };
    if session.as_ref().is_some_and(|session| session.state.editor_id != snapshot.editor_id) {
        session.take();
    }
    // Do not initialize the spelling service on startup, focus, or a key's
    // insertion path. The first committed edit must be laid out first.
    if session.is_none()
        && (!after_layout || snapshot.inserted_range.is_none() || snapshot.marked_range.is_some())
    {
        return;
    }
    let initial = session.is_none();
    let mut session = session.unwrap_or_else(|| Session::new(window, snapshot.clone()));
    let notifications = session.prepare(snapshot, initial, geometry, after_layout);
    // Native suggestion presentation can run a nested event loop. Publish the
    // session before any controller call so nested frames reuse its identity.
    let displaced = window.lock().text_checking.replace(session);
    drop(displaced);
    notifications.send();
}

fn client(object: &Object) -> &Client {
    // The Objective-C object owns this allocation from init through dealloc.
    unsafe { &*(*object.get_ivar::<*mut c_void>(CLIENT_STATE_IVAR)).cast::<Client>() }
}

fn with_handler<R>(object: &Object, f: impl FnOnce(&mut PlatformInputHandler) -> R) -> Option<R> {
    let client = client(object);
    if !client.active.get() {
        return None;
    }
    let window = client.window.upgrade()?;
    let mut handler = {
        let mut state = window.lock();
        if unsafe { state.native_window.isKeyWindow() } != YES {
            return None;
        }
        state.input_handler.take()?
    };
    let state = handler.text_checking_state();
    let result = if state.as_ref() == Some(&*client.expected.borrow()) {
        Some(f(&mut handler))
    } else {
        None
    };
    window.lock().input_handler = Some(handler);
    result
}

fn refresh_text(object: &Object, range: Option<Range<usize>>) -> Option<Range<usize>> {
    let (text, range) = with_handler(object, |handler| handler.text_checking_text(range))??;
    let mut cache = client(object).annotated.borrow_mut();
    if let Some(old) = cache.as_mut() {
        if old.range == range && old.text == text {
            return Some(range);
        }
        if old.range.start == range.start {
            let (replaced, replacement) = changed_text(&old.text, &text);
            unsafe {
                let _: () = msg_send![*old.string,
                    replaceCharactersInRange: NSRange::from(replaced.clone())
                    withString: ns_string(replacement)
                ];
            }
            let changed = range.start + replaced.start
                ..range.start + replaced.start + replacement.encode_utf16().count();
            old.text = text;
            old.range = range;
            return Some(changed);
        }
    }
    let string = unsafe {
        let string: id = msg_send![class!(NSMutableAttributedString), alloc];
        StrongPtr::new(msg_send![string, initWithString: ns_string(&text)])
    };
    *cache = Some(AnnotatedText { range: range.clone(), text, string });
    Some(range)
}

fn changed_text<'a>(old: &str, new: &'a str) -> (Range<usize>, &'a str) {
    let prefix = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    let suffix = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    let start = old[..prefix].encode_utf16().count();
    (start..old[..old.len() - suffix].encode_utf16().count(), &new[prefix..new.len() - suffix])
}

/// AppKit permits null output pointers, including for adjusted ranges.
pub(super) fn write_range(output: *mut c_void, range: Range<usize>) {
    if !output.is_null() {
        // These pointers come from AppKit's NSTextInput/CheckingClient calls.
        unsafe {
            output.cast::<NSRange>().write(NSRange::from(range));
        }
    }
}

fn adjusted_range(range: NSRange, scope: Range<usize>) -> Option<Range<usize>> {
    if !range.is_valid() {
        return Some(scope);
    }
    let start = usize::try_from(range.location).ok()?;
    let end = start.checked_add(usize::try_from(range.length).ok()?)?;
    (start <= scope.end && end >= scope.start).then(|| start.max(scope.start)..end.min(scope.end))
}

fn native_view(object: &Object) -> Option<id> {
    with_handler(object, |_| ())?;
    let window = client(object).window.upgrade()?;
    Some(window.lock().native_view.as_ptr())
}

fn client_class() -> &'static Class {
    static CLASS: OnceLock<&'static Class> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut decl = ClassDecl::new("GPUITextCheckingClient", class!(NSObject)).unwrap();
        decl.add_ivar::<*mut c_void>(CLIENT_STATE_IVAR);
        unsafe {
            decl.add_protocol(Protocol::get("NSTextCheckingClient").unwrap());
            decl.add_method(sel!(dealloc), dealloc as extern "C" fn(&Object, Sel));
            decl.add_method(
                sel!(annotatedSubstringForProposedRange:actualRange:),
                annotated_substring as extern "C" fn(&Object, Sel, NSRange, *mut c_void) -> id,
            );
            decl.add_method(
                sel!(setAnnotations:range:),
                set_annotations as extern "C" fn(&Object, Sel, id, NSRange),
            );
            decl.add_method(
                sel!(addAnnotations:range:),
                add_annotations as extern "C" fn(&Object, Sel, id, NSRange),
            );
            decl.add_method(
                sel!(removeAnnotation:range:),
                remove_annotation as extern "C" fn(&Object, Sel, id, NSRange),
            );
            decl.add_method(
                sel!(replaceCharactersInRange:withAnnotatedString:),
                replace_annotated as extern "C" fn(&Object, Sel, NSRange, id),
            );
            decl.add_method(
                sel!(selectAndShowRange:),
                select_range as extern "C" fn(&Object, Sel, NSRange),
            );
            decl.add_method(
                sel!(viewForRange:firstRect:actualRange:),
                view_for_range
                    as extern "C" fn(&Object, Sel, NSRange, *mut c_void, *mut c_void) -> id,
            );
            decl.add_method(
                sel!(candidateListTouchBarItem),
                nil_object as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(
                sel!(validAttributesForMarkedText),
                valid_attributes as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(sel!(hasMarkedText), has_marked as extern "C" fn(&Object, Sel) -> BOOL);
            decl.add_method(sel!(markedRange), marked as extern "C" fn(&Object, Sel) -> NSRange);
            decl.add_method(
                sel!(selectedRange),
                selected as extern "C" fn(&Object, Sel) -> NSRange,
            );
            decl.add_method(
                sel!(insertText:replacementRange:),
                insert as extern "C" fn(&Object, Sel, id, NSRange),
            );
            decl.add_method(
                sel!(setMarkedText:selectedRange:replacementRange:),
                mark as extern "C" fn(&Object, Sel, id, NSRange, NSRange),
            );
            decl.add_method(sel!(unmarkText), unmark as extern "C" fn(&Object, Sel));
            decl.add_method(
                sel!(doCommandBySelector:),
                command as extern "C" fn(&Object, Sel, Sel),
            );
            decl.add_method(
                sel!(attributedSubstringForProposedRange:actualRange:),
                plain_substring as extern "C" fn(&Object, Sel, NSRange, *mut c_void) -> id,
            );
            decl.add_method(
                sel!(firstRectForCharacterRange:actualRange:),
                first_rect as extern "C" fn(&Object, Sel, NSRange, *mut c_void) -> NSRect,
            );
            decl.add_method(
                sel!(characterIndexForPoint:),
                character_index as extern "C" fn(&Object, Sel, NSPoint) -> u64,
            );
            for selector in [sel!(smartQuotesType), sel!(smartDashesType)] {
                decl.add_method(
                    selector,
                    default_trait as extern "C" fn(&Object, Sel) -> NSInteger,
                );
            }
            for selector in [
                sel!(spellCheckingType),
                sel!(autocorrectionType),
                sel!(textReplacementType),
                sel!(grammarCheckingType),
                sel!(smartInsertDeleteType),
                sel!(dataDetectionType),
                sel!(linkDetectionType),
                sel!(textCompletionType),
                sel!(inlinePredictionType),
                sel!(mathExpressionCompletionType),
            ] {
                decl.add_method(selector, no_trait as extern "C" fn(&Object, Sel) -> NSInteger);
            }
            decl.add_method(
                sel!(writingToolsBehavior),
                no_writing_tools as extern "C" fn(&Object, Sel) -> NSInteger,
            );
        }
        decl.register()
    })
}

extern "C" fn dealloc(object: &Object, _: Sel) {
    unsafe {
        let raw = *object.get_ivar::<*mut c_void>(CLIENT_STATE_IVAR);
        if !raw.is_null() {
            drop(Box::from_raw(raw.cast::<Client>()));
        }
        let _: () = msg_send![super(object, class!(NSObject)), dealloc];
    }
}

extern "C" fn annotated_substring(
    object: &Object,
    _: Sel,
    range: NSRange,
    actual: *mut c_void,
) -> id {
    let length = client(object).expected.borrow().document_length;
    let Some(range) = adjusted_range(range, 0..length) else { return nil };
    if refresh_text(object, Some(range)).is_none() {
        return nil;
    }
    let cache = client(object).annotated.borrow();
    let cache = cache.as_ref().unwrap();
    write_range(actual, cache.range.clone());
    unsafe {
        msg_send![*cache.string, attributedSubstringFromRange: NSRange::from(0..cache.range.len())]
    }
}

fn annotate(object: &Object, range: NSRange, f: impl FnOnce(id, NSRange)) {
    if with_handler(object, |_| ()).is_none() {
        return;
    }
    let cache = client(object).annotated.borrow();
    if let Some(cache) = cache.as_ref()
        && let Some(range) = adjusted_range(range, cache.range.clone())
        && !range.is_empty()
    {
        f(*cache.string, (range.start - cache.range.start..range.end - cache.range.start).into());
    }
}

extern "C" fn set_annotations(object: &Object, _: Sel, annotations: id, range: NSRange) {
    annotate(object, range, |string, range| unsafe {
        let _: () = msg_send![string, setAttributes: annotations range: range];
    });
}

extern "C" fn add_annotations(object: &Object, _: Sel, annotations: id, range: NSRange) {
    annotate(object, range, |string, range| unsafe {
        let _: () = msg_send![string, addAttributes: annotations range: range];
    });
}

extern "C" fn remove_annotation(object: &Object, _: Sel, key: id, range: NSRange) {
    annotate(object, range, |string, range| unsafe {
        let _: () = msg_send![string, removeAttribute: key range: range];
    });
}

extern "C" fn replace_annotated(object: &Object, _: Sel, range: NSRange, text: id) {
    let expected = client(object).expected.borrow().clone();
    let Some(range) = range.to_range() else { return };
    let accepted = with_handler(object, |handler| unsafe {
        let string: id = msg_send![text, string];
        handler.replace_text_for_checking(&expected, range.clone(), string.to_str())
    })
    .unwrap_or(false);
    if !accepted {
        return;
    }
    // A correction may be followed immediately by annotations/selection from
    // the same controller turn, before GPUI paints the changed text.
    update_expected(object);
    refresh_text(object, None);
    let length: NSUInteger = unsafe { msg_send![text, length] };
    annotate(object, (range.start..range.start + length as usize).into(), |string, local| unsafe {
        let _: () = msg_send![string, replaceCharactersInRange: local withAttributedString: text];
    });
}

fn update_expected(object: &Object) {
    if let Some(window) = client(object).window.upgrade() {
        let mut handler = window.lock().input_handler.take();
        if let Some(next) = handler.as_mut().and_then(|handler| handler.text_checking_state()) {
            if next.editor_id == client(object).expected.borrow().editor_id {
                *client(object).expected.borrow_mut() = next;
            }
        }
        window.lock().input_handler = handler;
    }
}

extern "C" fn select_range(object: &Object, _: Sel, range: NSRange) {
    let expected = client(object).expected.borrow().clone();
    let Some(range) = range.to_range() else { return };
    if with_handler(object, |handler| handler.select_text_for_checking(&expected, range))
        .unwrap_or(false)
    {
        update_expected(object);
    }
}

extern "C" fn view_for_range(
    object: &Object,
    _: Sel,
    range: NSRange,
    rect: *mut c_void,
    actual: *mut c_void,
) -> id {
    let Some(view) = native_view(object) else { return nil };
    let result = with_handler(object, |handler| {
        let range = adjusted_range(range, 0..client(object).expected.borrow().document_length)?;
        handler.bounds_for_range_with_actual(range)
    })
    .flatten();
    let Some((bounds, range)) = result else { return nil };
    write_range(actual, range);
    if !rect.is_null() {
        let height = unsafe { NSView::bounds(view).size.height };
        let native = NSRect::new(
            NSPoint::new(
                bounds.origin.x.0 as f64,
                height - bounds.origin.y.0 as f64 - bounds.size.height.0 as f64,
            ),
            NSSize::new(bounds.size.width.0 as f64, bounds.size.height.0 as f64),
        );
        unsafe {
            rect.cast::<NSRect>().write(native);
        }
    }
    view
}

extern "C" fn nil_object(_: &Object, _: Sel) -> id {
    nil
}
extern "C" fn valid_attributes(_: &Object, _: Sel) -> id {
    unsafe { NSArray::array(nil) }
}
extern "C" fn default_trait(_: &Object, _: Sel) -> NSInteger {
    0
}
extern "C" fn no_trait(_: &Object, _: Sel) -> NSInteger {
    1
}
extern "C" fn no_writing_tools(_: &Object, _: Sel) -> NSInteger {
    -1
}

extern "C" fn has_marked(object: &Object, _: Sel) -> BOOL {
    with_handler(object, |handler| handler.marked_text_range().is_some()).unwrap_or(false).to_objc()
}

extern "C" fn marked(object: &Object, _: Sel) -> NSRange {
    with_handler(object, |handler| handler.marked_text_range())
        .flatten()
        .map_or(NSRange::invalid(), Into::into)
}

extern "C" fn selected(object: &Object, _: Sel) -> NSRange {
    with_handler(object, |handler| handler.selected_text_range(false))
        .flatten()
        .map_or(NSRange::invalid(), |selection| selection.range.into())
}

extern "C" fn insert(object: &Object, _: Sel, text: id, range: NSRange) {
    let text = unsafe {
        if msg_send![text, isKindOfClass: class!(NSAttributedString)] {
            text
        } else {
            let string: id = msg_send![class!(NSAttributedString), alloc];
            let string: id = msg_send![string, initWithString: text];
            string.autorelease()
        }
    };
    let range =
        range.to_range().unwrap_or_else(|| client(object).expected.borrow().selection.clone());
    replace_annotated(
        object,
        sel!(replaceCharactersInRange:withAnnotatedString:),
        range.into(),
        text,
    );
}

extern "C" fn mark(
    object: &Object,
    selector: Sel,
    text: id,
    selected: NSRange,
    replacement: NSRange,
) {
    if let Some(view) = native_view(object) {
        super::set_marked_text(unsafe { &*view }, selector, text, selected, replacement);
        update_expected(object);
    }
}

extern "C" fn unmark(object: &Object, selector: Sel) {
    if let Some(view) = native_view(object) {
        super::unmark_text(unsafe { &*view }, selector);
        update_expected(object);
    }
}

extern "C" fn command(object: &Object, selector: Sel, action: Sel) {
    if let Some(view) = native_view(object) {
        super::do_command_by_selector(unsafe { &*view }, selector, action);
    }
}

extern "C" fn plain_substring(
    object: &Object,
    selector: Sel,
    range: NSRange,
    actual: *mut c_void,
) -> id {
    native_view(object).map_or(nil, |view| {
        super::attributed_substring_for_proposed_range(unsafe { &*view }, selector, range, actual)
    })
}

extern "C" fn first_rect(
    object: &Object,
    selector: Sel,
    range: NSRange,
    actual: *mut c_void,
) -> NSRect {
    native_view(object).map_or(NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.)), |view| {
        super::first_rect_for_character_range(unsafe { &*view }, selector, range, actual)
    })
}

extern "C" fn character_index(object: &Object, selector: Sel, point: NSPoint) -> u64 {
    native_view(object).map_or(NSNotFound as u64, |view| {
        super::character_index_for_point(unsafe { &*view }, selector, point)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_indicator_is_invalidated_when_caret_leaves_viewport() {
        // Scrolling can hide the caret without changing text or selection.
        // Exercise the real session transition without a window or checking service.
        let state = TextCheckingState {
            editor_id: 1,
            revision: 0,
            model_generation: 0,
            document_length: 7,
            selection: 7..7,
            marked_range: None,
            inserted_range: None,
        };
        let object = unsafe {
            let object: id = msg_send![client_class(), new];
            let client = Box::new(Client {
                window: Weak::new(),
                active: Cell::new(true),
                expected: RefCell::new(state.clone()),
                annotated: RefCell::new(None),
                suggestions: RefCell::new(suggestions::State::default()),
            });
            (*object).set_ivar(CLIENT_STATE_IVAR, Box::into_raw(client).cast::<c_void>());
            StrongPtr::new(object)
        };
        let geometry = Some((Bounds::new(point(px(10.), px(10.)), size(px(1.), px(20.))), [0.; 4]));
        extern "C" fn invalidate(_: &Object, _: Sel) {}
        let controller = unsafe {
            let mut class =
                ClassDecl::new("GPUITextCheckingGeometryTestController", class!(NSObject)).unwrap();
            class.add_method(sel!(invalidate), invalidate as extern "C" fn(&Object, Sel));
            StrongPtr::new(msg_send![class.register(), new])
        };
        let mut session = Session {
            client: object,
            controller,
            state: state.clone(),
            geometry,
            pending_consider: None,
        };
        assert!(session.prepare(state.clone(), false, None, true).geometry_changed);
        assert!(!session.prepare(state.clone(), false, None, true).geometry_changed);
        // Missing layout during an input callback is temporary, not scrolling.
        session.geometry = geometry;
        assert!(!session.prepare(state, false, None, false).geometry_changed);
    }

    #[test]
    fn annotation_ranges_follow_the_appkit_adjustment_contract() {
        // Clamp intersecting ranges and accept insertion points at both ends.
        assert_eq!(adjusted_range(NSRange::invalid(), 4..9), Some(4..9));
        assert_eq!(adjusted_range((0..6).into(), 4..9), Some(4..6));
        assert_eq!(adjusted_range((9..9).into(), 4..9), Some(9..9));
        assert_eq!(adjusted_range((10..11).into(), 4..9), None);
        assert_eq!(adjusted_range(NSRange { location: 1, length: NSUInteger::MAX }, 0..9), None);
    }

    #[test]
    fn annotation_edits_use_utf16_scalar_boundaries() {
        // Keep unchanged suffix annotations when a multicode-unit word changes.
        assert_eq!(changed_text("a😀kbright z", "a😀→ z"), (3..10, "→"));
        assert_eq!(changed_text("café", "café!"), (4..4, "!"));
        assert_eq!(changed_text("", "日本"), (0..0, "日本"));
    }

    #[test]
    fn optional_actual_range_output_accepts_null() {
        // AppKit clients may omit actualRange even when a request is adjusted.
        write_range(ptr::null_mut(), 1..3);
        let mut actual = NSRange::invalid();
        write_range((&mut actual as *mut NSRange).cast(), 1..3);
        assert_eq!(actual.to_range(), Some(1..3));
    }
}
