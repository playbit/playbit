#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|bytes: &[u8]| {
    playbit::runtime::syscall::fuzz::wasm_operations(bytes);
});
