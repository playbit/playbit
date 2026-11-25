import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { ABI } from './guest-layout.ts';
import { captureResult, Input, Output, record, recordArray, publishRecord } from './guest-memory.ts';

function initialize(memory, pointer, size) { const data = new DataView(memory.buffer); data.setUint32(pointer, size, true); data.setUint32(pointer + 4, 0, true); }

test('web layouts match every field of the compiler-verified C records', async () => {
    const expected = (await readFile(new URL('../../native/wasm-probe/layout-v2.tsv', import.meta.url), 'utf8')).trim().split('\n').map(line => Number(line.split(/\s+/).at(-1)));
    const actual = Object.values(ABI).flatMap(layout => [layout.size, layout.alignment, layout.fields.length, ...layout.fields.flat()]);
    assert.deepEqual(actual, expected);
});

test('volume role tail preserves the released 80-byte output contract', () => {
    const memory = new WebAssembly.Memory({ initial: 1 }), data = new DataView(memory.buffer);
    const source = new Uint8Array(ABI.volume.size), current = new DataView(source.buffer);
    current.setUint32(ABI.volume.offsets.role, 2, true);
    initialize(memory, 128, 80); data.setUint32(208, 0xdeadbeef, true);
    publishRecord(memory, record(memory, 128, ABI.volume), ABI.volume, source);
    assert.equal(data.getUint32(132, true), 80);
    assert.equal(data.getUint32(208, true), 0xdeadbeef, 'legacy output cannot overwrite the role tail');
    initialize(memory, 256, ABI.volume.size);
    publishRecord(memory, record(memory, 256, ABI.volume), ABI.volume, source);
    assert.equal(data.getUint32(260, true), 84);
    assert.equal(data.getUint32(256 + ABI.volume.offsets.role, true), 2);
    // A released host's short records normalize to an absent role (zero).
    initialize(memory, 512, ABI.volumes.size);
    data.setUint32(528, 128, true); data.setUint32(536, 1, true); data.setUint32(540, 80, true);
    const snapshot = captureResult(memory, 512, ABI.volumes, [[16, ABI.volume, 1]], 16, 104);
    assert.equal(new DataView(snapshot.arrays[0].bytes.buffer).getUint32(ABI.volume.offsets.role, true), 0);
});

test('record admission bounds capacity, rejects unknown input semantics and preserves future output tails', () => {
    const memory = new WebAssembly.Memory({ initial: 1 }), data = new DataView(memory.buffer);
    initialize(memory, 16, 24); assert.equal(record(memory, 16, ABI.event).length, 24);
    data.setUint32(16, 23, true); assert.throws(() => record(memory, 16, ABI.event), error => error.status === -11);
    initialize(memory, 16, 40); new Uint8Array(memory.buffer, 44, 12).fill(7);
    assert.throws(() => record(memory, 16, ABI.event, true), error => error.status === -7);
    const output = record(memory, 16, ABI.event), source = new Uint8Array(32);
    const event = new DataView(source.buffer);
    event.setUint32(16, 2, true);
    event.setUint32(24, 9, true);
    publishRecord(memory, output, ABI.event, source);
    assert.equal(data.getUint32(16, true), 40);
    assert.equal(data.getUint32(20, true), 28);
    assert.equal(data.getUint32(40, true), 9);
    assert(new Uint8Array(memory.buffer, 44, 12).every(byte => byte === 7));
    for (const capacity of [24, 25, 26, 27]) {
        initialize(memory, 16, capacity);
        new Uint8Array(memory.buffer, 40, 4).fill(7);
        publishRecord(memory, record(memory, 16, ABI.event), ABI.event, source);
        assert.equal(data.getUint32(20, true), 24);
        assert.equal(data.getUint32(40, true), 0x07070707);
    }
    data.setUint32(16, 4097, true); assert.throws(() => record(memory, 16, ABI.event), error => error.status === -9);
    initialize(memory, 64, 32); initialize(memory, 96, 40);
    assert.throws(() => recordArray(memory, 64, 2, 32, ABI.event, false, 2), error => error.status === -3);
    const extended = { minimum: 16, size: 24, alignment: 8, fields: [[0,4],[4,4],[8,4],[12,4],[16,8]] };
    initialize(memory, 128, 20); assert.throws(() => record(memory, 128, extended), error => error.status === -3);
    initialize(memory, 128, 16); assert.equal(record(memory, 128, extended).length, 16);
});

test('result capture bounds aggregate bytes before copying and copies using each guest stride', () => {
    const memory = new WebAssembly.Memory({ initial: 1 }), data = new DataView(memory.buffer);
    initialize(memory, 16, 36); initialize(memory, 128, 16); initialize(memory, 144, 16);
    data.setUint32(24, 128, true); data.setUint32(32, 2, true); data.setUint32(36, 16, true);
    data.setUint32(40, 256, true); data.setUint32(48, 3, true); new Uint8Array(memory.buffer, 256, 3).fill(9);
    const layouts = [[8, ABI.range, 4], [24, null, 4]];
    assert.throws(() => captureResult(memory, 16, ABI.capabilities, layouts, 8, 42), error => error.status === -9);
    const snapshot = captureResult(memory, 16, ABI.capabilities, layouts, 8, 43);
    assert.deepEqual(snapshot.arrays.map(array => array.count), [2, 3]);
    initialize(memory, 512, 44); initialize(memory, 640, 24); initialize(memory, 664, 24);
    new Uint8Array(memory.buffer, 656, 8).fill(7); new Uint8Array(memory.buffer, 680, 8).fill(8);
    data.setUint32(520, 640, true); data.setUint32(524, 2, true); data.setUint32(532, 24, true);
    data.setUint32(536, 768, true); data.setUint32(540, 3, true);
    const output = new Output(memory, 512, ABI.capabilities, layouts); assert.equal(output.copySnapshot(snapshot), 0);
    assert.equal(data.getUint32(512, true), 44); assert.equal(data.getUint32(516, true), 36);
    assert.equal(data.getUint32(640, true), 24); assert.equal(data.getUint32(644, true), 16);
    assert(new Uint8Array(memory.buffer, 656, 8).every(byte => byte === 7));
    assert(new Uint8Array(memory.buffer, 680, 8).every(byte => byte === 8));
    assert.deepEqual([...new Uint8Array(memory.buffer, 768, 3)], [9,9,9]);
    data.setUint32(48, 5, true); assert.throws(() => captureResult(memory, 16, ABI.capabilities, layouts, 8, 65536), error => error.status === -9);
});

test('nested mutation input uses caller strides and rejects semantic tails before staging', () => {
    const memory = new WebAssembly.Memory({ initial: 1 }), data = new DataView(memory.buffer);
    initialize(memory, 64, 48); data.setUint32(88, 256, true); data.setUint32(92, 2, true); data.setUint32(96, 88, true); data.setUint32(100, 1, true);
    initialize(memory, 256, 88); initialize(memory, 344, 88); data.setUint32(264, 2, true); data.setUint32(352, 2, true);
    const input = new Input(memory), fields = input.changes(64, 1, 48), staged = input.finish(), normalized = new DataView(staged.buffer);
    assert.equal(normalized.getUint32(fields, true), 40); assert.equal(normalized.getUint32(fields + 32, true), 80);
    const values = normalized.getUint32(fields + 24, true); assert.equal(normalized.getUint32(values, true), 80); assert.equal(normalized.getUint32(values + 80, true), 80);
    data.setUint8(424, 1); assert.throws(() => new Input(memory).changes(64, 1, 48), error => error.status === -7);
});
