import { numberedId } from './identifier-fixture.mjs';
import { encodeId } from '../src/identifier.ts';
// Run as a module worker on an isolated test origin, post {samples:1000}.
// Real web browser OPFS, encrypted account records and compiled Rust kvdb. No guest,
// account metadata outbox, AP or UI is involved in this component measurement.
import { OpfsStore, OpfsResources } from '../src/opfs-store.ts';
import { createStorageBridge } from '../src/opfs-bridge.ts';
import { accountCipher } from '../src/account-record.ts';
const encoder = new TextEncoder(), decoder = new TextDecoder();
const summary = samples => {
    const values = samples.slice().sort((a, b) => a - b);
    return { n: values.length, p50_ms: values[Math.ceil(values.length * .5) - 1],
        p95_ms: values[Math.ceil(values.length * .95) - 1], p99_ms: values[Math.ceil(values.length * .99) - 1], max_ms: values.at(-1) };
};
self.onmessage = async ({ data }) => {
    self.onmessage = null;
    const name = `account-store-benchmark-${crypto.randomUUID()}`;
    let root, directory, resources, store, bridge, host;
    const handles = new Set();
    const io = { reads: 0, writes: 0, flushes: 0, truncates: 0, read_ms: 0, write_ms: 0, flush_ms: 0, truncate_ms: 0 };
    let measuring = false;
    try {
        if (!Number.isInteger(data.samples) || data.samples < 1 || data.samples > 1000) throw Error('Use 1–1000 samples');
        root = await navigator.storage.getDirectory();
        directory = await root.getDirectoryHandle(name, { create: true });
        const file = async filename => {
            const handle = await (await directory.getFileHandle(filename, { create: true })).createSyncAccessHandle();
            handles.add(handle);
            const timed = (count, timing, callback) => {
                if (!measuring) return callback();
                io[count]++; const start = performance.now();
                try { return callback(); } finally { io[timing] += performance.now() - start; }
            };
            return { getSize: () => handle.getSize(), read: (bytes, options) => timed('reads', 'read_ms', () => handle.read(bytes, options)),
                write: (bytes, options) => timed('writes', 'write_ms', () => handle.write(bytes, options)),
                flush: () => timed('flushes', 'flush_ms', () => handle.flush()),
                truncate: length => timed('truncates', 'truncate_ms', () => handle.truncate(length)),
                close: () => { if (handles.delete(handle)) handle.close(); } };
        };
        resources = new OpfsResources([await file('writer-0'), await file('writer-1')], [await file('stage-0'), await file('stage-1')]);
        const module = await WebAssembly.compile(await (await fetch(new URL('../playbit.wasm', import.meta.url))).arrayBuffer());
        const account = encodeId(new Uint8Array(16).fill(0x12)), cipher = await accountCipher(account, new Uint8Array(16).fill(3));
        const open = async () => {
            store = await OpfsStore.open(await file('account.kvdb'), resources, async () => {});
            bridge = createStorageBridge(new Map([[0, store]]), resources, () => host.exports.memory,
                bytes => crypto.getRandomValues(bytes), () => Math.floor(Date.now() / 1000), error => { throw Error(error); });
            host = await WebAssembly.instantiate(module, { opfs: { ...bridge.imports, engine_panic() { throw new Error("Unexpected engine panic"); } } });
            call('pb_account_open', encoder.encode(JSON.stringify({ account, replica_id: 7 })));
        };
        const call = (operation, bytes = new Uint8Array()) => {
            const e = host.exports, pointer = e.pb_input(bytes.length);
            if (!pointer && bytes.length) throw Error('Host input allocation failed');
            new Uint8Array(e.memory.buffer, pointer, bytes.length).set(bytes);
            const length = e[operation](bytes.length);
            if (length < 0) {
                const n = e.pb_error(); throw Error(decoder.decode(new Uint8Array(e.memory.buffer, e.pb_output(), n)) || `Host error ${length}`);
            }
            return new Uint8Array(e.memory.buffer, e.pb_output(), length).slice();
        };
        const start = performance.now(); await open(); const open_ms = performance.now() - start;
        const plain = encoder.encode(JSON.stringify({ kind: 'display_name', name: "true" }).padEnd(512));
        const key = await cipher.recordKey(encoder.encode('settings/display-name'));
        const record = async (key, clock, value) => {
            const ciphertext = await cipher.seal(key, clock, value), bytes = new Uint8Array(40 + ciphertext.length);
            bytes.set(key); new DataView(bytes.buffer).setBigUint64(32, clock, true); bytes.set(ciphertext, 40); return bytes;
        };
        for (let index = 0; index < 128; index++) {
            const key = await cipher.recordKey(encoder.encode(`volume/${numberedId(index)}`));
            call('pb_account_stage', await record(key, BigInt(index + 1), plain));
        }
        const stage = [], seal = [], read = [];
        let last, maxDeltaBytes = 0;
        for (let index = 0; index < data.samples; index++) {
            const clock = BigInt(index + 200), start = performance.now();
            last = await record(key, clock, plain); seal.push(performance.now() - start);
            measuring = true;
            const committed = performance.now(), delta = call('pb_account_stage', last);
            stage.push(performance.now() - committed); maxDeltaBytes = Math.max(maxDeltaBytes, delta.length);
            const lookup = performance.now(), value = call('pb_account_read', key); read.push(performance.now() - lookup);
            measuring = false;
            if (new DataView(value.buffer).getBigUint64(32, true) !== clock) throw Error('Read did not see committed clock');
            const decrypted = await cipher.open(key, clock, value.subarray(44));
            if (decrypted.length !== plain.length || !decrypted.every((byte, i) => byte === plain[i])) throw Error('Wrong retained plaintext');
            if ((index + 1) % 100 === 0) self.postMessage({ progress: index + 1 });
        }
        const size = store.end;
        call('pb_account_stage', last);
        if (store.end !== size) throw Error('Exact replay wrote another transaction');
        const largeKey = await cipher.recordKey(encoder.encode('settings/large-fixture'));
        const largeClock = 2000n, largePlain = new Uint8Array(48 * 1024).fill(7);
        const large = await record(largeKey, largeClock, largePlain), largeStart = performance.now();
        const largeDelta = call('pb_account_stage', large), large_stage_ms = performance.now() - largeStart;
        if (largeDelta.length > 65536) throw Error('Large record exceeds changeset bound');
        const largeRead = call('pb_account_read', largeKey);
        const largeOpened = await cipher.open(largeKey, largeClock, largeRead.subarray(44));
        if (largeOpened.length !== largePlain.length || !largeOpened.every(byte => byte === 7)) throw Error('Large record mismatch');
        const expected = call('pb_account_read', key);
        bridge.close(); store.close(); await open();
        const retained = call('pb_account_read', key);
        if (retained.length !== expected.length || !retained.every((byte, index) => byte === expected[index])) throw Error('Reopen lost acknowledged account state');
        self.postMessage({ result: { boundary: 'FFI stage entry through durable OPFS commit and kvdb delta; seal and committed lookup reported separately; excludes guest/UI/AP',
            userAgent: navigator.userAgent, crossOriginIsolated, retained_records_during_measurement: 129, payload_bytes: 512, open_ms,
            stage: summary(stage), seal: summary(seal), read: summary(read), io, maxDeltaBytes,
            large_record: { plaintext_bytes: largePlain.length, delta_bytes: largeDelta.length, stage_ms: large_stage_ms },
            database_handles: resources.stores, bookkeeping_bytes: resources.bytes, account_file_bytes: store.end,
            exact_replay_no_commit: true, reopen_retained_record: true,
            timing_caution: 'Non-isolated web browser timers may quantize small spans; zero-valued seal/flush samples do not establish zero software or I/O cost.' } });
    } catch (error) { self.postMessage({ error: error.message }); }
    finally {
        measuring = false; bridge?.close(); store?.close(); resources?.close();
        for (const handle of handles) handle.close();
        if (directory) await root.removeEntry(name, { recursive: true });
        self.close();
    }
};
