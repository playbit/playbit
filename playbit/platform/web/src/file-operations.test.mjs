import { decodeId } from './identifier.ts';
import test from 'node:test';
import assert from 'node:assert/strict';
import { fixture, ownedFiles } from '../tests/file-io-fixture.mjs';
import { GuestWork } from "./guest-work.ts";
import { createWork } from '../tests/work-fixture.mjs';
import { guestOperations } from './guest-operations.ts';
const parts = handle => [handle >> 32n, Number(handle & 0xffffffffn)];

async function setup() {
    const f = await fixture(), api = ownedFiles(f), memory = new WebAssembly.Memory({ initial: 4 }), events = [], failures = [];
    const work = new GuestWork(events, () => api.host); work.beginTurn();
    const ops = guestOperations({ memory: () => memory, host: () => api.host, events, work, log() {}, submit() {},
        files: () => api.backend, fileFailure: error => failures.push(error) });
    const view = () => new DataView(memory.buffer);
    let latest;
    const operation = () => latest;
    for (const name of Object.keys(ops).filter(name => name.endsWith("_begin"))) {
        const begin = ops[name]; ops[name] = (...args) => { latest = begin(...args); return latest; };
    }
    const settle = async () => { while (ops.data.waiting) await ops.data.wait(); };
    const take = async (method, slot, ...args) => {
        const size = { pb_file_close_take: 24, pb_file_status_take: 32, pb_file_list_next_take: 28 }[method];
        if (size) { view().setUint32(args[0], size, true); view().setUint32(args[0] + 4, 0, true); }
        await settle();
        let status = ops[method](...parts(slot), ...args);
        if (status === -6) { await settle(); status = ops[method](...parts(slot), ...args); }
        return status;
    };
    const parts = handle => [handle >> 32n, Number(handle & 0xffffffffn)];
    const open = async (path, flags = 4) => {
        const encoded = new TextEncoder().encode(path); new Uint8Array(memory.buffer, 512, encoded.length).set(encoded);
        assert.equal(ops.pb_file_open_begin(BigInt(api.root[0]), api.root[1], 512, encoded.length, flags) > 0n, true);
        const handle = await take('pb_file_open_take', operation()); assert(handle > 0n);
        return parts(handle);
    };
    return { f, api, memory, ops, view, operation, settle, take, open, failures,
        async close() { ops.data.dispose(); await settle(); work.endTurn(); while (work.events.length) { work.beginTurn(); while (work.poll()) {} work.endTurn(); } assert.equal(api.host.pb_web_guest_reset(), 0); await api.backend.cleanup(); await f.blobs.cleanup(); f.close(); assert.deepEqual(failures, []); } };
}

test('owned guest ABI copies accepted writes, orders status, retains reads through memory growth and retries', async () => {
    // Drive actual Rust handles and disk-backed OPFS through the guest Begin/Take boundary.
    const a = await setup();
    try {
        const writer = await a.open('guest/note');
        new Uint8Array(a.memory.buffer, 512, 4).set([4, 3, 2, 1]);
        assert.equal(a.ops.pb_file_write_begin(...writer, 0n, 512, 4) > 0n, true); const write = a.operation();
        new Uint8Array(a.memory.buffer, 512, 4).fill(0);
        assert.equal(a.ops.pb_file_status_begin(...writer) > 0n, true); const status = a.operation();
        assert.equal(a.ops.pb_file_close_begin(...writer, 0) > 0n, true); const close = a.operation();
        assert.equal(a.ops.pb_file_status_begin(...writer), -8);
        assert.equal(await a.take('pb_file_close_take', close, 256), 0); assert.equal(a.view().getUint32(268, true), 1);
        assert.equal(await a.take('pb_file_status_take', status, 256), 0); assert.equal(a.view().getBigUint64(272, true), 4n);
        assert.equal(await a.take('pb_file_write_take', write), 0);
        const reader = await a.open('guest/note', 1);
        assert.equal(a.ops.pb_file_read_begin(...reader, 0n, 4) > 0n, true); const read = a.operation();
        assert.equal(a.ops.pb_file_read_take(...parts(read), 1024, 2, 72), -6);
        a.memory.grow(1); await a.settle();
        assert.equal(a.ops.pb_file_write_take(...parts(read)), -8);
        assert.equal(a.ops.pb_file_read_take(...parts(read), 72, 8, 72), -3);
        const before = new Uint8Array(a.memory.buffer).slice();
        assert.equal(await a.take('pb_file_read_take', read, 1024, 2, 72), -11);
        new DataView(before.buffer).setUint32(72, 4, true); assert.deepEqual(new Uint8Array(a.memory.buffer), before);
        assert.equal(a.ops.pb_file_read_take(...parts(read), 1024, 4, 72), 0);
        assert.deepEqual([...new Uint8Array(a.memory.buffer, 1024, 4)], [4, 3, 2, 1]);
        assert.equal(a.ops.pb_file_read_take(...parts(read), 1024, 4, 72), -8);
        assert.equal(a.ops.pb_file_read_begin(...reader, 0n, 65537), -9);
        assert.equal(a.ops.pb_file_read_begin(...reader, 0xffffffffffffffffn, 4), -9);
        assert.equal(a.ops.pb_file_read_begin(...reader, 4n, 0) > 0n, true);
        assert.equal(await a.take('pb_file_read_take', a.operation(), 1024, 4, 72), 0); assert.equal(a.view().getUint32(72, true), 0);
    } finally { await a.close(); }
});

for (const cause of [1, 4]) for (const phase of ['in flight', 'retained']) {
    test(`owned guest read withholds bytes ${phase} after authority cause ${cause}`, async () => {
        // The root handle remains allocated; persistent negative evidence must gate both pending and retained results.
        const a = await setup(); let hold;
        try {
            const writer = await a.open('authority');
            new Uint8Array(a.memory.buffer, 512, 4).set([1, 2, 3, 4]);
            assert.equal(a.ops.pb_file_write_begin(...writer, 0n, 512, 4) > 0n, true);
            assert.equal(await a.take('pb_file_write_take', a.operation()), 0);
            assert.equal(a.ops.pb_file_close_begin(...writer, 0) > 0n, true);
            assert.equal(await a.take('pb_file_close_take', a.operation(), 256), 0);
            const reader = await a.open('authority', 1);
            let entry;
            const readVersion = a.f.blobs.readVersion.bind(a.f.blobs);
            a.f.blobs.readVersion = reference => { entry = reference.entry; return readVersion(reference); };
            hold = a.f.hold();
            assert.equal(a.ops.pb_file_read_begin(...reader, 0n, 4) > 0n, true); const read = a.operation();
            await new Promise(setImmediate); assert(entry);
            if (phase === 'retained') { hold.resolve(); await a.settle(); }
            const volume = Buffer.from('10'.repeat(16), 'hex');
            const terminal = Uint8Array.from([...volume, ...(cause === 4 ? decodeId(entry, 15) : []), ...volume, cause]);
            assert(a.f.host.raw('pb_live_terminal', terminal, terminal.length) instanceof Uint8Array);
            const before = new Uint8Array(a.memory.buffer).slice();
            assert.equal(a.ops.pb_file_read_take(...parts(read), 1024, 4, 72), -6);
            hold.resolve(); await a.settle();
            assert.equal(await a.take('pb_file_read_take', read, 1024, 4, 72), -2);
            assert.deepEqual(new Uint8Array(a.memory.buffer), before);
            assert.equal(a.ops.data.pending, 0);
        } finally { hold?.resolve(); await a.close(); }
    });
}

test('guest FileList retains entries across short output retries and rejects invalid ownership generations', async () => {
    // List capture and cursor advancement are independent of the parent handle and output allocation.
    const a = await setup();
    try {
        const root = [BigInt(a.api.root[0]), a.api.root[1]];
        assert.equal(a.ops.pb_file_dispose(root[0] + 0x100000000n, root[1]), -8);
        assert.equal(a.ops.pb_file_list_open_begin(...root, 0) > 0n, true); const open = a.operation();
        assert.equal(a.ops.pb_file_close_begin(...root, 0) > 0n, true); const close = a.operation();
        const listHandle = await a.take('pb_file_list_open_take', open); assert(listHandle > 0n);
        const list = parts(listHandle);
        assert.equal(await a.take('pb_file_close_take', close, 256), 0);
        assert.equal(a.ops.pb_file_list_close(list[0] + 0x100000000n, list[1]), -8);
        assert.equal(a.ops.pb_file_list_next_begin(...list) > 0n, true); const next = a.operation();
        assert.equal(a.ops.pb_file_list_next_begin(...list), -12);
        a.view().setUint32(80, 0, true); a.view().setUint32(84, 0, true);
        assert.equal(await a.take('pb_file_list_next_take', next, 64), -11);
        const required = a.view().getUint32(88, true); assert(required > 0);
        a.view().setUint32(80, 1024, true); a.view().setUint32(84, required, true);
        assert.equal(a.ops.pb_file_list_next_take(...parts(next), 64), 0);
        assert.equal(a.view().getUint32(72, true), 1);
        assert.equal(a.ops.pb_file_list_next_begin(...list) > 0n, true);
        assert.equal(await a.take('pb_file_list_next_take', a.operation(), 64), 0);
        assert.equal(a.view().getUint32(72, true), 0);
        assert.equal(a.ops.pb_file_list_close(...list), 0);
    } finally { await a.close(); }
});
