import test from 'node:test';
import { readFile } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { bootstrap, createHost } from '../tests/host-fixture.mjs';
import { files } from '../tests/file-metadata-fixture.mjs';
import { encodeId } from './identifier.ts';
import { ABI as L } from './guest-layout.ts';
import { GuestWork } from './guest-work.ts';
import { guestOperations } from './guest-operations.ts';
const encoder = new TextEncoder(), decoder = new TextDecoder();
function bundle(extras = []) {
    const contents = new Map([
        ['app/main.wasm', Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0)],
        ['app/manifest.json', encoder.encode('{"app_id":"test","name":"Bundle test"}')],
        ['app/volumes.json', encoder.encode('{}')],
        ['nested/empty', new Uint8Array()], ['nested/text', encoder.encode('hello')],
    ]);
    for (const [path, bytes] of extras) contents.set(path, bytes);
    const paths = [...contents.keys()].sort(), align = n => (n + 7) & ~7;
    const indexLength = paths.reduce((n, path) => n + align(24 + encoder.encode(path).length), 0);
    let length = 32 + indexLength;
    const entries = paths.map(path => { length = align(length); const offset = length; length += contents.get(path).length; return { path, offset }; });
    const bytes = new Uint8Array(length), view = new DataView(bytes.buffer);
    bytes.set(encoder.encode('PBAPP')); view.setUint32(8, 1, true); view.setUint32(12, paths.length, true);
    view.setBigUint64(16, BigInt(indexLength), true); view.setBigUint64(24, BigInt(length), true);
    let cursor = 32;
    for (const { path, offset } of entries) {
        const name = encoder.encode(path), data = contents.get(path);
        view.setUint32(cursor, name.length, true); view.setBigUint64(cursor + 8, BigInt(offset), true);
        view.setBigUint64(cursor + 16, BigInt(data.length), true); bytes.set(name, cursor + 24); bytes.set(data, offset);
        cursor = align(cursor + 24 + name.length);
    }
    return bytes;
}
test('structural bundle discovery retains its backing until rejection or explicit discard', async () => {
    // Independent wire construction inspects metadata without granting execution.
    let target, closed = 0, maximumRead = 0;
    const bytes = bundle();
    target = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0x41))), undefined, () => 1000, imports => ({
        ...imports, bundle_length: () => bytes.length,
        bundle_read(_slot, offset, pointer, length) {
            maximumRead = Math.max(maximumRead, length);
            new Uint8Array(target.host.exports.memory.buffer, pointer, length).set(bytes.subarray(offset, offset + length));
            return length;
        }, bundle_close() { closed++; },
    }));
    const host = target.host.exports;
    let length, steps = 0;
    do { length = host.pb_package(1); steps++; } while (length === 0);
    assert(length > 0); assert(steps > 1, 'verification yields between bounded steps');
    const metadata = JSON.parse(decoder.decode(new Uint8Array(host.memory.buffer, host.pb_output(), length)));
    assert.equal(metadata.name, 'Bundle test'); assert.equal(metadata.length, 8);
    assert.equal(closed, 0, 'structural discovery retains the OPFS backing');
    assert.equal(host.pb_package_adopt(1), -7, 'unsigned inspection grants no execution authority');
    assert.equal(closed, 1, 'rejected adoption releases its retained backing');
    const api = files(target), roots = api.roots();
    assert.equal(roots.length, 1, 'rejection leaves only the authorized profile root');
    assert.equal(host.pb_web_file_dispose(...roots[0]), 0);
    do { length = host.pb_package(2); } while (length === 0);
    assert(length > 0); assert.equal(closed, 1);
    assert.equal(host.pb_package_discard(2), 0);
    assert.equal(closed, 2, 'explicit discard releases the inspected backing');
    assert.equal(host.pb_package_discard(2), 0); assert.equal(closed, 2);
    assert.equal(host.pb_web_guest_reset(), 0);
    assert(maximumRead <= 65536);
});

test('signed TODO preparation requires Shared in the retained session before replacing its build', async () => {
    const web = new URL('../../../../target/web/', import.meta.url);
    const bytes = await readFile(new URL('todo.pbapp', web));
    const evidence = JSON.parse(await readFile(new URL('example-publisher.json', web)));
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x51)));
    let target, now = Math.floor(Date.now() / 1000);
    target = await createHost(profile, undefined, () => now, imports => ({
        ...imports, bundle_length: () => bytes.length,
        bundle_read(_slot, offset, pointer, length) {
            new Uint8Array(target.host.exports.memory.buffer, pointer, length).set(bytes.subarray(offset, offset + length));
            return length;
        }, bundle_close() {},
    }));
    const host = target.host.exports;
    let length;
    do { length = host.pb_package(1); } while (length === 0);
    assert(length > 0);
    const metadata = JSON.parse(decoder.decode(new Uint8Array(host.memory.buffer, host.pb_output(), length)));
    const app = { publisher: metadata.publisher, app_id: metadata.app_id };
    const bound = { ...profile.session, app };
    const authenticate = (session, retained) => {
        const input = encoder.encode(JSON.stringify({ anchor: { release_issuer: evidence.issuer }, status: evidence.status,
            expected_app: app, session_id: bound.id, bound_session: session, retained_example: retained }));
        return target.raw('pb_package_authenticate', input, 1, input.length);
    };
    assert.equal(authenticate(bound), -7, 'an exact-build Root session cannot acquire Shared on update');
    const message = host.pb_error();
    assert.match(decoder.decode(new Uint8Array(host.memory.buffer, host.pb_output(), message)), /session-volume.v1/);
    const request = encoder.encode(JSON.stringify({ app, session: bound.id }));
    const addresses = JSON.parse(decoder.decode(target.raw('pb_scope_addresses', request, request.length)));
    const shared = { ...bound, root_volume: addresses.world, shared_volume: addresses.shared };
    const original = JSON.parse(decoder.decode(authenticate(shared)));
    assert.equal(original.shared, addresses.shared);
    now += 366 * 86400;
    assert(authenticate(shared) < 0, 'expired signing certificate cannot establish a new admission');
    assert.equal(JSON.parse(decoder.decode(authenticate(shared, original.local_admission))).shared, addresses.shared);
    const different = [...original.local_admission]; different[33] ^= 1;
    assert(authenticate(shared, different) < 0, 'another build cannot borrow the retained admission time');
    assert.equal(host.pb_package_discard(1), 0);
});

test('capability discovery does not authenticate a structurally admitted package', async () => {
    // Supported callbacks do not substitute for publisher authentication.
    // The existing profile root still executes schema Begin/Take normally.
    let target, bytes;
    target = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0x42))), undefined, () => 1000, imports => ({
        ...imports, bundle_length: () => bytes.length,
        bundle_read(_slot, offset, pointer, length) {
            new Uint8Array(target.host.exports.memory.buffer, pointer, length).set(bytes.subarray(offset, offset + length));
            return length;
        }, bundle_close() {},
    }));
    const host = target.host.exports;
    host.pb_input(0); assert.equal(host.pb_web_capabilities(), 0);
    const result = host.pb_web_result(), view = new DataView(host.memory.buffer);
    const ranges = view.getUint32(result + L.capabilities.offsets.names_data, true);
    const text = view.getUint32(result + L.capabilities.offsets.bytes_data, true);
    const names = Array.from({ length: view.getUint32(result + L.capabilities.offsets.names_count, true) }, (_, index) => {
        const range = ranges + index * L.range.size;
        return decoder.decode(new Uint8Array(host.memory.buffer, text + view.getUint32(range + L.range.offsets.offset, true), view.getUint32(range + L.range.offsets.count, true)));
    });
    // The scalar dispatcher provides clock.read even without a host callback.
    const installed = new Set(Object.keys(target.operations).filter(name => name.startsWith('pb_')));
    installed.add('pb_clock_read');
    const features = ['ui.canvas.clear.v1', 'display-names.v1'];
    for (const feature of features) assert(names.includes(feature), feature);
    assert.equal(names.length, installed.size + features.length);
    for (const name of ['object.schema.read.begin', 'object.schema.read.take', 'object.schema.establish.begin',
        'kv.keys.begin', 'kv.read.begin', 'kv.put.begin', 'kv.delete.begin', 'entry.resolve.begin', 'session.read', 'session.channel', 'clock.read']) {
        assert(names.includes(name), name);
    }
    bytes = bundle([['app/manifest.json', encoder.encode(JSON.stringify({ app_id: 'test', name: 'Callbacks', required_host_features: names.toSorted() }))]]);
    let length; do { length = host.pb_package(1); } while (length === 0);
    assert(length > 0, `package validation returned ${length}`);
    assert.deepEqual(JSON.parse(decoder.decode(new Uint8Array(host.memory.buffer, host.pb_output(), length))).unsupported_host_features, []);
    assert.equal(host.pb_package_adopt(1), -7, 'known capabilities grant no publisher authority');
    const memory = new WebAssembly.Memory({ initial: 1 }), data = new DataView(memory.buffer), events = [];
    const work = new GuestWork(events, () => host);
    const operations = guestOperations({ memory: () => memory, host: () => host, events, work, log() {}, submit() {} });
    new Uint8Array(memory.buffer, 8, 32).set(target.handle()); operations.refreshVolumes();
    work.beginTurn(); const operation = operations.pb_schema_read_begin(8); assert(operation > 0n); work.endTurn();
    await operations.data.wait();
    data.setUint32(128, L.schemaSchema.size, true);
    for (const [offset, pointer, capacity, layout] of [[8, 1024, 16, L.schemaType], [24, 4096, 32, L.schemaField], [40, 8192, 4096, null]]) {
        data.setUint32(128 + offset, pointer, true); data.setUint32(132 + offset, capacity, true);
        if (layout) {
            data.setUint32(140 + offset, layout.size, true);
            for (let index = 0; index < capacity; index++) data.setUint32(pointer + index * layout.size, layout.size, true);
        }
    }
    work.beginTurn(); while (events.length) work.poll();
    assert.equal(operations.pb_schema_read_take(operation >> 32n, Number(operation & 0xffffffffn), 128), 0);
    work.endTurn();
    assert.equal(data.getUint32(128 + L.schemaSchema.offsets.types_count, true), 1);
    assert.equal(data.getUint32(128 + L.schemaSchema.offsets.fields_count, true), 2);
});

for (const required of [null, 'future.operation', 'trace.record', 'file.copy.begin', 'ui.canvas.pixels.v1', 'audio.open.begin', 'sound.open.pcm']) test(`structural inspection reports required features without granting unsigned execution (${required})`, async () => {
    let target, closed = 0;
    const manifest = { app_id: 'test', future: { label: 'advisory' }, name: 'Future',
        ...(required ? { required_host_features: [required] } : {}) };
    const bytes = bundle([
        ['app/manifest.json', encoder.encode(JSON.stringify(manifest))],
        ['app/extensions/example.note/data', encoder.encode('retained')],
    ]);
    target = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0x41))), undefined, () => 1000, imports => ({
        ...imports, bundle_length: () => bytes.length,
        bundle_read(_slot, offset, pointer, length) {
            new Uint8Array(target.host.exports.memory.buffer, pointer, length).set(bytes.subarray(offset, offset + length));
            return length;
        }, bundle_close() { closed++; },
    }));
    const host = target.host.exports;
    let length;
    do { length = host.pb_package(1); } while (length === 0);
    assert(length > 0, 'structural admission accepts unknown required host features');
    const metadata = JSON.parse(decoder.decode(new Uint8Array(host.memory.buffer, host.pb_output(), length)));
    assert.deepEqual(metadata.unsupported_host_features, required ? [required] : []);
    assert.equal(metadata.name, 'Future');
    assert.equal(host.pb_package_adopt(1), -7, 'unsigned packages cannot execute regardless of required features');
    assert.equal(closed, 1);
});
