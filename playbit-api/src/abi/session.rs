//! Current session metadata and session channel calls.
use super::{operations::Context, syscall::Opcode};

/// Set when the session's invitation seed is available to the guest.
pub const HAS_KEY_SECRET: u32 = 1;
/// A fixed-width base58 invitation code holds all 128 bits of the seed.
pub const CODE_CHARS: usize = 22;
pub const CODE_ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Versioned output of session.info. The key secret is a bearer credential.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SessionInfo {
    pub size: u32,
    pub written: u32,
    pub flags: u32,
    pub reserved: u32,
    pub internal_id: [u8; 32],
    pub key_secret: [u8; 16],
}

impl Default for SessionInfo {
    fn default() -> Self {
        Self {
            size: size_of::<Self>() as u32,
            written: 0,
            flags: 0,
            reserved: 0,
            internal_id: [0; 32],
            key_secret: [0; 16],
        }
    }
}

super::record::record!(SessionInfo, 64, { flags: u32, reserved: u32, internal_id: [u8; 32], key_secret: [u8; 16] });

/// Format a session's key secret for the fragment of its invitation URL.
pub fn session_code(key_secret: &[u8; 16]) -> String {
    let mut value = u128::from_be_bytes(*key_secret);
    let mut code = [b'1'; CODE_CHARS];
    for slot in code.iter_mut().rev() {
        *slot = CODE_ALPHABET[(value % 58) as usize];
        value /= 58;
    }
    assert_eq!(value, 0, "fixed-width base58 code holds the complete seed");
    code.iter().map(|&byte| byte as char).collect()
}

/// Read a versioned session record. A context outside a session returns NotFound.
/// # Safety
/// The context must be live and output must lend its full declared writable capacity.
#[allow(unsafe_code, reason = "Typed session operation on the scalar dispatcher.")]
pub unsafe fn pb_session_info(context: *mut Context, output: *mut SessionInfo) -> i64 {
    // SAFETY: the caller lends the initialized output record for this call.
    unsafe { call(context, Opcode::SessionInfo, [output as usize as u64, 0, 0, 0, 0, 0]) }
}

/// # Safety
/// Live callback context, stable inputs and disjoint initialized outputs are required.
#[allow(unsafe_code, reason = "Typed adapter to the checked scalar dispatcher.")]
pub(crate) unsafe fn call(context: *mut Context, operation: Opcode, slots: [u64; 6]) -> i64 {
    // SAFETY: callers uphold the operation's borrowed pointer contract.
    unsafe { super::invocation::invoke(context, operation as u32, slots) }
}
