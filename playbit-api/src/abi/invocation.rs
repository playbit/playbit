//! Target lowering for the scalar syscall entry. No operation-specific
//! symbol is imported. Native calls borrow the explicit instance context.
#![allow(unsafe_code, reason = "C import declarations and raw-pointer forwarding.")]
use super::operations::Context;
#[cfg(target_arch = "wasm32")]
use super::syscall::Error;
#[cfg(target_arch = "wasm32")]
pub(crate) const CONTEXT_TOKEN: usize = 1;
#[cfg(not(target_arch = "wasm32"))]
unsafe extern "C" {
    fn pb_syscall(
        context: *mut Context,
        operation: u32,
        a: u64,
        b: u64,
        c: u64,
        d: u64,
        e: u64,
        f: u64,
    ) -> i64;
}
#[cfg(target_arch = "wasm32")]
mod wasm {
    #[link(wasm_import_module = "playbit")]
    unsafe extern "C" {
        pub fn pb_syscall(operation: u32, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64;

    }
}
/// # Safety
/// Context and slot pointers obey the selected operation's borrowing contract.
#[inline]
pub(super) unsafe fn invoke(context: *mut Context, operation: u32, arguments: [u64; 6]) -> i64 {
    let [a, b, c, d, e, f] = arguments;
    #[cfg(target_arch = "wasm32")]
    {
        if context as usize != CONTEXT_TOKEN {
            return Error::InvalidArgument as i64;
        }
        // SAFETY: caller-supplied guest offsets remain valid for the call.
        unsafe { wasm::pb_syscall(operation, a, b, c, d, e, f) }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // SAFETY: forwards the caller's live context and borrowed pointer contract.
        unsafe { pb_syscall(context, operation, a, b, c, d, e, f) }
    }
}
