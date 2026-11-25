use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Compact,
    Grouped,
}

#[derive(Clone, Copy)]
pub(super) enum InputMode {
    Overwrite,
    Insert { low_digit: Option<usize> },
}

impl InputMode {
    pub(super) fn finish_byte(&mut self) {
        if let Self::Insert { low_digit } = self {
            *low_digit = None;
        }
    }
}

impl Format {
    pub(crate) const fn display_len(self, digits: usize) -> usize {
        if matches!(self, Self::Grouped) { digits + digits.saturating_sub(1) / 8 } else { digits }
    }

    pub(crate) fn display(self, bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(self.display_len(bytes.len() * 2));
        self.append(
            &mut text,
            0,
            bytes
                .iter()
                .flat_map(|byte| [DIGITS[(byte >> 4) as usize], DIGITS[(byte & 15) as usize]]),
        );
        text
    }

    fn separator(self, digit: usize) -> Option<u8> {
        (self == Self::Grouped && digit != 0 && digit.is_multiple_of(8))
            .then_some(if digit.is_multiple_of(32) { b'\n' } else { b' ' })
    }

    fn append(self, text: &mut String, start: usize, digits: impl IntoIterator<Item = u8>) {
        for (index, digit) in digits.into_iter().enumerate() {
            if let Some(separator) = self.separator(start + index) {
                text.push(char::from(separator));
            }
            text.push(char::from(digit));
        }
    }

    pub(super) fn is_canonical(self, text: &str) -> bool {
        // An unfinished last digit still has valid display positions.
        text.as_bytes().last().is_none_or(u8::is_ascii_hexdigit)
            && text.bytes().enumerate().all(|(index, byte)| {
                if self == Self::Grouped && index % 9 == 8 {
                    self.separator(self.digit(index)) == Some(byte)
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
    }

    // Either side of a display separator represents the same digit boundary.
    fn digit(self, offset: usize) -> usize {
        if self == Self::Grouped { offset - offset / 9 } else { offset }
    }

    fn offset(self, digit: usize, text_len: usize) -> usize {
        let offset = if self == Self::Grouped { digit + digit / 8 } else { digit };
        offset.min(text_len)
    }

    fn selected_digits(self, range: Range<usize>, text_len: usize) -> Range<usize> {
        let start = self.digit(range.start) / 2 * 2;
        let end = (self.digit(range.end).div_ceil(2) * 2).min(self.digit(text_len));
        start..end
    }

    pub(super) fn selection(self, selection: Selection, text_len: usize) -> Selection {
        if selection.is_empty() || self.digit(selection.start()) == self.digit(selection.end()) {
            return Selection::caret(selection.head);
        }
        let digits = self.selected_digits(selection.range(), text_len);
        let start = self.offset(digits.start, text_len);
        let end = self.display_len(digits.end);
        if selection.anchor <= selection.head {
            Selection::new(start, end)
        } else {
            Selection::new(end, start)
        }
    }

    pub(super) fn movement(
        self,
        text: &str,
        selection: Selection,
        movement: Movement,
        extend: bool,
    ) -> Option<Selection> {
        let backward = match movement {
            Movement::PreviousGrapheme | Movement::PreviousWord => true,
            Movement::NextGrapheme | Movement::NextWord => false,
            _ => return None,
        };
        if !extend && !selection.is_empty() {
            let offset = if backward { selection.start() } else { selection.end() };
            return Some(Selection::caret(self.offset(self.digit(offset), text.len())));
        }
        let digit = self.digit(selection.head);
        let whole_byte = extend || matches!(movement, Movement::PreviousWord | Movement::NextWord);
        let target = if backward {
            if whole_byte { digit.saturating_sub(1) / 2 * 2 } else { digit.saturating_sub(1) }
        } else if whole_byte {
            (digit / 2 + 1) * 2
        } else {
            digit
                + usize::from(
                    text.as_bytes().get(selection.head).is_some_and(u8::is_ascii_hexdigit),
                )
        };
        let offset = self.offset(target, text.len());
        Some(if extend {
            self.selection(Selection::new(selection.anchor, offset), text.len())
        } else {
            Selection::caret(offset)
        })
    }
}

impl TextEditor {
    pub(crate) fn can_insert_hex(&self) -> bool {
        self.hex_format() == Some(Format::Grouped)
    }

    pub(crate) fn inserts_hex_bytes(&self) -> bool {
        matches!(self.hex_input, InputMode::Insert { .. })
    }

    pub(crate) fn toggle_hex_insert(&mut self, cx: &mut Context<Self>) {
        if self.enabled && !self.read_only && self.can_insert_hex() {
            self.hex_input = if self.inserts_hex_bytes() {
                InputMode::Overwrite
            } else {
                InputMode::Insert { low_digit: None }
            };
            cx.notify();
        }
    }

    pub(super) fn hex_format(&self) -> Option<Format> {
        self.hex.filter(|_| self.canonical_hex)
    }

    pub(super) fn refresh_hex_format(&mut self) {
        self.canonical_hex = self.hex.is_some_and(|format| format.is_canonical(self.text()));
    }

    pub(super) fn hex_selection(&self, selection: Selection) -> Selection {
        self.hex_format().map_or(selection, |format| format.selection(selection, self.text().len()))
    }

    pub(super) fn paste_hex(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        let Some(format) = self.hex_format() else { return };
        let Some(digits) = self.hex_paste_digits(text) else { return };
        let mut selected = format.selected_digits(range.clone(), self.text().len());
        if range.is_empty() && range.start < self.text().len() {
            selected.end = selected.start;
        }
        self.replace_hex_digits(format, selected, &digits, digits.len(), cx);
    }

    pub(super) fn hex_paste_digits(&self, text: &str) -> Option<Vec<u8>> {
        let format = self.hex_format()?;
        if text.len() > self.hex_clipboard_limit(format) {
            return None;
        }
        let digit_limit = format.digit(self.max_bytes.unwrap_or(64 * 1024));
        let mut digits = Vec::with_capacity(text.len().min(digit_limit));
        for byte in text.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
            if digits.len() == digit_limit || !byte.is_ascii_hexdigit() {
                return None;
            }
            digits.push(byte.to_ascii_lowercase());
        }
        (!digits.is_empty() && digits.len().is_multiple_of(2)).then_some(digits)
    }

    pub(super) fn hex_clipboard_limit(&self, format: Format) -> usize {
        // Enough for byte-spaced clipboard text, including CRLF line endings.
        format.digit(self.max_bytes.unwrap_or(64 * 1024)) / 2 * 4
    }

    pub(super) fn type_hex(
        &mut self,
        format: Format,
        range: Range<usize>,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        let [digit] = text.as_bytes() else {
            if text.is_empty() {
                self.delete_hex_selection(format, range, cx);
            } else {
                self.paste_hex(range, text, cx);
            }
            return;
        };
        if !digit.is_ascii_hexdigit() {
            return;
        }
        let digit = digit.to_ascii_lowercase();
        let len = self.text().len();
        let index = format.digit(range.start);
        let offset = format.offset(index, len);
        if range.is_empty() && offset < len {
            if let InputMode::Insert { low_digit } = &mut self.hex_input
                && low_digit.take() != Some(index)
            {
                let start = index / 2 * 2;
                if self.replace_hex_digits(format, start..start, &[digit, b'0'], 1, cx) {
                    self.hex_input = InputMode::Insert { low_digit: Some(start + 1) };
                }
                return;
            }
            let caret = format.offset(index + 1, len);
            self.apply_hex_edit(
                offset..offset + 1,
                char::from(digit).encode_utf8(&mut [0; 1]),
                caret,
                cx,
            );
        } else if range.is_empty() {
            self.replace_hex_digits(format, index..index, &[digit], 1, cx);
        } else {
            let selected = format.selected_digits(range, len);
            // If there are bytes after the selection, keep them aligned.
            if selected.end < format.digit(len) {
                let low_digit = selected.start + 1;
                if self.replace_hex_digits(format, selected, &[digit, b'0'], 1, cx)
                    && self.inserts_hex_bytes()
                {
                    self.hex_input = InputMode::Insert { low_digit: Some(low_digit) };
                }
            } else {
                self.replace_hex_digits(format, selected, &[digit], 1, cx);
            }
        }
    }

    pub(super) fn delete_hex_selection(
        &mut self,
        format: Format,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if !range.is_empty() {
            self.replace_hex_digits(
                format,
                format.selected_digits(range, self.text().len()),
                &[],
                0,
                cx,
            );
        }
    }

    pub(super) fn delete_hex(&mut self, format: Format, backward: bool, cx: &mut Context<Self>) {
        let selection = self.buffer.primary_selection();
        if !selection.is_empty() {
            self.delete_hex_selection(format, selection.range(), cx);
            return;
        }
        let digit = format.digit(selection.head);
        let count = format.digit(self.text().len());
        if (backward && digit == 0) || (!backward && digit == count) {
            return;
        }
        let start = if backward { digit - 1 } else { digit } / 2 * 2;
        self.replace_hex_digits(format, start..(start + 2).min(count), &[], 0, cx);
    }

    fn replace_hex_digits(
        &mut self,
        format: Format,
        digits: Range<usize>,
        replacement: &[u8],
        advance: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let text = self.text();
        let count = format.digit(text.len());
        let next_count = count - digits.len() + replacement.len();
        let next_len = format.display_len(next_count);
        if self.max_bytes.is_some_and(|limit| next_len > limit) {
            return false;
        }
        let start = format.display_len(digits.start);
        let mut end = format.display_len(digits.end);
        let mut formatted = String::new();
        format.append(&mut formatted, digits.start, replacement.iter().copied());
        if digits.len() != replacement.len() {
            let suffix = text[end..].bytes().filter(u8::is_ascii_hexdigit);
            format.append(&mut formatted, digits.start + replacement.len(), suffix);
            end = text.len();
        }
        let caret = format.offset(digits.start + advance, next_len);
        self.apply_hex_edit(start..end, &formatted, caret, cx);
        true
    }

    fn apply_hex_edit(
        &mut self,
        range: Range<usize>,
        text: &str,
        caret: usize,
        cx: &mut Context<Self>,
    ) {
        let changed = &self.text()[range.clone()] != text;
        if changed {
            self.buffer.replace_range(range, text);
        }
        self.buffer.set_selection(Selection::caret(caret));
        if changed {
            self.did_change(cx);
        } else {
            self.preferred_x = None;
            self.scroll_to_caret = true;
            self.primary_affinity = CaretAffinity::Downstream;
            self.reset_caret_blink();
            cx.notify();
        }
    }
}
