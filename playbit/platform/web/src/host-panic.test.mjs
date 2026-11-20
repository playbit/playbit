import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

// This export exists only in the testing-feature artifact. Product builds have
// no fault switch. Real Wasm guards also cover JS cleanup's attempted re-entry.
test("engine panic logs a bounded report and every host export refuses re-entry", async () => {
    execFileSync("cargo", ["rustc", "--locked", "--release", "-p", "playbit", "--lib",
        "--crate-type", "cdylib", "--target", "wasm32-unknown-unknown",
        "--no-default-features", "--features", "test-support"], {
        cwd: fileURLToPath(new URL("../../../../", import.meta.url)), stdio: "inherit",
    });
    const module = await WebAssembly.compile(await readFile(new URL("../../../../target/wasm32-unknown-unknown/release/playbit.wasm", import.meta.url)));
    const imports = {};
    for (const { module: name, name: field, kind } of WebAssembly.Module.imports(module)) {
        assert.equal(kind, "function");
        (imports[name] ??= {})[field] = () => { throw Error(`Unexpected I/O: ${name}.${field}`); };
    }
    let host, report;
    imports.opfs.engine_panic = (pointer, length) => {
        assert(length <= 8192);
        report = new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array(host.exports.memory.buffer, pointer, length));
    };
    host = await WebAssembly.instantiate(module, imports);
    assert(host.exports.pb_input(1) > 0); // installs the production hook before engine state
    assert.throws(() => host.exports.pb_test_panic(), WebAssembly.RuntimeError);
    assert.match(report, /W7 injected web engine panic/);
    assert.match(report, /Restart Playbit/);
    const entries = Object.entries(host.exports).filter(([name, value]) => name.startsWith("pb_") && typeof value === "function");
    assert(entries.length >= 37);
    for (const [name, entry] of entries) {
        const args = Array(entry.length).fill(0);
        if (["pb_live_ack", "pb_web_entry_resolve", "pb_web_volume_authorize", "pb_web_volume_check", "pb_web_volumes_acquire", "pb_web_channel_name", "pb_web_channel_close"].includes(name)) args[0] = 0n;
        if (["pb_web_file_finish", "pb_web_file_seal"].includes(name)) args[1] = 0n;
        if (name === "pb_web_file_begin") args[3] = 0n;
        if (["pb_web_file_execute", "pb_web_file_writer_ready", "pb_web_file_write_complete", "pb_web_file_read_complete", "pb_web_file_io_failed", "pb_web_file_seal", "pb_web_file_publish", "pb_web_file_close_failed", "pb_web_file_qualify", "pb_web_file_result", "pb_web_file_check", "pb_web_file_finish", "pb_web_file_release", "pb_web_file_cleaned", "pb_web_guest_ready", "pb_web_guest_poll", "pb_web_guest_take", "pb_web_guest_release", "pb_web_guest_check", "pb_web_guest_abandon", "pb_web_guest_complete"].includes(name)) args[0] = 0n;
        if (name === "pb_web_guest_queue" || name === "pb_web_guest_reject") args[0] = 0n;
        if (name === "pb_web_guest_begin") args[1] = 0n;
        assert.throws(() => entry(...args), WebAssembly.RuntimeError, `${name} re-entered failed engine state`);
    }
});
