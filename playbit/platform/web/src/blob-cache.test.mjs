import { encodeId } from './identifier.ts';
import test from 'node:test';
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fixture, ownedFiles, cacheService } from '../tests/file-io-fixture.mjs';
import { bootstrap, createHost } from '../tests/host-fixture.mjs';
import { RuntimeBlobs } from './runtime-blobs.ts';
import { VolumeWriteEpoch } from './volume-publication.ts';
import { DiskFile } from '../tests/opfs-fixture.mjs';

const extent = 64 * 1024 * 1024;
const digest = byte => byte.toString(16).padStart(2, '0').repeat(32);
function retained(f, byte, confirmed = true) {
    // Sparse files model already-verified contents at the cache policy boundary.
    const value = digest(byte), filename = path.join(f.location, `blob-${value}`);
    const file = fs.openSync(filename, 'wx'); fs.ftruncateSync(file, extent); fs.closeSync(file);
    f.services.cache({ kind: 'saved', digest: value, length: extent, confirmed });
    return filename;
}
const occupancy = f => fs.readdirSync(f.location).filter(name => /^blob-[a-f0-9]{64}$/.test(name))
    .reduce((total, name) => total + fs.statSync(path.join(f.location, name)).size, 0);
function pending(f, volume = f.profile.root.id) {
    const bytes = new TextEncoder().encode(JSON.stringify({ volume, after: null }));
    const output = f.host.raw('pb_blob_pending', bytes, bytes.length);
    assert(output instanceof Uint8Array);
    return JSON.parse(new TextDecoder().decode(output))[0];
}

test('physical cache returns to 256 MiB, retains unsynchronized contents, and restores receipts after restart', async () => {
    // Use real Rust policy and physical files; memory lease count is irrelevant.
    const f = await fixture(); let reopened;
    try {
        for (let byte = 1; byte <= 5; byte++) retained(f, byte);
        assert.equal(occupancy(f), 5 * extent);
        await f.blobs.cleanup(); assert.equal(occupancy(f), 4 * extent);
        retained(f, 6, false); await f.blobs.cleanup();
        assert.equal(occupancy(f), 4 * extent);
        assert(fs.existsSync(path.join(f.location, `blob-${digest(6)}`)));
        const host = await createHost(f.profile, f.host.stores);
        const services = { ...f.services, cache: cacheService(host) };
        reopened = new RuntimeBlobs(services);
        await reopened.open();
        retained({ ...f, services }, 7);
        await reopened.cleanup(); assert.equal(occupancy(f), 4 * extent);
        assert(fs.existsSync(path.join(f.location, `blob-${digest(6)}`)));
    } finally { reopened?.close(); f.close(); }
});

test('deletion failure retains byte accounting and the exclusive fence rejects new owners until completion', async () => {
    // Suspend real removal and try to pin its selected digest before releasing it.
    const f = await fixture();
    const directory = f.services.directory(), remove = directory.removeEntry.bind(directory);
    try {
        for (let byte = 1; byte <= 5; byte++) retained(f, byte, byte === 1);
        let release, entered;
        const held = new Promise(resolve => { release = resolve; });
        const ready = new Promise(resolve => { entered = resolve; });
        directory.removeEntry = async name => { entered(name); await held; throw new Error('Injected cache deletion failure'); };
        const cleanup = f.blobs.cleanup();
        const selected = (await ready).slice(5);
        assert.throws(() => f.services.cache({ kind: 'pin', digest: selected }), error => error.code === -12);
        release(); await assert.rejects(cleanup, /cache deletion failure/);
        assert.equal(occupancy(f), 5 * extent);
        f.services.cache({ kind: 'pin', digest: selected }); f.services.cache({ kind: 'unpin', digest: selected });
        directory.removeEntry = remove;
        await f.blobs.cleanup(); assert.equal(occupancy(f), 4 * extent);
    } finally { directory.removeEntry = remove; f.close(); }
});

test('receipt bookkeeping follows retained contents across repeated generations of eviction', async () => {
    // Sequential confirmed downloads must release obsolete live index rows too.
    const f = await fixture(), rows = f.host.stores.get(1).keys.length;
    try {
        for (let byte = 1; byte <= 48; byte++) {
            retained(f, byte); await f.blobs.cleanup();
            assert(f.host.stores.get(1).keys.length <= rows + 4);
        }
        assert.equal(occupancy(f), 4 * extent);
    } finally { f.close(); }
});

test('receipt deletion failure keeps conservative accounting and an explicit retry completes cleanup', async () => {
    // Fail the index flush after physical removal, then retry the absent file.
    const f = await fixture(), index = f.host.stores.get(1).handle, flush = index.flush.bind(index);
    try {
        for (let byte = 1; byte <= 5; byte++) retained(f, byte, byte === 1);
        let fail = true;
        index.flush = () => { if (fail) { fail = false; throw new Error('Injected receipt flush failure'); } flush(); };
        await assert.rejects(f.blobs.cleanup(), error => error.code === -4);
        assert.equal(occupancy(f), 4 * extent);
        const next = f.services.cache({ kind: 'next' });
        assert.equal(next.kind, 'remove', 'The removed copy remains charged after metadata failure');
        f.services.cache({ kind: 'deleted', digest: next.digest, success: false });
        await f.blobs.cleanup();
        assert.equal(f.services.cache({ kind: 'next' }).bytes, occupancy(f));
    } finally { index.flush = flush; f.close(); }
});

test('root recovery accepts its own receipt and still rejects a concurrent write during cleanup', async () => {
    // Rebase only synchronous receipt bookkeeping; async cleanup cannot absorb a foreign edit.
    const f = await fixture(), epoch = new VolumeWriteEpoch(); let source;
    const directory = f.services.directory(), remove = directory.removeEntry.bind(directory);
    try {
        f.host.stores.get(1).fence = epoch;
        const bytes = Uint8Array.of(5, 6), reference = f.seed(f.file(3), bytes);
        await f.remote(f.file(3), bytes);
        const filename = path.join(f.location, 'recovery.blob'); fs.writeFileSync(filename, bytes);
        source = new DiskFile(filename, false);
        fs.unlinkSync(path.join(f.location, `blob-${reference.digest}`));
        let revision = epoch.capture();
        const check = () => epoch.check(revision);
        let ownWrite = false;
        const receipt = write => { check(); write(); revision = epoch.capture(); ownWrite = true; };
        await f.blobs.restore(source, reference, check, receipt);
        check(); assert(ownWrite);
        const unchanged = epoch.capture();
        f.services.cache({ kind: 'saved', digest: reference.digest, length: bytes.length, confirmed: false });
        assert.equal(epoch.capture(), unchanged, 'An unchanged receipt starts no write transaction');
        ownWrite = false;
        fs.unlinkSync(path.join(f.location, `blob-${reference.digest}`));
        directory.removeEntry = async name => {
            if (ownWrite) { epoch.begin(); epoch.end(); }
            return remove(name);
        };
        await assert.rejects(f.blobs.restore(source, reference, check, receipt), /Local volume changed/);
    } finally { directory.removeEntry = remove; source?.close(); f.close(); }
});

test('unknown contents and active preparation remain retained beyond the target', async () => {
    // A missing receipt cannot authorize eviction; a pin excludes async I/O.
    const f = await fixture();
    try {
        const unknown = retained(f, 1);
        // Publish an untracked file through startup observation in a fresh host.
        const file = fs.openSync(path.join(f.location, `blob-${digest(9)}`), 'wx'); fs.ftruncateSync(file, extent); fs.closeSync(file);
        f.services.cache({ kind: 'observe', digest: digest(9), length: extent });
        for (let byte = 2; byte <= 4; byte++) retained(f, byte, false);
        const release = f.blobs.retain({ digest: digest(1) });
        await f.blobs.cleanup(); assert.equal(occupancy(f), 5 * extent);
        release(); await f.blobs.cleanup(); assert.equal(occupancy(f), 4 * extent);
        assert(!fs.existsSync(unknown));
        assert(fs.existsSync(path.join(f.location, `blob-${digest(9)}`)));
    } finally { f.close(); }
});

test('a recovery rejected after physical publication still accounts for the unknown copy', async () => {
    // Mutate the root during OPFS rename, before the fenced receipt can run.
    const f = await fixture(), epoch = new VolumeWriteEpoch(); let source;
    const directory = f.services.directory(), get = directory.getFileHandle.bind(directory);
    try {
        f.host.stores.get(1).fence = epoch;
        const bytes = Uint8Array.of(17, 18), reference = f.seed(f.file(4), bytes);
        await f.remote(f.file(4), bytes);
        const filename = path.join(f.location, 'recovery.blob'); fs.writeFileSync(filename, bytes);
        source = new DiskFile(filename, false);
        fs.unlinkSync(path.join(f.location, `blob-${reference.digest}`));
        let revision = epoch.capture(); const check = () => epoch.check(revision);
        directory.getFileHandle = async (...args) => {
            const handle = await get(...args), move = handle.move.bind(handle);
            handle.move = async (...args) => { await move(...args); epoch.begin(); epoch.end(); };
            return handle;
        };
        await assert.rejects(f.blobs.restore(source, reference, check, write => {
            check(); write(); revision = epoch.capture();
        }), /Local volume changed/);
        for (let byte = 1; byte <= 4; byte++) retained(f, byte, false);
        await f.blobs.cleanup();
        assert.equal(f.services.cache({ kind: 'next' }).bytes, occupancy(f));
        assert.equal(occupancy(f), 4 * extent + bytes.length);
    } finally { directory.getFileHandle = get; source?.close(); f.close(); }
});

test('shared contents stay protected by another volume and by retained File operations after disposal', async () => {
    // Publish the same bytes twice, acknowledge one volume, and drop a public reader during accepted I/O.
    const f = await fixture({ connected: true }), api = ownedFiles(f);
    try {
        const content = Uint8Array.of(7, 8, 9);
        const a = await api.open('shared'); await api.run(a, 9, content); await api.run(a, 25);
        const root = pending(f)[0]; assert(root);
        // Select another volume explicitly; storage ownership belongs to the guest.
        const otherProfile = bootstrap(encodeId(new Uint8Array(16).fill(0x31)), { personal: true });
        otherProfile.root.share_key_with_service=true;
        const other = await createHost(otherProfile);
        f.host.stores.set(3, other.stores.get(1)); f.host.stores.set(4, other.stores.get(2));
        f.host.call('pb_register_personal', { volume: otherProfile.root, key: otherProfile.root_key, replica_ids: [7, 8] });
        f.host.call('pb_select_personal', { session: f.profile.session.id, grants: [{ volume: otherProfile.root.id, write: true, incarnation: encodeId(new Uint8Array(16).fill(1)) }] });
        assert.equal(api.host.pb_web_volumes(), 0);
        const revision = new DataView(api.host.memory.buffer).getBigUint64(api.host.pb_web_result() + 8, true);
        assert.equal(api.host.pb_web_volumes_acquire(revision), 0);
        const roots = new DataView(api.host.memory.buffer, api.host.pb_output(), 16);
        const selected = [roots.getUint32(12, true), roots.getUint32(8, true)];
        const second = (await api.run(selected, 23, new TextEncoder().encode('shared'), 4)).file;
        await api.run(second, 9, content); await api.run(second, 25);
        const peer = pending(f, otherProfile.root.id)[0]; assert.equal(peer.digest, root.digest);
        f.host.call('pb_blob_acknowledge', root);
        // Fill all other space with unsynchronized bytes so only the shared digest could be removed.
        for (let byte = 1; byte <= 4; byte++) retained(f, byte, false);
        await f.blobs.cleanup(); assert.equal(occupancy(f), 4 * extent + content.length);
        const reader = await api.open('shared', 1), token = api.begin(reader, 8, new Uint8Array(), 0, 0n, 1);
        assert.equal(api.host.pb_web_file_dispose(...reader), 0);
        f.host.call('pb_blob_acknowledge', peer);
        await f.blobs.cleanup(); assert.equal(occupancy(f), 4 * extent + content.length);
        const output = await api.backend.execute(token, new Uint8Array());
        assert.deepEqual(output.Read.bytes, content.subarray(0, 1));
        api.finish(token, output);
        await api.backend.cleanup();
        assert.equal(occupancy(f), 4 * extent);
    } finally { await api.backend.cleanup(); f.close(); }
});

test('withdrawn and unloaded personal volumes still protect shared pending contents across restart', async () => {
    // Global retention must not rely on the currently authorized upload catalog.
    const f = await fixture({ connected: true }), api = ownedFiles(f); let reopened;
    try {
        const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x30))); profile.root.share_key_with_service = true;
        const personal = await createHost(profile);
        f.host.stores.set(3, personal.stores.get(1)); f.host.stores.set(4, personal.stores.get(2));
        f.host.call('pb_register_personal', { volume: profile.root, key: profile.root_key, replica_ids: [7, 8] });
        f.host.call('pb_select_personal', { session: f.profile.session.id, grants: [{ volume: profile.root.id, write: true, incarnation: encodeId(new Uint8Array(16).fill(0x01)) }] });
        assert.equal(api.host.pb_web_volumes(), 0);
        const revision = new DataView(api.host.memory.buffer).getBigUint64(api.host.pb_web_result() + 8, true);
        assert.equal(api.host.pb_web_volumes_acquire(revision), 0);
        const roots = new DataView(api.host.memory.buffer, api.host.pb_output(), 16);
        const selected = [roots.getUint32(12, true), roots.getUint32(8, true)];
        const contents = Uint8Array.of(31, 32);
        const a = await api.open('root-copy'); await api.run(a, 9, contents); await api.run(a, 25);
        const b = (await api.run(selected, 23, new TextEncoder().encode('personal-copy'), 4)).file;
        await api.run(b, 9, contents); await api.run(b, 25);
        f.host.call('pb_blob_acknowledge', pending(f)[0]);
        f.host.call('pb_select_personal', { session: f.profile.session.id, grants: [] });
        const input = new TextEncoder().encode(JSON.stringify({ volume: profile.root.id, after: null }));
        assert.deepEqual(JSON.parse(new TextDecoder().decode(f.host.raw('pb_blob_pending', input, input.length))), [[], null]);
        for (let byte = 1; byte <= 4; byte++) retained(f, byte, false);
        await f.blobs.cleanup(); assert.equal(occupancy(f), 4 * extent + contents.length);
        const host = await createHost(f.profile, f.host.stores);
        reopened = new RuntimeBlobs({ ...f.services, cache: cacheService(host) });
        await reopened.open();
        assert.equal(occupancy(f), 4 * extent + contents.length);
    } finally { reopened?.close(); await api.backend.cleanup(); f.close(); }
});
