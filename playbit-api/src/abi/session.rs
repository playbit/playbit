//! Typed session channel calls on the scalar dispatcher.
use super::{operations::Context, syscall::Opcode};
/// # Safety
/// Live callback context, stable inputs and disjoint initialized outputs are required.
#[allow(unsafe_code, reason = "Typed adapter to the checked scalar dispatcher.")]
pub(crate) unsafe fn call(context: *mut Context, operation: Opcode, slots: [u64; 6]) -> i64 {
    // SAFETY: callers uphold the operation's borrowed pointer contract.
    unsafe { super::invocation::invoke(context, operation as u32, slots) }
}
