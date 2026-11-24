import { encodeTextInput, MAX_TEXT_EVENT_BYTES } from "./text-input.ts";
import { ABI as L } from './guest-layout.ts';
import { encodeId, decodeId } from './identifier.ts';
import { createWork } from "../tests/work-fixture.mjs";
import { GuestWork } from "./guest-work.ts";
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { guestOperations } from "./guest-operations.ts";
import { bindOperations, supportsDeadline } from "./guest-imports.ts";
import { prepareGuest } from "./updates.ts";
import { decodeScene } from "./scene.ts";
import { bootstrap, createHost, entryId, account, key } from "../tests/host-fixture.mjs";
const todo = await readFile(new URL("../../../../target/wasm32-unknown-unknown/release/todo.wasm", import.meta.url));
async function host(existing, shared = false) {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x10)));
    if (shared) {
        const resolver = await createHost({ ...profile, local: null });
        const request = new TextEncoder().encode(JSON.stringify({ app: profile.session.app, session: profile.session.id }));
        const addresses = JSON.parse(new TextDecoder().decode(resolver.raw('pb_scope_addresses', request, request.length)));
        profile.session.root_volume = addresses.world;
        profile.session.shared_volume = addresses.shared;
        profile.root = { ...profile.root, id: addresses.shared, name: 'Shared', share_key_with_service: true, personal: false };
    }
    if (existing) profile.local = null;
    const result = await createHost(profile, existing?.stores);
    return { exports: result.host.exports, stores: result.stores };
}
const parts = handle => [handle >> 32n, Number(handle & 0xffffffffn)];
function initialize(memory, pointer, size) { const data = new DataView(memory.buffer); data.setUint32(pointer, size, true); data.setUint32(pointer + 4, 0, true); }
function withVolumes(host) {
    host.pb_web_result = () => 4096;
    host.pb_web_volume_check = () => 0; host.pb_web_volume_authorize = () => 0; host.pb_web_entry_authorize = () => 0;
    host.pb_web_volumes = () => {
        const data = new DataView(host.memory.buffer);
        new Uint8Array(host.memory.buffer, 4096, 144).fill(0);
        initialize(host.memory, 4096, 64); data.setUint32(4100, 60, true); data.setBigUint64(4104, 1n, true);
        data.setUint32(4112, 4160, true); data.setUint32(4116, 1, true); data.setUint32(4120, 1, true); data.setUint32(4124, 80, true);
        data.setUint32(4140, 16, true);
        initialize(host.memory, 4160, 80); data.setUint32(4164, 80, true);
        data.setUint32(4168, 1, true); data.setUint32(4172, 1, true); data.setUint32(4208, 3, true);
        return 0;
    };
    return host;
}
function adapter(memory = new WebAssembly.Memory({ initial: 4 }), host) {
    const events = [], logs = [];
    let calls = 0;
    const work = host?.pb_web_guest_begin ? new GuestWork(events, () => host) : createWork(events); work.beginTurn();
    const services = { memory: () => memory, events, work, files: () => ({ cleanup: async () => {} }), log: text => logs.push(text), submit() {}, host: () => { calls++; return host; } };
    return { operations: guestOperations(services), services, events, logs, memory, calls: () => calls };
}

test("named web adapters run the compiled TODO, commit a toggle, and reopen it", async () => {
    const database = await host(undefined, true);
    async function app(database, invalidateQuery = false) {
        let instance, scene;
        let revision = 0;
        const events = [], logs = [];
        const work = new GuestWork(events, () => database.exports); work.reset();
        work.queue({ kind: 1, target: 0, data: new Uint8Array() });
        const exports = { ...database.exports, pb_web_object_query(...args) {
            const result = database.exports.pb_web_object_query(...args);
            if (invalidateQuery && result === 0) {
                // Opening a lazily requested database changes the captured cut
                // while the app's first query is still pending.
                invalidateQuery = false; revision++;
                work.queue({ kind: 4, target: 0, data: new Uint8Array() });
            }
            return result;
        } };
        const operations = guestOperations({ memory: () => instance.exports.memory, host: () => exports, revision: () => String(revision), events, work, files: () => ({ cleanup: async () => {} }), log: text => logs.push(text), submit: bytes => { scene = decodeScene(bytes); } });
        instance = await prepareGuest(todo, operations);
        operations.refreshVolumes();
        const turn = () => { work.beginTurn(); try { instance.exports.pb_main(); } finally { work.endTurn(); operations.data.flushReady(); } };
        const settle = async () => {
            for (let i = 0; i < 32; i++) {
                if (operations.data.waiting) await operations.data.wait();
                if (events.length) turn();
                else if (!operations.data.pending) return;
                else throw new Error("Readiness was consumed without taking its result");
            }
            throw new Error("Fixture did not settle");
        };
        turn(); await settle();
        return { events, work, logs, turn, settle, scene: () => scene };

    }
    const first = await app(database, true);
    assert(first.scene().controls.has(100), JSON.stringify({scene:first.scene().root, logs:first.logs}));
    assert.equal(first.scene().controls.get(100).state, 0);
    first.work.queue({ kind: 14, target: 1, data: encodeTextInput("Web phone draft", [{start:0,end:0,inserted_len:15}]) });
    first.turn(); await first.settle();
    // A mobile keyboard changes the viewport, not the host's text-input support.
    for (const height of [600, 331, 1, 600]) {
        const bytes = new Uint8Array(16), surface = new DataView(bytes.buffer);
        surface.setUint32(0, 343, true); surface.setUint32(4, height, true);
        surface.setFloat32(8, 3, true); surface.setUint32(12, 6, true);
        first.work.queue({ kind: 15, target: 0, data: bytes });
        first.turn(); await first.settle();
        assert.equal(first.scene().controls.get(1)?.kind, "input");
        assert.equal(first.scene().controls.get(1).value, "Web phone draft");
        assert.equal(first.scene().controls.get(2)?.kind, "button");
        assert.equal(first.scene().controls.get(100).state, 0);
    }
    first.work.queue({ kind: 2, target: 100, data: new Uint8Array() });
    first.turn(); await first.settle();
    assert.equal(first.scene().controls.get(100).state, 1);
    assert.deepEqual(first.logs, ["TODO changed"]);
    const reopened = await app(await host(database, true));
    assert.equal(reopened.scene().controls.get(100).state, 1);
});

test("event probes and malformed destinations preserve the queue and output bytes", () => {
    const { operations, services, events, memory } = adapter();
    const output = new Uint8Array(memory.buffer); output.fill(0xa5); initialize(memory, 16, 24);
    const before = output.slice();
    services.work.queue({ kind: 2, target: 100, data: Uint8Array.of(1, 2, 3) });
    assert.equal(operations.pb_event_poll(16, 64, 2), 3);
    assert.equal(events.length, 1); assert.deepEqual(output, before);
    assert.equal(operations.pb_event_poll(16, 20, 3), -3);
    assert.equal(operations.pb_event_poll(17, 64, 3), -3);
    assert.equal(operations.pb_event_poll(0, 64, 3), -3);
    assert.equal(operations.pb_event_poll(16, 64, MAX_TEXT_EVENT_BYTES + 1), -9);
    assert.equal(events.length, 1); assert.deepEqual(output, before);
    memory.grow(1);
    assert.equal(operations.pb_event_poll(16, 64, 3), 3);
    assert.equal(events.length, 0);
    assert.deepEqual([...new Uint8Array(memory.buffer, 64, 3)], [1, 2, 3]);
    assert.equal(new DataView(memory.buffer).getBigUint64(24, true), services.work.epoch);
});

function entry(memory, pointer = 1024) {
    const bytes = new Uint8Array(memory.buffer, pointer, 32), data = new DataView(memory.buffer);
    bytes.fill(0); initialize(memory, pointer, 32); data.setUint32(pointer + 8, 1, true); data.setUint32(pointer + 12, 1, true);
    bytes.set(Buffer.from(decodeId(entryId, 15)), 16); return pointer;
}
function change(memory, text = 256) {
    const data = new DataView(memory.buffer); initialize(memory, 64, 40); initialize(memory, 128, 80);
    data.setUint32(88, 128, true); data.setUint32(92, 1, true); data.setUint32(96, 80, true); data.setUint32(100, 1, true);
    data.setUint32(136, 1, true); data.setUint32(140, 4, true); data.setUint32(200, text, true);
}
test('queued account-dependent mutations recheck authority before using cached mounts', async () => {
    let paused = false, writes = 0;
    const hostMemory = new WebAssembly.Memory({ initial: 4 });
    const a = adapter(undefined, withVolumes({ memory: hostMemory, pb_input: () => 8,
        pb_web_kv_put() { writes++; return 0; } }));
    a.services.admitOperation = () => paused ? -2 : 0;
    a.operations.refreshVolumes();
    const memory = a.memory, key = new TextEncoder().encode('key');
    new Uint8Array(memory.buffer, 256, key.length).set(key);
    const slot = a.operations.pb_kv_put_begin(entry(memory), 256, key.length, 512, 0); assert(slot > 0n);
    paused = true;
    while (a.operations.data.waiting) await a.operations.data.wait();
    assert.equal(writes, 0);
    assert.equal(a.operations.pb_kv_put_take(...parts(slot)), -2);
    assert.equal(a.operations.pb_kv_put_begin(entry(memory), 256, key.length, 512, 0), -2);
    assert.equal(writes, 0);
});
test("creation rejects invalid nested records before invoking the host", () => {
    const a = adapter(), data = new DataView(a.memory.buffer);
    change(a.memory);
    assert.equal(a.operations.pb_object_create_begin(entry(a.memory), 32, 64, 1, 39), -3);
    assert.equal(a.operations.pb_object_create_begin(entry(a.memory), 32, 0, 1, 40), -3);
    data.setUint32(92, 4097, true);
    assert.equal(a.operations.pb_object_create_begin(entry(a.memory), 32, 64, 1, 40), -9);
    assert.equal(a.calls(), 0);
});

test("C record inputs relocate only referenced data, including nested UTF-8", async () => {
    const memory = new WebAssembly.Memory({ initial: 256 }), hostMemory = new WebAssembly.Memory({ initial: 4 });
    let staged = 0;
    const host = { memory: hostMemory, pb_input: length => { staged = length; return 1024; }, pb_output: () => 8,
        pb_web_object_create(entry, type, fields, count) {
            assert.equal(count, 1);
            const data = new DataView(hostMemory.buffer, 1024, staged);
            assert.equal(data.getUint32(entry, true), 32); assert.equal(data.getUint32(entry + 8, true), 1); assert.equal(data.getUint32(entry + 12, true), 1);
            assert.equal(data.getUint8(type), 9);
            const values = data.getUint32(fields + 24, true), text = data.getUint32(values + 72, true);
            assert.equal(values % 8, 0);
            assert.equal(new TextDecoder().decode(new Uint8Array(hostMemory.buffer, 1024 + text, 4)), "\u00e5\u00e5");
            new Uint8Array(hostMemory.buffer, 8, 16).fill(42); return 0;
        } };
    const a = adapter(memory, withVolumes(host)), data = new DataView(memory.buffer);
    data.setUint8(32, 9); change(memory, 0xffff00);
    new Uint8Array(memory.buffer, 0xffff00, 4).set(new TextEncoder().encode("\u00e5\u00e5"));
    a.operations.refreshVolumes();
    const pendingHandle = a.operations.pb_object_create_begin(entry(a.memory), 32, 64, 1, 40); assert(pendingHandle > 0n);
    await a.operations.data.wait();
    assert.equal(a.operations.pb_object_create_poll(...parts(pendingHandle), 512), 0);
    assert(staged < 256, `copied ${staged} bytes from a 16 MiB guest`);
    assert(new Uint8Array(memory.buffer, 512, 16).every(byte => byte === 42));
});

test("variable result probes update counts only; invalid array overlap never reaches the host", async () => {
    const database = await host(), a = adapter(undefined, database.exports), data = new DataView(a.memory.buffer);
    a.operations.refreshVolumes();
    const output = new Uint8Array(a.memory.buffer); output.fill(0xa5);
    new Uint8Array(a.memory.buffer, 32, 64).fill(0); initialize(a.memory, 32, 64); data.setUint32(60, 80, true); data.setUint32(76, 16, true);
    assert.equal(a.operations.pb_volumes(32), -11);
    assert.equal(data.getUint32(56, true), 1); assert.equal(data.getUint32(72, true), 1); assert.equal(data.getUint32(88, true), 8);
    data.setUint32(48, 32, true); data.setUint32(52, 2, true);
    const before = output.slice();
    assert.equal(a.operations.pb_volumes(32), -3); assert.equal(a.calls(), 1);
    assert.deepEqual(output.subarray(48), before.subarray(48));
});

for (const language of ["c", "rust"]) test(`all independently compiled ${language} operations link through web scalar bindings`, async () => {
    // Independently compiled probes check lowering, including unsigned clock bits.
    const module = await WebAssembly.compile(await readFile(new URL(`../../../../target/wasm-lowering/${language}.wasm`, import.meta.url)));
    const fixture = await readFile(new URL("../../native/wasm-probe/imports.txt", import.meta.url), "utf8");
    assert.equal(WebAssembly.Module.imports(module).length, 1);
    const names = fixture.trim().split("\n").map(line => line.split(" ")[0]);
    const calls = [], callbacks = Object.fromEntries(names.map(name => [name, (...args) => {
        calls.push([name, args]);
        return name === "pb_clock_read" ? 0x8877665544332201n
            : name.endsWith("_begin") || name === "pb_file_open_take" || name === "pb_file_list_open_take"
                || name === "pb_audio_open_take" || name === "pb_sound_open_pcm" ? 0x7edcba9889abcdefn : -7;
    }]));
    const deadline = supportsDeadline();
    const instance = await WebAssembly.instantiate(module, { playbit: bindOperations(module, callbacks, deadline) });
    const probe = deadline ? WebAssembly.promising(instance.exports.probe_imports) : instance.exports.probe_imports;
    assert.equal(await probe(), 0);
    const supported = names.filter(name => deadline || name !== "pb_event_poll_deadline");
    assert.equal(calls.length, supported.length);
    assert.deepEqual(new Set(calls.map(([name]) => name)), new Set(supported));
    assert.deepEqual(calls.find(([name]) => name === "pb_host_limit")[1], [0x80000001]);
    const generation = calls.find(([name]) => name === "pb_update_defer")[1][0];
    assert.equal(typeof generation, "bigint");
    assert.equal(BigInt.asUintN(64, generation), 0x8877665544332201n);
    const trace = calls.find(([name]) => name === "pb_trace_record")[1];
    assert.equal(trace[1], 0x80000001);
    assert.deepEqual(trace.slice(2), [0x8877665544332202n, 0x8877665544332203n]);
    assert.deepEqual(calls.find(([name]) => name === "pb_clock_read")[1], []);
    assert.deepEqual(calls.find(([name]) => name === "pb_audio_open_begin")[1], [0x80000001]);
    assert.deepEqual(calls.find(([name]) => name === "pb_audio_open_take")[1], [0x7edcba98n, 0x89abcdef]);
    assert.deepEqual(calls.find(([name]) => name === "pb_sound_play")[1],
        [0x7edcba98n, 0x89abcdef, 0x80000001, 0x80000002, 0x8877665544332201n, 0x8877665544332202n, 0x80000003]);
    assert.deepEqual(calls.find(([name]) => name === "pb_sound_seek")[1],
        [0x7edcba98n, 0x89abcdef, 0x80000001, 0x8877665544332202n]);
    assert.deepEqual(calls.find(([name]) => name === "pb_sound_set_balance")[1],
        [0x7edcba98n, 0x89abcdef, 0x80000001, 0x80000002]);
});

test("maximum nested C inputs admit alignment padding without exceeding the staging bound", async () => {
    const memory = new WebAssembly.Memory({ initial: 32 }), hostMemory = new WebAssembly.Memory({ initial: 32 });
    const data = new DataView(memory.buffer);
    let staged = 0, calls = 0;
    const host = { memory: hostMemory, pb_input: length => { staged = length; return 8; }, pb_web_object_create: () => { calls++; return -7; } };
    for (let i = 0; i < 256; i++) {
        const field = 64 + i * 40, values = 16384 + i * 16 * 80;
        initialize(memory, field, 40); data.setUint16(field + 8, i, true); data.setUint32(field + 24, values, true);
        data.setUint32(field + 28, 16, true); data.setUint32(field + 32, 80, true); data.setUint32(field + 36, 1, true);
        for (let j = 0; j < 16; j++) { initialize(memory, values + j * 80, 80); data.setUint32(values + j * 80 + 8, 1, true); }
        const last = values + 15 * 80;
        data.setUint32(last + 12, i === 255 ? 1048576 - 255 : 1, true);
        data.setUint32(last + 72, 524288 + i, true);
    }
    const a = adapter(memory, withVolumes(host)); a.operations.refreshVolumes();
    const pendingHandle = a.operations.pb_object_create_begin(entry(a.memory, 2097088), 8, 64, 256, 40); assert(pendingHandle > 0n);
    await a.operations.data.wait();
    assert.equal(a.operations.pb_object_create_poll(...parts(pendingHandle), 2097120), -7);
    assert.equal(calls, 1);
    assert(staged > 1048576 + 256 * 40 + 4096 * 80 + 128, "this input requires the per-field alignment allowance");
    assert(staged <= 1048576 + 256 * (40 + 7) + 4096 * 80 + 128);
});

test("host platform identifies the web without memory or data service access", async () => {
    const { operations, calls } = adapter();
    assert.equal(operations.pb_host_platform(), 3); // PB_PLATFORM_WEB.
    assert.equal(calls(), 0);
    // The real scalar binding is callable without linear memory.
    const module = new WebAssembly.Module(Uint8Array.of(0,97,115,109,1,0,0,0));
    const entries = bindOperations(module, operations);
    assert.equal(entries.pb_syscall(12, 0n, 0n, 0n, 0n, 0n, 0n), 3n);
    assert.equal(calls(), 0);

});

test("session.info reads only the current binding and preserves unknown output fields", () => {
    const { services, operations, memory } = adapter();
    const module = new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0));
    const { pb_syscall } = bindOperations(module, operations);
    const read = (extra = 0n) => pb_syscall(0x163, 16n, extra, 0n, 0n, 0n, 0n);
    const bytes = new Uint8Array(memory.buffer, 16, 80).fill(0xa5);
    const data = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    initialize(memory, 16, bytes.length);
    const original = bytes.slice();
    assert.equal(read(), -1n);
    assert.deepEqual(bytes, original);
    const internalId = new Uint8Array(32).fill(0x36);
    const keySecret = new Uint8Array(16).fill(0x19);
    services.identity = () => ({ session: internalId });
    services.sessionKeySecret = () => keySecret;
    assert.equal(read(1n), -3n);
    assert.deepEqual(bytes, original);
    assert.equal(read(), 0n);
    assert.equal(data.getUint32(0, true), 80);
    assert.equal(data.getUint32(4, true), 64);
    assert.equal(data.getUint32(8, true), 1);
    assert.equal(data.getUint32(12, true), 0);
    assert.deepEqual(bytes.slice(16, 48), internalId);
    assert.deepEqual(bytes.slice(48, 64), keySecret);
    assert.deepEqual(bytes.slice(64), original.slice(64));
    services.sessionKeySecret = () => undefined;
    assert.equal(read(), 0n);
    assert.equal(data.getUint32(8, true), 0);
    assert.deepEqual(bytes.slice(48, 64), new Uint8Array(16));
    services.identity = () => undefined;
    assert.equal(read(), -1n);
    assert.equal(data.getUint32(4, true), 0);
});

test("web capability discovery advertises owned File operations", async () => {
    const database = await host(), a = adapter(undefined, database.exports);
    const data = new DataView(a.memory.buffer);
    initialize(a.memory, 16, 36);
    data.setUint32(24, 128, true); data.setUint32(28, 64, true); data.setUint32(36, 16, true);
    for (let i = 0; i < 64; i++) initialize(a.memory, 128 + i * 16, 16);
    data.setUint32(40, 2048, true); data.setUint32(44, 2048, true);
    assert.equal(a.operations.pb_capabilities(16), 0);
    const names = Array.from({ length: data.getUint32(32, true) }, (_, i) => {
        const offset = data.getUint32(136 + i * 16, true), length = data.getUint32(140 + i * 16, true);
        return new TextDecoder().decode(new Uint8Array(a.memory.buffer, 2048 + offset, length));
    });
    assert(names.includes("host.platform"));
    assert(names.includes("host.limit"));
    assert(names.includes("session.info"));
    for (const name of ['file.open.begin', 'file.open.take', 'file.status.begin', 'file.status.take',
        'file.close.begin', 'file.close.take', 'file_list.open.begin', 'file_list.open.take',
        'file_list.next.begin', 'file_list.next.take', 'file_list.close', 'file.dispose']) {
        assert(names.includes(name), name);
    }
    assert(!names.some(name => name.startsWith("trace.")));
});

test("unknown opcodes and noncanonical arguments fail without effects", async () => {
    let calls = 0;
    const module = new WebAssembly.Module(Uint8Array.of(0,97,115,109,1,0,0,0));
    const entries = bindOperations(module, { pb_log: () => { calls++; return 0; } });
    assert.equal(entries.pb_syscall(0x7fffffff, -1n, -1n, -1n, -1n, -1n, -1n), -7n);
    assert.equal(entries.pb_syscall(2, 0n, 0x100000000n, 0n, 0n, 0n, 0n), -3n);
    assert.equal(entries.pb_syscall(2, 0n, 0n, 0n, 0n, 0n, 1n), -3n);
    assert.equal(calls, 0);
});

test("web limits come from the Rust host through scalar dispatch", async () => {
    const database = await host(), a = adapter(undefined, database.exports);
    const empty = new WebAssembly.Module(Uint8Array.of(0,97,115,109,1,0,0,0));
    const entries = bindOperations(empty, a.operations);
    assert.equal(entries.pb_syscall(13, 1n, 0n, 0n, 0n, 0n, 0n), 64n);
    assert.equal(entries.pb_syscall(13, 2n, 0n, 0n, 0n, 0n, 0n), 1048576n);
    assert.equal(entries.pb_syscall(13, 0xffffffffn, 0n, 0n, 0n, 0n, 0n), -7n);
    const calls = a.calls();
    assert.equal(entries.pb_syscall(13, 0x100000000n, 0n, 0n, 0n, 0n, 0n), -3n);
    assert.equal(entries.pb_syscall(13, 1n, 1n, 0n, 0n, 0n, 0n), -3n);
    assert.equal(a.calls(), calls);
});


test("app tracing is unsupported in web browsers without host or memory effects", () => {
    // Canonical trace.record slots are unsupported; malformed scalar slots fail first.
    const a = adapter();
    const module = new WebAssembly.Module(Uint8Array.of(0,97,115,109,1,0,0,0));
    const entries = bindOperations(module, a.operations);
    const args = [0n, 8n, 0x8877665544332202n, 0x8877665544332203n, 0n, 0n];
    assert.equal(entries.pb_syscall(0xf, ...args), -7n);
    for (const invalid of [args.with(0, 0x100000000n), args.with(1, 0x100000000n), args.with(4, 1n)]) {
        assert.equal(entries.pb_syscall(0xf, ...invalid), -3n);
    }
    assert.equal(a.calls(), 0);
    assert(new Uint8Array(a.memory.buffer).every(byte => byte === 0));
});


test("last-window intent is local, Boolean, immutable, and per instance", () => {
    const first = adapter(), second = adapter();
    assert.equal(first.operations.quitOnLastWindow, true);
    assert.equal(first.operations.pb_quit_on_last_window(2), -3);
    assert.equal(first.operations.quitOnLastWindow, true);
    assert.equal(first.operations.pb_quit_on_last_window(0), 0);
    assert.equal(first.operations.pb_quit_on_last_window(0), 0);
    assert.equal(first.operations.pb_quit_on_last_window(1), -10);
    assert.equal(first.operations.quitOnLastWindow, false);
    assert.equal(second.operations.pb_quit_on_last_window(1), 0);
    assert.equal(second.operations.quitOnLastWindow, true);
    assert.equal(first.calls() + second.calls(), 0);
});


test("replacement resets last-window intent only after the old owner drains", async () => {
    const { operations, services } = adapter(undefined, {});
    for (const choice of [1, 0, 1]) {
        assert.equal(operations.pb_quit_on_last_window(choice), 0);
        assert.equal(operations.pb_quit_on_last_window(1 - choice), -10);
        const slot = operations.data.begin(9, 1, async () => ({ status: 0, value: {} }), () => 0);
        assert.throws(() => operations.reset(), /accepted work/);
        assert.equal(operations.quitOnLastWindow, Boolean(choice));
        assert.equal(operations.pb_quit_on_last_window(1 - choice), -10);
        await operations.data.wait();
        assert.equal(operations.data.take(...parts(slot), 9, () => 0), 0);
        services.work.endTurn();
        operations.reset();
        assert.equal(operations.quitOnLastWindow, true);
        services.work.beginTurn();
    }
});

test("owned root enumeration is atomic, independent, bounded, and revoked on reload", async () => {
    let io = 0;
    const runtime = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0x10))), undefined, () => 1000,
        imports => Object.fromEntries(Object.entries(imports).map(([name, method]) => [name, (...args) => { io++; return method(...args); }])));
    const personalProfile = bootstrap(encodeId(new Uint8Array(16).fill(0x30)), { personal: true });
    const personal = await createHost(personalProfile);
    for (let slot = 1; slot <= 2; slot++) runtime.stores.set(slot + 2, personal.stores.get(slot));
    runtime.call('pb_register_personal', { volume: personalProfile.root, key, replica_ids: [7, 8] });
    runtime.call('pb_select_personal', { session: encodeId(new Uint8Array(32).fill(3)), grants: [{ volume: personalProfile.root.id, write: true, incarnation: encodeId(new Uint8Array(16).fill(1)) }] });
    const a = adapter(undefined, runtime.host.exports);
    const ops = a.operations;
    ops.refreshVolumes(); io = 0;
    const memory = a.memory, data = new DataView(memory.buffer);
    initialize(memory, 32, 64); data.setUint32(60, 80, true); data.setUint32(76, 16, true);
    // Repeated probes consume no File credits, and include the coherent revision.
    for (let i = 0; i < 300; i++) assert.equal(ops.pb_volumes(32), -11);
    assert.equal(data.getBigUint64(40, true), 0n, "short output leaves revision unchanged");
    assert.equal(data.getUint32(36, true), 0);
    const revision = new DataView(runtime.host.exports.memory.buffer).getBigUint64(runtime.host.exports.pb_web_result() + 8, true);
    assert(revision > 0n);
    data.setUint32(48, 128, true); data.setUint32(52, 2, true);
    initialize(memory, 128, 80); initialize(memory, 208, 80);
    data.setUint32(64, 512, true); data.setUint32(68, 2, true);
    initialize(memory, 512, 16); initialize(memory, 528, 16);
    data.setUint32(80, 1024, true); data.setUint32(84, 24, true);
    const acquired = [];
    for (let i = 0; i < 255; i++) {
        assert.equal(ops.pb_volumes(32), 0);
        assert.equal(data.getBigUint64(40, true), revision);
        for (let row = 0; row < 2; row++) {
            const offset = 128 + row * 80;
            assert.equal(data.getUint32(offset + 52, true) & 0xff00, 0x100);
            acquired.push([BigInt(data.getUint32(offset + 60, true)), data.getUint32(offset + 56, true)]);
            assert(new Uint8Array(memory.buffer, offset + 64, 16).some(byte => byte));
        }
    }
    assert.equal(new Set(acquired.map(handle => handle.join(':'))).size, 510);
    // Two private roots plus 510 guest aliases use the shared 512 ceiling.
    assert.equal(ops.pb_volumes(32), -9);
    assert.equal(ops.pb_file_dispose(...acquired[0]), 0);
    assert.equal(ops.pb_volumes(32), -9); // Failure cannot leak the one free slot.
    assert.equal(ops.pb_file_dispose(...acquired[1]), 0);
    assert.equal(ops.pb_volumes(32), 0);
    assert.equal(ops.pb_file_dispose(...acquired[0]), -8);
    for (const file of acquired.slice(2)) assert.equal(ops.pb_file_dispose(...file), 0);
    const last = [BigInt(data.getUint32(188, true)), data.getUint32(184, true)];
    assert.equal(io, 0, "enumeration and disposal use no storage or platform imports");
    a.services.work.endTurn(); ops.reset(); ops.refreshVolumes();
    assert.equal(ops.pb_file_dispose(...last), -8);
    assert.equal(ops.pb_volumes(32), 0);
    assert.equal(a.services.work.epoch > 1n, true);
    assert(data.getBigUint64(40, true) > revision);
});

test('volume observation catches up once, replaces queued notices and quiesces on reset or close', () => {
    const memory = new WebAssembly.Memory({ initial: 4 });
    const host = withVolumes({ memory, pb_input: () => 8,  });
    const enumerate = host.pb_web_volumes;
    let revision = 1n;
    host.pb_web_volumes = () => { enumerate(); new DataView(memory.buffer).setBigUint64(4104, revision, true); return 0; };
    const { operations, services, events } = adapter(memory, host);
    operations.refreshVolumes();
    assert.equal(operations.pb_volumes_observe(0n), -3);
    assert.equal(operations.pb_volumes_observe(2n), -3);
    assert.equal(operations.pb_volumes_observe(1n), 0); assert.equal(events.length, 0);
    revision = 2n; operations.refreshVolumes();
    assert.equal(events.length, 1); assert.equal(events[0].kind, 17);
    revision = 3n; operations.refreshVolumes(); assert.equal(events.length, 1);
    assert.equal(operations.pb_volumes_observe(1n), 0); assert.equal(events.length, 1);
    assert.equal(new DataView(events[0].data.buffer).getBigUint64(0, true), 3n);
    initialize(memory, 64, 24);
    assert.equal(operations.pb_event_poll(64, 128, 8), 8); assert.equal(events.length, 0);
    assert.equal(new DataView(memory.buffer).getUint32(80, true), 17);
    assert.equal(new DataView(memory.buffer).getBigUint64(128, true), 3n);
    revision = 4n; operations.refreshVolumes(); assert.equal(events.length, 0, 'The watch is one-shot');
    assert.equal(operations.pb_volumes_observe(4n), 0);
    services.work.endTurn(); operations.reset(); services.work.beginTurn();
    revision = 5n; operations.refreshVolumes(); assert.equal(events.length, 0, 'Reload clears registration');
    assert.equal(operations.pb_volumes_observe(4n), 0); assert.equal(events.length, 1);
    services.work.endTurn(); services.work.deactivate(); assert.equal(events.length, 0);
    services.work.beginTurn();
    assert.equal(operations.pb_volumes_observe(5n), -10);
    revision = 6n; operations.refreshVolumes(); assert.equal(events.length, 0);
});

test('a full event queue retains volume observation until a poll makes space', () => {
    const memory = new WebAssembly.Memory({ initial: 4 });
    const host = withVolumes({ memory, pb_input: () => 8 });
    const enumerate = host.pb_web_volumes;
    let revision = 1n;
    host.pb_web_volumes = () => { enumerate(); new DataView(memory.buffer).setBigUint64(4104, revision, true); return 0; };
    const { operations, services, events } = adapter(memory, host);
    operations.refreshVolumes(); assert.equal(operations.pb_volumes_observe(1n), 0);
    let full = false;
    for (let count = 0; count < 1024; count++) {
        try { services.work.queue({ kind: 14, target: 1, data: new Uint8Array() }); }
        catch (error) { assert.equal(error.status, -9); full = true; break; }
    }
    assert(full); const count = events.length;
    revision = 2n; operations.refreshVolumes(); assert.equal(events.length, count);
    assert.equal(services.work.failed, false);
    initialize(memory, 64, 24);
    assert.equal(operations.pb_event_poll(64, 128, 8), 0);
    assert.equal(events.length, count); assert.equal(events.at(-1).kind, 17);
    assert.equal(new DataView(events.at(-1).data.buffer).getBigUint64(0, true), 2n);
});

test('identity and presence publish names in complete extended records and preserve baseline strides', () => {
    const a = adapter(), data = new DataView(a.memory.buffer), encoder = new TextEncoder();
    const name = '梅'.repeat(85) + '!';
    a.services.identity = () => ({ account: new Uint8Array(16).fill(1), device: new Uint8Array(16).fill(2),
        instance: new Uint8Array(16).fill(3), session: new Uint8Array(32).fill(4), displayName: name });
    a.services.channels = () => ({ participants: [5, 6].map((value, i) => ({
        source: new Uint8Array(80).fill(value), displayName: i ? '' : name,
    })) });
    for (const [size, written] of [[104, 104], [128, 104], [359, 104], [368, 360]]) {
        initialize(a.memory, 128, size); new Uint8Array(a.memory.buffer, 128 + written, Math.max(8, size - written)).fill(7);
        assert.equal(a.operations.pb_identity(128), 0);
        assert.equal(data.getUint32(128, true), size); assert.equal(data.getUint32(132, true), written);
        assert.equal(data.getBigUint64(136, true), a.services.work.epoch); assert.equal(data.getUint8(144), 1); assert.equal(data.getUint8(160), 2);
        assert.equal(data.getUint8(176), 3); assert.equal(data.getUint8(192), 4); assert.equal(data.getUint32(224, true), 2);
        if (written > 104) assert.deepEqual(new Uint8Array(a.memory.buffer, 232, 256), encoder.encode(name));
        assert(new Uint8Array(a.memory.buffer, 128 + written, Math.max(8, size - written)).every(byte => byte === 7));
    }
    a.services.identity = () => undefined;
    assert.equal(a.operations.pb_identity(128), -1); assert.equal(data.getUint32(132, true), 0);
    initialize(a.memory, 864, 352);
    new Uint8Array(a.memory.buffer, 600, 256).fill(8); new Uint8Array(a.memory.buffer, 1208, 8).fill(9);
    for (const size of [88, 96, 343]) {
        initialize(a.memory, 512, size);
        assert.equal(a.operations.pb_presence(512, 2, 352, 16), 0); assert.equal(data.getUint32(16, true), 2);
        assert.equal(data.getUint32(512, true), size);
        assert.equal(data.getUint32(516, true), 88); assert.equal(data.getUint32(868, true), 344);
        assert(new Uint8Array(a.memory.buffer, 520, 80).every(byte => byte === 5));
        assert(new Uint8Array(a.memory.buffer, 872, 80).every(byte => byte === 6));
        assert(new Uint8Array(a.memory.buffer, 952, 256).every(byte => byte === 0));
        assert(new Uint8Array(a.memory.buffer, 600, 256).every(byte => byte === 8));
        assert(new Uint8Array(a.memory.buffer, 1208, 8).every(byte => byte === 9));
    }
    initialize(a.memory, 512, 344);
    assert.equal(a.operations.pb_presence(512, 2, 352, 16), 0);
    assert.deepEqual(new Uint8Array(a.memory.buffer, 600, 256), encoder.encode(name));
    assert.equal(a.operations.pb_presence(512, 1, 352, 16), -11);
    assert.equal(data.getUint32(16, true), 2); assert.equal(data.getUint32(516, true), 0);
    data.setUint32(864, 353, true); data.setUint32(16, 99, true);
    const before = new Uint8Array(a.memory.buffer).slice();
    assert.equal(a.operations.pb_presence(512, 2, 352, 16), -3); assert.deepEqual(new Uint8Array(a.memory.buffer), before);
});

test('identity and presence changes coalesce and survive a saturated event queue', () => {
    const { operations, services, memory, events } = adapter();
    for (;;) {
        try { services.work.queue({ kind: 14, target: 1, data: new Uint8Array() }); }
        catch (error) { assert.equal(error.status, -9); break; }
    }
    for (let i = 0; i < 3; i++) {
        operations.environmentChanged(0x80000015);
        operations.environmentChanged(5);
    }
    initialize(memory, 64, 24);
    for (let i = 0; i < 2; i++) assert.equal(operations.pb_event_poll(64, 128, 0), 0);
    assert.deepEqual(events.slice(-2).map(({ kind, target, data }) => [kind, target, data.length]), [[0x80000015, 0, 0], [5, 0, 0]]);
});
