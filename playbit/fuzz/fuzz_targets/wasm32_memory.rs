#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|bytes: &[u8]| {
    playbit::runtime::syscall::fuzz::wasm32_memory(bytes);
    playbit::runtime::syscall::fuzz::wasm32_outputs(bytes);
    playbit::runtime::syscall::fuzz::wasm32_schema(bytes);
});
