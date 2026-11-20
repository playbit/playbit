import { encodeId, decodeId } from './identifier.ts';
import { files } from '../tests/file-metadata-fixture.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { bootstrap, createHost, account, key, version } from '../tests/host-fixture.mjs';
const empty = new Uint8Array(), encoder = new TextEncoder(), decoder = new TextDecoder();

test('compiled Files pin accepted directory work through parent close and retain captured list cursors', async () => {
    let ioForbidden = false, ioCalls = 0;
    const target = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0x10))), undefined, () => 1000,
        imports => Object.fromEntries(Object.entries(imports).map(([name, fn]) => [name, (...args) => {
            if (name !== 'now' && name !== 'random') { ioCalls++; assert.equal(ioForbidden, false, `${name} during memory-only admission/delivery`); }
            return fn(...args);
        }])));
    const api = files(target), [root] = api.roots();
    ioForbidden = true;
    const listOpen = api.begin(root, 26); assert(listOpen > 0);
    const close = api.begin(root, 25); assert(close > 0);
    assert.equal(api.begin(root, 24), -8);
    assert.equal(api.host.pb_web_guest_reset(), -12);
    ioForbidden = false;
    assert.equal(api.execute(close), -6, 'Close waits for earlier accepted operations');
    const { file: list } = api.done(listOpen);
    api.done(close);
    const first = api.begin(list, 27); assert(first > 0);
    assert.equal(api.begin(list, 27), -12);
    const task = api.execute(first); assert.equal(task.kind, 'done'); assert(task.output.Entry);
    assert.equal(api.host.pb_web_file_qualify(first), 0);
    ioForbidden = true;
    const expected = api.result(first); assert.deepEqual(expected, task.output);
    assert.deepEqual(api.result(first), expected, 'Peeking cannot advance the cursor');
    assert.equal(api.host.pb_web_file_release(first), 0);
    ioForbidden = false;
    const retry = api.begin(list, 27);
    assert.deepEqual(api.execute(retry).output, expected, 'Disposal cannot consume an entry');
    assert.equal(api.host.pb_web_file_qualify(retry), 0);
    ioForbidden = true;
    api.finish(retry, expected);
    ioForbidden = false;
    const end = api.begin(list, 27); assert.equal(api.execute(end).output, 'End');
    assert.equal(api.host.pb_web_file_qualify(end), 0);
    assert.equal(api.host.pb_web_file_list_close(...list), 0);
    assert.equal(api.host.pb_web_file_check(end, 1), -13);
    assert.equal(api.host.pb_web_file_release(end), 0);
    assert.equal(api.begin(list, 27), -8);
    assert(ioCalls > 0);
    assert.equal(api.host.pb_web_guest_reset(), 0);
});

test('compiled File admission bounds pending work and captured-list reservations without consuming roots', async () => {
    const target = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0x30)))), api = files(target), [root] = api.roots();
    const operations = Array.from({ length: 64 }, () => api.begin(root, 24));
    assert(operations.every(token => token > 0)); assert.equal(api.begin(root, 25), -9);
    for (const token of operations) assert.equal(api.host.pb_web_file_release(token), 0);
    const lists = Array.from({ length: 8 }, () => api.begin(root, 26));
    assert(lists.every(token => token > 0)); assert.equal(api.begin(root, 26), -9);
    for (const token of lists) assert.equal(api.host.pb_web_file_release(token), 0);
    assert.deepEqual(api.done(api.begin(root, 24)).value, { Status: { kind: 2, flags: 0, size: 0, modification_time: 0 } });
    const closed = api.begin(root, 25); api.done(closed);
    assert.equal(api.host.pb_web_file_release(closed), -8);
});

test('compiled retained reads keep the selected revision and require fresh authorization before bounded delivery', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x50))), blob = encodeId(new Uint8Array(15).fill(0x77));
    profile.local.blobs = [{ id: blob, name: 'folder/note.txt', bytes: [1, 2, 3] }];
    const target = await createHost(profile), api = files(target);
    const encoded = encoder.encode(JSON.stringify(blob));
    const reference = JSON.parse(decoder.decode(target.raw('pb_seed_blob_reference', encoded, encoded.length)));
    reference.encrypted_key = '88'.repeat(32); target.call('pb_seed_blob', reference);
    const [root] = api.roots();
    const { file: reader } = api.done(api.begin(root, 23, { flags: 1, data: encoder.encode('folder/note.txt') }));
    const read = api.begin(reader, 8, { length: 3 });
    assert.equal(api.host.pb_web_file_dispose(...reader), 0);
    const task = api.execute(read); assert.equal(task.kind, 'read'); assert.equal(task.reference.revision, reference.revision);
    api.stage(Uint8Array.from([0,0,0,0,0,0,0,0,1,2,3]));
    assert.equal(api.host.pb_web_file_read_complete(read, 3), 0);
    assert.equal(api.host.pb_web_file_qualify(read), 0);
    assert.deepEqual(api.result(read), { Read: { bytes: [1, 2, 3] } });
    assert.equal(target.edit(true), 0);
    assert.equal(api.host.pb_web_file_check(read, 1), -6, 'Storage changes retire the cached delivery fence');
    assert.equal(api.result(read), -6, 'No bytes or required sizes escape an obsolete qualification');
    assert.equal(api.host.pb_web_file_qualify(read), 0);
    assert.deepEqual(api.result(read), { Read: { bytes: [1, 2, 3] } });
    assert.equal(api.execute(read), -8, 'Requalification never replays accepted I/O');
    api.finish(read, api.result(read));
});

test('compiled File results are withheld after grant withdrawal while accepted closes remain collectible', async () => {
    const personalId = encodeId(new Uint8Array(16).fill(0x90)), profile = bootstrap(encodeId(new Uint8Array(16).fill(0x91)));
    const main = await createHost(profile);
    const personal = await createHost(bootstrap(personalId, { personal: true }));
    for (let slot = 1; slot <= 2; slot++) main.stores.set(slot + 2, personal.stores.get(slot));
    main.call('pb_register_personal', { volume: { id: personalId, owner: account, name: 'Personal', share_key_with_service: false, personal: true }, key, replica_ids: [7,8] });
    main.call('pb_select_personal', { session: profile.session.id, grants: [{ volume: personalId, write: false, incarnation: encodeId(new Uint8Array(16).fill(0x94)) }] });
    const api = files(main), root = api.roots()[1];
    const status = api.begin(root, 24), close = api.begin(root, 25);
    assert.equal(api.execute(status).kind, 'done'); assert.equal(api.host.pb_web_file_qualify(status), 0);
    main.call('pb_select_personal', { session: profile.session.id, grants: [] });
    assert.equal(api.result(status), -2); assert.equal(api.host.pb_web_file_release(status), 0);
    assert.deepEqual(api.done(close).value, { Close: { error: 0, publication: 0, recovery: null } });
});

// These tests drive the Rust destination/publication state directly. The worker
// owns actual OPFS writes and encryption, exercised by the web browser I/O tests.
test('compiled private writers bind destinations, order accepted writes, and publish only at close', async () => {
    const target = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0xa0)))), api = files(target), [root] = api.roots();
    const open = api.begin(root, 23, { flags: 4, data: encoder.encode('created/note.txt') });
    assert.equal(api.execute(open).kind, 'create_writer');
    assert.equal(api.host.pb_web_file_writer_ready(open), 0);
    assert.equal(api.host.pb_web_file_qualify(open), 0);
    const writer = api.finish(open, api.result(open));
    const missing = api.begin(root, 23, { flags: 1, data: encoder.encode('created/note.txt') });
    assert.equal(api.execute(missing), -1); assert.equal(api.host.pb_web_file_release(missing), 0);
    const first = api.begin(writer, 9, { offset: 6n, data: Uint8Array.from([7, 8]) });
    const second = api.begin(writer, 9, { offset: 1n, data: Uint8Array.from([9]) });
    const close = api.begin(writer, 25, { flags: 1 });
    assert.equal(api.begin(writer, 24), -8);
    assert.equal(api.host.pb_web_file_release(first), -12, 'Disposal cannot cancel an accepted write');
    assert.equal(api.execute(second), -6); assert.equal(api.execute(close), -6);
    assert.equal(api.execute(first).kind, 'write');
    assert.equal(api.host.pb_web_file_write_complete(first, 0), 0);
    assert.equal(api.execute(second).kind, 'write');
    assert.equal(api.host.pb_web_file_write_complete(second, 0), 0);
    assert.equal(api.execute(close).kind, 'close');
    api.stage(Uint8Array.from([...Array(8).fill(0), ...Array(32).fill(1)]));
    assert.equal(api.host.pb_web_file_seal(close, 7n), -3);
    const sealedLength = api.host.pb_web_file_seal(close, 8n); assert(sealedLength > 0);
    const reference = JSON.parse(decoder.decode(new Uint8Array(api.host.memory.buffer, api.host.pb_output(), sealedLength)));
    reference.encrypted_key = '02'.repeat(32);
    const encoded = encoder.encode(JSON.stringify(reference)), input = new Uint8Array(8 + encoded.length); input.set(encoded, 8); api.stage(input);
    assert.equal(api.host.pb_web_file_publish(close, encoded.length), 0);
    assert.deepEqual(api.result(close), { Close: { error: 0, publication: 1, recovery: null } });
    api.finish(close, api.result(close));
    for (const token of [first, second]) assert.equal(api.host.pb_web_file_release(token), 0);
    const reader = api.done(api.begin(root, 23, { flags: 1, data: encoder.encode('created/note.txt') })).file;
    assert.equal(api.done(api.begin(reader, 24)).value.Status.size, 8);
    const read = api.begin(reader, 8, { offset: 2n, length: 2 });
    const task = api.execute(read); assert.equal(task.reference.revision, reference.revision);
    assert.equal(api.host.pb_web_file_io_failed(read, -5), 0); assert.equal(api.host.pb_web_file_release(read), 0);
    assert.equal(api.host.pb_web_file_dispose(...reader), 0);
});

test('failed private writes never expose complete recovery and stage reservations include uncollected opens', async () => {
    const target = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0xb0)))), api = files(target), [root] = api.roots();
    const opens = Array.from({ length: 8 }, (_, i) => api.begin(root, 23, { flags: 4, data: encoder.encode(`stage/${i}`) }));
    assert(opens.every(token => token > 0));
    assert.equal(api.begin(root, 23, { flags: 4, data: encoder.encode('stage/full') }), -9);
    const token = opens.shift(); assert.equal(api.execute(token).kind, 'create_writer');
    assert.equal(api.host.pb_web_file_writer_ready(token), 0); assert.equal(api.host.pb_web_file_qualify(token), 0);
    const writer = api.finish(token, api.result(token));
    const write = api.begin(writer, 9, { data: Uint8Array.of(1) }); assert.equal(api.execute(write).kind, 'write');
    assert.equal(api.host.pb_web_file_write_complete(write, -5), 0);
    assert.equal(api.host.pb_web_file_release(write), 0);
    const close = api.begin(writer, 25); assert.equal(api.execute(close).kind, 'close');
    api.stage(new Uint8Array(40)); assert.equal(api.host.pb_web_file_seal(close, 0n), -13);
    assert.equal(api.host.pb_web_file_close_failed(close, -13), 0);
    assert.deepEqual(api.result(close), { Close: { error: -13, publication: 0, recovery: null } });
    api.finish(close, api.result(close));
    for (const token of opens) assert.equal(api.host.pb_web_file_release(token), 0);
    const retry = api.begin(root, 23, { flags: 4, data: encoder.encode('stage/retry') }); assert(retry > 0);
    assert.equal(api.host.pb_web_file_release(retry), 0);
});

function openedWriter(api, root, path, flags = 4) {
    const open = api.begin(root, 23, { flags, data: encoder.encode(path) }); assert(open > 0);
    assert.equal(api.execute(open).kind, 'create_writer');
    assert.equal(api.host.pb_web_file_writer_ready(open), 0); assert.equal(api.host.pb_web_file_qualify(open), 0);
    return api.finish(open, api.result(open));
}
function publishEmpty(api, file) {
    const close = api.begin(file, 25); assert.equal(api.execute(close).kind, 'close');
    api.stage(Uint8Array.from([...Array(8).fill(0), ...Array(32).fill(3)]));
    const length = api.host.pb_web_file_seal(close, 0n); assert(length > 0);
    const reference = JSON.parse(decoder.decode(new Uint8Array(api.host.memory.buffer, api.host.pb_output(), length)));
    reference.encrypted_key = '04'.repeat(32);
    const bytes = encoder.encode(JSON.stringify(reference)), input = new Uint8Array(8 + bytes.length); input.set(bytes, 8); api.stage(input);
    assert.equal(api.host.pb_web_file_publish(close, bytes.length), 0);
    assert.equal(api.host.pb_web_file_qualify(close), 0);
    assert.deepEqual(api.result(close), { Close: { error: 0, publication: 1, recovery: null } });
    api.finish(close, api.result(close));
    return reference;
}

test('exclusive close conflicts transfer complete recovery without reviving the writer or releasing its stage quota', async () => {
    const target = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0xc0)))), api = files(target), [root] = api.roots();
    const a = openedWriter(api, root, 'conflict', 12), b = openedWriter(api, root, 'conflict', 12);
    publishEmpty(api, a);
    assert.equal(api.host.pb_web_file_cleanup(), 8);
    const published = new DataView(api.host.memory.buffer).getBigUint64(api.host.pb_output(), true);
    assert.equal(api.host.pb_web_file_cleaned(published), 0);
    const close = api.begin(b, 25); assert.equal(api.execute(close).kind, 'close');
    api.stage(new Uint8Array(40)); assert.equal(api.host.pb_web_file_seal(close, 0n), -10);
    assert.equal(api.host.pb_web_file_close_failed(close, -10), 0);
    assert.equal(api.result(close), -6, 'Recovery must be qualified before delivery');
    assert.equal(api.host.pb_web_file_qualify(close), 0);
    const outcome = api.result(close); assert.equal(outcome.Close.error, -10); assert.equal(outcome.Close.publication, 0);
    assert(outcome.Close.recovery > 0);
    const recovery = api.finish(close, outcome); assert.equal(api.begin(b, 24), -8);
    assert.equal(api.done(api.begin(recovery, 24)).value.Status.flags, 5);
    const pending = Array.from({ length: 7 }, (_, i) => api.begin(root, 23, { flags: 4, data: encoder.encode(`other-${i}`) }));
    assert(pending.every(token => token > 0));
    assert.equal(api.begin(root, 23, { flags: 4, data: encoder.encode('overflow') }), -9);
    const read = api.begin(recovery, 8, { length: 10 }); assert.equal(api.execute(read).kind, 'private_read');
    assert.equal(api.host.pb_web_file_dispose(...recovery), 0);
    assert.equal(api.begin(root, 23, { flags: 4, data: encoder.encode('still-pinned') }), -9);
    api.stage(new Uint8Array(8)); assert.equal(api.host.pb_web_file_read_complete(read, 0), 0);
    assert.equal(api.host.pb_web_file_release(read), 0);
    assert.equal(api.begin(root, 23, { flags: 4, data: encoder.encode('cleanup-pending') }), -9);
    const cleanupLength = api.host.pb_web_file_cleanup(); assert.equal(cleanupLength, 8);
    const retired = new DataView(api.host.memory.buffer, api.host.pb_output(), cleanupLength);
    const engines = [retired.getBigUint64(0, true)];
    for (const engine of engines) assert.equal(api.host.pb_web_file_cleaned(engine), 0);
    const next = api.begin(root, 23, { flags: 4, data: encoder.encode('released') }); assert(next > 0);
    for (const token of [...pending, next]) assert.equal(api.host.pb_web_file_release(token), 0);
});

test('selected blob revision survives replacement and accepted reads retain the view ceiling after File disposal', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xd0))), blob = encodeId(new Uint8Array(15).fill(0xd2));
    profile.local.blobs = [{ id: blob, name: 'note.txt', bytes: [1, 2, 3] }];
    const target = await createHost(profile), api = files(target), encoded = encoder.encode(JSON.stringify(blob));
    const old = JSON.parse(decoder.decode(target.raw('pb_seed_blob_reference', encoded, encoded.length)));
    old.encrypted_key = 'd3'.repeat(32); target.call('pb_seed_blob', old);
    const [root] = api.roots(), readers = Array.from({ length: 32 }, () => api.done(api.begin(root, 23, { flags: 1, data: encoder.encode('note.txt') })).file);
    let overflow = api.begin(root, 23, { flags: 1, data: encoder.encode('note.txt') });
    assert.equal(api.execute(overflow), -9); assert.equal(api.host.pb_web_file_release(overflow), 0);
    const read = api.begin(readers[0], 8, { length: 3 });
    assert.equal(api.host.pb_web_file_dispose(...readers[0]), 0);
    overflow = api.begin(root, 23, { flags: 1, data: encoder.encode('note.txt') });
    assert.equal(api.execute(overflow), -9); assert.equal(api.host.pb_web_file_release(overflow), 0);
    const current = publishEmpty(api, openedWriter(api, root, 'note.txt'));
    assert.notEqual(current.revision, old.revision);
    const task = api.execute(read); assert.equal(task.reference.revision, old.revision);
    api.stage(Uint8Array.from([0,0,0,0,0,0,0,0,1,2,3])); assert.equal(api.host.pb_web_file_read_complete(read, 3), 0);
    assert.equal(api.host.pb_web_file_qualify(read), 0); api.finish(read, api.result(read));
    const newest = api.done(api.begin(root, 23, { flags: 1, data: encoder.encode('note.txt') })).file;
    assert.equal(api.done(api.begin(newest, 24)).value.Status.size, 0);
    for (const file of [...readers.slice(1), newest]) assert.equal(api.host.pb_web_file_dispose(...file), 0);
});

test('mutable File operations preserve submission snapshots through later writes and failures', async () => {
    // Execute out of order deliberately; only execution retirement releases the barrier.
    const target = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0xe0)))), api = files(target), [root] = api.roots();
    const writer = openedWriter(api, root, 'ordered');
    const first = api.begin(writer, 9, { data: Uint8Array.of(1, 2, 3) });
    const before = api.begin(writer, 24);
    const second = api.begin(writer, 9, { offset: 8n, data: Uint8Array.of(4) });
    const after = api.begin(writer, 24);
    const failure = api.begin(writer, 9, { data: Uint8Array.of(5) });
    assert.equal(api.execute(before), -6);
    assert.equal(api.execute(first).kind, 'write');
    assert.equal(api.host.pb_web_file_write_complete(first, 0), 0);
    assert.equal(api.execute(second), -6, 'An earlier status must capture before the later write');
    assert.equal(api.execute(before).output.Status.size, 3);
    assert.equal(api.execute(second).kind, 'write');
    assert.equal(api.host.pb_web_file_write_complete(second, 0), 0);
    assert.equal(api.execute(after).output.Status.size, 9);
    assert.equal(api.execute(failure).kind, 'write');
    assert.equal(api.host.pb_web_file_write_complete(failure, -5), 0);
    for (const token of [after, before, first, second]) assert.equal(api.host.pb_web_file_qualify(token), 0);
    assert.equal(api.result(after).Status.size, 9);
    assert.equal(api.result(before).Status.size, 3);
    assert.equal(api.result(first), 'Written'); assert.equal(api.result(second), 'Written');
    const failedStatus = api.begin(writer, 24), close = api.begin(writer, 25);
    assert.equal(api.execute(close), -6);
    assert.equal(api.execute(failedStatus), -13);
    assert.equal(api.execute(close).kind, 'close');
    assert.equal(api.host.pb_web_file_close_failed(close, -13), 0);
    assert.deepEqual(api.result(close), { Close: { error: -13, publication: 0, recovery: null } });
    for (const token of [first, before, second, after, failure, failedStatus, close]) assert.equal(api.host.pb_web_file_release(token), 0);
});

test('close recovery is withheld after entry authority changes without withdrawing its root grant', async () => {
    // A persistent entry denial retires a previously qualified recovery, preserving the close error.
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xf0))), blob = encodeId(new Uint8Array(15).fill(0xf2));
    profile.local.blobs = [{ id: blob, name: 'note.txt', bytes: [] }];
    const target = await createHost(profile), api = files(target), encoded = encoder.encode(JSON.stringify(blob));
    const reference = JSON.parse(decoder.decode(target.raw('pb_seed_blob_reference', encoded, encoded.length)));
    reference.encrypted_key = 'f3'.repeat(32); target.call('pb_seed_blob', reference);
    const [root] = api.roots(), writer = openedWriter(api, root, 'note.txt');
    const close = api.begin(writer, 25); assert.equal(api.execute(close).kind, 'close');
    assert.equal(api.host.pb_web_file_close_failed(close, -5), 0);
    assert.equal(api.result(close), -6);
    assert.equal(api.host.pb_web_file_qualify(close), 0);
    const recovery = api.result(close).Close.recovery; assert(recovery > 0);
    const terminal = Uint8Array.from([...Buffer.from(decodeId(profile.root.id)), ...decodeId(blob, 15), ...Buffer.from(decodeId(profile.root.id)), 4]);
    assert(target.raw('pb_live_terminal', terminal, terminal.length) instanceof Uint8Array);
    assert.equal(api.result(close), -6);
    assert.equal(api.host.pb_web_file_finish(close, BigInt(recovery), 0), -6);
    assert.equal(api.host.pb_web_file_qualify(close), 0);
    assert.deepEqual(api.result(close), { Close: { error: -5, publication: 0, recovery: null } });
    assert.equal(api.host.pb_web_file_finish(close, BigInt(recovery), 0), -3);
    api.finish(close, api.result(close));
    assert.equal(api.done(api.begin(root, 24)).value.Status.kind, 2, 'The root grant remains active');
});
