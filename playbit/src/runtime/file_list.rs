//! Frontend-owned FileList delivery cursor. Backend completion only supplies an
//! indexed entry; successful guest Take commits the cursor exactly once.
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Busy,
    Closed,
    LimitExceeded,
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Default)]
struct State {
    index: u32,
    sequence: u64,
    reservation: Option<u64>,
    closed: bool,
}
#[derive(Default)]
pub struct Cursor(Rc<RefCell<State>>);
pub struct Next {
    cursor: Rc<RefCell<State>>,
    sequence: u64,
    pending: bool,
}
impl Cursor {
    pub fn begin(&self) -> Result<Next> {
        let mut state = self.0.borrow_mut();
        if state.closed {
            return Err(Error::Closed);
        }
        if state.reservation.is_some() {
            return Err(Error::Busy);
        }
        let sequence = state.sequence.checked_add(1).ok_or(Error::LimitExceeded)?;
        state.sequence = sequence;
        state.reservation = Some(sequence);
        Ok(Next { cursor: self.0.clone(), sequence, pending: true })
    }
    /// The caller also makes an outstanding operation ready with Closed and
    /// releases the engine capture. Neither action waits for backend cleanup.
    pub fn close(&self) -> Result<()> {
        let mut state = self.0.borrow_mut();
        if state.closed {
            return Err(Error::Closed);
        }
        state.closed = true;
        Ok(())
    }
}
impl Next {
    /// Check again at Take, including ready results arriving before list close.
    /// Authority and guest memory validation are separate, mandatory checks.
    pub fn index(&self) -> Result<u32> {
        let state = self.cursor.borrow();
        if !self.pending || state.closed || state.reservation != Some(self.sequence) {
            return Err(Error::Closed);
        }
        Ok(state.index)
    }
    /// The callback performs bounded delivery under the current authority guard.
    /// False retains the reservation (BufferTooSmall); invalid memory never calls
    /// this method. `entry` is false only for a validated End result at this index.
    pub fn take(&mut self, entry: bool, deliver: impl FnOnce() -> bool) -> Result<bool> {
        let index = self.index()?;
        let next = if entry { index.checked_add(1).ok_or(Error::LimitExceeded)? } else { index };
        if !deliver() {
            return Ok(false);
        }
        let mut state = self.cursor.borrow_mut();
        assert_eq!(
            state.reservation,
            Some(self.sequence),
            "bounded delivery cannot reenter the runtime"
        );
        assert!(!state.closed, "bounded delivery cannot close its own list");
        state.index = next;
        state.reservation = None;
        self.pending = false;
        Ok(true)
    }
}
impl Drop for Next {
    fn drop(&mut self) {
        if self.pending {
            let mut state = self.cursor.borrow_mut();
            if state.reservation == Some(self.sequence) {
                state.reservation = None;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests exercise delivery and disposal transitions.")]
mod tests {
    use super::*;
    #[test]
    fn only_successful_take_advances_and_disposal_cannot_skip_a_late_result() {
        let cursor = Cursor::default();
        let mut first = cursor.begin().unwrap();
        assert_eq!(first.index(), Ok(0));
        assert!(matches!(cursor.begin(), Err(Error::Busy)));
        assert_eq!(first.take(true, || false), Ok(false));
        assert_eq!(first.index(), Ok(0));
        assert!(matches!(cursor.begin(), Err(Error::Busy)));
        // A late backend response for this abandoned operation has no Next
        // reservation to commit, even after another operation starts.
        drop(first);
        let mut retry = cursor.begin().unwrap();
        assert_eq!(retry.index(), Ok(0));
        assert_eq!(retry.take(true, || true), Ok(true));
        assert_eq!(retry.take(true, || panic!("consumed result")), Err(Error::Closed));
        let mut second = cursor.begin().unwrap();
        assert_eq!(second.index(), Ok(1));
        second.take(true, || true).unwrap();
        for _ in 0..2 {
            let mut end = cursor.begin().unwrap();
            assert_eq!(end.index(), Ok(2));
            assert_eq!(end.take(false, || true), Ok(true));
        }
    }
    #[test]
    fn close_prevents_delivery_of_already_ready_results() {
        let cursor = Cursor::default();
        let mut next = cursor.begin().unwrap();
        assert_eq!(next.index(), Ok(0));
        cursor.close().unwrap();
        assert_eq!(next.take(true, || panic!("closed output")), Err(Error::Closed));
        assert!(matches!(cursor.begin(), Err(Error::Closed)));
        drop(next);
        assert_eq!(cursor.close(), Err(Error::Closed));
    }
}
