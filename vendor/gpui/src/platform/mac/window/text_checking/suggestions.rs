//! Public AppKit text checking and indicators, with application-owned acceptance.
//! A visible proposal is applied before a submit/focus key reaches GPUI. One
//! request per window and one current proposal bound asynchronous native work.

use super::*;

const REPLACEMENT: u64 = 1 << 8;
const CORRECTION: u64 = 1 << 9;
const SPELLING: u64 = 1 << 1;
const ORTHOGRAPHY: u64 = 1;
const ACCEPTED: NSInteger = 1;
const REJECTED: NSInteger = 2;
const REVERTED: NSInteger = 5;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSTextCheckingSelectedRangeKey: id;
}

#[derive(Default)]
pub(super) struct State {
    candidate: Option<Rc<Candidate>>,
    requested: Option<(u64, u64)>,
    reversion: Option<Reversion>,
    rejected: Option<Rejection>,
}

struct Rejection {
    range: Range<usize>,
    original: String,
    revision: u64,
    model_generation: u64,
}

#[derive(Clone, Copy)]
struct Preferences {
    spelling: bool,
    capitalization: bool,
    replacements: bool,
}

impl Preferences {
    fn allows(self, kind: u64, original: &str, replacement: &str) -> bool {
        if kind == REPLACEMENT {
            return self.replacements;
        }
        if kind == SPELLING {
            // A detected spelling error can be a proper noun with wrong case.
            return self.spelling;
        }
        kind == CORRECTION
            && if case_only(original, replacement) { self.capitalization } else { self.spelling }
    }
}

struct Candidate {
    state: TextCheckingState,
    range: Range<usize>,
    original: String,
    replacement: String,
    revert: bool,
    language: String,
}

struct Reversion {
    range: Range<usize>,
    original: String,
    replacement: String,
    revision: u64,
    model_generation: u64,
    language: String,
}

pub(super) fn observe(object: &Object, next: &TextCheckingState, changed: Option<&Range<usize>>) {
    let cache = client(object).annotated.borrow();
    let context = cache.as_ref().map(|cache| (cache.text.as_str(), cache.range.start));
    client(object).suggestions.borrow_mut().observe(next, changed, context);
}

impl State {
    fn observe(
        &mut self,
        next: &TextCheckingState,
        changed: Option<&Range<usize>>,
        context: Option<(&str, usize)>,
    ) {
        if let Some(rejected) = &mut self.rejected {
            if rejected.model_generation != next.model_generation
                || (rejected.revision != next.revision
                    && changed.is_some_and(|changed| changed.start < rejected.range.end))
            {
                self.rejected = None;
            } else {
                rejected.revision = next.revision;
            }
        }
        if let Some(reversion) = &mut self.reversion {
            if reversion.model_generation != next.model_generation
                || (reversion.revision != next.revision
                    && changed.is_some_and(|range| range.start < reversion.range.end))
            {
                // Undo (or a manual edit) restoring the original word rejects
                // that correction just as accepting its reversion does.
                if reversion.model_generation == next.model_generation
                    && let Some((text, offset)) = context
                    && let Some(start) = reversion.range.start.checked_sub(offset)
                    && utf16_substring(
                        &text.encode_utf16().collect::<Vec<_>>(),
                        start..start + reversion.original.encode_utf16().count(),
                    )
                    .as_ref()
                        == Some(&reversion.original)
                {
                    self.rejected = Some(Rejection {
                        range: reversion.range.start
                            ..reversion.range.start + reversion.original.encode_utf16().count(),
                        original: reversion.original.clone(),
                        revision: next.revision,
                        model_generation: next.model_generation,
                    });
                }
                self.reversion = None;
            } else {
                reversion.revision = next.revision;
            }
        }
    }
}

pub(super) fn request(object: &Object) {
    let snapshot = client(object).expected.borrow().clone();
    if snapshot.marked_range.is_some() || snapshot.inserted_range.is_none() {
        return;
    }
    let token = (snapshot.revision, snapshot.model_generation);
    if client(object).suggestions.borrow().requested == Some(token) {
        return;
    }
    let Some((text, range)) =
        with_handler(object, |handler| handler.text_checking_text(None)).flatten()
    else {
        return;
    };
    let Some(window) = client(object).window.upgrade() else { return };
    let executor = {
        let mut state = window.lock();
        if state.text_checking_request_in_flight {
            return;
        }
        state.text_checking_request_in_flight = true;
        state.executor.clone()
    };
    client(object).suggestions.borrow_mut().requested = Some(token);
    let checker = unsafe { checker() };
    let spelling: BOOL =
        unsafe { msg_send![class!(NSSpellChecker), isAutomaticSpellingCorrectionEnabled] };
    let capitalization: BOOL =
        unsafe { msg_send![class!(NSSpellChecker), isAutomaticCapitalizationEnabled] };
    let replacements: BOOL =
        unsafe { msg_send![class!(NSSpellChecker), isAutomaticTextReplacementEnabled] };
    let preferences = Preferences {
        spelling: spelling == YES,
        capitalization: capitalization == YES,
        replacements: replacements == YES,
    };
    let types = if replacements == YES { REPLACEMENT } else { 0 }
        | if spelling == YES || capitalization == YES {
            CORRECTION | SPELLING | ORTHOGRAPHY
        } else {
            0
        };
    if types == 0 {
        window.lock().text_checking_request_in_flight = false;
        return;
    }
    let selection = snapshot.selection.start.saturating_sub(range.start)
        ..snapshot.selection.end.saturating_sub(range.start);
    let (sender, receiver) = oneshot::channel::<(usize, usize)>();
    let sender = Mutex::new(Some(sender));
    let completion =
        ConcreteBlock::new(move |_: NSInteger, results: id, orthography: id, _: NSInteger| {
            // AppKit may complete on any thread. Only retain and send the immutable
            // array here; all editor and UI access stays on the foreground executor.
            let retained: id = unsafe { msg_send![results, retain] };
            let orthography: id = unsafe { msg_send![orthography, retain] };
            if sender.lock().take().is_none_or(|sender| {
                sender.send((retained as usize, orthography as usize)).is_err()
            }) {
                unsafe {
                    let _: () = msg_send![retained, release];
                    let _: () = msg_send![orthography, release];
                }
            }
        })
        .copy();
    let retained = unsafe { StrongPtr::retain(object as *const Object as id) };
    let weak = Arc::downgrade(&window);
    let tag = document_tag(object);
    unsafe {
        let selection: id = msg_send![class!(NSValue), valueWithRange: NSRange::from(selection)];
        let options: id = msg_send![class!(NSDictionary), dictionaryWithObject: selection forKey: NSTextCheckingSelectedRangeKey];
        let _: NSInteger = msg_send![checker,
            requestCheckingOfString: ns_string(&text)
            range: NSRange::from(0..range.len()) types: types options: options
            inSpellDocumentWithTag: tag completionHandler: &*completion];
    }
    executor
        .spawn(async move {
            let results = receiver.await.ok().map(|(results, orthography)| unsafe {
                (StrongPtr::new(results as id), StrongPtr::new(orthography as id))
            });
            if let Some(window) = weak.upgrade() {
                window.lock().text_checking_request_in_flight = false;
                let object = unsafe { &**retained };
                if let Some((results, orthography)) = results
                    && client(object).active.get()
                    && *client(object).expected.borrow() == snapshot
                    && with_handler(object, |_| ()).is_some()
                {
                    if let Some(candidate) = candidate(
                        object,
                        &snapshot,
                        &text,
                        range.start,
                        &results,
                        &orthography,
                        preferences,
                    ) {
                        let rejected =
                            client(object).suggestions.borrow().rejected.as_ref().is_some_and(
                                |rejected| {
                                    rejected.range == candidate.range
                                        && rejected.original == candidate.original
                                },
                            );
                        if !rejected {
                            // Fast typing can finish the delimiter before the
                            // async request. Apply only a proposal for the
                            // current snapshot with a delimiter after its word.
                            if snapshot.selection.end > candidate.range.end {
                                apply(object, &candidate);
                            } else {
                                show(object, candidate);
                            }
                        }
                    }
                }
                // A later edit may have coalesced while this request was running.
                request_current(&window);
            }
        })
        .detach();
}

fn request_current(window: &Arc<Mutex<MacWindowState>>) {
    let object = window
        .lock()
        .text_checking
        .as_ref()
        .filter(|session| session.geometry.is_some())
        .map(|session| session.client.clone());
    if let Some(object) = object {
        request(unsafe { &**object });
    }
}

fn candidate(
    object: &Object,
    state: &TextCheckingState,
    text: &str,
    offset: usize,
    results: &StrongPtr,
    orthography: &StrongPtr,
    preferences: Preferences,
) -> Option<Candidate> {
    let units = text.encode_utf16().collect::<Vec<_>>();
    // Prefer the user's explicit replacement over a spelling suggestion.
    for kind in [REPLACEMENT, CORRECTION, SPELLING] {
        for result in unsafe { (*results).iter() } {
            let result_type: u64 = unsafe { msg_send![result, resultType] };
            if result_type != kind {
                continue;
            }
            let range: NSRange = unsafe { msg_send![result, range] };
            let Some(range) = range.to_range() else { continue };
            if !state.selection.is_empty() {
                continue;
            }
            let Some(original) = utf16_substring(&units, range.clone()) else { continue };
            let caret = state.selection.end.saturating_sub(offset);
            let nearby = candidate_near_caret(&units, range.clone(), caret);
            if !nearby && kind != CORRECTION {
                continue;
            }
            let checker = unsafe { checker() };
            let language: id = if (**orthography).is_null() {
                nil
            } else {
                unsafe {
                    msg_send![checker, languageForWordRange: NSRange::from(range.clone()) inString: ns_string(text) orthography: **orthography]
                }
            };
            let language = if language.is_null() || unsafe { language.to_str() } == "und" {
                unsafe { msg_send![checker, language] }
            } else {
                language
            };
            // Unified checking can return a misspelled range without a
            // correction. Ask for one only for the current word, after the
            // asynchronous check, never on the key event path.
            let replacement: id = if kind == SPELLING && preferences.spelling {
                unsafe {
                    msg_send![checker, correctionForWordRange: NSRange::from(range.clone()) inString: ns_string(text) language: language inSpellDocumentWithTag: document_tag(object)]
                }
            } else {
                unsafe { msg_send![result, replacementString] }
            };
            if replacement.is_null() || original.is_empty() {
                continue;
            }
            let replacement = unsafe { replacement.to_str() }.to_owned();
            if replacement == original || replacement.encode_utf16().count() > 16_384 {
                continue;
            }
            if !preferences.allows(kind, &original, &replacement) {
                continue;
            }
            if !nearby && (range.end > caret || !case_only(&original, &replacement)) {
                continue;
            }
            if nearby
                && caret > range.end
                && !permits_correction(&utf16_substring(&units, range.end..caret)?)
            {
                continue;
            }
            let (Some(start), Some(end)) =
                (offset.checked_add(range.start), offset.checked_add(range.end))
            else {
                continue;
            };
            return Some(Candidate {
                state: state.clone(),
                range: start..end,
                original,
                replacement,
                revert: false,
                language: unsafe { language.to_str() }.to_owned(),
            });
        }
    }
    None
}

fn utf16_substring(units: &[u16], range: Range<usize>) -> Option<String> {
    String::from_utf16(units.get(range)?).ok()
}

fn candidate_near_caret(units: &[u16], range: Range<usize>, caret: usize) -> bool {
    range.start <= caret
        && (caret <= range.end
            || units.get(range.end..caret).is_some_and(|tail| {
                tail.iter().all(|unit| {
                    char::from_u32(u32::from(*unit))
                        .is_some_and(|c| c.is_whitespace() || c.is_ascii_punctuation())
                })
            }))
}

fn case_only(original: &str, replacement: &str) -> bool {
    original
        .chars()
        .flat_map(char::to_lowercase)
        .eq(replacement.chars().flat_map(char::to_lowercase))
}

fn delimiter(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_whitespace() || c.is_ascii_punctuation())
}

fn permits_correction(text: &str) -> bool {
    unsafe {
        let prevents: BOOL =
            msg_send![checker(), preventsAutocorrectionBeforeString: ns_string(text) language: nil];
        prevents == NO
    }
}

fn show(object: &Object, candidate: Candidate) {
    dismiss(object, None);
    let mut rect = NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.));
    let view = view_for_range(
        object,
        sel!(viewForRange:firstRect:actualRange:),
        candidate.range.clone().into(),
        (&mut rect as *mut NSRect).cast(),
        ptr::null_mut(),
    );
    if view.is_null() {
        return;
    }
    let candidate = Rc::new(candidate);
    client(object).suggestions.borrow_mut().candidate = Some(candidate.clone());
    let retained = unsafe { StrongPtr::retain(object as *const Object as id) };
    let presented = candidate.clone();
    let completion = ConcreteBlock::new(move |accepted: id| {
        let object = unsafe { &**retained };
        {
            let mut state = client(object).suggestions.borrow_mut();
            if !state.candidate.as_ref().is_some_and(|current| Rc::ptr_eq(current, &presented)) {
                return;
            }
            state.candidate.take();
        }
        if with_handler(object, |_| ()).is_none() {
            return;
        }
        if accepted.is_null() {
            record(object, &presented, REJECTED);
        } else {
            apply(object, &presented);
        }
    })
    .copy();
    unsafe {
        let _: () = msg_send![checker(), showCorrectionIndicatorOfType: if candidate.revert { 1isize } else { 0isize }
            primaryString: ns_string(&candidate.replacement) alternativeStrings: NSArray::array(nil)
            forStringInRect: rect view: view completionHandler: &*completion];
    }
}

fn apply(object: &Object, candidate: &Candidate) -> bool {
    if *client(object).expected.borrow() != candidate.state {
        return false;
    }
    let applied = with_handler(object, |handler| {
        handler.replace_text_for_checking(
            &candidate.state,
            candidate.range.clone(),
            &candidate.replacement,
        )
    })
    .unwrap_or(false);
    if !applied {
        return false;
    }
    update_expected(object);
    refresh_text(object, None);
    let next = client(object).expected.borrow().clone();
    client(object).suggestions.borrow_mut().reversion = (!candidate.revert).then(|| Reversion {
        range: candidate.range.start
            ..candidate.range.start + candidate.replacement.encode_utf16().count(),
        original: candidate.original.clone(),
        replacement: candidate.replacement.clone(),
        revision: next.revision,
        model_generation: next.model_generation,
        language: candidate.language.clone(),
    });
    record(object, candidate, if candidate.revert { REVERTED } else { ACCEPTED });
    true
}

pub(super) fn show_reversion(object: &Object) {
    let next = client(object).expected.borrow().clone();
    let candidate = {
        let state = client(object).suggestions.borrow();
        let Some(reversion) = state.reversion.as_ref() else { return };
        if !next.selection.is_empty()
            || next.selection.end < reversion.range.start
            || next.selection.end > reversion.range.end
        {
            return;
        }
        Candidate {
            state: next,
            range: reversion.range.clone(),
            original: reversion.replacement.clone(),
            replacement: reversion.original.clone(),
            revert: true,
            language: reversion.language.clone(),
        }
    };
    let matches = with_handler(object, |handler| {
        let mut actual = None;
        handler.text_for_range(candidate.range.clone(), &mut actual).is_some_and(|text| {
            text == candidate.original && actual == Some(candidate.range.clone())
        })
    })
    .unwrap_or(false);
    if matches {
        show(object, candidate);
    }
}

pub(super) fn dismiss(object: &Object, response: Option<NSInteger>) {
    // Native dismissal invokes the completion synchronously with nil. Retire
    // the token first so programmatic dismissal cannot reject or apply it twice.
    let candidate = client(object).suggestions.borrow_mut().candidate.take();
    if let Some(candidate) = candidate {
        if let Some(response) = response {
            record(object, &candidate, response);
        }
        if let Some(window) = client(object).window.upgrade() {
            let view = window.lock().native_view.as_ptr();
            unsafe {
                let _: () = msg_send![checker(), dismissCorrectionIndicatorForView: view];
            }
        }
    }
}

pub(super) fn before_key(window: &Arc<Mutex<MacWindowState>>, key: &Keystroke) -> bool {
    let object = window.lock().text_checking.as_ref().map(|session| session.client.clone());
    let Some(object) = object else { return false };
    let object = unsafe { &**object };
    let candidate = client(object).suggestions.borrow().candidate.clone();
    let Some(candidate) = candidate else { return false };
    let plain = !key.modifiers.platform && !key.modifiers.control && !key.modifiers.alt;
    let accepts = matches!(key.key.as_str(), "enter" | "return" | "tab" | "space")
        || key.key_char.as_ref().is_some_and(|text| delimiter(text) && permits_correction(text));
    if plain
        && accepts
        && (!candidate.revert || matches!(key.key.as_str(), "enter" | "return" | "tab"))
    {
        dismiss(object, None);
        apply(object, &candidate);
    } else {
        dismiss(object, (key.key == "escape").then_some(REJECTED));
    }
    plain && key.key == "escape"
}

fn document_tag(object: &Object) -> NSInteger {
    let controller = client(object).window.upgrade().and_then(|window| {
        window.lock().text_checking.as_ref().map(|session| session.controller.clone())
    });
    controller.map_or(0, |controller| unsafe { msg_send![*controller, spellCheckerDocumentTag] })
}

fn record(object: &Object, candidate: &Candidate, response: NSInteger) {
    // Closing a reversion keeps the correction the user already accepted.
    if candidate.revert && response == REJECTED {
        return;
    }
    if response == REJECTED || response == REVERTED {
        let (range, original) = if candidate.revert {
            (
                candidate.range.start
                    ..candidate.range.start + candidate.replacement.encode_utf16().count(),
                candidate.replacement.clone(),
            )
        } else {
            (candidate.range.clone(), candidate.original.clone())
        };
        let next = client(object).expected.borrow();
        client(object).suggestions.borrow_mut().rejected = Some(Rejection {
            range,
            original,
            revision: next.revision,
            model_generation: next.model_generation,
        });
    }
    let tag = document_tag(object);
    let (word, correction) = if candidate.revert {
        (&candidate.replacement, &candidate.original)
    } else {
        (&candidate.original, &candidate.replacement)
    };
    unsafe {
        let _: () = msg_send![checker(), recordResponse: response toCorrection: ns_string(correction)
            forWord: ns_string(word) language: ns_string(&candidate.language) inSpellDocumentWithTag: tag];
    }
}

unsafe fn checker() -> id {
    unsafe { msg_send![class!(NSSpellChecker), sharedSpellChecker] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_substrings_use_exact_utf16_boundaries() {
        // OS candidates must not split surrogate pairs or escape bounded context.
        assert_eq!(
            utf16_substring(&"a🙂b".encode_utf16().collect::<Vec<_>>(), 1..3),
            Some("🙂".into())
        );
        assert_eq!(utf16_substring(&"a🙂b".encode_utf16().collect::<Vec<_>>(), 2..3), None);
        assert_eq!(utf16_substring(&"a🙂b".encode_utf16().collect::<Vec<_>>(), 0..99), None);
    }

    #[test]
    fn candidates_follow_each_independent_system_preference() {
        let spelling = Preferences { spelling: true, capitalization: false, replacements: false };
        assert!(spelling.allows(CORRECTION, "teh", "the"));
        assert!(spelling.allows(SPELLING, "london", "London"));
        assert!(!spelling.allows(CORRECTION, "hello", "Hello"));
        assert!(!spelling.allows(REPLACEMENT, "kbright", "→"));
        let capitalization =
            Preferences { spelling: false, capitalization: true, replacements: true };
        assert!(!capitalization.allows(CORRECTION, "teh", "the"));
        assert!(capitalization.allows(CORRECTION, "hello", "Hello"));
        assert!(capitalization.allows(REPLACEMENT, "kbright", "→"));
    }

    #[test]
    fn delayed_candidates_only_finish_the_current_word() {
        assert!(candidate_near_caret(&"🙂kbright".encode_utf16().collect::<Vec<_>>(), 2..9, 9));
        assert!(candidate_near_caret(&"🙂kbright ".encode_utf16().collect::<Vec<_>>(), 2..9, 10));
        assert!(candidate_near_caret(&"kbright.".encode_utf16().collect::<Vec<_>>(), 0..7, 8));
        assert!(!candidate_near_caret(
            &"kbright next".encode_utf16().collect::<Vec<_>>(),
            0..7,
            12
        ));
        assert!(!candidate_near_caret(&"kbright".encode_utf16().collect::<Vec<_>>(), 0..7, 99));
    }

    #[test]
    fn rejected_words_survive_delimiters_and_retire_when_edited() {
        // Reversion already updates the client snapshot before its next frame.
        // That frame must preserve the rejection; later word edits must not.
        let mut snapshot = TextCheckingState {
            editor_id: 1,
            revision: 2,
            model_generation: 0,
            document_length: 7,
            selection: 7..7,
            marked_range: None,
            inserted_range: None,
        };
        let mut state = State {
            rejected: Some(Rejection {
                range: 0..7,
                original: "kbright".into(),
                revision: 2,
                model_generation: 0,
            }),
            ..State::default()
        };
        state.observe(&snapshot, Some(&(0..7)), None);
        assert!(state.rejected.is_some());
        snapshot.revision += 1;
        state.observe(&snapshot, Some(&(7..8)), None);
        assert!(state.rejected.is_some());
        snapshot.revision += 1;
        state.observe(&snapshot, Some(&(0..1)), None);
        assert!(state.rejected.is_none());
        state.reversion = Some(Reversion {
            range: 0..1,
            original: "kbright".into(),
            replacement: "→".into(),
            revision: snapshot.revision,
            model_generation: snapshot.model_generation,
            language: "en".into(),
        });
        snapshot.revision += 1;
        state.observe(&snapshot, Some(&(0..7)), Some(("kbright", 0)));
        assert!(state.reversion.is_none());
        assert_eq!(state.rejected.as_ref().unwrap().original, "kbright");
    }
}
