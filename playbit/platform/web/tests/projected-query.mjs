import { encodeId } from '../src/identifier.ts';
// Actual Rust host + named web adapters over memory-backed OPFS handles.
// This covers result layouts and bounded stored-record decoding, not physical I/O.
import assert from "node:assert/strict";
import { bootstrap, createHost } from "./host-fixture.mjs";
import { guestOperations } from "../src/guest-operations.ts";
import { GuestWork } from "../src/guest-work.ts";
import { ABI as L } from "../src/guest-layout.ts";

let forcedRandomByte, runtime, maxRead = 0, reads = 0;
const transactions = new Map(), readers = new Map();
const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xa1)));
profile.local.databases[0].kind.schema.types[encodeId(new Uint8Array(16).fill(1))]
    .fields[encodeId(new Uint8Array(16).fill(2))].repeated = true;
const title = "x".repeat(80_000);
runtime = await createHost(profile, undefined, () => 1000, imports => ({ ...imports,
    begin(database, write) {
        const transaction = imports.begin(database, write);
        if (transaction > 0) transactions.set(transaction, database);
        return transaction;
    },
    end(transaction, commit) {
        transactions.delete(transaction);
        return imports.end(transaction, commit);
    },
    get(transaction, key, length, out, capacity) {
        if (transactions.get(transaction) === 65) reads++;
        return imports.get(transaction, key, length, out, capacity);
    },
    read_value(transaction, key, length, offset, out, capacity) {
        if (transactions.get(transaction) === 65) reads++;
        return imports.read_value(transaction, key, length, offset, out, capacity);
    },
    next(transaction, key, length, exclusive, reverse, out, capacity) {
        if (transactions.get(transaction) === 65) reads++;
        return imports.next(transaction, key, length, exclusive, reverse, out, capacity);
    },
    pin(transaction, key, length) {
        const reader = imports.pin(transaction, key, length);
        if (reader > 0) readers.set(reader, transactions.get(transaction));
        return reader;
    },
    reader_end(reader) {
        readers.delete(reader);
        return imports.reader_end(reader);
    },
    reader_read(reader, offset, out, capacity) {
        // Entry admission may read index metadata synchronously. Object content
        // in the named backing database must remain deferred until completion.
        if (readers.get(reader) === 65) reads++;
        maxRead = Math.max(maxRead, capacity);
        return imports.reader_read(reader, offset, out, capacity);
    },
    random(pointer, length) {
        if (forcedRandomByte === undefined) imports.random(pointer, length);
        else new Uint8Array(runtime.host.exports.memory.buffer, pointer, length).fill(forcedRandomByte);
    },
}));
const memory = new WebAssembly.Memory({ initial: 32 });
const data = new DataView(memory.buffer), bytes = new Uint8Array(memory.buffer);
const entry = 1024;
bytes.set(runtime.handle(), entry);
const events = [], work = new GuestWork(events, () => runtime.host.exports);
const operations = guestOperations({ memory: () => memory, host: () => runtime.host.exports, events, work, log() {}, submit() {} });
operations.refreshVolumes();
const hex = byte => byte.toString(16).padStart(2, "0").repeat(16);
const id = pointer => Buffer.from(bytes.subarray(pointer, pointer + 16)).toString("hex");
const regions = [[32, 4096, 64, L.objectInfo], [48, 8192, 128, L.field], [64, 16384, 1024, L.value], [80, 262144, 1048576, null]];
function initialize(pointer, layout) {
    bytes.fill(0, pointer, pointer + layout.size);
    data.setUint32(pointer, layout.size, true);
}
function turn(callback) {
    work.beginTurn();
    try { return callback(); } finally { work.endTurn(); }
}
// Only the fixture driver waits. The guest callbacks are separate bounded turns.
async function complete(begin, take, short = false) {
    const before = reads, status = turn(begin);
    assert.equal(reads, before, "Begin performs no stored-record I/O");
    if (status < 0) return status;
    const generation = status >> 32n, slot = Number(status & 0xffffffffn);
    assert.equal(turn(() => take(generation, slot)), -6, "same-turn Take is pending");
    if (operations.data.waiting) await operations.data.wait();
    return turn(() => {
        initialize(432, L.event);
        assert.equal(operations.pb_event_poll(432, 464, 8), 8);
        assert.equal(data.getBigUint64(464, true), status);
        const before = reads;
        if (short) {
            data.setUint32(128 + L.page.offsets.values_capacity, 0, true);
            assert.equal(take(generation, slot), -11);
            assert(operations.data.pending, "short output retains the completion");
            data.setUint32(128 + L.page.offsets.values_capacity, 1024, true);
        }
        const result = take(generation, slot);
        assert.equal(reads, before, "Take and short retry perform no stored-record I/O");
        assert.equal(operations.data.pending, 0);
        return result;
    });
}
async function query(fields, after, short = false) {
    initialize(8, L.query); bytes.fill(1, 8 + L.query.offsets.type_id, 8 + L.query.offsets.type_id + 16);
    if (after) bytes.fill(after, 8 + L.query.offsets.after, 8 + L.query.offsets.after + 16);
    data.setUint32(8 + L.query.offsets.flags, (after ? 1 : 0) | (fields ? 2 : 0), true);
    data.setUint32(8 + L.query.offsets.limit, 64, true);
    if (fields) {
        data.setUint32(8 + L.query.offsets.fields, 80, true); data.setUint32(8 + L.query.offsets.count, fields.length, true);
        fields.forEach((field, i) => bytes.fill(field, 80 + i * 16, 96 + i * 16));
    }
    initialize(128, L.page);
    for (const [at, pointer, capacity, layout] of regions) {
        data.setUint32(128 + at, pointer, true); data.setUint32(132 + at, capacity, true);
        if (layout) {
            data.setUint32(140 + at, layout.size, true);
            for (let i = 0; i < capacity; i++) initialize(pointer + i * layout.size, layout);
        }
    }
    const status = await complete(() => operations.pb_object_query_begin(entry, 8),
        (generation, slot) => operations.pb_object_query_take(generation, slot, 128), short);
    assert.equal(runtime.stores.get(65).resources.pins, 0, "query releases every pinned value");
    assert.equal(runtime.stores.get(65).resources.transactions, 0, "query closes its snapshot");
    if (status < 0) return status;
    const objects = [];
    for (let i = 0; i < data.getUint32(128 + L.page.offsets.objects_count, true); i++) {
        const record = 4096 + i * L.objectInfo.size, fields = [];
        const start = data.getUint32(record + L.objectInfo.offsets.fields_offset, true), count = data.getUint32(record + L.objectInfo.offsets.fields_count, true);
        for (let j = 0; j < count; j++) {
            const field = 8192 + (start + j) * L.field.size, values = [];
            for (let k = 0; k < data.getUint32(field + L.field.offsets.values_count, true); k++) {
                const value = 16384 + (data.getUint32(field + L.field.offsets.values_offset, true) + k) * L.value.size, kind = data.getUint32(value + L.value.offsets.kind, true);
                if (kind === 1) {
                    const at = 262144 + data.getUint32(value + L.value.offsets.bytes_offset, true), size = data.getUint32(value + L.value.offsets.bytes_count, true);
                    values.push(new TextDecoder().decode(bytes.subarray(at, at + size)));
                } else if (kind === 2) values.push(data.getBigUint64(value + L.value.offsets.scalar, true) === 1n);
                else throw new Error(`Unexpected field kind ${kind}`);
            }
            fields.push({ id: id(field + L.field.offsets.id), values: data.getUint32(field + L.field.offsets.present, true) ? values : null });
        }
        objects.push({ id: id(record + L.objectInfo.offsets.id), type: id(record + L.objectInfo.offsets.type_id), fields });
    }
    return { objects, next: data.getUint32(128 + L.page.offsets.has_next, true) ? id(128 + L.page.offsets.next) : null, projected: Boolean(data.getUint32(128 + L.page.offsets.projected, true)) };
}
async function editText(object, texts) {
    if (typeof texts === "string") texts = [texts];
    bytes.fill(object, 256, 272); initialize(512, L.fieldInput); bytes.fill(2, 520, 536);
    data.setUint32(512 + L.fieldInput.offsets.values, 2048, true);
    data.setUint32(512 + L.fieldInput.offsets.count, texts.length, true);
    data.setUint32(512 + L.fieldInput.offsets.stride, L.valueInput.size, true);
    data.setUint32(512 + L.fieldInput.offsets.present, 1, true);
    let offset = 65536;
    for (const [index, text] of texts.entries()) {
        const value = 2048 + index * L.valueInput.size, encoded = new TextEncoder().encode(text);
        initialize(value, L.valueInput); data.setUint32(value + L.valueInput.offsets.kind, 1, true);
        data.setUint32(value + L.valueInput.offsets.length, encoded.length, true);
        data.setUint32(value + L.valueInput.offsets.data, offset, true);
        bytes.set(encoded, offset); offset += encoded.length;
    }
    assert.equal(await complete(() => operations.pb_object_edit_begin(entry, 256, 512, 1, L.fieldInput.size),
        (generation, slot) => operations.pb_object_edit_take(generation, slot)), 0);
}
await editText(1, title);
const ids = await query();
assert.equal(ids.objects.length, 1); assert.equal(ids.objects[0].id, hex(1)); assert.equal(ids.projected, false);
const projected = await query([2, 3], undefined, true);
assert.deepEqual(projected.objects[0].fields, [{ id: hex(2), values: [title] }, { id: hex(3), values: [false] }]);
assert.deepEqual(await query([2, 3]), projected, "a distinct Begin reads an equivalent current snapshot");
assert.equal(await query([]), -3); assert.equal(await query([2, 2]), -3);
// Tombstone preserves the stored field, while an absent projection stays absent.
bytes.fill(1, 256, 272); initialize(512, L.fieldInput); bytes.fill(3, 520, 536);
data.setUint32(512 + L.fieldInput.offsets.stride, L.valueInput.size, true);
assert.equal(await complete(() => operations.pb_object_edit_begin(entry, 256, 512, 1, L.fieldInput.size),
    (generation, slot) => operations.pb_object_edit_take(generation, slot)), 0);
assert.deepEqual((await query([3, 4])).objects[0].fields, [{ id: hex(3), values: null }]);
assert.deepEqual(await query(), ids);
forcedRandomByte = 2; bytes.fill(1, 256, 272);
assert.equal(await complete(() => operations.pb_object_create_begin(entry, 256, 0, 0, L.fieldInput.size),
    (generation, slot) => operations.pb_object_create_poll(generation, slot, 320)), 0);
forcedRandomByte = undefined;
assert.equal(id(320), hex(2));
const rawFieldKey = object => Buffer.concat([Buffer.from([0]), Buffer.from(
    `field/${encodeId(new Uint8Array(16).fill(object))}/${encodeId(new Uint8Array(16).fill(2))}`)]);
// A repeated text field with bounded optional value extensions exceeds the
// 2 MiB OPFS get cap while its projected output fits. Seed its creation and
// origin-clock bindings through the named ABI, then extend the stored PBR value
// to exercise the query reader alone. Ordinary edits retain their get cap.
const texts = Array(512).fill("\0".repeat(700));
await editText(2, texts);
const seeded = Buffer.from(runtime.stores.get(65).get(rawFieldKey(2)));
// Full kvdb leaf: hash[32], flags[1], clock[8], followed by PBR magic.
const leafHeader = 41;
assert.deepEqual(seeded.subarray(leafHeader, leafHeader + 4), Buffer.from([80, 66, 82, 1]));
function mapField(value, wanted, update) {
    assert.equal(value[0], 8, "PBR struct");
    const parts = [value.subarray(0, 1)]; let found = false;
    for (let offset = 1; offset < value.length;) {
        const length = value.readUInt32LE(offset + 2), end = offset + 6 + length;
        assert(end <= value.length);
        if ((value.readUInt16LE(offset) & 0x3fff) === wanted) {
            assert.equal(found, false); found = true;
            const body = update(value.subarray(offset + 6, end)), header = Buffer.from(value.subarray(offset, offset + 6));
            header.writeUInt32LE(body.length, 2); parts.push(header, body);
        } else parts.push(value.subarray(offset, end));
        offset = end;
    }
    assert(found); return Buffer.concat(parts);
}
const extension = Buffer.alloc(4096, 0xa5);
extension.writeUInt16LE(99, 0); extension.writeUInt32LE(4090, 2);
extension[6] = 255; // An opaque optional wire kind, within the 4 KiB extension cap.
const record = mapField(seeded.subarray(leafHeader + 4), 2, cell => mapField(cell, 2, sequence => {
    assert.equal(sequence[0], 7, "PBR sequence");
    const parts = [sequence.subarray(0, 1)]; let count = 0;
    for (let offset = 1; offset < sequence.length;) {
        const length = sequence.readUInt32LE(offset), end = offset + 4 + length;
        assert(end <= sequence.length);
        const value = sequence.subarray(offset + 4, end), header = Buffer.alloc(4);
        assert.deepEqual(value.subarray(0, 4), Buffer.from([10, 1, 0, 8]), "PBR text variant");
        header.writeUInt32LE(length + extension.length); parts.push(header, value, extension);
        offset = end; count++;
    }
    assert.equal(count, texts.length); return Buffer.concat(parts);
}));
// Preserve the kvdb origin, immutable creation binding, and edit stamp.
const original = Buffer.concat([seeded.subarray(0, leafHeader + 4), record]);
let transaction = runtime.stores.get(65).begin(true);
transaction.put(rawFieldKey(2), original); transaction.end(true);
const admitted = await query([2]);
assert.equal(typeof admitted, "object", `admitted query returned ${admitted}`);
assert.equal(admitted.objects.length, 2); assert.equal(admitted.next, null);
assert.deepEqual(admitted.objects[1].fields, [{ id: hex(2), values: texts }]);
assert(maxRead <= 65536, "large records use bounded OPFS snapshot reads");
assert(original.length > 2 * 1024 * 1024, "binary field exercises pinned reads above the get cap");
assert(original.length - leafHeader <= 4 * 1024 * 1024, "binary field stays within the stored-record bound");
assert(data.getUint32(128 + L.page.offsets.bytes_count, true) < 1048576, "projected text fits the result budget");
const wrongClock = Buffer.from(original); wrongClock[leafHeader - 1] ^= 1;
transaction = runtime.stores.get(65).begin(true);
transaction.put(rawFieldKey(2), wrongClock); transaction.end(true);
assert.equal(await query([2]), -2, "field edit stamps must match their originating entry clocks");
// Corruption within the scratch budget fails the whole page, even after a prefix.
const truncated = original.subarray(0, original.length - 1);
transaction = runtime.stores.get(65).begin(true);
transaction.put(rawFieldKey(2), truncated); transaction.end(true);
assert.equal(await query([2]), -4, "truncated PBR fields fail the whole page");
// An 8 MiB cell exceeds the stored-record bound and therefore fails before
// reading or decoding its invalid binary contents.
// A nonempty page returns its admitted prefix; an oversized first record fails.
const oversized = new Uint8Array(8 * 1024 * 1024);
transaction = runtime.stores.get(65).begin(true);
transaction.put(rawFieldKey(2), oversized); transaction.end(true);
maxRead = 0;
const prefix = await query([2]);
assert.equal(typeof prefix, "object", `prefix query returned ${prefix}`);
assert.equal(prefix.objects.length, 1); assert.equal(prefix.next, hex(1));
assert.equal(await query([2], 1), -9);
transaction = runtime.stores.get(65).begin(true);
transaction.put(rawFieldKey(1), oversized); transaction.end(true);
assert.equal(await query([2]), -9);
assert.equal((await query()).objects.length, 2, "IDs-only queries never decode unrequested oversized fields");
assert(maxRead < truncated.length, "oversized cells are rejected before their bytes are read");
assert(reads > 0, "instrumentation observes backing reads during completion work");
assert.equal(transactions.size, 0, "the fixture closes all storage transactions");
assert.equal(readers.size, 0, "the fixture closes all pinned readers");
assert.equal(work.pending, false, "the fixture drains every accepted operation");
console.log("Projected web async operations: passed");
