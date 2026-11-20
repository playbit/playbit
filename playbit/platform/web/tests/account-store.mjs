import { encodeId } from '../src/identifier.ts';
// Real Rust Wasm/kvdb and production OPFS bridge with native file handles.
// This checks byte/layout, replay and failure behavior; it is not web UI,
// WebTransport, typed-record validation or physical-durability qualification.
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { randomFillSync } from 'node:crypto';
import { OpfsStore } from '../src/opfs-store.ts';
import { createStorageBridge } from '../src/opfs-bridge.ts';
import { accountCipher, openAccountRecord } from '../src/account-record.ts';
import { fixture } from './opfs-fixture.mjs';
import { accountRecordCases } from './account-record-cases.mjs';
import { accountPlain, frame, field, object, text, magic, join } from './account-wire-fixture.mjs';

assert(process.argv.length === 3 || (process.argv.length === 4 && process.argv[3] === '--benchmark'),
    'usage: node account-store.mjs WASM [--benchmark]');
const benchmark = process.argv[3] === '--benchmark';
const module = await WebAssembly.compile(await readFile(process.argv[2]));
const encoder = new TextEncoder(), decoder = new TextDecoder();
const account = encodeId(new Uint8Array(16).fill(0x12)), secret = new Uint8Array(16).fill(3);
const cipher = await accountCipher(account, secret);
const files = fixture();
async function open(name, create = true, identity = account) {
    const file = files.file(name, create), store = await OpfsStore.open(file, files.resources, async () => {});
    let instance;
    const errors = [];
    const bridge = createStorageBridge(new Map([[0, store]]), files.resources,
        () => instance.exports.memory, bytes => randomFillSync(bytes), () => 1800000000,
        error => errors.push(error));
    instance = await WebAssembly.instantiate(module, { opfs: { ...bridge.imports, engine_panic() { throw new Error("Unexpected engine panic"); } } });
    const e = instance.exports;
    const invoke = (operation, ...args) => {
        const length = e[operation](...args);
        if (length < 0) {
            const n = e.pb_error();
            throw Error(decoder.decode(new Uint8Array(e.memory.buffer, e.pb_output(), n)) || `Host error ${length}`);
        }
        return new Uint8Array(e.memory.buffer, e.pb_output(), length).slice();
    };
    const call = (operation, input = new Uint8Array()) => {
        const pointer = e.pb_input(input.length); assert(pointer || !input.length);
        new Uint8Array(e.memory.buffer, pointer, input.length).set(input);
        return invoke(operation, input.length);
    };
    const close = () => { e.pb_sync_finish(0); bridge.close(); store.close(); };
    try { call('pb_account_open', encoder.encode(JSON.stringify({ account: identity, replica_id: 7 }))); }
    catch (error) { close(); throw error; }
    return { call, invoke, e, close, file, store, errors };
}
function entries(bytes) {
    const result = [];
    for (let offset = 0; offset < bytes.length;) {
        assert(bytes.length - offset >= 44);
        const view = new DataView(bytes.buffer, bytes.byteOffset + offset, bytes.length - offset);
        const length = view.getUint32(40, true); assert(length && offset + 44 + length <= bytes.length);
        result.push({ key: bytes.slice(offset, offset + 32), clock: view.getBigUint64(32, true), value: bytes.slice(offset + 44, offset + 44 + length) });
        offset += 44 + length;
    }
    return result;
}
async function record(logical, clock, plain) {
    const key = await cipher.recordKey(encoder.encode(logical));
    const value = await cipher.seal(key, clock, plain);
    const input = new Uint8Array(40 + value.length);
    input.set(key); new DataView(input.buffer).setBigUint64(32, clock, true); input.set(value, 40);
    return { key, clock, value, input, plain };
}
async function receive(host, delta) {
    // The trusted client must authenticate every entry, even a losing value,
    // before asking the storage export to commit anything.
    const decoded = entries(host.call('pb_account_entries', delta));
    for (const item of decoded) await cipher.open(item.key, item.clock, item.value);
    host.call('pb_account_apply', delta);
    return decoded;
}
function download(source, target) {
    let request = target.invoke('pb_sync_begin', 0, 0);
    source.invoke('pb_sync_begin', 0, 1);
    try {
        while (!target.e.pb_sync_done()) {
            assert(request.length);
            source.call('pb_sync_step', request); request = new Uint8Array();
            for (;;) {
                const fragment = source.invoke('pb_sync_output');
                if (!fragment.length) break;
                for (let offset = 0; offset < fragment.length; offset += 137) {
                    const next = target.call('pb_sync_step', fragment.subarray(offset, offset + 137));
                    if (next.length) { assert.equal(request.length, 0); request = next; }
                }
            }
        }
        source.call('pb_sync_step', request);
        assert(source.e.pb_sync_done());
        source.invoke('pb_sync_finish', 1);
    } catch (error) { source.e.pb_sync_finish(0); target.e.pb_sync_finish(0); throw error; }
}
async function inspect(host, visit = async () => {}) {
    host.invoke('pb_account_sync_prepare');
    let count = 0;
    for (;;) {
        const bytes = host.invoke('pb_account_sync_next');
        if (!bytes.length) break;
        assert(bytes.length <= 44 + 49152 + 29);
        if (bytes.length === 44) {
            assert.equal(new DataView(bytes.buffer).getBigUint64(32, true), 0n);
        } else {
            const [entry] = entries(bytes);
            await cipher.open(entry.key, entry.clock, entry.value);
        }
        await visit(++count);
    }
    return count;
}
let source, destination, bootstrap;
try {
    // A malformed authenticated loser must fail before any storage mutation;
    // treating LWW rejection as sufficient would skip capability validation.
    const typedSource = await open('typed-source'), typedTarget = await open('typed-target');
    try {
        const invalid = await record('settings/display-name', 1n,
            frame('display_name', 'settings/display-name', false, join(magic, Uint8Array.of(10, 1, 0), object([1, text('true')]), field(0xc001, text('false')))));
        const delta = typedSource.call('pb_account_stage', invalid.input);
        const valid = await record('settings/display-name', 10n,
            accountPlain({ kind: 'display_name', name: 'true' }));
        typedTarget.call('pb_account_stage', valid.input);
        const before = typedTarget.call('pb_account_read', valid.key), end = typedTarget.store.end;
        await assert.rejects(async () => {
            for (const item of entries(typedTarget.call('pb_account_entries', delta))) {
                await openAccountRecord(cipher, item.key, item.clock, item.value,
                    plain => typedTarget.call('pb_account_decode', plain));
            }
            typedTarget.call('pb_account_apply', delta);
        });
        assert.equal(typedTarget.store.end, end);
        assert.deepEqual(typedTarget.call('pb_account_read', valid.key), before);
    } finally { typedSource.close(); typedTarget.close(); }

    source = await open('source'); destination = await open('destination');
    assert.match(decoder.decode(source.invoke('pb_version')), /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?\+(?:[0-9a-f]{8}|src)$/);
    console.log(JSON.stringify(await accountRecordCases(plain => source.call('pb_account_decode', plain), { benchmark })));
    assert.equal(source.call('pb_account_next').length, 0, 'tree starts without a guest');
    download(source, destination);
    assert.equal(await inspect(destination), 0, 'empty bootstrap completes inspection');
    destination.invoke('pb_sync_finish', 1);
    assert.throws(() => source.call('pb_account_open', encoder.encode(JSON.stringify({ account, replica_id: 7 }))));
    const plain = new Uint8Array(512).fill(7); // Opaque storage fixture, not an interpreted record.
    let item = await record('settings/display-name', 1n, plain);
    const delta = source.call('pb_account_stage', item.input);
    assert(delta.length && delta.length <= 65536);
    const savedEnd = source.store.end;
    const replay = source.call('pb_account_stage', item.input);
    assert.deepEqual(entries(source.call('pb_account_entries', replay)), entries(source.call('pb_account_entries', delta)));
    assert.equal(source.store.end, savedEnd, 'exact replay performs no database commit');
    for (const length of [0, 1, 12, delta.length - 1]) {
        const saved = destination.store.end;
        assert.throws(() => destination.call('pb_account_apply', delta.subarray(0, length)));
        assert.equal(destination.store.end, saved, 'malformed input commits no prefix');
        assert.equal(destination.call('pb_account_read', item.key).length, 0);
    }
    assert.deepEqual(await receive(destination, delta), [{ key: item.key, clock: item.clock, value: item.value }]);
    await receive(destination, replay);
    for (let index = 0; index < 128; index++) {
        const value = await record(`volume/${encodeId(Buffer.from(index.toString(16).padStart(32, '0'), 'hex'))}`, BigInt(index + 2), plain);
        await receive(destination, source.call('pb_account_stage', value.input));
    }
    let cursor = new Uint8Array(), count = 0;
    for (;;) {
        const bytes = destination.call('pb_account_next', cursor);
        if (!bytes.length) break;
        const decoded = entries(bytes); assert.equal(decoded.length, 1);
        assert(bytes.length <= 44 + 49152 + 29);
        cursor = decoded[0].key; count++;
    }
    assert.equal(count, 129);
    const timings = [];
    for (let index = 0; index < (benchmark ? 1000 : 1); index++) {
        item = await record('settings/display-name', BigInt(index + 200), plain);
        const started = benchmark ? performance.now() : 0;
        source.call('pb_account_stage', item.input);
        if (benchmark) timings.push(performance.now() - started);
    }
    const stale = await record('settings/display-name', 1n, plain);
    assert.equal(source.call('pb_account_stage', stale.input).length, 0);
    const tied = await record('settings/display-name', item.clock, plain);
    const winner = Buffer.compare(item.value, tied.value) >= 0 ? item : tied;
    source.call('pb_account_stage', tied.input);
    assert.deepEqual(entries(source.call('pb_account_read', item.key))[0].value, winner.value);
    const large = await record('settings/large-fixture', 1400n, new Uint8Array(48 * 1024).fill(7));
    await receive(destination, source.call('pb_account_stage', large.input));
    assert.deepEqual(await cipher.open(large.key, large.clock, entries(destination.call('pb_account_read', large.key))[0].value), large.plain);
    bootstrap = await open('bootstrap');
    download(source, bootstrap);
    assert.throws(() => bootstrap.invoke('pb_sync_finish', 1), 'account bootstrap cannot bypass inspection');
    assert.equal(bootstrap.call('pb_account_next').length, 0);
    download(source, bootstrap);
    bootstrap.invoke('pb_account_sync_prepare');
    bootstrap.invoke('pb_account_sync_next');
    assert.throws(() => bootstrap.invoke('pb_sync_finish', 1), 'incomplete inspection cannot commit');
    assert.equal(bootstrap.call('pb_account_next').length, 0);
    download(source, bootstrap);
    bootstrap.invoke('pb_account_sync_prepare');
    bootstrap.invoke('pb_account_sync_next');
    bootstrap.invoke('pb_sync_finish', 0);
    assert.equal(bootstrap.call('pb_account_next').length, 0, 'cancellation keeps old tree');
    assert(files.resources.staging.every(value => !value.busy));
    const local = await record('settings/display-name', 1450n, encoder.encode('concurrent local edit'));
    const localOnly = await record('settings/local-only', 1440n, plain);
    bootstrap.call('pb_account_stage', localOnly.input);
    download(source, bootstrap);
    assert.equal(await inspect(bootstrap, async count => {
        if (count === 16) bootstrap.call('pb_account_stage', local.input);
    }), 131);
    bootstrap.invoke('pb_sync_finish', 1);
    assert.deepEqual(entries(bootstrap.call('pb_account_read', local.key))[0].value, local.value);
    assert.deepEqual(entries(bootstrap.call('pb_account_read', large.key))[0].value, large.value);
    assert.deepEqual(entries(bootstrap.call('pb_account_read', localOnly.key))[0].value, localOnly.value,
        'missing remote entry keeps local-only data');
    // A well-framed but unauthentic envelope must fail outside a live writer;
    // canceling the journal leaves even its already-validated prefix unapplied.
    const remoteLogical = `volume/${'0'.repeat(32)}`;
    const retainedWinner = await record(remoteLogical, 1470n, plain);
    bootstrap.call('pb_account_stage', retainedWinner.input);
    const bad = await record(remoteLogical, 1460n, plain);
    bad.input[bad.input.length - 1] ^= 1;
    source.call('pb_account_stage', bad.input);
    const before = bootstrap.store.end;
    download(source, bootstrap);
    await assert.rejects(inspect(bootstrap));
    bootstrap.invoke('pb_sync_finish', 0);
    assert.equal(bootstrap.store.end, before);
    assert.deepEqual(entries(bootstrap.call('pb_account_read', bad.key))[0].value, retainedWinner.value,
        'even a losing remote ciphertext must be authenticated');
    assert(files.resources.staging.every(value => !value.busy));
    assert.equal(files.resources.transactions, 0); assert.equal(files.resources.pins, 0);
    bootstrap.close(); bootstrap = await open('bootstrap', false);
    assert.deepEqual(entries(bootstrap.call('pb_account_read', local.key))[0].value, local.value);
    bootstrap.close(); bootstrap = undefined;
    source.close(); source = undefined;
    await assert.rejects(open('source', false, encodeId(new Uint8Array(16).fill(0x34))), /identity does not match/);
    source = await open('source', false);
    assert.deepEqual(entries(source.call('pb_account_read', item.key))[0].value, winner.value);
    const failed = await record('settings/display-name', 1500n, plain);
    source.file.failFlush = true;
    assert.throws(() => source.call('pb_account_stage', failed.input), /storage|flush|error/i);
    assert.equal(files.resources.transactions, 0); assert.equal(files.resources.pins, 0);
    source.close(); source = await open('source', false);
    const retained = entries(source.call('pb_account_read', item.key))[0];
    assert(retained.clock === winner.clock || retained.clock === failed.clock, 'uncertain commit reopens as a complete old or new record');
    timings.sort((a, b) => a - b);
    console.log(JSON.stringify({ checks: 'account tree without guest, 128 retained records, authenticated receive, malformed framing, replay, stale/tied clocks, reopen, 48 KiB, flush failure; full bootstrap inspection, concurrent local commit, rejected ciphertext, incomplete inspection and cancellation',
        boundary: 'native-file adapter: stage FFI entry to committed delta, excluding encryption, web browser I/O, UI and AP',
        ...(benchmark ? { samples: timings.length, p50_ms: timings[499], p95_ms: timings[949], p99_ms: timings[989], max_ms: timings[999] } : {}),
        retained_handles: files.resources.stores, bookkeeping_bytes: files.resources.bytes }));
} finally {
    bootstrap?.close(); source?.close(); destination?.close();
    assert.equal(files.resources.stores, 0); assert.equal(files.resources.bytes, 0);
    files.cleanup();
}
