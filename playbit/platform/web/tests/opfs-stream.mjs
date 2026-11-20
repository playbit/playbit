import assert from "node:assert/strict";
import { OpfsStore, MAX_VALUE, IO_CHUNK } from "../src/opfs-store.ts";
import { fixture, bytes } from "./opfs-fixture.mjs";

async function basic() {
    const f = fixture(), file = f.file("database"), store = OpfsStore.empty(file, f.resources);
    let tx = store.begin(true); tx.put(bytes("a"), bytes("first")); tx.put(bytes("c"), bytes("third")); tx.end(true);
    const snapshot = store.begin(false), pin = snapshot.pin(bytes("a")); snapshot.end(false);
    tx = store.begin(true); tx.put(bytes("a"), bytes("later")); tx.put(bytes("b"), bytes("middle")); tx.end(true);
    const out = new Uint8Array(5); assert.equal(pin.read(0, out), 5); assert.equal(new TextDecoder().decode(out), "first"); pin.close();
    const read = store.begin(false);
    for (const [key, exclusive, reverse, expected] of [["b",false,false,"b"],["b",true,false,"c"],["b",false,true,"b"],["b",true,true,"a"],["",false,true,null]]) {
        const selected = read.nextKey(bytes(key), exclusive, reverse);
        assert.equal(selected && new TextDecoder().decode(selected), expected);
    }
    read.end(false);
    const oldEnd = store.end;
    tx = store.begin(true); tx.startPut(bytes("partial"), 20); tx.writePart(bytes("incomplete")); assert.throws(() => tx.end(true), /Incomplete/);
    assert.equal(store.end, oldEnd); assert.equal(store.get(bytes("partial")), null);
    tx = store.begin(true); tx.put(bytes("a"), bytes("not committed")); tx.put(bytes("new"), bytes("no")); file.failFlush = true;
    assert.throws(() => tx.end(true), /flush failure/); assert.equal(store.end, oldEnd); assert.equal(store.get(bytes("new")), null); assert.equal(new TextDecoder().decode(store.get(bytes("a"))), "later");
    store.close();
    const tail = f.file("database", false); tail.write(new Uint8Array([20,0,0,0,1,2]), { at: oldEnd }); tail.close();
    const recoveredFile = f.file("database", false), recovered = await OpfsStore.open(recoveredFile, f.resources, async () => {});
    assert.equal(recovered.end, oldEnd); assert.equal(recoveredFile.getSize(), oldEnd); assert.equal(new TextDecoder().decode(recovered.get(bytes("a"))), "later");
    assert(recoveredFile.maxRead <= IO_CHUNK); recovered.close(); assert.equal(f.resources.bytes, 0); f.cleanup();
}
function admissions() {
    const f = fixture(), stores = [0,1,2].map(() => OpfsStore.empty(f.file(), f.resources));
    const a = stores[0].begin(true), b = stores[1].begin(true);
    const before = f.resources.bytes;
    assert.throws(() => stores[2].begin(true), /writer capacity/); assert.equal(f.resources.bytes, before); assert.equal(stores[2].writer, undefined);
    assert.throws(() => stores[0].begin(true), /busy/); a.end(false); b.end(false);
    const reads = Array.from({ length: 32 }, () => stores[0].begin(false));
    assert.throws(() => stores[0].begin(false), /transaction capacity/); for (const read of reads) read.end(false);
    let tx = stores[0].begin(true); tx.put(bytes("key"), bytes("v")); tx.end(true);
    tx = stores[0].begin(false); const pins = Array.from({ length: 31 }, () => tx.pin(bytes("key"))); assert.throws(() => tx.pin(bytes("key")), /reader capacity/);
    tx.end(false); assert.equal(f.resources.pins, 31); for (const pin of pins) pin.close();
    const reserved = 128 * 1024 * 1024 - f.resources.bytes;
    f.resources.reserve(reserved); assert.throws(() => stores[0].begin(false), /bookkeeping capacity/); f.resources.release(reserved);
    tx = stores[0].begin(true); assert.throws(() => tx.pin(bytes("key")), /read transaction/); tx.end(false);
    // Exhaust publication credits after the pending value is already staged.
    // The committed journal and previous value must remain untouched.
    const end = stores[0].end, size = stores[0].handle.getSize();
    tx = stores[0].begin(true); tx.put(bytes("new-key"), bytes("new"));
    const publicationReserve = 128 * 1024 * 1024 - f.resources.bytes;
    f.resources.reserve(publicationReserve);
    assert.throws(() => tx.end(true), /bookkeeping capacity/);
    assert.equal(stores[0].end, end); assert.equal(stores[0].handle.getSize(), size);
    f.resources.release(publicationReserve); assert.equal(stores[0].get(bytes("new-key")), null);
    tx = stores[0].begin(true); assert.throws(() => tx.startPut(new Uint8Array(513), 1), /key/); assert.throws(() => tx.startPut(bytes("k"), MAX_VALUE + 1), /value/); tx.end(false);
    for (const store of stores) store.close(); assert.equal(f.resources.bytes, 0); f.cleanup();
}
async function corruptAndHandleLimit() {
    const f = fixture(), store = OpfsStore.empty(f.file("corrupt"), f.resources);
    const tx = store.begin(true); tx.put(bytes("key"), bytes("value")); tx.end(true); store.close();
    const damaged = f.file("corrupt", false); damaged.write(new Uint8Array([255]), { at: 8 }); damaged.close();
    const rejected = f.file("corrupt", false);
    await assert.rejects(OpfsStore.open(rejected, f.resources, async () => {}), /Corrupt database transaction/);
    assert.equal(rejected.fd, undefined); assert.equal(f.resources.stores, 0); assert.equal(f.resources.bytes, 0);
    const stores = Array.from({ length: 96 }, () => OpfsStore.empty(f.file(), f.resources));
    const excess = f.file(); assert.throws(() => OpfsStore.empty(excess, f.resources), /handle capacity/);
    assert.equal(excess.fd, undefined); assert.equal(f.resources.stores, 96);
    for (const entry of stores) entry.close(); assert.equal(f.resources.bytes, 0); f.cleanup();
}
async function maximumValue() {
    const f = fixture(), file = f.file("large"), store = OpfsStore.empty(file, f.resources);
    const tx = store.begin(true), key = new Uint8Array(512).fill(65), chunk = new Uint8Array(IO_CHUNK);
    tx.startPut(key, MAX_VALUE);
    for (let offset = 0; offset < MAX_VALUE; offset += chunk.length) {
        const n = Math.min(chunk.length, MAX_VALUE - offset);
        for (let i = 0; i < n; i++) chunk[i] = (offset + i) % 251;
        tx.writePart(chunk.subarray(0, n));
    }
    tx.finishPut(); tx.end(true);
    assert(file.maxWrite <= IO_CHUNK); assert(file.maxRead <= IO_CHUNK);
    store.close();
    const recoveredFile = f.file("large", false), recovered = await OpfsStore.open(recoveredFile, f.resources, async () => {});
    const read = recovered.begin(false), pin = read.pin(key); read.end(false);
    assert.equal(pin.length, MAX_VALUE);
    for (let offset = 0; offset < MAX_VALUE; offset += chunk.length) {
        const n = pin.read(offset, chunk);
        for (let i = 0; i < n; i++) assert.equal(chunk[i], (offset + i) % 251);
    }
    assert(recoveredFile.maxRead <= IO_CHUNK); pin.close(); recovered.close(); assert.equal(f.resources.bytes, 0); f.cleanup();
}
function representativeIndex() {
    const f = fixture(), store = OpfsStore.empty(f.file(), f.resources);
    const read = store.begin(false), tx = store.begin(true), value = new Uint8Array(400);
    for (let i = 0; i < 10000; i++) tx.put(bytes(`key-${String(i).padStart(5,"0")}`), value);
    tx.end(true); read.end(false);
    assert.equal(store.index.size, 10000); assert(f.resources.bytes <= 128 * 1024 * 1024);
    const snapshot = store.begin(false); assert.equal(snapshot.length(bytes("key-09999")), 400); snapshot.end(false);
    store.close(); assert.equal(f.resources.bytes, 0); f.cleanup();
}
await basic(); admissions(); await corruptAndHandleLimit(); await maximumValue(); representativeIndex();
console.log("OPFS streaming journal, maximum-value, snapshots, recovery and admission: passed");
