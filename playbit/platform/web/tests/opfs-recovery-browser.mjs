// Actual OPFS recovery and same-worker readiness. This component excludes the
// guest, authorization, AP, layout and physical presentation.
import { OpfsStore, OpfsResources } from '../src/opfs-store.ts';
const encoder = new TextEncoder();
let opening = false, pending, root, directory, resources, name, opened;
const handles = new Set(), readiness = new MessageChannel();
readiness.port1.onmessage = () => { const ready = pending; pending = undefined; ready?.(); };
const yieldReady = () => new Promise(resolve => { pending = resolve; readiness.port2.postMessage(0); });
async function file(name) {
    const handle = await (await directory.getFileHandle(name, { create: true })).createSyncAccessHandle();
    handles.add(handle);
    return { getSize: () => handle.getSize(), read: (out, options) => handle.read(out, options),
        write: (bytes, options) => handle.write(bytes, options), flush: () => handle.flush(),
        truncate: length => handle.truncate(length), close() { if (handles.delete(handle)) handle.close(); } };
}
async function run(rows) {
    if (rows !== 500 && rows !== 10000) throw new Error('Use the declared 500/10,000-entry workload');
    name = `opfs-recovery-${crypto.randomUUID()}`;
    root = await navigator.storage.getDirectory(); directory = await root.getDirectoryHandle(name, { create: true });
    resources = new OpfsResources([await file('write-0'), await file('write-1')], [await file('sync-0'), await file('sync-1')]);
    const store = OpfsStore.empty(await file('backing'), resources), writer = store.begin(true);
    for (let index = rows - 1; index >= 0; index--) writer.put(encoder.encode(String(index).padStart(5, '0')), new Uint8Array(400).fill(index & 255));
    writer.startPut(encoder.encode('large'), 8 * 1024 * 1024);
    const chunk = new Uint8Array(4096).fill(7);
    for (let offset = 0; offset < 8 * 1024 * 1024; offset += chunk.length) writer.writePart(chunk);
    writer.finishPut(); writer.end(true); store.close();
    let largestRead = 0, readBytes = 0;
    const turns = [], reopens = [];
    self.postMessage({ opening: rows }); opening = true;
    const start = performance.now();
    for (let iteration = 0; iteration < 6; iteration++) {
        opened?.close();
        const backing = await file('backing'), read = backing.read;
        backing.read = (out, options) => { largestRead = Math.max(largestRead, out.length); readBytes += out.length; return read(out, options); };
        const began = performance.now(); let turn = began;
        opened = await OpfsStore.open(backing, resources, async () => {
            turns.push(performance.now() - turn); await yieldReady(); turn = performance.now();
        });
        reopens.push(performance.now() - began);
    }
    const elapsed = performance.now() - start; opening = false; turns.sort((a, b) => a - b);
    if (opened.keys.length !== rows + 1 || opened.keys[0] !== '3030303030' || largestRead > 4096) throw new Error('Recovery content/bounds failed');
    const reader = opened.begin(false), pin = reader.pin(encoder.encode('large')); reader.end(false);
    const out = new Uint8Array(4096); pin.read(pin.length - out.length, out); pin.close();
    if (out.some(value => value !== 7) || opened.get(encoder.encode(String(rows - 1).padStart(5, '0'))).at(-1) !== ((rows - 1) & 255)) throw new Error('Recovered data changed');
    self.postMessage({ done: { rows, elapsed_ms: elapsed, reopen_ms: reopens,
        turns: { n: turns.length, p99_ms: turns[Math.ceil(turns.length * .99) - 1], max_ms: turns.at(-1) },
        largest_read_bytes: largestRead, journal_read_bytes: readBytes, bookkeeping_bytes: resources.bytes,
        userAgent: navigator.userAgent, crossOriginIsolated,
        boundary: 'actual OPFS background journal checksum, record recovery and ordered index; probes acknowledged by this same worker; no guest/AP/UI or physical presentation' } });
}
async function close() {
    opened?.close(); resources?.close(); for (const handle of handles) handle.close();
    if (directory) await root.removeEntry(name, { recursive: true });
    readiness.port1.close(); readiness.port2.close(); self.close();
}
self.onmessage = ({ data }) => {
    if (data.probe !== undefined) { self.postMessage({ probe: data.probe, opening }); return; }
    if (data.close) { void close(); return; }
    void run(data.rows).catch(error => { opening = false; self.postMessage({ error: error.message }); });
};
