//! Native text mutations use UTF-8 byte coordinates, in application order.
//! The final value remains authoritative, including snapshot-only retries.
extern crate alloc;

use super::syscall::{Error, MAX_EVENT_BYTES};
use alloc::vec::Vec;

pub const MAX_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_TEXT_EDITS: usize = 256;
pub const TEXT_EVENT_OVERHEAD: usize = 4 + MAX_TEXT_EDITS * 12;
const _: () = assert!(MAX_TEXT_BYTES + TEXT_EVENT_OVERHEAD == MAX_EVENT_BYTES);

/// Replace `start..end` in the preceding native value with `inserted_len` bytes.
/// Insertion has `start == end`; deletion has `inserted_len == 0`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextEdit {
    pub start: usize,
    pub end: usize,
    pub inserted_len: usize,
}

impl TextEdit {
    pub fn inverse(self) -> Self {
        assert!(self.start <= self.end, "native edit range is ordered");
        Self {
            start: self.start,
            end: self.start + self.inserted_len,
            inserted_len: self.end - self.start,
        }
    }

    /// The span changed by two consecutive edits, for native undo groups.
    pub fn then(self, next: Self) -> Self {
        assert!(self.start <= self.end, "native edit range is ordered");
        assert!(next.start <= next.end, "next native edit range is ordered");
        let start = self.start.min(next.start);
        let inserted_end = self.start + self.inserted_len;
        Self {
            start,
            end: self.end + next.end.saturating_sub(inserted_end),
            inserted_len: inserted_end.max(next.end) - (next.end - next.start) + next.inserted_len
                - start,
        }
    }
}

/// A repeatable, allocation-free iterator over native edits in delivery order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextEdits<'a> {
    bytes: &'a [u8],
}

impl Iterator for TextEdits<'_> {
    type Item = TextEdit;

    fn next(&mut self) -> Option<Self::Item> {
        let (head, tail) = self.bytes.split_first_chunk::<12>()?;
        self.bytes = tail;
        Some(decode_edit(head))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = self.bytes.len() / 12;
        (length, Some(length))
    }
}

impl DoubleEndedIterator for TextEdits<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let (head, tail) = self.bytes.split_last_chunk::<12>()?;
        self.bytes = head;
        Some(decode_edit(tail))
    }
}

impl ExactSizeIterator for TextEdits<'_> {}

fn decode_edit(bytes: &[u8; 12]) -> TextEdit {
    let word = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("edit word")) as usize;
    TextEdit { start: word(0), end: word(4), inserted_len: word(8) }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextInput<'a> {
    pub text: &'a str,
    pub edits: Option<TextEdits<'a>>,
}

impl<'a> TextInput<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_EVENT_BYTES {
            return Err(Error::LimitExceeded);
        }
        let (count, bytes) = bytes.split_first_chunk::<4>().ok_or(Error::InvalidArgument)?;
        let count = u32::from_le_bytes(*count) as usize;
        if count == u32::MAX as usize {
            if bytes.len() > MAX_TEXT_BYTES {
                return Err(Error::LimitExceeded);
            }
            let text = core::str::from_utf8(bytes).map_err(|_| Error::InvalidArgument)?;
            return Ok(Self { text, edits: None });
        }
        if count > MAX_TEXT_EDITS {
            return Err(Error::LimitExceeded);
        }
        let (edits, text) = bytes.split_at_checked(count * 12).ok_or(Error::InvalidArgument)?;
        if text.len() > MAX_TEXT_BYTES {
            return Err(Error::LimitExceeded);
        }
        let text = core::str::from_utf8(text).map_err(|_| Error::InvalidArgument)?;
        let edits = TextEdits { bytes: edits };
        // Intermediate values are not duplicated in the payload. Validate every
        // extent backwards against its resulting byte length before exposing it.
        let mut length = text.len();
        for edit in edits.rev() {
            if edit.start > edit.end
                || edit.start > length
                || edit.inserted_len > length - edit.start
            {
                return Err(Error::InvalidArgument);
            }
            length = (length - edit.inserted_len)
                .checked_add(edit.end - edit.start)
                .filter(|length| *length <= u32::MAX as usize)
                .ok_or(Error::InvalidArgument)?;
        }
        if let Some(last) = edits.bytes.last_chunk::<12>() {
            let last = decode_edit(last);
            if !text.is_char_boundary(last.start)
                || !text.is_char_boundary(last.start + last.inserted_len)
            {
                return Err(Error::InvalidArgument);
            }
        }
        Ok(Self { text, edits: Some(edits) })
    }

    /// A platform value for which the native editing API did not supply a range.
    pub fn snapshot(text: &str) -> Result<Vec<u8>, Error> {
        if text.len() > MAX_TEXT_BYTES {
            return Err(Error::LimitExceeded);
        }
        let mut bytes = Vec::with_capacity(4 + text.len());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(text.as_bytes());
        Ok(bytes)
    }

    pub fn encode(text: &str, edits: &[TextEdit]) -> Result<Vec<u8>, Error> {
        if text.len() > MAX_TEXT_BYTES || edits.len() > MAX_TEXT_EDITS {
            return Err(Error::LimitExceeded);
        }
        let mut bytes = Vec::with_capacity(4 + edits.len() * 12 + text.len());
        bytes.extend_from_slice(&(edits.len() as u32).to_le_bytes());
        for edit in edits {
            for value in [edit.start, edit.end, edit.inserted_len] {
                let value = u32::try_from(value).map_err(|_| Error::InvalidArgument)?;
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        bytes.extend_from_slice(text.as_bytes());
        TextInput::decode(&bytes)?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_edits_round_trip_without_relocating_repeated_text() {
        // Delete a multibyte character, then insert at the first repeated letter.
        let edits = [
            TextEdit { start: 2, end: 6, inserted_len: 0 },
            TextEdit { start: 0, end: 0, inserted_len: 2 },
        ];
        let bytes = TextInput::encode("ééaa", &edits).expect("bounded edits");
        let input = TextInput::decode(&bytes).expect("valid edits");
        assert_eq!(input.text, "ééaa");
        assert_eq!(input.edits.expect("available").collect::<Vec<_>>(), edits);
        assert_eq!(
            input.edits.expect("available").rev().collect::<Vec<_>>(),
            edits.into_iter().rev().collect::<Vec<_>>()
        );
        // A native draft or guest model can be oversized before a deletion brings it back.
        let shrink = [TextEdit { start: 0, end: MAX_TEXT_BYTES + 1, inserted_len: 0 }];
        let bytes = TextInput::encode("", &shrink).expect("only the resulting value is bounded");
        assert_eq!(
            TextInput::decode(&bytes).expect("deletion").edits.expect("available").next(),
            Some(shrink[0])
        );
        let maximum = "a".repeat(MAX_TEXT_BYTES);
        let repeated = [TextEdit { start: 0, end: 1, inserted_len: 1 }; MAX_TEXT_EDITS];
        let bytes = TextInput::encode(&maximum, &repeated).expect("exact maximum");
        assert_eq!(bytes.len(), MAX_EVENT_BYTES);
        assert_eq!(
            TextInput::decode(&bytes).expect("maximum").edits.expect("available").len(),
            MAX_TEXT_EDITS
        );
    }

    #[test]
    fn malformed_edits_fail_before_exposure() {
        // Reject truncated headers, invalid UTF-8, inverted ranges, overflow and split characters.
        assert_eq!(TextInput::decode(&[]), Err(Error::InvalidArgument));
        assert_eq!(TextInput::decode(&[1, 0, 0, 0]), Err(Error::InvalidArgument));
        assert_eq!(TextInput::decode(&[0, 0, 0, 0, 255]), Err(Error::InvalidArgument));
        for edit in [
            TextEdit { start: 2, end: 1, inserted_len: 0 },
            TextEdit { start: 0, end: usize::MAX, inserted_len: 0 },
            TextEdit { start: 1, end: 1, inserted_len: 1 },
            TextEdit { start: 0, end: 0, inserted_len: 3 },
        ] {
            assert_eq!(TextInput::encode("é", &[edit]), Err(Error::InvalidArgument));
        }
        assert_eq!(TextInput::decode(&257u32.to_le_bytes()), Err(Error::LimitExceeded));
    }
}
