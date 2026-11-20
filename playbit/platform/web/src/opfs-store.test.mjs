import test from "node:test";
import assert from "node:assert/strict";
import { OpfsStore, OpfsResources, IO_CHUNK } from "./opfs-store.ts";

class File {
    bytes = new Uint8Array();
    getSize() { return this.bytes.length; }
    read(out, { at }) { const bytes = this.bytes.subarray(at, at + out.length); out.set(bytes); return bytes.length; }
    write(bytes, { at }) { const next = new Uint8Array(Math.max(at + bytes.length, this.bytes.length)); next.set(this.bytes); next.set(bytes, at); this.bytes = next; return bytes.length; }
    truncate(length) { this.bytes = this.bytes.slice(0, length); }
    flush() {}
    close() {}
}
const key = value => new TextEncoder().encode(value);
async function open(file) { return OpfsStore.open(file, new OpfsResources([new File(), new File()], [new File(), new File()]), async () => {}); }
function commit(store, entries) {
    const tx = store.begin(true);
    try { for (const [key, value] of entries) tx.put(key, value); tx.end(true); }
    catch (error) { tx.end(false); throw error; }
}

test("durable batches recover, replace/delete and scan in unsigned byte order", async () => {
    const file = new File();
    let store = await open(file);
    commit(store, [[key("a"), key("1")], [key("c"), key("3")], [key("b"), key("2")]]);
    store = await open(file);
    const reader = store.begin(false);
    assert.deepEqual(reader.next(key("b"), false, false).key, key("b"));
    assert.deepEqual(reader.next(key("b"), true, false).key, key("c"));
    assert.deepEqual(reader.next(key("b"), false, true).key, key("b"));
    assert.deepEqual(reader.next(key("b"), true, true).key, key("a"));
    reader.end(false);
    commit(store, [[key("a"), null], [key("b"), key("new")]]);
    store = await open(file);
    assert.equal(store.get(key("a")), null);
    assert.deepEqual(store.get(key("b")), key("new"));
});

test("an interrupted final batch is removed without losing acknowledged data", async () => {
    const file = new File();
    const store = await open(file);
    commit(store, [[key("a"), key("saved")]]);
    const committed = file.getSize();
    commit(store, [[key("a"), key("unfinished")]]);
    file.truncate(file.getSize() - 1);
    const restored = await open(file);
    assert.equal(file.getSize(), committed);
    assert.deepEqual(restored.get(key("a")), key("saved"));
});

test("corrupt acknowledged data fails explicitly", async () => {
    const file = new File();
    commit(await open(file), [[key("a"), key("saved")]]);
    file.bytes[17] ^= 1;
    await assert.rejects(open(file), /Corrupt/);
});

test("snapshots retain old disk values and dropped writers do not commit", async () => {
    const file = new File(), store = await open(file);
    commit(store, [[key("a"), key("old")]]);
    const reader = store.begin(false), writer = store.begin(true);
    assert.throws(() => store.begin(true), /busy/);
    writer.put(key("a"), key("new"));
    assert.deepEqual(reader.get(key("a")), key("old"));
    assert.deepEqual(writer.get(key("a")), key("new"));
    writer.end(true);
    assert.deepEqual(reader.get(key("a")), key("old"));
    assert.deepEqual(store.get(key("a")), key("new"));
    const abandoned = store.begin(true);
    abandoned.put(key("b"), key("unsaved")); abandoned.end(false);
    assert.equal(store.get(key("b")), null);
    assert.throws(() => reader.put(key("a"), key("forged")), /Invalid database write state/);
    reader.end(false);
});

test("binary keys retain byte order and identity through maximum length and reopen", async () => {
    const file = new File(), store = await open(file);
    const keys = Array.from({ length: 256 }, (_, byte) => Uint8Array.of(byte));
    keys.push(Uint8Array.from({ length: 511 }, (_, i) => i & 255));
    keys.push(Uint8Array.from({ length: 512 }, (_, i) => 255 - (i & 255)));
    const ordered = keys.toSorted((a, b) => Buffer.compare(a, b));
    commit(store, keys.map(bytes => [bytes, bytes]));
    store.close();
    const restored = await open(file), reader = restored.begin(false);
    for (const bytes of keys) assert.deepEqual(reader.get(bytes), bytes);
    let cursor = new Uint8Array();
    for (const bytes of ordered) {
        const next = reader.nextKey(cursor, true, false);
        assert.deepEqual(next, bytes); cursor = next;
    }
    assert.equal(reader.nextKey(cursor, true, false), null);
    for (const bytes of ordered.toReversed().slice(1)) {
        const previous = reader.nextKey(cursor, true, true);
        assert.deepEqual(previous, bytes); cursor = previous;
    }
    assert.equal(reader.nextKey(cursor, true, true), null);
    reader.end(false); restored.close();
});

test("compact batches use one file write and acknowledge only after durable flush", async () => {
    const file = new File(), store = await open(file), calls = [];
    const write = file.write.bind(file);
    file.write = (bytes, at) => { calls.push("write"); return write(bytes, at); };
    file.flush = () => { calls.push("flush"); };
    commit(store, [[key("a"), key("one")], [key("b"), key("two")]]);
    assert.deepEqual(calls, ["write", "flush"]);
    const restored = await open(file);
    assert.deepEqual(restored.get(key("a")), key("one"));
    assert.deepEqual(restored.get(key("b")), key("two"));
});

test("buffer boundaries preserve headers, values, trailer and snapshot offsets", async () => {
    for (let length = IO_CHUNK - 30; length <= IO_CHUNK + 10; length++) {
        const file = new File(), store = await open(file), writes = [];
        const write = file.write.bind(file);
        file.write = (bytes, at) => { writes.push(bytes.length); return write(bytes, at); };
        const value = Uint8Array.from({ length }, (_, i) => i & 255);
        commit(store, [[key("a"), value], [key("b"), value], [key("c"), new Uint8Array()]]);
        assert.equal(writes.length, Math.ceil(file.getSize() / IO_CHUNK));
        assert.ok(writes.every(length => length <= IO_CHUNK));
        const reader = store.begin(false);
        commit(store, [[key("a"), key("replacement")], [key("b"), null]]);
        assert.deepEqual(reader.get(key("a")), value);
        assert.deepEqual(reader.get(key("b")), value);
        reader.end(false);
        const restored = await open(file);
        assert.deepEqual(restored.get(key("a")), key("replacement"));
        assert.equal(restored.get(key("b")), null);
        assert.deepEqual(restored.get(key("c")), new Uint8Array());
    }
});

test("partial buffered writes and failed flushes preserve acknowledged data", async () => {
    for (const fault of ["write", "flush"]) {
        const file = new File(), store = await open(file);
        commit(store, [[key("a"), key("saved")]]);
        const end = file.getSize(), credit = store.resources.bytes;
        const operation = file[fault].bind(file);
        let failed = false;
        file[fault] = (...args) => {
            if (!failed) {
                failed = true;
                if (fault === "write") return operation(args[0].subarray(0, 5), args[1]);
                throw new Error("flush failed");
            }
            return operation(...args);
        };
        const tx = store.begin(true);
        tx.put(key("a"), new Uint8Array(IO_CHUNK * 2));
        tx.put(key("b"), key("new"));
        assert.throws(() => tx.end(true), /Database is full|flush failed/);
        assert.equal(file.getSize(), end);
        assert.equal(store.resources.bytes, credit);
        assert.equal(store.resources.transactions, 0);
        assert.deepEqual(store.get(key("a")), key("saved"));
        assert.equal(store.get(key("b")), null);
        assert.deepEqual((await open(file)).get(key("a")), key("saved"));
    }
});

test('recovery bounds journal reads and key ordering between resumptions without exposing partial state', async () => {
    const file = new File(), store = await open(file);
    const entries = Array.from({ length: 1024 }, (_, index) => [key(String(1023 - index).padStart(5, '0')), key('value')]);
    entries.push([key('large'), new Uint8Array(2 * 1024 * 1024).fill(7)]);
    commit(store, entries); store.close();
    const read = file.read.bind(file); let bytes = 0, largest = 0, resumptions = 0;
    file.read = (out, options) => { bytes += out.length; assert(out.length <= IO_CHUNK); return read(out, options); };
    const ledger = new OpfsResources([new File(), new File()], [new File(), new File()]);
    const reopened = await OpfsStore.open(file, ledger, async () => {
        largest = Math.max(largest, bytes); bytes = 0; resumptions++;
        assert.equal(ledger.recoveries, 1); assert.equal(ledger.transactions, 0);
    });
    assert(largest <= 65536 + 12); assert(resumptions > 32);
    file.read = read;
    assert.deepEqual(reopened.keys, reopened.keys.toSorted());
    assert.deepEqual(reopened.get(key('00000')), key('value'));
    assert.equal(reopened.get(key('large')).at(-1), 7);
    reopened.close(); assert.equal(ledger.recoveries, 0); assert.equal(ledger.bytes, 0);
});

test('recovery admission and cancellation release all handles and bookkeeping credits', async () => {
    const file = new File(), store = await open(file); commit(store, [[key('a'), key('saved')]]); store.close();
    const ledger = new OpfsResources([new File(), new File()], [new File(), new File()]);
    let release;
    const held = new Promise(resolve => { release = resolve; });
    const copy = () => { const next = new File(); next.bytes = file.bytes.slice(); return next; };
    const first = OpfsStore.open(copy(), ledger, () => held), second = OpfsStore.open(copy(), ledger, () => held);
    assert.equal(ledger.recoveries, 2);
    await assert.rejects(OpfsStore.open(copy(), ledger, async () => {}), /recovery capacity/);
    release(); for (const opened of await Promise.all([first, second])) opened.close();
    await assert.rejects(OpfsStore.open(copy(), ledger, async () => { throw new Error('Retired'); }), /Retired/);
    assert.equal(ledger.recoveries, 0); assert.equal(ledger.stores, 0); assert.equal(ledger.bytes, 0);
    assert.throws(() => OpfsStore.empty(copy(), ledger), /empty database/);
});
