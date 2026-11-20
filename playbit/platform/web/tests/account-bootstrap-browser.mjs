import { encodeId } from '../src/identifier.ts';
// Module worker on an isolated local test origin. Post {} to exercise actual
// OPFS account bootstrap without guest/UI/AP. Files are removed after the run.
import { OpfsStore, OpfsResources } from '../src/opfs-store.ts';
import { createStorageBridge } from '../src/opfs-bridge.ts';
import { accountCipher } from '../src/account-record.ts';
const encoder = new TextEncoder(), decoder = new TextDecoder();
const check = (condition, message) => { if (!condition) throw Error(message); };
const summary = values => {
    values = values.slice().sort((a, b) => a - b);
    return { n: values.length, p50_ms: values[Math.ceil(values.length * .5) - 1],
        p99_ms: values[Math.ceil(values.length * .99) - 1], max_ms: values.at(-1) };
};
self.onmessage = async () => {
    self.onmessage = null;
    const name = `account-bootstrap-test-${crypto.randomUUID()}`, handles = new Set(), hosts = [];
    let root, directory, resources;
    try {
        root = await navigator.storage.getDirectory();
        directory = await root.getDirectoryHandle(name, { create: true });
        const file = async name => {
            const handle = await (await directory.getFileHandle(name, { create: true })).createSyncAccessHandle();
            handles.add(handle);
            return { getSize: () => handle.getSize(), read: (bytes, options) => handle.read(bytes, options),
                write: (bytes, options) => handle.write(bytes, options), flush: () => handle.flush(),
                truncate: length => handle.truncate(length), close: () => { if (handles.delete(handle)) handle.close(); } };
        };
        resources = new OpfsResources([await file('writer0'), await file('writer1')], [await file('stage0'), await file('stage1')]);
        const module = await WebAssembly.compile(await (await fetch(new URL('../playbit.wasm', import.meta.url))).arrayBuffer());
        const account = encodeId(new Uint8Array(16).fill(0x12)), cipher = await accountCipher(account, new Uint8Array(16).fill(3));
        const open = async name => {
            const store = await OpfsStore.open(await file(name), resources, async () => {});
            let instance;
            const bridge = createStorageBridge(new Map([[0, store]]), resources, () => instance.exports.memory,
                bytes => crypto.getRandomValues(bytes), () => Math.floor(Date.now() / 1000), error => { throw Error(error); });
            instance = await WebAssembly.instantiate(module, { opfs: { ...bridge.imports, engine_panic() { throw new Error("Unexpected engine panic"); } } });
            const e = instance.exports;
            const invoke = (operation, ...args) => {
                const length = e[operation](...args);
                if (length < 0) {
                    const n = e.pb_error(); throw Error(decoder.decode(new Uint8Array(e.memory.buffer, e.pb_output(), n)) || `Host error ${length}`);
                }
                return new Uint8Array(e.memory.buffer, e.pb_output(), length).slice();
            };
            const call = (operation, bytes = new Uint8Array()) => {
                const pointer = e.pb_input(bytes.length); check(pointer || !bytes.length, 'input allocation');
                new Uint8Array(e.memory.buffer, pointer, bytes.length).set(bytes);
                return invoke(operation, bytes.length);
            };
            const host = { e, store, call, invoke, close() { e.pb_sync_finish(0); bridge.close(); store.close(); } };
            hosts.push(host);
            call('pb_account_open', encoder.encode(JSON.stringify({ account, replica_id: 7 })));
            return host;
        };
        const source = await open('source'), target = await open('target');
        const record = async (logical, clock, plain) => {
            const key = await cipher.recordKey(encoder.encode(logical));
            const value = await cipher.seal(key, clock, plain), input = new Uint8Array(40 + value.length);
            input.set(key); new DataView(input.buffer).setBigUint64(32, clock, true); input.set(value, 40);
            return { key, input, clock };
        };
        for (let index = 0; index < 128; index++) {
            const item = await record(`fixture/${index}`, BigInt(index + 1), new Uint8Array(512).fill(index));
            source.call('pb_account_stage', item.input);
        }
        const large = await record('fixture/large', 129n, new Uint8Array(48 * 1024).fill(7));
        source.call('pb_account_stage', large.input);
        const download = () => {
            let request = target.invoke('pb_sync_begin', 0, 0), transferred = 0;
            source.invoke('pb_sync_begin', 0, 1);
            while (!target.e.pb_sync_done()) {
                check(request.length, 'missing request'); source.call('pb_sync_step', request); request = new Uint8Array();
                for (;;) {
                    const fragment = source.invoke('pb_sync_output');
                    if (!fragment.length) break;
                    transferred += fragment.length;
                    const next = target.call('pb_sync_step', fragment);
                    if (next.length) { check(!request.length, 'duplicate request'); request = next; }
                }
            }
            source.call('pb_sync_step', request); check(source.e.pb_sync_done(), 'sender terminal');
            source.invoke('pb_sync_finish', 1); return transferred;
        };
        const inspection = [], authenticate = [];
        const inspect = async visit => {
            target.invoke('pb_account_sync_prepare');
            let count = 0;
            for (;;) {
                const start = performance.now(), bytes = target.invoke('pb_account_sync_next');
                if (!bytes.length) break;
                inspection.push(performance.now() - start);
                check(bytes.length >= 44 && bytes.length <= 44 + 49152 + 29, 'entry bounds');
                const view = new DataView(bytes.buffer), clock = view.getBigUint64(32, true), length = view.getUint32(40, true);
                check(length === bytes.length - 44, 'entry framing');
                if (length) {
                    const start = performance.now();
                    await cipher.open(bytes.subarray(0, 32), clock, bytes.subarray(44));
                    authenticate.push(performance.now() - start);
                } else check(clock === 0n, 'missing entry clock');
                await visit?.(++count);
            }
            return count;
        };
        const transferred = download();
        let refused = false;
        try { target.invoke('pb_sync_finish', 1); } catch { refused = true; }
        check(refused && !target.call('pb_account_next').length, 'bypass committed');
        download(); target.invoke('pb_account_sync_prepare'); target.invoke('pb_account_sync_next');
        target.invoke('pb_sync_finish', 0);
        check(!target.call('pb_account_next').length, 'cancel committed prefix');
        check(resources.staging.every(value => !value.busy), 'cancellation leaked journal');
        const local = await record('fixture/0', 1000n, encoder.encode('concurrent local edit'));
        download();
        const count = await inspect(async count => {
            if (count === 16) target.call('pb_account_stage', local.input);
        });
        check(count === 129, 'inspection count');
        const mergeStart = performance.now(); target.invoke('pb_sync_finish', 1); const merge_ms = performance.now() - mergeStart;
        const read = target.call('pb_account_read', local.key);
        check(new DataView(read.buffer).getBigUint64(32, true) === local.clock, 'lost concurrent local edit');
        const largeRead = target.call('pb_account_read', large.key);
        const largePlain = await cipher.open(large.key, large.clock, largeRead.subarray(44));
        check(largePlain.length === 48 * 1024 && largePlain.every(byte => byte === 7), 'large record changed');
        const bad = await record('fixture/0', 999n, encoder.encode('tampered losing value'));
        bad.input[bad.input.length - 1] ^= 1; source.call('pb_account_stage', bad.input);
        const end = target.store.end; download();
        let rejected = false;
        try { await inspect(); } catch { rejected = true; }
        target.invoke('pb_sync_finish', 0);
        check(rejected && target.store.end === end, 'invalid losing ciphertext accepted');
        check(new DataView(target.call('pb_account_read', bad.key).buffer).getBigUint64(32, true) === local.clock, 'local winner changed');
        check(resources.staging.every(value => !value.busy) && !resources.transactions && !resources.pins, 'retained sync resources');
        self.postMessage({ result: { checks: 'inspection required, cancellation, 129 encrypted records including48 KiB, concurrent local write retained, tampered losing ciphertext rejected without prefix, resources released',
            boundary: 'actual OPFS/Wasm: per-entry inspection FFI includes journal payload read; WebCrypto authentication separate; merge includes durable local commit; no AP/guest/UI',
            userAgent: navigator.userAgent, crossOriginIsolated, transferred_bytes: transferred,
            inspection: summary(inspection), authenticate: summary(authenticate), merge_ms,
            database_handles: resources.stores, bookkeeping_bytes: resources.bytes,
            timing_caution: 'Non-isolated timers quantize small spans; this component check is not full account or UI qualification.' } });
    } catch (error) { self.postMessage({ error: error.stack || error.message }); }
    finally {
        for (const host of hosts) host.close();
        resources?.close();
        for (const handle of handles) handle.close();
        if (directory) await root.removeEntry(name, { recursive: true });
        self.close();
    }
};
