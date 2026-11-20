import { encodeId, decodeId } from './identifier.ts';
import { files, prepareWriter, selectedReference } from '../tests/file-metadata-fixture.mjs';
import { Directory } from '../tests/memory-directory.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { File, bootstrap, createHost, entryId } from '../tests/host-fixture.mjs';
import { OpfsStore } from './opfs-store.ts';
import { VolumePublication, LocalVolumeChanged } from './volume-publication.ts';
import { retainRejectedCut } from './volume-archive.ts';
import { openRecoveryIndex, openRecoveryDatabase, openRecoveryFile } from './volume-recovery.ts';
import { RuntimeBlobs } from './runtime-blobs.ts';
import { cacheService } from '../tests/file-io-fixture.mjs';
import { encryptBlob } from './blob.ts';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { bloblogInteger, replicaJSON } from './replication.ts';
import { bloblogPage } from './bloblog.ts';
import { guestOperations } from './guest-operations.ts';
import { GuestWork } from "./guest-work.ts";
import { createWork } from '../tests/work-fixture.mjs';
const empty = new Uint8Array(), encoder = new TextEncoder(), decoder = new TextDecoder();
class ArchiveDirectory extends Directory {
    async getDirectoryHandle(name, options = {}) {
        if (!this.entries.has(name) && options.create) this.entries.set(name, new ArchiveDirectory());
        return super.getDirectoryHandle(name, options);
    }
    async getFileHandle(name, options = {}) {
        if (!this.entries.has(name) && options.create) this.entries.set(name, archiveFile(new File()));
        return super.getFileHandle(name, options);
    }
}
function archiveFile(file) {
    return { getFile: async () => new Blob([file.bytes.slice()]), createSyncAccessHandle: async () => file, file };
}
function hostHash(target) {
    const id = target.host.exports.pb_blob_hash_begin();
    return {
        update(bytes) { assert(target.raw('pb_blob_hash_update', bytes, id, bytes.length) instanceof Uint8Array); },
        finish() { return Buffer.from(target.raw('pb_blob_hash_finish', empty, id)).toString('hex'); },
        close() { target.host.exports.pb_blob_hash_close(id); },
    };
}
function hostJson(host, name, value) {
    const input = value === undefined ? empty : encoder.encode(JSON.stringify(value));
    const result = host.raw(name, input, ...(value === undefined ? [] : [input.length]));
    assert(result instanceof Uint8Array, `${name}: ${result}`);
    return result.length ? JSON.parse(decoder.decode(result)) : undefined;
}
async function recoveryOwner(profile, index, resources) {
    const owner = structuredClone(profile);
    owner.account = owner.root.owner;
    owner.session.creator = owner.root.owner = owner.account;
    owner.local = { ...bootstrap(owner.root.id).local, databases: [], blobs: [] };
    const stores = new Map();
    const copy = new File(); copy.bytes = index.bytes.slice();
    stores.set(1, await OpfsStore.open(copy, resources, async () => {}));
    for (const slot of [2, 3, 4]) stores.set(slot, OpfsStore.empty(new File(), resources));
    return createHost(owner, stores, () => 1000, imports => imports, false);
}
function blobKey(host, reference) {
    const input = new Uint8Array(80); input.fill(9, 0, 32);
    input.set(Buffer.from(decodeId(reference.volume)), 32); input.set(Buffer.from(reference.digest, 'hex'), 48);
    const result = host.raw('pb_blob_cipher_key', input);
    assert(result instanceof Uint8Array); return result;
}
// Optional, synthetic profiles for exercising this same retained cut through
// the packaged web UI. No production account or device data is involved.
async function exportRecoveryFixture(name, profile, host, root, publication, backings = []) {
    const output = process.env.PLAYBIT_WEB_RECOVERY_FIXTURES;
    if (!output) return;
    const owner = structuredClone(profile);
    owner.account = owner.root.owner;
    owner.session.creator = owner.root.owner = owner.account;
    owner.root_credentials = { database_key: owner.root_key, blob_key: Array(32).fill(9) };
    const files = {}, encode = bytes => Buffer.from(bytes).toString('base64');
    const walk = (directory, prefix = '') => {
        for (const [name, value] of directory.entries) {
            if (value.entries) walk(value, prefix + name + '/');
            else files[prefix + name] = encode(value.file.bytes);
        }
    };
    walk(root);
    const current = publication.directory(owner.root.id, publication.current(owner.root.id));
    files[current + '/index.kvdb'] = encode(host.stores.get(1).handle.bytes);
    files[current + '/revocations.kvdb'] = encode(host.stores.get(2).handle.bytes);
    for (const [entry, file] of backings) files[current + '/' + entry + '.kvdb'] = encode(file.bytes);
    const session = publication.prepare(owner.root.id);
    await publication.create(root, session); publication.publish(session, () => {}, () => {});
    const sessionPath = publication.directory(owner.root.id, session.generation);
    files[sessionPath + '/index.kvdb'] = encode(host.stores.get(3).handle.bytes);
    files[sessionPath + '/revocations.kvdb'] = encode(host.stores.get(4).handle.bytes);
    files['volume-generations.kvdb'] = encode(publication.pointers.handle.bytes);
    files['profile.json'] = encode(encoder.encode(JSON.stringify(owner)));
    await mkdir(output, { recursive: true });
    await writeFile(`${output}/${name}.json`, JSON.stringify({ files }));
}

async function storedHost(profile, rowsBySlot) {
    const initial = await createHost(profile), resources = initial.stores.get(1).resources;
    for (const [slot, rows] of rowsBySlot) {
        initial.stores.get(slot)?.close();
        const store = OpfsStore.empty(new File(), resources), tx = store.begin(true);
        for (const [key, value] of rows) tx.put(Uint8Array.from(key), Uint8Array.from(value));
        tx.end(true); initial.stores.set(slot, store);
    }
    return createHost(profile, initial.stores, () => 1000, imports => imports, false);
}

test('database upload catalog omits read-only backings while retaining writable and appendable entries', async () => {
    const fixture = JSON.parse(await readFile(new URL('../tests/backing-authority-vectors.json', import.meta.url)));
    const profile = bootstrap(fixture.volume, { personal: true });
    profile.local = null; profile.root.owner = encodeId(new Uint8Array(16).fill(0x09)); profile.root.share_key_with_service = true;
    const input = encoder.encode(JSON.stringify({ volume: fixture.volume, after: null, expected_root: null }));
    for (const mode of ['read_write', 'append_read', 'read_only', 'denied', 'write_only']) {
        const target = await storedHost(profile, [[1, fixture.snapshots[mode]], [65, fixture.backing]]);
        const bytes = target.raw('pb_upload_catalog_page', input, input.length);
        assert(bytes instanceof Uint8Array, mode);
        assert.deepEqual(JSON.parse(decoder.decode(bytes)).databases,
            mode === 'read_write' || mode === 'append_read' ? [fixture.entry] : [], mode);
    }
});

function cachedLog(target, entry) {
    const input = new Uint8Array(40); input.set(target.handle(1, 1, entry), 8);
    const bytes = target.raw('pb_web_bloblog_read', input, 8, 0, 0);
    assert(bytes instanceof Uint8Array);
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    return view.getUint32(5 + view.getUint32(0, true), true);
}

test('legacy pending log data stays hidden and retained across browser restart', async () => {
    const fixture = JSON.parse(await readFile(new URL('../tests/log-authority-vectors.json', import.meta.url)));
    const profile = bootstrap(fixture.volume, { personal: true });
    profile.local = null; profile.root.owner = encodeId(new Uint8Array(16).fill(0x09)); profile.root.share_key_with_service = true;
    const target = await storedHost(profile, [[1, fixture.snapshots.read_write], [65, fixture.backing]]);
    target.register(fixture.entry);
    const backing = target.stores.get(65).handle.bytes.slice();
    assert.equal(cachedLog(target, fixture.entry), 0);
    assert.deepEqual(target.stores.get(65).handle.bytes, backing);
    const restarted = await createHost(profile, target.stores, () => 1000, imports => imports, false);
    restarted.register(fixture.entry);
    assert.equal(cachedLog(restarted, fixture.entry), 0);
    assert.deepEqual(restarted.stores.get(65).handle.bytes, backing);
});

test('guest bloblog owns inputs and retains one remote result through Take retries', async () => {
    const fixture = JSON.parse(await readFile(new URL('../tests/log-authority-vectors.json', import.meta.url)));
    const profile = bootstrap(fixture.volume, { personal: true });
    profile.local = null; profile.root.owner = encodeId(new Uint8Array(16).fill(0x09)); profile.root.share_key_with_service = true;
    const target = await storedHost(profile, [[1, fixture.snapshots.read_write], [65, fixture.backing]]);
    target.register(fixture.entry);
    assert.equal(target.host.exports.pb_live_rebase(), 0);

    const memory = new WebAssembly.Memory({ initial: 4 }), events = [], work = new GuestWork(events, () => target.host.exports);
    work.beginTurn();
    let sent = 0, accept;
    const committed = new Promise(resolve => { accept = resolve; });
    const operations = guestOperations({ memory: () => memory, host: () => target.host.exports, events, work, log() {}, submit() {},
        async logConnection() { return { generation: 1, expires: Date.now() + 30000 }; },
        async appendLog(prepared, appendId) {
            sent++;
            assert.deepEqual([...appendId], Array(16).fill(7));
            await committed;
            assert(target.raw('pb_bloblog_cache_append', prepared, prepared.length) instanceof Uint8Array);
        },
        async readLog(_entry, read) { return read(); },
    });
    operations.refreshVolumes();
    let data = new DataView(memory.buffer);
    new Uint8Array(memory.buffer, 8, 32).set(target.handle(1, 1, fixture.entry));
    data.setUint32(64, 16, true); data.setUint32(72, 128, true); data.setUint32(76, 3, true);
    new Uint8Array(memory.buffer, 80, 8).fill(0xa5);
    data.setUint32(88, 24, true);
    new Uint8Array(memory.buffer, 128, 3).set([0, 255, 128]);
    new Uint8Array(memory.buffer, 160, 16).fill(7);
    assert.equal(operations.pb_bloblog_append_begin(1n, 1, 24, 64, 129, 24, 160), -9);
    assert.equal(operations.pb_bloblog_append_begin(1n, 1, 24, 64, 0, 24, 160), -3);
    assert.equal(operations.pb_bloblog_append_begin(1n, 1, 24, 64, 2, 23, 160), -3);
    for (const [size, expected] of [[15, -3], [25, -3], [4097, -3]]) {
        data.setUint32(88, size, true);
        assert.equal(operations.pb_bloblog_append_begin(1n, 1, 24, 64, 2, 24, 160), expected);
    }
    data.setUint32(88, 24, true); data.setUint8(104, 1);
    assert.equal(operations.pb_bloblog_append_begin(1n, 1, 24, 64, 2, 24, 160), -7);
    assert.equal(data.getBigUint64(512, true), 0n);
    data.setUint8(104, 0);
    const append = operations.pb_bloblog_append_begin(1n, 1, 24, 64, 2, 24, 160); assert(append > 0n);
    new Uint8Array(memory.buffer, 128, 3).fill(9);
    new Uint8Array(memory.buffer, 160, 16).fill(8);
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(sent, 1);
    assert.equal(operations.pb_bloblog_append_take(append >> 32n, Number(append & 0xffffffffn), 640, 104, 552), -6);
    assert.equal(cachedLog(target, fixture.entry), 0);
    accept();
    await operations.data.wait();
    assert.equal(operations.pb_bloblog_read_take(append >> 32n, Number(append & 0xffffffffn), 640, 1, 552), -8);
    assert.equal(operations.pb_bloblog_append_take(append >> 32n, Number(append & 0xffffffffn), 640, 1, 552), -11);
    const cursorLength = data.getUint32(552, true);
    memory.grow(1); data = new DataView(memory.buffer);
    assert.equal(operations.pb_bloblog_append_take(append >> 32n, Number(append & 0xffffffffn), 640, 104, 640), -3);
    assert.equal(operations.pb_bloblog_append_take(append >> 32n, Number(append & 0xffffffffn), 640, 104, 552), 0);
    assert.equal(operations.pb_bloblog_append_take(append >> 32n, Number(append & 0xffffffffn), 640, 104, 552), -8);
    const read = operations.pb_bloblog_read_begin(1n, 1, 24, 640, cursorLength); assert(read > 0n);
    await operations.data.wait();
    assert.equal(operations.pb_bloblog_read_take(read >> 32n, Number(read & 0xffffffffn), 1024, 65645, 552), 0);
    const countOffset = 1024 + 4 + data.getUint32(1024, true) + 1;
    assert.equal(data.getUint32(countOffset, true), 2);
    assert.equal(data.getUint32(countOffset + 4 + 55, true), 3);
    assert.deepEqual([...new Uint8Array(memory.buffer, countOffset + 4 + 59, 3)], [0, 255, 128]);

    assert.equal(sent, 1);
    assert.equal(cachedLog(target, fixture.entry), 2);

    const denied = await storedHost(profile, [[1, fixture.snapshots.read_only], [65, fixture.backing]]);
    denied.register(fixture.entry);
    const input = new Uint8Array(memory.buffer, 0, 176).slice();
    denied.handle(1, 1, fixture.entry);
    assert.equal(denied.raw('pb_web_bloblog_append', input, 8, 64, 2, 24, 160), -2);
    operations.data.dispose(); work.endTurn();
});

test('compiled log preparation preserves authorized Append and archives pending data when Append is withdrawn', async () => {
    const fixture = JSON.parse(await readFile(new URL('../tests/log-authority-vectors.json', import.meta.url)));
    const profile = bootstrap(fixture.volume, { personal: true });
    profile.local = null; profile.root.owner = encodeId(new Uint8Array(16).fill(0x09)); profile.root.share_key_with_service = true;
    for (const mode of ['read_only', 'append_read']) {
        const target = await storedHost(profile, [[1, fixture.snapshots.read_write], [65, fixture.backing]]);
        target.register(fixture.entry);
        const original = target.stores.get(65).handle;
        const source = await storedHost(profile, [[1, fixture.snapshots[mode]]]);
        await prepare(target, profile);
        await pull(source, target, 1, 100, true); await pull(source, target, 2, 101, true);
        const backing = await copyBacking(target, 65, 102);
        target.call('pb_volume_register_backing', { entry: fixture.entry, slot: 102, replica_id: 113 });
        const received = bloblogPage({ page: fixture.page, policy_revision: receipt().policy_revision, source: receipt().source, database_clock: 1n }, fixture.volume, fixture.entry);
        assert.equal(target.raw('pb_volume_log_page', received.bytes, received.bytes.length), -5,
            'Receipt requires completed explicit preparation');
        for (let repeat = 0; repeat < 2; repeat++) {
            const result = target.raw('pb_volume_log_prepare', empty);
            assert.equal(JSON.parse(decoder.decode(result)), true);
        }
        assert(target.raw('pb_volume_log_page', received.bytes, received.bytes.length) instanceof Uint8Array);
        assert.equal(JSON.parse(decoder.decode(target.raw('pb_volume_log_validate', empty))), true);
        assert.equal(target.host.exports.pb_volume_close_backing(), 0);
        await pull(source, target, 1, 100, true);
        const qualification = encoder.encode(replicaJSON(receipt()));
        assert(target.raw('pb_volume_qualify', qualification, qualification.length) instanceof Uint8Array);
        const { root, publication, operation } = await archivePrepared(target, profile, [[fixture.entry, original]]);
        const directory = await root.getDirectoryHandle(publication.directory(operation.volume, operation.generation));
        assert.equal(directory.entries.has('rejected'), mode === 'read_only');
        if (mode === 'read_only') {
            const archive = await directory.getDirectoryHandle('rejected');
            assert.deepEqual((await archive.getFileHandle(`${fixture.entry}.kvdb`)).file.bytes, original.bytes);
        }
        const acceptedIndex = target.stores.get(100).handle;
        publication.publish(operation, () => assert.equal(target.host.exports.pb_volume_validate(), 0),
            () => assert.equal(target.host.exports.pb_volume_publish(), 0));
        const reopened = await OpfsStore.open(backing, target.resources, async () => {});
        const pending = fixture.backing.filter(([key]) => key[0] === 0 && key[1] === 112);
        assert.equal(pending.length, 1);
        for (const [key, value] of pending) {
            const actual = reopened.get(Uint8Array.from(key));
            if (mode === 'append_read') assert.deepEqual(actual, Uint8Array.from(value));
            else assert(actual.length < value.length, 'The pending reference is now a kvdb tombstone');
        }
        reopened.close();
        if (mode === 'read_only') {
            const resources = target.resources;
            const recovery = await recoveryOwner(profile, acceptedIndex, resources);
            const invoke = (name, value) => hostJson(recovery, name, value);
            const cut = invoke('pb_recovery_cut_get', { volume: fixture.volume, cut: operation.generation });
            const appendOnly = await storedHost(profile, [[1, fixture.snapshots.append_read]]);
            const marker = Buffer.concat([Buffer.from([0x81]), Buffer.from('web-rejected/'), decodeId(cut.generation)]);
            const metadata = appendOnly.stores.get(1).begin(true);
            metadata.put(marker, recovery.stores.get(1).get(marker)); metadata.end(true);
            appendOnly.stores.set(200, await openRecoveryIndex(root, publication, fixture.volume, cut, resources,
                () => hostHash(appendOnly), async () => {}, () => {}));
            const restoreRequest = { volume: fixture.volume, cut: cut.generation, entry: fixture.entry, name: 'Recovered/log', slot: 200 };
            const beforeDeniedRecovery = appendOnly.stores.get(1).handle.bytes.slice();
            const denied = encoder.encode(JSON.stringify(restoreRequest));
            assert.equal(appendOnly.raw('pb_recovery_begin', denied, denied.length), -2,
                'Append without Write cannot restore a log whose records require Write to upload');
            assert.deepEqual(appendOnly.stores.get(1).handle.bytes, beforeDeniedRecovery,
                'Denied recovery publishes no entry or name mapping');
            recovery.stores.set(200, await openRecoveryIndex(root, publication, fixture.volume, cut, resources,
                () => hostHash(recovery), async () => {}, () => {}));
            const prepared = invoke('pb_recovery_begin', { volume: fixture.volume, cut: cut.generation, entry: fixture.entry, name: 'Recovered/log', slot: 200 });
            recovery.stores.set(201, await openRecoveryDatabase(root, publication, fixture.volume, cut,
                `${fixture.entry}.kvdb`, resources, () => hostHash(recovery), async () => {}, () => {}));
            const fresh = new File(); recovery.stores.set(202, OpfsStore.empty(fresh, resources));
            invoke('pb_recovery_backing', { source: 201, destination: 202, replica_id: 900 });
            let complete = false, pages = 0;
            for (; !complete && pages < 10; pages++) complete = invoke('pb_recovery_copy');
            assert(complete && pages >= 2, 'Log validation finishes before paged copying');
            invoke('pb_recovery_publish', { generation: cut.generation, blob: null });
            recovery.stores.set(203, await OpfsStore.open(fresh, resources, async () => {}));
            recovery.register(prepared.entry, fixture.volume, 203, cut.generation);
            assert.notEqual(prepared.entry, fixture.entry, 'Recovery preserves data in a separate log');
            assert.equal(cachedLog(recovery, prepared.entry), 0, 'Copied pending entries need explicit remote acceptance');
            assert.deepEqual((await (await directory.getDirectoryHandle('rejected')).getFileHandle(`${fixture.entry}.kvdb`)).file.bytes, original.bytes);
        }
    }
});

async function archivePrepared(target, profile, backings) {
    const root = new ArchiveDirectory(), resources = target.resources;
    let next = 0xb0;
    const publication = await VolumePublication.open(new File(), resources, () => encodeId(new Uint8Array(16).fill(++next)), async () => {});
    const original = publication.prepare(profile.root.id);
    await publication.create(root, original); publication.publish(original, () => {}, () => {});
    const previous = await root.getDirectoryHandle(publication.directory(original.volume, original.generation));
    previous.entries.set('index.kvdb', archiveFile(target.stores.get(1).handle));
    for (const [entry, file] of backings) previous.entries.set(`${entry}.kvdb`, archiveFile(file));
    const operation = publication.prepare(profile.root.id); await publication.create(root, operation);
    await retainRejectedCut(root, publication, operation, async (name, value) => {
        const input = encoder.encode(JSON.stringify(value)), result = target.raw(name, input, input.length);
        assert(result instanceof Uint8Array, `${name}: ${result}`);
        return result.length ? JSON.parse(decoder.decode(result)) : undefined;
    }, () => hostHash(target), async () => {});
    return { root, publication, operation };
}

test('object recovery preserves its schema and object identity while publishing a fresh database entry', async () => {
    const fixture = JSON.parse(await readFile(new URL('../tests/object-authority-vectors.json', import.meta.url)));
    const profile = bootstrap(fixture.volume, { personal: true });
    profile.local = null; profile.root.owner = encodeId(new Uint8Array(16).fill(0x09)); profile.root.share_key_with_service = true;
    const source = await storedHost(profile, [[1, fixture.snapshots.read_only], [65, fixture.backing]]);
    const target = await storedHost(profile, [[1, fixture.snapshots.read_write], [65, fixture.backing]]);
    source.register(fixture.entry); target.register(fixture.entry);
    const read = (host, entry) => {
        const input = new Uint8Array(72); input.set(host.handle(1, 1, entry), 8);
        input.fill(1, 40, 56); input.fill(3, 56, 72);
        assert(host.raw('pb_web_object_read', input, 8, 40, 56, 1) instanceof Uint8Array);
        const view = new DataView(host.host.exports.memory.buffer), result = host.host.exports.pb_web_result();
        const values = view.getUint32(result + 40, true);
        return view.getBigUint64(values + 16, true) === 1n;
    };
    assert.equal(read(target, fixture.entry), false);
    const edit = new Uint8Array(176), view = new DataView(edit.buffer);
    edit.set(target.handle(1, 1, fixture.entry), 8); edit.fill(1, 40, 56); edit.fill(3, 64, 80);
    view.setUint32(56, 40, true); view.setUint32(80, 96, true); view.setUint32(84, 1, true);
    view.setUint32(88, 80, true); view.setUint32(92, 1, true);
    view.setUint32(96, 80, true); view.setUint32(104, 2, true); view.setBigUint64(112, 1n, true);
    assert(target.raw('pb_web_object_edit', edit, 8, 40, 56, 1, 40) instanceof Uint8Array);
    assert.equal(read(target, fixture.entry), true);
    const original = target.stores.get(65).handle, resources = target.resources;
    await prepare(target, profile);
    await pull(source, target, 1, 100, true); await pull(source, target, 2, 101, true);
    const acceptedBody = await copyBacking(target, 65, 102);
    target.call('pb_volume_register_backing', { entry: fixture.entry, slot: 102, replica_id: 113 });
    await pull(source, target, 65, 102, true);
    assert.equal(target.host.exports.pb_volume_close_backing(), 0);
    await pull(source, target, 1, 100, true);
    const qualification = encoder.encode(replicaJSON(receipt()));
    assert(target.raw('pb_volume_qualify', qualification, qualification.length) instanceof Uint8Array);
    const { root, publication, operation } = await archivePrepared(target, profile, [[fixture.entry, original]]);
    const acceptedIndex = target.stores.get(100).handle;
    publication.publish(operation, () => assert.equal(target.host.exports.pb_volume_validate(), 0),
        () => assert.equal(target.host.exports.pb_volume_publish(), 0));
    target.stores.set(220, await OpfsStore.open(acceptedBody, resources, async () => {}));
    target.register(fixture.entry, fixture.volume, 220, operation.generation);
    assert.equal(read(target, fixture.entry), false, 'Current entry has the accepted field value');
    const recovery = await recoveryOwner(profile, acceptedIndex, resources);
    await exportRecoveryFixture('objects', profile, recovery, root, publication, [[fixture.entry, acceptedBody]]);
    const invoke = (name, value) => hostJson(recovery, name, value);
    const cut = invoke('pb_recovery_cut_get', { volume: fixture.volume, cut: operation.generation });
    recovery.stores.set(200, await openRecoveryIndex(root, publication, fixture.volume, cut, resources,
        () => hostHash(recovery), async () => {}, () => {}));
    const prepared = invoke('pb_recovery_begin', { volume: fixture.volume, cut: cut.generation,
        entry: fixture.entry, name: 'Recovered/objects', slot: 200 });
    recovery.stores.set(201, await openRecoveryDatabase(root, publication, fixture.volume, cut,
        `${fixture.entry}.kvdb`, resources, () => hostHash(recovery), async () => {}, () => {}));
    const fresh = new File(); recovery.stores.set(202, OpfsStore.empty(fresh, resources));
    invoke('pb_recovery_backing', { source: 201, destination: 202, replica_id: 900 });
    let complete = false;
    for (let page = 0; !complete && page < 10; page++) complete = invoke('pb_recovery_copy');
    assert(complete);
    invoke('pb_recovery_publish', { generation: cut.generation, blob: null });
    recovery.stores.set(203, await OpfsStore.open(fresh, resources, async () => {}));
    recovery.register(prepared.entry, fixture.volume, 203, cut.generation);
    assert.notEqual(prepared.entry, fixture.entry);
    assert.equal(read(recovery, prepared.entry), true, 'The same object and Boolean field resolve in the recovered schema');
    const directory = await root.getDirectoryHandle(publication.directory(fixture.volume, cut.generation));
    assert.deepEqual((await (await directory.getDirectoryHandle('rejected')).getFileHandle(`${fixture.entry}.kvdb`)).file.bytes, original.bytes);
});

test('backing-only offline edits are archived, quarantined without Read and never resurrect after Write restoration', async () => {
    const fixture = JSON.parse(await readFile(new URL('../tests/backing-authority-vectors.json', import.meta.url)));
    const profile = bootstrap(fixture.volume, { personal: true });
    profile.local = null; profile.root.owner = encodeId(new Uint8Array(16).fill(0x09)); profile.root.share_key_with_service = true;
    const put = (host, value, key = 'key') => {
        const input = new Uint8Array(256); input.set(host.handle(1, 1, fixture.entry), 8);
        input.set(encoder.encode(key), 40); input.set(encoder.encode(value), 64);
        return host.raw('pb_web_kv_put', input, 8, 40, key.length, 64, value.length);
    };
    const read = (host, key = 'key') => {
        const input = new Uint8Array(112); input.set(host.handle(1, 1, fixture.entry), 8);
        input.set(encoder.encode(key), 40); new DataView(input.buffer).setUint32(48, 64, true); new DataView(input.buffer).setUint32(64, 1024, true);
        const output = host.raw('pb_web_kv_read', input, 8, 40, key.length, 48);
        return output instanceof Uint8Array ? output[0] === 0 ? null : decoder.decode(output.subarray(49)) : output;
    };
    const fromRows = async name => {
        const host = await storedHost(profile, [[1, fixture.snapshots[name]], [65, fixture.backing]]);
        if (name !== 'denied' && name !== 'write_only') host.register(fixture.entry);
        return host;
    };
    const capture = async (target, policy, body, archiveExpected) => {
        const source = await fromRows(policy), oldBacking = target.stores.get(65).handle;
        await prepare(target, profile);
        await pull(source, target, 1, 100, true); await pull(source, target, 2, 101, true);
        let backing = oldBacking;
        if (body) {
            backing = await copyBacking(target, 65, 102);
            target.call('pb_volume_register_backing', { entry: fixture.entry, slot: 102, replica_id: 113 });
            await pull(source, target, 65, 102, true);
            assert.equal(target.host.exports.pb_volume_close_backing(), 0);
            assert(target.host.exports.pb_volume_validate() < 0, 'Replacement needs archival or AP qualification');
        }
        await pull(source, target, 1, 100, true);
        const qualification = encoder.encode(replicaJSON(receipt()));
        assert(target.raw('pb_volume_qualify', qualification, qualification.length) instanceof Uint8Array);
        const { root, publication, operation } = await archivePrepared(target, profile, [[fixture.entry, oldBacking]]);
        const directory = await root.getDirectoryHandle(publication.directory(operation.volume, operation.generation));
        assert.equal(directory.entries.has('rejected'), archiveExpected);
        if (archiveExpected) {
            const archive = await directory.getDirectoryHandle('rejected');
            assert.deepEqual((await archive.getFileHandle(`${fixture.entry}.kvdb`)).file.bytes, oldBacking.bytes);
        }
        const files = [target.stores.get(100).handle, target.stores.get(101).handle,
            backing];
        const premature = encoder.encode(JSON.stringify({ volume: fixture.volume, cut: operation.generation, slot: 100, after: null }));
        assert(target.raw('pb_recovery_entries', premature, premature.length) < 0);
        assert(target.stores.has(100), 'Recovery cannot close a prepared control slot');
        publication.publish(operation, () => assert.equal(target.host.exports.pb_volume_validate(), 0),
            () => assert.equal(target.host.exports.pb_volume_publish(), 0));
        const queued = target.raw('pb_live_peek', empty);
        assert(queued instanceof Uint8Array && queued.length <= 41, 'No rejected backing data remains in the outgoing queue');
        const stores = new Map(), resources = target.resources;
        for (const [offset, file] of files.entries()) {
            const copy = new File(); copy.bytes = file.bytes.slice();
            stores.set(offset < 2 ? offset + 1 : 65, await OpfsStore.open(copy, resources, async () => {}));
        }
        const reopened = await createHost(profile, stores);
        if (policy !== 'denied' && location(reopened, profile.root.id, fixture.entry).ready) reopened.register(fixture.entry);
        if (archiveExpected) {
            const json = (name, value) => {
                const input = encoder.encode(JSON.stringify(value));
                const result = reopened.raw(name, input, input.length);
                assert(result instanceof Uint8Array, `${name}: ${result}`);
                return JSON.parse(decoder.decode(result));
            };
            const cut = json('pb_recovery_cut', { volume: fixture.volume, after: null });
            assert.equal(cut.generation, operation.generation);
            assert.equal(json('pb_recovery_cut', { volume: fixture.volume, after: cut.generation }), null);
            const active = encoder.encode(JSON.stringify({ volume: fixture.volume, cut: cut.generation, slot: 1, after: null }));
            assert(reopened.raw('pb_recovery_entries', active, active.length) < 0);
            assert(reopened.stores.has(1), 'Recovery cannot close an attached control slot');
            const attached = await openRecoveryIndex(root, publication, fixture.volume, cut, resources,
                () => hostHash(reopened), async () => {}, () => {});
            reopened.stores.set(200, attached);
            const page = json('pb_recovery_entries', { volume: fixture.volume, cut: cut.generation, slot: 200, after: null });
            assert.deepEqual(page.entries.map(row => row.entry.id), policy === 'denied' ? [] : [fixture.entry]);
            assert.equal(page.next, null);
            assert(!reopened.stores.has(200), 'The retained index handle closes after one page');
            const archive = await directory.getDirectoryHandle('rejected');
            const file = (await archive.getFileHandle('index.kvdb')).file;
            const original = file.bytes.slice();
            const open = check => openRecoveryIndex(root, publication, fixture.volume, cut, resources,
                () => hostHash(reopened), async () => {}, check ?? (() => {}));
            file.bytes[0] ^= 1;
            await assert.rejects(open(), /digest differs/);
            file.bytes = original.slice(1);
            await assert.rejects(open(), /length differs/);
            file.bytes = original;
            let checks = 0;
            await assert.rejects(open(() => { if (++checks === 3) throw new Error('Permission changed'); }), /Permission changed/);
            file.bytes = Uint8Array.from([...original, 0]);
            const changed = structuredClone(cut), digest = hostHash(reopened);
            digest.update(file.bytes);
            changed.files['index.kvdb'] = { bytes: file.bytes.length, digest: Array.from(Buffer.from(digest.finish(), 'hex')) };
            digest.close();
            await assert.rejects(openRecoveryIndex(root, publication, fixture.volume, changed, resources,
                () => hostHash(reopened), async () => {}, () => {}), /requires repair/);
            assert.equal(file.bytes.length, original.length + 1, 'Discovery never repairs an archived file in place');
            file.bytes = original;
            const attach = async (host, slot, name = 'index.kvdb') => host.stores.set(slot,
                await openRecoveryDatabase(root, publication, fixture.volume, cut, name, resources,
                    () => hostHash(host), async () => {}, () => {}));
            await attach(reopened, 200);
            const request = { volume: fixture.volume, cut: cut.generation, entry: fixture.entry, name: 'Recovered/settings', slot: 200 };
            const input = encoder.encode(JSON.stringify(request));
            assert(reopened.raw('pb_recovery_begin', input, input.length) < 0, 'Entry Read does not grant volume creation');
            // A distinct owner session has current Read and creation permission.
            const recovery = await recoveryOwner(profile, reopened.stores.get(1).handle, resources);
            const invoke = (name, value) => hostJson(recovery, name, value);
            await attach(recovery, 200);
            const prepared = invoke('pb_recovery_begin', request);
            assert.notEqual(prepared.entry, fixture.entry); assert.equal(prepared.blob, null);
            const hidden = encoder.encode(JSON.stringify({ volume: fixture.volume, database: prepared.entry }));
            assert(recovery.raw('pb_database_backing', hidden, hidden.length) < 0, 'Preparation has no visible entry');
            const early = encoder.encode(JSON.stringify({ generation: cut.generation, blob: null }));
            assert(recovery.raw('pb_recovery_publish', early, early.length) < 0, 'Uncopied backing cannot become visible');
            await attach(recovery, 201, `${fixture.entry}.kvdb`);
            const recoveredFile = new File(); recovery.stores.set(202, OpfsStore.empty(recoveredFile, resources));
            invoke('pb_recovery_backing', { source: 201, destination: 202, replica_id: 900 });
            const write = recoveredFile.write.bind(recoveredFile);
            recoveredFile.write = () => { recoveredFile.write = write; throw new Error('Injected recovery copy failure'); };
            assert(recovery.raw('pb_recovery_copy', empty) < 0);
            assert(recovery.raw('pb_recovery_publish', early, early.length) < 0, 'Failed copy cannot be marked complete');
            let finished = false;
            for (let page = 0; !finished && page < 10; page++) finished = invoke('pb_recovery_copy');
            assert.equal(finished, true);
            assert.equal(invoke('pb_recovery_publish', { generation: cut.generation, blob: null }), prepared.entry);
            assert.deepEqual(location(recovery, fixture.volume, prepared.entry), { generation: cut.generation, ready: true });
            recovery.stores.set(203, await OpfsStore.open(recoveredFile, resources, async () => {}));
            recovery.register(prepared.entry, fixture.volume, 203, cut.generation);
            const read = new Uint8Array(112); read.set(recovery.handle(1, 1, prepared.entry), 8);
            read.set(encoder.encode('key'), 40); new DataView(read.buffer).setUint32(48, 64, true); new DataView(read.buffer).setUint32(64, 1024, true);
            const recovered = recovery.raw('pb_web_kv_read', read, 8, 40, 3, 48);
            assert(recovered instanceof Uint8Array);
            assert.equal(decoder.decode(recovered.subarray(49)), 'offline');
            assert.deepEqual(file.bytes, original, 'Recovery preserves the sealed original');
        }
        return reopened;
    };
    for (const initialPolicy of ['read_only', 'denied']) {
        let target = await fromRows('read_write');
        assert(put(target, 'offline') instanceof Uint8Array);
        assert(put(target, 'only local', 'extra') instanceof Uint8Array);
        assert.equal(read(target), 'offline');
        target = await capture(target, initialPolicy, initialPolicy === 'read_only', true);
        if (initialPolicy === 'read_only') {
            assert.equal(read(target), 'accepted'); assert.equal(read(target, 'extra'), -1);
        } else {
            const marker = Uint8Array.from([129, ...encoder.encode('unaccepted-backing/'), ...Buffer.from(decodeId(fixture.entry, 15))]);
            assert.deepEqual(target.stores.get(1).get(marker), Uint8Array.of(1));
            assert(read(target) < 0);
            target = await capture(target, 'write_only', false, false);
            assert.equal(location(target, profile.root.id, fixture.entry).ready, false);
            assert(put(target, 'must not write stale backing') < 0);
            target = await capture(target, 'read_restored', true, false);
            assert.equal(read(target), 'accepted'); assert.equal(read(target, 'extra'), -1);
            assert(put(target, 'new authorized value') instanceof Uint8Array);
            assert.equal(read(target), 'new authorized value');
        }
    }
});
const receipt = () => ({ namespace: { status: "unavailable" }, root: null, revision: 18446744073709551615n, policy_revision: 7n,
    source: { generation: encodeId(new Uint8Array(16).fill(0x55)), epoch: 9007199254740995n } });
function admit(target, slot, value = receipt()) {
    const bytes = encoder.encode(replicaJSON({ slot, receipt: value }));
    return target.raw('pb_volume_receipt', bytes, bytes.length);
}

async function pull(source, target, sourceSlot, targetSlot = sourceSlot, prepared = false) {
    let request = target.raw(prepared ? 'pb_volume_sync_begin' : 'pb_sync_begin', empty, targetSlot, 0);
    assert(request instanceof Uint8Array, `begin pull returned ${request}`);
    assert(source.raw('pb_sync_begin', empty, sourceSlot, 1) instanceof Uint8Array);
    try {
        for (let turns = 0; !target.host.exports.pb_sync_done(); turns++) {
            assert(turns < 1000);
            assert(source.raw('pb_sync_step', request, request.length) instanceof Uint8Array); request = empty;
            for (;;) {
                const fragment = source.raw('pb_sync_output', empty); assert(fragment instanceof Uint8Array);
                if (!fragment.length) break;
                const reply = target.raw('pb_sync_step', fragment, fragment.length); assert(reply instanceof Uint8Array);
                if (reply.length) { assert.equal(request.length, 0); request = reply; }
            }
        }
        assert(source.raw('pb_sync_step', request, request.length) instanceof Uint8Array);
        assert.equal(source.host.exports.pb_sync_finish(1), 0);
        assert.equal(target.host.exports.pb_sync_finish(1), 0);
        if (prepared) assert(admit(target, targetSlot) instanceof Uint8Array);
    } finally { source.host.exports.pb_sync_finish(0); target.host.exports.pb_sync_finish(0); }
}
async function pair() {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xf1)));
    profile.root.share_key_with_service = true;
    const remote = structuredClone(profile); remote.local = null; remote.replica_ids = [31, 32];
    const source = await createHost(profile), target = await createHost(remote);
    for (const slot of [1, 2]) await pull(source, target, slot);
    target.register(entryId); await pull(source, target, 65);
    assert.equal(target.read(), false);
    return { source, target, remote };
}
async function copyBacking(target, from, to) {
    const file = new File(); file.bytes = target.stores.get(from).handle.bytes.slice();
    target.stores.set(to, await OpfsStore.open(file, target.stores.get(from).resources, async () => {}));
    return file;
}
function catalog(target, copy_local, after = null, expected_root = null) {
    const input = encoder.encode(JSON.stringify({ after, expected_root, copy_local }));
    const result = target.raw('pb_volume_catalog_page', input, input.length);
    assert(result instanceof Uint8Array);
    return JSON.parse(decoder.decode(result));
}
async function prepare(target, remote, entry_scope, volume = remote.root.id) {
    const input = encoder.encode(JSON.stringify({ volume, entry_scope }));
    const admission = JSON.parse(decoder.decode(target.raw('pb_volume_admission', input, input.length)));
    const base = volume === remote.root.id ? 1 : 3;
    const controls = [await copyBacking(target, base, 100), await copyBacking(target, base + 1, 101)];
    target.stores.set(1100, OpfsStore.empty(new File(), target.stores.get(base).resources));
    target.call('pb_volume_prepare', { volume, base: 100, accepted_slot: 1100, accepted_replica_id: 119, replica_ids: [111, 112], generation: encodeId(new Uint8Array(16).fill(0xb2)), entry_scope, ...admission });
    return controls;
}

for (const namespaceReceipt of [true, false]) test(`qualified AP capture (${namespaceReceipt ? 'namespace evidence' : 'authority only'}) seals an offline blob cut and restores the accepted head across reopen`, async () => {
    const fixture = JSON.parse(await readFile(new URL('../tests/blob-authority-vectors.json', import.meta.url)));
    const profile = bootstrap(fixture.reference.volume, { personal: true });
    profile.local = { ...profile.local, databases: [], blobs: [] }; profile.root.owner = encodeId(new Uint8Array(16).fill(0x09)); profile.root.share_key_with_service = true;
    const target = await storedHost(profile, [[1, fixture.snapshots.file_write]]);
    const source = await storedHost({ ...profile, account: profile.root.owner }, [[1, fixture.snapshots.read_only]]);
    const content = encoder.encode('offline supplied content');
    const prepared = prepareWriter(target, 'supplied.txt', content), reference = prepared.reference;
    const supplied = new File(); supplied.bytes = content.slice();
    reference.encrypted_key = await encryptBlob(reference, blobKey(target, reference), supplied, new File(), () => hostHash(target));
    prepared.publish();
    await prepare(target, profile, fixture.reference.entry);
    await pull(source, target, 1, 100, true);
    const bytes = encoder.encode('null');
    const page = target.raw('pb_volume_rejected_page', bytes, bytes.length);
    assert(page instanceof Uint8Array);
    assert.deepEqual(JSON.parse(decoder.decode(page)), [[fixture.reference.entry], null]);
    const entries = encoder.encode(JSON.stringify([fixture.reference.entry]));
    assert.equal(target.raw('pb_volume_archive_plan', entries, entries.length), -2,
        'An unqualified source cannot initiate archival or canonical replacement');
    await pull(source, target, 2, 101, true);
    await pull(source, target, 1, 100, true);
    if (namespaceReceipt) {
        const qualified = { ...receipt(), root: fixture.receipts.read_only.root,
            namespace: { ...fixture.receipts.read_only.namespace, entry: fixture.reference.entry } };
        const qualification = encoder.encode(replicaJSON(qualified));
        assert.equal(target.raw('pb_volume_qualify', qualification, qualification.length), -2,
            'A receipt header alone cannot qualify namespace facts');
        const evidence = Uint8Array.from(fixture.receipts.read_only.evidence);
        const retain = value => {
            const metadata = encoder.encode(replicaJSON({ slot: 100, prepared: true,
                entry: fixture.reference.entry, receipt: value }));
            const bytes = Uint8Array.from([...metadata, ...evidence]);
            return target.raw('pb_namespace_evidence', bytes, bytes.length, metadata.length);
        };
        assert(retain({ ...qualified, root: null }) < 0, 'Evidence must bind the exact received root');
        assert(retain(qualified) instanceof Uint8Array,
            'The received cut qualifies independently of the newer offline working head');
        const downgraded = encoder.encode(replicaJSON(receipt()));
        assert.equal(target.raw('pb_volume_qualify', downgraded, downgraded.length), -2,
            'Qualification cannot discard already received namespace evidence');
        assert(target.raw('pb_volume_qualify', qualification, qualification.length) instanceof Uint8Array);
        assert(target.raw('pb_volume_qualify', qualification, qualification.length) instanceof Uint8Array,
            'Exact receipt replay is idempotent');
        await pull(source, target, 1, 100, true);
        assert.equal(target.raw('pb_volume_qualify', qualification, qualification.length), -2,
            'A new transfer invalidates evidence even when its replicated root is unchanged');
        assert(retain(qualified) instanceof Uint8Array);
        assert(target.raw('pb_volume_qualify', qualification, qualification.length) instanceof Uint8Array);
    } else {
        const qualification = encoder.encode(replicaJSON(receipt()));
        assert(target.raw('pb_volume_qualify', qualification, qualification.length) instanceof Uint8Array);
    }
    const observationPrefix = Buffer.from([129, ...encoder.encode('namespace-observation/')]).toString('hex');
    const observations = target.stores.get(100).keys.filter(key => key.startsWith(observationPrefix))
        .map(key => JSON.parse(decoder.decode(target.stores.get(100).get(Buffer.from(key, 'hex')))));
    assert.equal(observations.find(row => row.source.revision === reference.revision)?.status, 'Excluded',
        'Canonical rejection excludes the exact local authored content source');
    assert.equal(observations.some(row => row.status === 'Confirmed'), namespaceReceipt,
        'An authority-only cut never confirms a namespace source');
    const root = new ArchiveDirectory(), resources = target.resources;
    let next = 0xb0;
    const publication = await VolumePublication.open(new File(), resources, () => encodeId(new Uint8Array(16).fill(++next)), async () => {});
    const original = publication.prepare(profile.root.id);
    await publication.create(root, original); publication.publish(original, () => {}, () => {});
    const previous = await root.getDirectoryHandle(publication.directory(original.volume, original.generation));
    previous.entries.set('index.kvdb', archiveFile(target.stores.get(1).handle));
    const cached = new File(); cached.bytes = content.slice();
    root.entries.set(`blob-${reference.digest}`, archiveFile(cached));
    const operation = publication.prepare(profile.root.id); await publication.create(root, operation);
    const call = async (name, value) => {
        if (name === 'pb_volume_archive_commit') {
            for (const alter of [
                cut => { delete cut.files['index.kvdb']; },
                cut => { cut.bytes++; },
                cut => { cut.generation = encodeId(new Uint8Array(16).fill(0xb9)); },
                cut => { cut.files['../index.kvdb'] = cut.files['index.kvdb']; },
            ]) {
                const invalid = structuredClone(value); alter(invalid);
                const input = encoder.encode(JSON.stringify(invalid));
                assert(target.raw(name, input, input.length) < 0, 'Malformed seal receipts cannot publish');
                assert.equal(target.host.exports.pb_volume_validate(), -5);
            }
        }
        const input = encoder.encode(JSON.stringify(value)), output = target.raw(name, input, input.length);
        assert(output instanceof Uint8Array, `${name} returned ${output}`);
        if (name === 'pb_volume_archive_plan') assert.equal(target.host.exports.pb_volume_validate(), -5,
            'A started but unsealed archive cannot publish');
        if (name === 'pb_volume_archive_commit') assert(target.raw(name, input, input.length) < 0,
            'A sealed cut cannot be overwritten or charged twice');
        return output.length ? JSON.parse(decoder.decode(output)) : undefined;
    };
    await retainRejectedCut(root, publication, operation, call, () => hostHash(target), async () => {});
    const generation = await root.getDirectoryHandle(publication.directory(operation.volume, operation.generation));
    const archive = await generation.getDirectoryHandle('rejected');
    assert.deepEqual((await archive.getFileHandle(`${reference.digest}.blob`)).file.bytes, content);
    assert.deepEqual((await archive.getFileHandle('index.kvdb')).file.bytes, target.stores.get(1).handle.bytes);
    const controls = [target.stores.get(100).handle, target.stores.get(101).handle];
    publication.publish(operation, () => assert.equal(target.host.exports.pb_volume_validate(), 0),
        () => assert.equal(target.host.exports.pb_volume_publish(), 0));
    const acceptedOwner = await recoveryOwner(profile, controls[0], resources);
    const accepted = selectedReference(acceptedOwner, 'supplied.txt');
    const api = files(target), [rootFile] = api.roots();
    const pathRead = api.begin(rootFile, 23, { flags: 1, data: encoder.encode('supplied.txt') });
    assert.equal(api.execute(pathRead), -2, 'An entry-scoped catch-up cannot grant directory traversal');
    assert.equal(api.host.pb_web_file_release(pathRead), 0);
    assert.equal(accepted.digest, fixture.reference.digest);
    assert.equal(target.raw('pb_live_peek', empty).length, 41, 'Rejected queued facts become an empty acknowledgment, never a later replay');
    assert.equal(target.stores.has(1100), false, 'Publication closes the private AP index');
    const following = publication.prepare(profile.root.id); await publication.create(root, following);
    publication.publish(following, () => {}, () => {});
    await publication.reclaim(root, profile.root.id, () => false);
    assert.deepEqual((await archive.getFileHandle(`${reference.digest}.blob`)).file.bytes, content,
        'Obsolete generation cleanup preserves retained offline cuts');
    const stores = new Map();
    for (const [offset, file] of [...controls].entries()) {
        const copy = new File(); copy.bytes = file.bytes.slice();
        stores.set(offset + 1, await OpfsStore.open(copy, resources, async () => {}));
    }
    const reopened = await createHost(profile, stores);
    const persistedOwner = await recoveryOwner(profile, reopened.stores.get(1).handle, resources);
    assert.equal(selectedReference(persistedOwner, 'supplied.txt').digest, fixture.reference.digest,
        'The qualified accepted body survives reopening with the retained offline cut');
    const cutKey = Uint8Array.from([129, ...encoder.encode('web-rejected/'), ...decodeId(operation.generation)]);
    const retained = JSON.parse(decoder.decode(reopened.stores.get(1).get(cutKey)));
    assert.deepEqual(retained.entries, [fixture.reference.entry]);
    assert.equal(retained.files[`${reference.digest}.blob`].bytes, content.length);
    const catalog = encoder.encode(JSON.stringify({ volume: profile.root.id, after: null }));
    assert(reopened.raw('pb_recovery_cut', catalog, catalog.length) < 0,
        'Known-entry access does not permit enumerating retained cuts');
    await prepare(reopened, profile, fixture.reference.entry);
    await pull(source, reopened, 1, 100, true);
    assert.deepEqual(JSON.parse(decoder.decode(reopened.raw('pb_volume_rejected_page', bytes, bytes.length))), [[], null],
        'Canonical publication does not repeatedly archive the same rejected head');
    assert.equal(reopened.host.exports.pb_volume_abort(), 0);
    const recovery = await recoveryOwner(profile, reopened.stores.get(1).handle, resources);
    await exportRecoveryFixture('blob', profile, recovery, root, publication);
    const invoke = (name, value) => hostJson(recovery, name, value);
    const cut = invoke('pb_recovery_cut_get', { volume: profile.root.id, cut: operation.generation });
    recovery.stores.set(200, await openRecoveryIndex(root, publication, profile.root.id, cut, resources,
        () => hostHash(recovery), async () => {}, () => {}));
    const restored = invoke('pb_recovery_begin', { volume: profile.root.id, cut: cut.generation,
        entry: fixture.reference.entry, name: 'Recovered/supplied.txt', slot: 200 });
    assert.notEqual(restored.entry, fixture.reference.entry);
    assert.equal(restored.blob.digest, reference.digest);
    assert.equal(restored.blob.encrypted_key, reference.encrypted_key);
    const blobs = new RuntimeBlobs({ directory: () => root, hash: () => hostHash(recovery), cache: cacheService(recovery), failure() {},
        key: reference => blobKey(recovery, reference), yield: async () => {}, stop() {} });
    const retainedContent = await openRecoveryFile(root, publication, profile.root.id, cut, `${reference.digest}.blob`,
        () => hostHash(recovery), async () => {}, () => {});
    try { await blobs.restore(retainedContent, restored.blob, () => {}, write => write()); } finally { retainedContent.close(); blobs.close(); }
    const wrong = encoder.encode(JSON.stringify({ generation: following.generation, blob: { ...restored.blob, digest: '00'.repeat(32) } }));
    assert(recovery.raw('pb_recovery_publish', wrong, wrong.length) < 0, 'An unrelated content receipt cannot publish the recovered blob');
    assert.equal(invoke('pb_recovery_publish', { generation: following.generation, blob: restored.blob }), restored.entry);
    const recovered = selectedReference(recovery, 'Recovered/supplied.txt');
    assert.equal(recovered.digest, reference.digest); assert.equal(recovered.encrypted_key, reference.encrypted_key);
    assert.deepEqual((await archive.getFileHandle(`${reference.digest}.blob`)).file.bytes, content);
    const pending = invoke('pb_blob_pending', { volume: profile.root.id, after: null });
    assert(pending[0].some(item => item.entry === restored.entry && item.digest === reference.digest), 'Recovery retains durable initial-upload work');
});

function location(runtime, volume, database) {
    const input = encoder.encode(JSON.stringify({ volume, database }));
    const output = runtime.raw('pb_database_backing', input, input.length);
    assert(output instanceof Uint8Array);
    return JSON.parse(decoder.decode(output));
}

test('AP capture catalog excludes an unpublished local birth while the local copy catalog retains it', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xf7)));
    profile.root.share_key_with_service = true;
    const source = await createHost(profile);
    const local = structuredClone(profile), unpublished = encodeId(new Uint8Array(15).fill(0xe7));
    local.local.databases.push({ id: unpublished, name: 'offline settings', kind: { type: 'kv_db' } });
    const stores = new Map();
    for (const [slot, store] of source.stores) {
        const file = new File(); file.bytes = store.handle.bytes.slice();
        stores.set(slot, await OpfsStore.open(file, store.resources, async () => {}));
    }
    const target = await createHost(local, stores);
    await prepare(target, local);
    assert.deepEqual(catalog(target, true).backings.map(([id]) => id).sort(), [entryId, unpublished].sort());
    const request = encoder.encode(JSON.stringify({ after: null, expected_root: null, copy_local: false }));
    assert.equal(target.raw('pb_volume_catalog_page', request, request.length), -5,
        'An unreceived comparison copy is not an AP catalog');
    await pull(source, target, 1, 100, true);
    assert.deepEqual(catalog(target, false).backings, [[entryId, 'kvdb']]);
    assert.deepEqual(catalog(target, true).backings.map(([id]) => id).sort(), [entryId, unpublished].sort());
    assert.equal(target.host.exports.pb_volume_abort(), 0);
    assert.equal(location(target, local.root.id, unpublished).ready, true);
    assert.equal(target.read(), false, 'Aborting capture leaves the active backing unchanged');
});

async function captureEntry(source, target, remote, beforeQualify = () => {}) {
    const controls = await prepare(target, remote, entryId);
    await pull(source, target, 1, 100, true); await pull(source, target, 2, 101, true);
    const backing = await copyBacking(target, 65, 102);
    target.call('pb_volume_register_backing', { entry: entryId, slot: 102, replica_id: 113 });
    await pull(source, target, 65, 102, true);
    assert.equal(target.host.exports.pb_volume_close_backing(), 0);
    await pull(source, target, 1, 100, true);
    const input = encoder.encode(replicaJSON(receipt()));
    assert(target.raw('pb_volume_qualify', input, input.length) instanceof Uint8Array);
    beforeQualify();
    assert.equal(target.host.exports.pb_volume_validate(), 0);
    assert.equal(target.host.exports.pb_volume_publish(), 0);
    target.stores.set(120, await OpfsStore.open(backing, target.resources, async () => {}));
    return { controls, backing };
}

test('compiled selected capture clears only its entry, denies catalog and preserves that ceiling after reopening', async () => {
    const { source, target, remote } = await pair();
    const terminal = Uint8Array.from([...Buffer.from(decodeId(remote.root.id)), ...Buffer.from(decodeId(remote.root.id)), 1]);
    assert.equal(target.raw('pb_live_terminal', terminal, terminal.length).length, 0);
    assert(target.read() < 0);
    const plan = encoder.encode(JSON.stringify(remote.root.id));
    assert.equal(JSON.parse(decoder.decode(target.raw('pb_replica_catalog_allowed', plan, plan.length))), true);
    assert(target.read() < 0, 'Planning a fresh AP capture cannot clear negative evidence');
    source.edit(true);
    const { controls, backing } = await captureEntry(source, target, remote);
    target.register(entryId, remote.root.id, 120); assert.equal(target.read(), true);
    const memory = new WebAssembly.Memory({ initial: 4 }), events = [], work = createWork(events);
    work.beginTurn();
    const operations = guestOperations({ memory: () => memory, host: () => target.host.exports, events, work, log() {}, submit() {} });
    operations.refreshVolumes();
    new Uint8Array(memory.buffer, 8, 32).set(target.handle());
    new Uint8Array(memory.buffer, 40, 16).fill(1);
    new Uint8Array(memory.buffer, 56, 16).fill(3);
    new Uint8Array(memory.buffer, 80, 4).set(encoder.encode('data'));
    assert.equal(operations.pb_entry_resolve_begin(1n, 1, 80, 4), -2, 'Mount visibility does not grant catalog Read');
    const handle = operations.pb_object_read_begin(8, 40, 56, 1); assert(handle > 0n, 'The same mount permits selected content');
    await operations.data.wait();
    const descriptor = new DataView(memory.buffer); descriptor.setUint32(256, 68, true); descriptor.setUint32(292, 40, true); descriptor.setUint32(308, 80, true);
    assert.equal(operations.pb_object_read_take(handle >> 32n, Number(handle & 0xffffffffn), 256), -11, 'Selected content reaches its bounded result probe');
    operations.data.dispose();
    const name = new Uint8Array(12); name.set(encoder.encode('data'), 8);
    assert(target.raw('pb_web_entry_resolve', name, 1n, 1, 8, 4) < 0, 'Known-ID capture grants no catalog');
    const files = [...controls, backing];
    const stores = new Map(), resources = target.resources;
    for (const [index, original] of files.entries()) {
        const copy = new File(); copy.bytes = original.bytes.slice();
        stores.set(index < 2 ? index + 1 : 65, await OpfsStore.open(copy, resources, async () => {}));
    }
    const reopened = await createHost(remote, stores);
    assert.equal(reopened.read(), true);
    assert(reopened.raw('pb_web_entry_resolve', name, 1n, 1, 8, 4) < 0);
});


test('a newer selected terminal survives the older qualified capture', async () => {
    const { source, target, remote } = await pair();
    const terminal = Uint8Array.from([...Buffer.from(decodeId(remote.root.id)), ...Buffer.from(decodeId(entryId, 15)), ...Buffer.from(decodeId(remote.root.id)), 4]);
    await captureEntry(source, target, remote, () => {
        assert.equal(target.raw('pb_live_terminal', terminal, terminal.length).length, 0);
    });
    assert.throws(() => target.register(entryId, remote.root.id, 120));
    assert(target.read() < 0, 'Publication retains newer negative evidence');
});

test('detached unknown-entry receipt cannot publish a completed source that lacks its selected birth', async () => {
    const { target, remote } = await pair();
    const profile = bootstrap(remote.root.id);
    profile.root.share_key_with_service = true; profile.local.databases = [];
    const source = await createHost(profile);
    await prepare(target, remote, entryId, remote.root.id);
    await pull(source, target, 1, 100, true);
    await assert.rejects(pull(source, target, 2, 101, true), /AssertionError/);
    assert(target.host.exports.pb_volume_validate() < 0, 'A control receipt cannot qualify an absent entry');
    assert(target.host.exports.pb_volume_publish() < 0);
    assert.equal(target.host.exports.pb_volume_abort(), 0);
    assert.equal(target.read(), false, 'Rejected detached facts do not replace the active generation');
});

test('compiled selected live receipt applies duplicate and reordered content without catalog authority', async () => {
    const { source, target, remote } = await pair();
    source.edit(true);
    await captureEntry(source, target, remote);
    target.register(entryId, remote.root.id, 120);
    const pending = () => {
        const bytes = source.raw('pb_live_peek', empty), data = new DataView(bytes.buffer);
        const selected = new Uint8Array(47 + bytes.length - 41);
        selected.set(bytes.subarray(0, 16)); selected.set(Buffer.from(decodeId(entryId, 15)), 16);
        selected.set(bytes.subarray(16, 32), 31); selected.set(bytes.subarray(41), 47);
        return { bytes: selected, ticket: data.getBigUint64(32, true) };
    };
    const older = pending(); source.host.exports.pb_live_ack(older.ticket);
    source.edit(false); const newer = pending();
    assert.equal(target.raw('pb_live_apply_entry', newer.bytes, newer.bytes.length).length, 0);
    assert.equal(target.read(), false);
    for (const frame of [newer.bytes, older.bytes]) assert.equal(target.raw('pb_live_apply_entry', frame, frame.length).length, 0);
    assert.equal(target.read(), false);
    assert.equal(target.raw('pb_live_peek', empty).length, 0, 'Received facts do not echo');
    const unrelated = newer.bytes.slice(); unrelated.fill(9, 16, 31);
    assert(target.raw('pb_live_apply_entry', unrelated, unrelated.length) < 0);
    assert.equal(target.read(), false);
    const terminal = Uint8Array.from([...Buffer.from(decodeId(remote.root.id)), ...Buffer.from(decodeId(entryId, 15)), ...Buffer.from(decodeId(remote.root.id)), 4]);
    target.raw('pb_live_terminal', terminal, terminal.length);
    assert.equal(target.raw('pb_live_apply_entry', older.bytes, older.bytes.length), -2, 'Negative evidence fences stale frames');
});

test('unknown entry admission queues bounded scoped preparation while denied participants queue nothing', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xf5))), target = await createHost(profile);
    const unknown = encodeId(new Uint8Array(15).fill(0xe5)), input = new Uint8Array(40);
    input.set(target.handle(1, 1, unknown), 8);
    assert.equal(target.raw('pb_web_entry_authorize', input, 8), -6);
    assert.equal(target.raw('pb_web_entry_authorize', input, 8), -6);
    assert.deepEqual(JSON.parse(decoder.decode(target.raw('pb_database_needed', empty))), [{ volume: profile.root.id, database: unknown }]);
    target.call('pb_database_failed', { address: { volume: profile.root.id, database: unknown }, status: -2 });
    assert.equal(target.raw('pb_web_entry_authorize', input, 8), -2);
    assert.deepEqual(JSON.parse(decoder.decode(target.raw('pb_database_needed', empty))), []);
    const denied = await createHost({ ...profile, account: encodeId(new Uint8Array(16).fill(0x09)), local: null }, target.stores);
    denied.volumes();
    assert.equal(denied.raw('pb_web_entry_authorize', input, 8), -8);
    assert.deepEqual(JSON.parse(decoder.decode(denied.raw('pb_database_needed', empty))), []);
});

test('a selected prepared receiver rejects unrelated entry facts even when they would lose conflict resolution', async () => {
    const { source, target, remote } = await pair();
    const other = structuredClone(bootstrap(remote.root.id));
    other.root.share_key_with_service = true; other.root.share_key_with_service = true;
    other.local.databases.push({ id: encodeId(new Uint8Array(15).fill(0xe4)), name: 'private', kind: { type: 'kv_db' } });
    const malicious = await createHost(other);
    await prepare(target, remote, entryId);
    await assert.rejects(pull(malicious, target, 1, 100, true));
    assert(target.host.exports.pb_volume_validate() < 0);
    assert.equal(target.host.exports.pb_volume_abort(), 0);
    assert.equal(target.read(), false);
    assert.equal(source.read(), false);
});

test('persistent backing locations publish one database while retaining a sibling generation across reopen', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xf3))), sibling = encodeId(new Uint8Array(15).fill(0xe3));
    profile.root.share_key_with_service = true;
    profile.local.databases.push({ id: sibling, name: 'settings', kind: { type: 'kv_db' } });
    const source = await createHost(profile), remote = { ...structuredClone(profile), local: null, replica_ids: [31, 32] };
    const target = await createHost(remote);
    for (const slot of [1, 2]) await pull(source, target, slot);
    target.register(entryId); target.register(sibling, remote.root.id, 66);
    await pull(source, target, 65); await pull(source, target, 66);
    const siblingFile = target.stores.get(66).handle;
    const before = location(target, remote.root.id, entryId);
    assert.deepEqual(before, { ready: true, generation: encodeId(new Uint8Array(16).fill(0xb1)) });
    const controls = await prepare(target, remote), backing = await copyBacking(target, 65, 102);
    target.call('pb_volume_register_backing', { entry: entryId, slot: 102, replica_id: 113 });
    target.call('pb_volume_local_backing', entryId); target.host.exports.pb_volume_close_backing();
    assert.deepEqual(location(target, remote.root.id, entryId), before, 'Detached routing is invisible');
    await pull(source, target, 1, 100, true); await pull(source, target, 2, 101, true);
    assert.equal(target.host.exports.pb_volume_validate(), 0);
    assert.equal(target.host.exports.pb_volume_publish(), 0);
    assert.deepEqual(location(target, remote.root.id, entryId), { ready: true, generation: encodeId(new Uint8Array(16).fill(0xb2)) });
    assert.deepEqual(location(target, remote.root.id, sibling), before, 'No sibling copy or route rewrite');
    assert.throws(() => target.register(sibling, remote.root.id, 120, encodeId(new Uint8Array(16).fill(0xb2))), /Host error -10/);
    const files = [...controls, backing, siblingFile];
    const stores = new Map(), resources = target.resources;
    for (const [index, original] of files.entries()) {
        const copy = new File(); copy.bytes = original.bytes.slice();
        stores.set(index < 2 ? index + 1 : index + 63, await OpfsStore.open(copy, resources, async () => {}));
    }
    const reopened = await createHost(remote, stores);
    assert.deepEqual(location(reopened, remote.root.id, entryId), { ready: true, generation: encodeId(new Uint8Array(16).fill(0xb2)) });
    assert.deepEqual(location(reopened, remote.root.id, sibling), before);
    const directory = new Directory(), generations = [encodeId(new Uint8Array(16).fill(0xb1)), encodeId(new Uint8Array(16).fill(0xb2))];
    const publication = await VolumePublication.open(new File(), resources, () => generations.shift(), async () => {});
    for (let index = 0; index < 2; index++) {
        const prepared = publication.prepare(remote.root.id); await publication.create(directory, prepared);
        for (const database of ['index', 'revocations', entryId, ...(index === 0 ? [sibling] : [])]) {
            await publication.file(directory, remote.root.id, prepared.generation, database, true);
        }
        publication.publish(prepared, () => {}, () => {});
    }
    await publication.reclaim(directory, remote.root.id, (database, generation) => {
        const input = encoder.encode(JSON.stringify({ volume: remote.root.id, database }));
        const retained = reopened.raw('pb_database_retained_generation', input, input.length);
        assert(retained instanceof Uint8Array);
        return JSON.parse(decoder.decode(retained)) === generation;
    });
    const old = await directory.getDirectoryHandle(publication.directory(remote.root.id, encodeId(new Uint8Array(16).fill(0xb1))));
    assert.deepEqual([...old.entries.keys()], [`${sibling}.kvdb`]); publication.close();
    reopened.register(sibling, remote.root.id, 66);
    assert.equal(reopened.read(), false);
});

test('compiled prepared receipt stays invisible until publication, then stable handles read it and reopen retains it', async () => {
    // Exercise the production Wasm merge and OPFS pointer rather than a model of guest visibility.
    const { source, target, remote } = await pair(), ledger = target.resources;
    const rootMetadata = () => {
        const host = target.host.exports;
        assert.equal(host.pb_web_volumes(), 0);
        const data = new DataView(host.memory.buffer), result = host.pb_web_result();
        const records = data.getUint32(result + 16, true);
        return { revision: data.getBigUint64(result + 8, true),
            grant: Buffer.from(host.memory.buffer, records + 64, 16).toString('hex') };
    };
    const root = rootMetadata();
    assert.equal(target.host.exports.pb_web_volumes_acquire(root.revision), 0);
    const rootFiles = new Uint32Array(target.host.exports.memory.buffer, target.host.exports.pb_output(), 2).slice();
    const pointers = new File(); let next = 10;
    const publication = await VolumePublication.open(pointers, ledger, () => encodeId(Buffer.from((++next).toString(16).padStart(32, '0'), 'hex')), async () => {}), directory = new Directory();
    const epoch = publication.epoch(remote.root.id);
    for (const slot of [1, 2, 65]) target.stores.get(slot).attachFence(epoch);
    const initial = publication.prepare(remote.root.id); await publication.create(directory, initial); publication.publish(initial, () => {}, () => {});
    const operation = publication.prepare(remote.root.id), originalHandle = target.handle(); await publication.create(directory, operation);
    const controls = await prepare(target, remote);
    assert.deepEqual(catalog(target, true).backings, [[entryId, 'kvdb']]);
    source.edit(true);
    await pull(source, target, 1, 100, true); await pull(source, target, 2, 101, true);
    assert.deepEqual(catalog(target, false).backings, [[entryId, 'kvdb']]);
    const backing = await copyBacking(target, 65, 102);
    target.call('pb_volume_register_backing', { entry: entryId, slot: 102, replica_id: 113 });
    await pull(source, target, 65, 102, true);
    assert.equal(target.read(), false, 'Even the completed staged database stays invisible');
    assert.equal(target.host.exports.pb_volume_close_backing(), 0);
    assert(target.host.exports.pb_volume_publish() < 0, 'Unvalidated files cannot be installed');
    publication.publish(operation, () => assert.equal(target.host.exports.pb_volume_validate(), 0), () => {
        for (const slot of [100, 101]) target.stores.get(slot).attachFence(epoch);
        assert.equal(target.host.exports.pb_volume_publish(), 0);
    });
    assert.deepEqual(target.handle(), originalHandle, 'Entry and volume handles survive publication');
    assert.deepEqual(rootMetadata(), root, 'Physical replica replacement preserves the logical root grant');
    for (let i = 0; i < rootFiles.length; i += 2) {
        assert.equal(target.host.exports.pb_web_file_dispose(rootFiles[i + 1], rootFiles[i]), 0);
    }
    target.stores.set(120, await OpfsStore.open(backing, ledger, async () => {}, epoch)); target.register(entryId, remote.root.id, 120);
    assert.equal(target.read(), true);
    const reopenedFiles = [...controls, backing];
    const reopenLedger = target.resources;
    const reopenedStores = new Map();
    for (const [index, original] of reopenedFiles.entries()) {
        const file = new File(); file.bytes = original.bytes.slice();
        reopenedStores.set(index === 2 ? 65 : index + 1, await OpfsStore.open(file, reopenLedger, async () => {}));
    }
    const reopened = await createHost(remote, reopenedStores);
    assert.equal(reopened.read(), true); assert.deepEqual(reopened.handle(), originalHandle);
    publication.close();
});

test('rejected source or local cuts discard completed staged writes without changing active content', async () => {
    const { source, target, remote } = await pair(), ledger = target.resources;
    const publication = await VolumePublication.open(new File(), ledger, () => encodeId(new Uint8Array(16).fill(0x11)), async () => {}), epoch = publication.epoch(remote.root.id);
    target.stores.get(65).attachFence(epoch);
    const operation = publication.prepare(remote.root.id);
    await prepare(target, remote); source.edit(true);
    await pull(source, target, 1, 100, true); await pull(source, target, 2, 101, true);
    await copyBacking(target, 65, 102); target.call('pb_volume_register_backing', { entry: entryId, slot: 102, replica_id: 113 });
    await pull(source, target, 65, 102, true); target.host.exports.pb_volume_close_backing();
    assert.throws(() => publication.publish(operation, () => { throw new Error('Rejected AP cut'); }, () => assert.fail()), /Rejected AP/);
    assert.equal(target.read(), false); assert.equal(publication.current(remote.root.id), null);
    const writer = target.stores.get(65).begin(true); writer.end(false);
    assert.throws(() => publication.publish(operation, () => {}, () => assert.fail()), LocalVolumeChanged);
    assert.equal(target.host.exports.pb_volume_abort(), 0); assert.equal(target.read(), false);
    assert.equal(target.stores.has(100), false); assert.equal(target.stores.has(101), false);
    publication.close();
});

test('current consent changes and retired/unknown scopes cannot authorize a prepared publication', async () => {
    const { target, remote } = await pair();
    await prepare(target, remote);
    target.call('pb_select_personal', { session: remote.session.id, grants: [] });
    assert(target.host.exports.pb_volume_validate() < 0, 'A newer consent cut invalidates preparation');
    assert.equal(target.host.exports.pb_volume_abort(), 0); assert.equal(target.read(), false);
    await copyBacking(target, 1, 100); await copyBacking(target, 2, 101); await copyBacking(target, 1, 1100);
    assert.throws(() => target.call('pb_volume_prepare', { volume: encodeId(new Uint8Array(16).fill(0xee)), base: 100, accepted_slot: 1100, accepted_replica_id: 119, replica_ids: [1, 2] }));
    assert.equal(target.read(), false);
});

test('local fallback requires initialized data and exactly the same immutable creation', async () => {
    const { target, remote } = await pair();
    await prepare(target, remote); await copyBacking(target, 65, 102);
    target.call('pb_volume_register_backing', { entry: entryId, slot: 102, replica_id: 113 });
    target.call('pb_volume_local_backing', entryId);
    assert.throws(() => target.call('pb_volume_local_backing', encodeId(new Uint8Array(15).fill(0xee))));
    target.host.exports.pb_volume_close_backing(); target.host.exports.pb_volume_abort();
    const different = structuredClone(remote); different.local = structuredClone(bootstrap(remote.root.id).local);
    const other = await createHost(different);
    const copy = new File(); copy.bytes = other.stores.get(1).handle.bytes.slice();
    target.stores.set(100, await OpfsStore.open(copy, target.resources, async () => {})); await copyBacking(target, 2, 101); await copyBacking(target, 100, 1100);
    const input = encoder.encode(JSON.stringify({ volume: remote.root.id }));
    const admission = JSON.parse(decoder.decode(target.raw('pb_volume_admission', input, input.length)));
    assert.throws(() => target.call('pb_volume_prepare', { volume: remote.root.id, base: 100, accepted_slot: 1100, accepted_replica_id: 119, replica_ids: [1, 2], generation: encodeId(new Uint8Array(16).fill(0xb2)), ...admission }),
        'A changed local source cannot seed the detached AP comparison');
    assert.equal(target.host.exports.pb_volume_abort(), 0);
    assert.equal(target.read(), false);
});

test('admission captured before copying cannot adopt a later consent revision', async () => {
    const { target, remote } = await pair();
    const input = encoder.encode(JSON.stringify({ volume: remote.root.id }));
    const admission = JSON.parse(decoder.decode(target.raw('pb_volume_admission', input, input.length)));
    await copyBacking(target, 1, 100); await copyBacking(target, 2, 101); await copyBacking(target, 1, 1100);
    target.call('pb_select_personal', { session: remote.session.id, grants: [] });
    assert.throws(() => target.call('pb_volume_prepare', { volume: remote.root.id, base: 100, accepted_slot: 1100, accepted_replica_id: 119, replica_ids: [1, 2], generation: encodeId(new Uint8Array(16).fill(0xb2)), ...admission }));
    assert.equal(target.read(), false);
});

test('compiled source-cut admission requires both controls and permanently refuses a mixed generation, epoch or policy', async () => {
    for (const changed of [
        { ...receipt(), source: { ...receipt().source, generation: encodeId(new Uint8Array(16).fill(0x66)) } },
        { ...receipt(), source: { ...receipt().source, epoch: receipt().source.epoch - 1n } },
        { ...receipt(), policy_revision: 8n },
    ]) {
        const { source, target, remote } = await pair(); await prepare(target, remote);
        assert(target.host.exports.pb_volume_validate() < 0, 'Copied local controls carry no AP cut');
        await pull(source, target, 1, 100, true); assert(admit(target, 100) instanceof Uint8Array);
        assert(target.host.exports.pb_volume_validate() < 0, 'One control receipt cannot qualify the volume');
        assert.equal(admit(target, 101, changed), -10);
        assert.equal(admit(target, 101), -10, 'Ignoring a mismatch cannot later qualify already merged bytes');
        assert(target.host.exports.pb_volume_validate() < 0);
        assert(target.host.exports.pb_volume_publish() < 0); assert.equal(target.read(), false);
        assert.equal(target.host.exports.pb_volume_abort(), 0);
    }
});

test('native log storage receives through compiled OPFS, preserving pending identity across duplicate and reordered pages', async () => {
    // This fixture was generated by native Db/volume_index/Log storage. It has
    // one offline pending entry; AP pages contain it again plus 69 newer entries.
    const fixture = JSON.parse(await readFile(new URL('../tests/bloblog-vectors.json', import.meta.url), 'utf8'), bloblogInteger);
    const profile = bootstrap(fixture.volume, { personal: true }); profile.root.share_key_with_service = true; profile.local = null;
    const target = await createHost(profile), ledger = target.resources;
    const putRaw = (slot, rows) => {
        const store = OpfsStore.empty(new File(), ledger), writer = store.begin(true);
        for (const [key, value] of rows) writer.put(Uint8Array.from(key), Uint8Array.from(value));
        writer.end(true); target.stores.set(slot, store); return store.handle;
    };
    // Reopen with the actual native index before creating a prepared generation.
    target.stores.get(1).close(); putRaw(1, fixture.index);
    const original = await createHost(profile, target.stores);
    putRaw(65, fixture.local_backing); original.register(fixture.entry);
    const remoteStores = new Map();
    for (const slot of [1, 2]) {
        const file = new File(); file.bytes = original.stores.get(slot).handle.bytes.slice();
        remoteStores.set(slot, await OpfsStore.open(file, ledger, async () => {}));
    }
    const remote = await createHost(profile, remoteStores);
    const controls = await prepare(original, profile);
    assert.deepEqual(catalog(original, true).backings, [[fixture.entry, 'blob_log']]);
    const backing = await copyBacking(original, 65, 102);
    original.call('pb_volume_register_backing', { entry: fixture.entry, slot: 102, replica_id: 113 });
    assert(original.host.exports.pb_volume_close_backing() < 0, 'A log cannot become ready before validation completes');
    await pull(remote, original, 1, 100, true); await pull(remote, original, 2, 101, true);
    assert.equal(JSON.parse(decoder.decode(original.raw('pb_volume_log_prepare', empty))), true);
    const apply = page => {
        const input = bloblogPage({ page, database_clock: 18446744073709551615n, policy_revision: 7n,
            source: { generation: encodeId(new Uint8Array(16).fill(0x55)), epoch: 9007199254740995n } }, fixture.volume, fixture.entry);
        const result = original.raw('pb_volume_log_page', input.bytes, input.bytes.length);
        assert(result instanceof Uint8Array); assert.equal(JSON.parse(decoder.decode(result)), page.cursor);
    };
    apply({ ...fixture.pages[0], entries: fixture.pages[0].entries.toReversed() });
    apply(fixture.pages[0]); apply(fixture.pages[1]); apply(fixture.pages[1]);
    let calls = 0;
    for (;;) {
        const output = original.raw('pb_volume_log_validate', empty); assert(output instanceof Uint8Array);
        calls++; if (JSON.parse(decoder.decode(output))) break;
        assert(calls < 10);
    }
    assert(calls >= 3, 'Validation yields between bounded pages of stored entry/index/pending records');
    assert.equal(original.host.exports.pb_volume_close_backing(), 0);
    assert.equal(original.host.exports.pb_volume_validate(), 0);
    assert.equal(original.host.exports.pb_volume_publish(), 0);
    original.stores.set(120, await OpfsStore.open(backing, ledger, async () => {})); original.register(fixture.entry, fixture.volume, 120);
    const checkpoint = encoder.encode(JSON.stringify({ volume: fixture.volume, database: fixture.entry }));
    const cursor = original.raw('pb_bloblog_checkpoint', checkpoint, checkpoint.length);
    assert.equal(JSON.parse(decoder.decode(cursor)), fixture.pages[1].cursor);
    const entry = fixture.pages[0].entries[0], inline = new Uint8Array(4 + 59 + entry.data.length), view = new DataView(inline.buffer);
    view.setUint32(0, 1, true); inline.set(Buffer.from(decodeId(entry.log, 15)), 4); inline.set(decodeId(entry.id), 19);
    view.setBigUint64(35, entry.clock); view.setUint32(43, entry.data.length, true); inline.set(decodeId(entry.nonce), 47); inline.set(entry.data, 63);
    const live = new Uint8Array(32 + inline.length); live.set(Buffer.from(decodeId(fixture.volume))); live[16] = 2;
    live.set(Buffer.from(decodeId(fixture.entry, 15)), 17); live.set(inline, 32);
    assert.equal(original.raw('pb_live_apply', live, live.length).length, 0, 'Authorized inline delivery uses append identity admission');
    const corrupt = live.slice(); corrupt[corrupt.length - 1] ^= 1;
    assert(original.raw('pb_live_apply', corrupt, corrupt.length) < 0);
    const store = await OpfsStore.open(backing, ledger, async () => {});
    const reopenedRows = store.keys.map(key => [Array.from(Buffer.from(key, 'hex')), Array.from(store.get(Buffer.from(key, 'hex')))]);
    store.close();
    // kvdb raw nodes are written by the shared Log implementation. Save this
    // result for the native reciprocal probe, which checks 70 entries/one pending.
    if (process.env.PLAYBIT_TEST_LOG_RESULT) await (await import('node:fs/promises')).writeFile(process.env.PLAYBIT_TEST_LOG_RESULT, JSON.stringify(reopenedRows));
    assert(controls.every(file => file.getSize() > 0));
});

function authorityJSON(target, name, value) {
    const bytes = encoder.encode(JSON.stringify(value));
    const result = target.raw(name, bytes, bytes.length); assert(result instanceof Uint8Array);
    return JSON.parse(decoder.decode(result));
}

function terminal(target, source, changed = source, cause = 1) {
    const bytes = new Uint8Array(33);
    bytes.set(decodeId(source)); bytes.set(decodeId(changed), 16); bytes[32] = cause;
    return target.raw('pb_live_terminal', bytes, bytes.length);
}
async function completeReceived(source, target, remote) {
    await prepare(target, remote);
    await pull(source, target, 1, 100, true); await pull(source, target, 2, 101, true);
    const file = await copyBacking(target, 65, 102);
    target.call('pb_volume_register_backing', { entry: entryId, slot: 102, replica_id: 119 });
    await pull(source, target, 65, 102, true);
    assert.equal(target.host.exports.pb_volume_close_backing(), 0);
    return file;
}
function qualify(target, value = receipt()) {
    const bytes = encoder.encode(replicaJSON(value));
    return target.raw('pb_volume_qualify', bytes, bytes.length);
}
test('compiled authority fences deny cached content, persist through reopening, and exclude service loss', async () => {
    const { target, remote } = await pair();
    assert(terminal(target, remote.root.id, remote.root.id, 3) instanceof Uint8Array);
    assert.equal(target.read(), false); assert.equal(target.edit(true), 0);
    assert(terminal(target, remote.root.id) instanceof Uint8Array);
    assert.equal(target.read(), -2); assert.equal(target.edit(false), -2);
    assert.deepEqual(authorityJSON(target, 'pb_authority_refresh_required', remote.root.id), true);
    assert(terminal(target, remote.root.id, encodeId(new Uint8Array(16).fill(0x99))) < 0);
    assert(terminal(target, remote.root.id, remote.root.id, 0) < 0);
    const reopened = await createHost(remote, target.stores, undefined, undefined, false);
    assert.equal(reopened.read(), -2); assert.equal(reopened.edit(false), -2);
});
test('compiled volume publication clears only a current fully AP-qualified authority generation', async () => {
    const { source, target, remote } = await pair();
    assert(terminal(target, remote.root.id) instanceof Uint8Array);
    const backing = await completeReceived(source, target, remote);
    assert(qualify(target) instanceof Uint8Array);
    assert.equal(target.read(), -2);
    assert.equal(target.host.exports.pb_volume_validate(), 0);
    assert.equal(target.host.exports.pb_volume_publish(), 0);
    target.stores.set(103, await OpfsStore.open(backing, target.resources, async () => {}));
    target.register(entryId, remote.root.id, 103);
    assert.equal(target.read(), false); assert.equal(target.edit(true), 0);
});
test('peer-only publication and a newer terminal preserve durable negative evidence', async () => {
    for (const newer of [false, true]) {
        const { source, target, remote } = await pair();
        assert(terminal(target, remote.root.id) instanceof Uint8Array);
        await completeReceived(source, target, remote);
        if (newer) {
            assert(qualify(target) instanceof Uint8Array);
            assert(terminal(target, remote.root.id) instanceof Uint8Array);
        }
        assert.equal(target.host.exports.pb_volume_validate(), 0);
        assert.equal(target.host.exports.pb_volume_publish(), 0);
        assert.equal(target.read(), -2); assert.equal(target.edit(false), -2);
        assert.deepEqual(authorityJSON(target, 'pb_authority_refresh_required', remote.root.id), true);
    }
});
test('participant denial survives duplicate delivery and stale admission completion', async () => {
    const { target, remote } = await pair();
    const capture = authorityJSON(target, 'pb_session_authority', remote.session);
    assert(terminal(target, remote.root.id, remote.root.id, 2) instanceof Uint8Array);
    target.call('pb_session_admit', { admission: capture, registered: remote.session });
    assert.equal(target.read(), -2);
    const current = authorityJSON(target, 'pb_session_authority', remote.session);
    assert(terminal(target, remote.root.id, remote.root.id, 2) instanceof Uint8Array);
    target.call('pb_session_admit', { admission: current, registered: remote.session });
    assert.equal(target.read(), -2);
    const fresh = authorityJSON(target, 'pb_session_authority', remote.session);
    target.call('pb_session_admit', { admission: fresh, registered: remote.session });
    assert.equal(target.read(), false);
});
test('session admission requires the exact AP-authenticated identity and build', async () => {
    const { target, remote } = await pair();
    assert(terminal(target, remote.root.id, remote.root.id, 2) instanceof Uint8Array);
    const admission = authorityJSON(target, 'pb_session_authority', remote.session);
    const otherAccount = encodeId(new Uint8Array(16).fill(0x71));
    const otherBuild = encodeId(new Uint8Array(32).fill(0x72));
    const changes = [
        session => { session.app.publisher = session.creator = otherAccount; },
        session => { session.app.app_id = 'todo.testing'; },
        session => { session.root_volume = otherAccount; },
        session => { session.id = otherBuild; },
        session => { session.update_authority = 'app'; },
        session => { session.selector = { kind: 'app', app: { publisher: otherAccount, app_id: session.app.app_id } }; },
        session => { session.selector.build_id = session.resolved_build_id = otherBuild; },
    ];
    for (const change of changes) {
        const registered = structuredClone(remote.session); change(registered);
        assert.throws(() => target.call('pb_session_admit', { admission, registered }));
        assert.equal(target.read(), -2, 'untrusted provenance must not clear denial evidence');
    }
    assert.throws(() => target.call('pb_session_admit', { admission }));
    target.call('pb_session_admit', { admission, registered: remote.session });
    assert.equal(target.read(), false);
    // A retained stream requests its own version even after the local exact
    // selector advances. AP returns that authorized version's descriptor.
    assert(terminal(target, remote.root.id, remote.root.id, 3) instanceof Uint8Array);
    const retained = structuredClone(remote.session); retained.selector.build_id = otherBuild;
    const older = authorityJSON(target, 'pb_session_authority', retained);
    target.call('pb_session_admit', { admission: older, registered: remote.session });
    assert.equal(target.read(), false);
});

test('negative evidence can persist while a prepared sync owns its read snapshot', async () => {
    const { target, remote } = await pair();
    assert(target.raw('pb_sync_begin', empty, 1, 0) instanceof Uint8Array);
    assert(terminal(target, remote.root.id) instanceof Uint8Array);
    assert.equal(target.host.exports.pb_sync_finish(0), 0);
    assert.equal(target.read(), -2); assert.equal(target.edit(true), -2);
});

test('namespace qualification refuses missing retained-payload accounting before adding a new source', async () => {
    // Corrupt only the local accounting row; source roots and prior accepted
    // payloads stay unchanged. A new receipt must not reset their capacity.
    const fixture = JSON.parse(await readFile(new URL('../tests/blob-authority-vectors.json', import.meta.url)));
    const profile = bootstrap(fixture.reference.volume, { personal: true });
    profile.local = null; profile.root.owner = encodeId(new Uint8Array(16).fill(0x09)); profile.root.share_key_with_service = true;
    const usage = Uint8Array.from([129, ...encoder.encode('namespace-confirmed-payload-usage')]);
    const damaged = fixture.snapshots.confirmed.filter(([key]) => !Buffer.from(key).equals(usage));
    assert.equal(damaged.length, fixture.snapshots.confirmed.length - 1);
    const target = await storedHost(profile, [[1, damaged]]);
    const source = await storedHost({ ...profile, account: profile.root.owner }, [[1, fixture.snapshots.replacement]]);
    await prepare(target, profile, fixture.reference.entry);
    await pull(source, target, 1, 100, true); await pull(source, target, 2, 101, true);
    const vector = fixture.receipts.replacement;
    assert.equal(vector.namespace.count, 2, 'The receipt adds one source beside an existing accepted birth');
    const qualified = { ...receipt(), root: vector.root,
        namespace: { ...vector.namespace, entry: fixture.reference.entry } };
    const metadata = encoder.encode(replicaJSON({ slot: 100, prepared: true,
        entry: fixture.reference.entry, receipt: qualified }));
    const evidence = Uint8Array.from([...metadata, ...vector.evidence]);
    assert(target.raw('pb_namespace_evidence', evidence, evidence.length, metadata.length) instanceof Uint8Array);
    const before = target.stores.get(100).handle.bytes.slice();
    const input = encoder.encode(replicaJSON(qualified));
    assert(target.raw('pb_volume_qualify', input, input.length) < 0);
    assert.deepEqual(target.stores.get(100).handle.bytes, before, 'Failure rolls back every candidate ledger and payload write');
    assert.equal(target.stores.get(100).get(usage), null, 'A new receipt cannot recreate a partial usage counter');
    assert.equal(target.host.exports.pb_volume_abort(), 0);
});
