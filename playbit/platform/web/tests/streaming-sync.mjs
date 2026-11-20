import { encodeId } from '../src/identifier.ts';
// Actual Rust Wasm full-sync over the production OPFS bridge/journal, using
// native file handles. Web browser/WebTransport/UI/RSS qualification is separate.
import assert from "node:assert/strict";
import { bootstrap, createHost, entryId } from "./host-fixture.mjs";
import { OpfsStore } from "../src/opfs-store.ts";
import { fixture } from "./opfs-fixture.mjs";
import { ABI as L } from "../src/guest-layout.ts";
// Exceed two output chunks so cancellation interrupts the snapshot body.
const title = "streamed ".repeat(80000), encoder = new TextEncoder();
async function makeHost(seed, replica) {
    const files = fixture(), stores = new Map(Array.from({ length: 4 }, (_, i) => [i + 1, OpfsStore.empty(files.file(), files.resources)]));
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x09)));
    profile.root.share_key_with_service = profile.root.share_key_with_service = true;
    profile.replica_ids = [replica, replica + 1];
    if (seed) stores.set(65, OpfsStore.empty(files.file(), files.resources)); else profile.local = null;
    const runtime = await createHost(profile, stores, () => 1000);
    const e = runtime.host.exports;
    const memory = (pointer, length) => new Uint8Array(e.memory.buffer, pointer, length);
    const input = bytes => { const pointer = e.pb_input(bytes.length); assert(pointer); memory(pointer, bytes.length).set(bytes); };
    const output = length => {
        if (length < 0) { const n = e.pb_error(); throw new Error(new TextDecoder().decode(memory(e.pb_output(), n)) + ` (${length}; ${runtime.storageErrors.join(", ")})`); }
        return memory(e.pb_output(), length).slice();
    };
    return { ...runtime, e, input, output, files, errors: runtime.storageErrors, close() { e.pb_sync_finish(0); for (const store of stores.values()) store.close(); assert.equal(files.resources.bytes, 0); files.cleanup(); } };
}
const source = await makeHost(true, 1), target = await makeHost(false, 11);
function editTitle() {
    const object = 8 + L.entryHandle.size, field = object + 16;
    const value = field + L.fieldInput.size, payload = value + L.valueInput.size;
    const text = encoder.encode(title), input = new Uint8Array(payload + text.length), view = new DataView(input.buffer);
    input.set(source.handle(), 8); input.fill(1, object, object + 16);
    view.setUint32(field, L.fieldInput.size, true); input.fill(2, field + L.fieldInput.offsets.id, field + L.fieldInput.offsets.id + 16);
    view.setUint32(field + L.fieldInput.offsets.values, value, true); view.setUint32(field + L.fieldInput.offsets.count, 1, true);
    view.setUint32(field + L.fieldInput.offsets.stride, L.valueInput.size, true); view.setUint32(field + L.fieldInput.offsets.present, 1, true);
    view.setUint32(value, L.valueInput.size, true); view.setUint32(value + L.valueInput.offsets.kind, 1, true);
    view.setUint32(value + L.valueInput.offsets.length, text.length, true); view.setUint32(value + L.valueInput.offsets.data, payload, true); input.set(text, payload);
    source.input(input); source.output(source.e.pb_web_object_edit(8, object, field, 1, L.fieldInput.size));
}
function transfer(slot, cancel = false) {
    let request = target.output(target.e.pb_sync_begin(slot, 0)); source.output(source.e.pb_sync_begin(slot, 1));
    let transferred = 0, chunks = 0, done = false;
    try {
        while (!done) {
            assert(request.length); source.input(request); source.output(source.e.pb_sync_step(request.length)); request = new Uint8Array();
            for (;;) {
                const fragment = source.output(source.e.pb_sync_output());
                if (!fragment.length) break;
                assert(fragment.length <= 256 * 1024); transferred += fragment.length; chunks++;
                // Exercise partial headers, then partial entry bodies; do not
                // accumulate the peer's full response between Wasm instances.
                let offset = 0;
                for (const length of [1, 3, fragment.length]) {
                    const part = fragment.subarray(offset, Math.min(fragment.length, offset + length)); offset += part.length;
                    if (!part.length) continue;
                    target.input(part); const next = target.output(target.e.pb_sync_step(part.length));
                    if (next.length) { assert.equal(request.length, 0); request = next; }
                    done = Boolean(target.e.pb_sync_done());
                }
                if (cancel && transferred > 270000) { assert(!done); return { chunks, transferred, cancelled: true }; }
            }
        }
        source.input(request); source.output(source.e.pb_sync_step(request.length));
        assert(source.e.pb_sync_done()); target.output(target.e.pb_sync_finish(1)); source.output(source.e.pb_sync_finish(1));
        return { chunks, transferred };
    } finally { if (cancel) { target.e.pb_sync_finish(0); source.e.pb_sync_finish(0); } }
}
try {
    transfer(1); transfer(2);
    target.stores.set(65, OpfsStore.empty(target.files.file(), target.files.resources)); target.register(entryId);
    assert.equal(target.read(), -6, "index arrival cannot expose an empty backing");
    editTitle();
    const initialEnd = target.stores.get(65).end;
    assert(transfer(65, true).cancelled); assert.equal(target.stores.get(65).end, initialEnd);
    assert(target.files.resources.staging.every(value => !value.busy)); assert.equal(target.files.resources.transactions, 0); assert.equal(target.files.resources.pins, 0);
    const result = transfer(65); assert(result.chunks >= 3); assert(result.transferred > title.length);
    const reader = source.stores.get(65).begin(false), other = target.stores.get(65).begin(false);
    let key = new Uint8Array();
    for (;;) {
        key = reader.nextKey(key, true, false); if (!key || key[0] >= 0x80) break;
        assert.deepEqual(other.get(key), reader.get(key));
    }
    reader.end(false); other.end(false);
    const equal = transfer(65); assert.equal(equal.transferred, 52);
    assert.deepEqual(source.errors, []); assert.deepEqual(target.errors, []);
    console.log(`Web Wasm streamed sync, cancellation, equal-root terminal: passed (${result.transferred} bytes, ${result.chunks} chunks)`);
} catch (error) { console.error("Primary failure", error); throw error; } finally { source.close(); target.close(); }
