import { encodeId } from './identifier.ts';
import { Directory } from '../tests/memory-directory.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { OpfsStore, OpfsResources } from './opfs-store.ts';
import { VolumePublication, VolumeWriteEpoch, LocalVolumeChanged, PublicationUncertain, copyVolumeBacking } from './volume-publication.ts';
import { retainRejectedCut } from './volume-archive.ts';

class File {
    bytes = new Uint8Array();
    failFlush = false;
    getSize() { return this.bytes.length; }
    read(out, { at }) { const bytes = this.bytes.subarray(at, at + out.length); out.set(bytes); return bytes.length; }
    write(bytes, { at }) { const next = new Uint8Array(Math.max(at + bytes.length, this.bytes.length)); next.set(this.bytes); next.set(bytes, at); this.bytes = next; return bytes.length; }
    truncate(length) { this.bytes = this.bytes.slice(0, length); }
    flush() { if (this.failFlush) throw new Error('Injected flush failure'); }
    close() {}
}
const resources = () => new OpfsResources([new File(), new File()], [new File(), new File()]);
const id = number => encodeId(Buffer.from(number.toString(16).padStart(32, '0'), 'hex'));
const entry = number => encodeId(Buffer.from(number.toString(16).padStart(30, '0'), 'hex'));
test('offline archive failures never record a cut or replace the active pointer and discard removes partial files', async () => {
    for (const fault of ['quota', 'short-write', 'flush', 'writer', 'hash', 'blob-digest']) {
        const { publication, directory } = await fixture();
        const original = publication.prepare(id(1)); await publication.create(directory, original);
        publication.publish(original, () => {}, () => {});
        const previous = await directory.getDirectoryHandle(publication.directory(id(1), original.generation));
        const source = new Blob([new Uint8Array(70 * 1024)]);
        previous.entries.set('index.kvdb', { getFile: async () => source });
        const operation = publication.prepare(id(1)); await publication.create(directory, operation);
        const generation = await directory.getDirectoryHandle(publication.directory(id(1), operation.generation));
        const archive = await generation.getDirectoryHandle('rejected', { create: true });
        let opened = 0, closed = 0, written = 0, commits = 0;
        archive.getFileHandle = async name => {
            const file = new File(); archive.entries.set(name, file);
            file.failFlush = fault === 'flush';
            const write = file.write.bind(file);
            file.write = (bytes, options) => { written++; return fault === 'short-write' ? 0 : write(bytes, options); };
            file.close = () => { closed++; };
            return { createSyncAccessHandle: async () => { opened++; return file; } };
        };
        directory.entries.set(`blob-${'44'.repeat(32)}`, { getFile: async () => new Blob(['abc']) });
        const call = async name => {
            if (name === 'pb_volume_rejected_page') return [[entry(1)], null];
            if (name === 'pb_volume_archive_plan') return { available: 65536 + source.size + (fault === 'quota' ? -1 : 100), reserve: 65536, backings: [] };
            if (name === 'pb_volume_archive_blobs') return [fault === 'blob-digest' ? [[Array(32).fill(0x44), 3]] : [], null];
            if (name === 'pb_volume_archive_commit') { commits++; return; }
            throw new Error(`Unexpected archive operation ${name}`);
        };
        const hash = () => {
            if (fault === 'hash') throw new Error('Injected hash admission failure');
            return { update() {}, finish: () => '55'.repeat(32), close() {} };
        };
        const yieldReady = async () => {
            if (fault === 'writer' && written) { operation.epoch.begin(); operation.epoch.end(); }
        };
        await assert.rejects(retainRejectedCut(directory, publication, operation, call, hash, yieldReady));
        assert.equal(commits, 0, fault); assert.equal(closed, opened, `${fault}: every file handle closes`);
        assert.equal(publication.current(id(1)), original.generation, fault);
        await publication.discard(directory, id(1), operation.generation);
        assert.equal(directory.entries.has(publication.directory(id(1), operation.generation)), false, fault);
        assert.equal(publication.pending(id(1)), null, fault);
    }
});
async function fixture(file = new File()) {
    let generation = 100;
    const ledger = resources(), publication = await VolumePublication.open(file, ledger, () => id(generation++), async () => {});
    return { file, ledger, publication, directory: new Directory() };
}

test('root initialization retains its original package across control publication and reopen', async () => {
    const { file, publication, directory } = await fixture(), volume = id(1);
    const build = encodeId(new Uint8Array(32).fill(7));
    const original = publication.prepare(volume); await publication.create(directory, original);
    publication.beginInitialization(volume, build, 2);
    assert.throws(() => publication.finishInitialization(volume, build), /changed/);
    publication.close();
    const pending = (await fixture(file)).publication;
    await pending.discardPending(directory);
    assert.deepEqual(pending.initialization(volume), { build, storage: 2, complete: false });
    const replacement = pending.prepare(volume); await pending.create(directory, replacement);
    pending.publish(replacement, () => {}, () => {});
    pending.close();
    const selected = (await fixture(file)).publication;
    await selected.discardPending(directory);
    assert.deepEqual(selected.initialization(volume), { build, storage: 2, complete: false });
    assert.throws(() => selected.beginInitialization(volume, build, 0), /Invalid/);
    assert.throws(() => selected.finishInitialization(volume, encodeId(new Uint8Array(32).fill(8))), /changed/);
    selected.finishInitialization(volume, build); selected.close();
    const completed = (await fixture(file)).publication;
    assert.deepEqual(completed.initialization(volume), { build, storage: 2, complete: true });
    assert.throws(() => completed.finishInitialization(volume, build), /changed/);
    completed.close();
});

test('forty full publications retain only current controls and backings, including after reopen', async () => {
    const { file, publication, directory } = await fixture(), volume = id(1);
    for (let index = 0; index < 40; index++) {
        const prepared = publication.prepare(volume);
        await publication.create(directory, prepared);
        await publication.acceptedIndex(directory, prepared);
        for (const database of ['index', 'revocations', entry(1)]) await publication.file(directory, volume, prepared.generation, database, true);
        publication.publish(prepared, () => {}, () => {});
        await publication.reclaim(directory, volume, (_database, generation) => generation === prepared.generation);
        assert.equal(directory.entries.size, 1);
    }
    publication.close();
    const reopened = (await fixture(file)).publication;
    await reopened.reclaim(directory, volume, () => false);
    assert.equal(directory.entries.size, 1);
    const current = await directory.getDirectoryHandle(reopened.directory(volume, reopened.current(volume)));
    assert.equal(current.entries.size, 4); reopened.close();
});

test('partial publication retains older referenced siblings and retries interrupted cleanup after reopen', async () => {
    const { file, publication, directory } = await fixture(), volume = id(1);
    const initial = publication.prepare(volume); await publication.create(directory, initial);
    for (const database of ['index', 'revocations', entry(1), entry(2)]) await publication.file(directory, volume, initial.generation, database, true);
    publication.publish(initial, () => {}, () => {});
    const partial = publication.prepare(volume); await publication.create(directory, partial);
    for (const database of ['index', 'revocations', entry(1)]) await publication.file(directory, volume, partial.generation, database, true);
    publication.publish(partial, () => {}, () => {});
    await assert.rejects(publication.reclaim(directory, volume, () => { throw new Error('Interrupted lookup'); }), /Interrupted/);
    const old = await directory.getDirectoryHandle(publication.directory(volume, initial.generation));
    assert(old.entries.has(`${entry(1)}.kvdb`)); assert(old.entries.has(`${entry(2)}.kvdb`));
    publication.close();
    const reopened = (await fixture(file)).publication;
    await reopened.reclaim(directory, volume, (database, generation) => database === entry(2) && generation === initial.generation);
    assert.deepEqual([...old.entries.keys()], [`${entry(2)}.kvdb`]);
    assert.equal(directory.entries.size, 2);
    await reopened.reclaim(directory, volume, () => false);
    assert.equal(directory.entries.size, 1); reopened.close();
});

test('cleanup preserves pending and unknown files and rejects a changed publication pointer', async () => {
    const { publication, directory } = await fixture(), volume = id(1);
    const initial = publication.prepare(volume); await publication.create(directory, initial);
    const old = await directory.getDirectoryHandle(publication.directory(volume, initial.generation));
    await old.getFileHandle('unknown', { create: true });
    publication.publish(initial, () => {}, () => {});
    const next = publication.prepare(volume); await publication.create(directory, next);
    publication.publish(next, () => {}, () => {});
    const pending = publication.prepare(volume); await publication.create(directory, pending);
    await publication.reclaim(directory, volume, () => assert.fail());
    assert.equal(directory.entries.size, 3);
    const values = directory.values.bind(directory);
    directory.values = async function* () {
        for await (const item of values()) {
            publication.publish(pending, () => {}, () => {});
            yield item;
        }
    };
    await assert.rejects(publication.reclaim(directory, volume, () => assert.fail()), LocalVolumeChanged);
    assert(old.entries.has('unknown')); publication.close();
});

test('publication retains the old generation until all validation succeeds and reopens the new pointer', async () => {
    // A rejected source/local/permission check must leave durable routing intact.
    const { file, publication, directory } = await fixture(), volume = id(1);
    const initial = publication.prepare(volume); await publication.create(directory, initial); publication.publish(initial, () => {}, () => {});
    const rejected = publication.prepare(volume); await publication.create(directory, rejected);
    assert.throws(() => publication.publish(rejected, () => { throw new Error('Rejected source cut'); }, () => assert.fail()), /Rejected source/);
    assert.equal(publication.current(volume), initial.generation);
    await publication.discard(directory, volume, rejected.generation);
    const prepared = publication.prepare(volume), target = new File(); await publication.create(directory, prepared);
    await copyVolumeBacking(prepared, new Blob([new Uint8Array(3 * 65536 + 1).fill(7)]), target, async () => {});
    let visible = initial.generation;
    publication.publish(prepared, () => assert.equal(target.getSize(), 3 * 65536 + 1), () => { visible = prepared.generation; });
    assert.equal(visible, publication.current(volume)); publication.close();
    const reopened = (await fixture(file)).publication;
    assert.equal(reopened.current(volume), prepared.generation);
    assert.equal(target.bytes.at(-1), 7); reopened.close();
});

test('every local transaction invalidates preparation, including aborted and unfinished raw writers', async () => {
    // The actual OPFS backend fence covers writes that bypass host operations.
    const { ledger, publication } = await fixture(), volume = id(1), epoch = publication.epoch(volume);
    const backend = OpfsStore.empty(new File(), ledger, epoch), prepared = publication.prepare(volume);
    const unfinished = backend.begin(true);
    assert.throws(() => publication.prepare(volume), LocalVolumeChanged);
    assert.throws(() => publication.publish(prepared, () => {}, () => assert.fail()), LocalVolumeChanged);
    unfinished.end(false);
    assert.throws(() => publication.publish(prepared, () => {}, () => assert.fail()), LocalVolumeChanged);
    const next = publication.prepare(volume), target = new File(); let yields = 0;
    await assert.rejects(copyVolumeBacking(next, new Blob([new Uint8Array(2 * 65536)]), target, async () => {
        if (++yields === 1) { const writer = backend.begin(true); writer.put(Uint8Array.of(1), Uint8Array.of(2)); writer.end(true); }
    }), LocalVolumeChanged);
    assert.equal(target.getSize(), 65536); assert.equal(publication.current(volume), null);
    backend.close(); publication.close(); assert.equal(ledger.stores, 0);
});

test('a crash before pointer publication preserves old routing; after pointer commit selects the complete new generation', async () => {
    // Activation can be interrupted after durable selection; recovery must use that pointer.
    const { file, publication, directory } = await fixture(), volume = id(1), initial = publication.prepare(volume);
    await publication.create(directory, initial);
    publication.publish(initial, () => {}, () => {});
    publication.prepare(volume); // Detached unselected files are never used on reopen.
    const prepared = publication.prepare(volume); await publication.create(directory, prepared);
    assert.throws(() => publication.publish(prepared, () => {}, () => { throw new Error('Injected crash after pointer'); }), PublicationUncertain);
    assert.throws(() => publication.current(volume), /stopped/); publication.close();
    const reopened = (await fixture(file)).publication;
    assert.equal(reopened.current(volume), prepared.generation); reopened.close();
});

test('a pointer flush failure stops further publication and incomplete durable tails recover the previous pointer', async () => {
    const { file, publication, directory } = await fixture(), volume = id(1), initial = publication.prepare(volume);
    await publication.create(directory, initial);
    publication.publish(initial, () => {}, () => {}); const committed = file.getSize();
    const prepared = publication.prepare(volume); await publication.create(directory, prepared); file.failFlush = true;
    assert.throws(() => publication.publish(prepared, () => {}, () => assert.fail()), PublicationUncertain);
    assert.throws(() => publication.prepare(volume), /stopped/); publication.close(); file.failFlush = false;
    file.truncate(committed);
    const reopened = (await fixture(file)).publication;
    assert.equal(reopened.current(volume), initial.generation); reopened.close();
});

test('pointer identities, generations and the 32-volume bound fail explicitly', async () => {
    const { file, ledger, publication, directory } = await fixture();
    for (let number = 1; number <= 32; number++) {
        const prepared = publication.prepare(id(number)); await publication.create(directory, prepared); publication.publish(prepared, () => {}, () => {});
    }
    assert.throws(() => publication.prepare(id(33)), /capacity/);
    assert.throws(() => publication.current(encodeId(new Uint8Array(16).fill(0x00))), /identity/);
    await assert.rejects(publication.file(directory, id(1), id(2), '../index', true), /address/);
    publication.close();
    const raw = await OpfsStore.open(file, ledger, async () => {}), writer = raw.begin(true);
    writer.put(Uint8Array.of(1), new Uint8Array(16).fill(1)); writer.end(true); raw.close();
    await assert.rejects(fixture(file), /capacity|Corrupt/);
});

test('detached copying bounds each read/write and rejects existing, missing or short output', async () => {
    const { publication } = await fixture(), prepared = publication.prepare(id(1)), target = new File();
    const write = target.write.bind(target); let largest = 0, calls = 0;
    target.write = (bytes, options) => { largest = Math.max(largest, bytes.length); calls++; return write(bytes, options); };
    await copyVolumeBacking(prepared, new Blob([new Uint8Array(5 * 65536 + 17)]), target, async () => {});
    assert.equal(largest, 65536); assert.equal(calls, 6);
    await assert.rejects(copyVolumeBacking(prepared, new Blob([]), target, async () => {}), /Invalid detached/);
    const short = new File(); short.write = () => 0;
    await assert.rejects(copyVolumeBacking(prepared, new Blob([Uint8Array.of(1)]), short, async () => {}), /incomplete/);
    const epoch = new VolumeWriteEpoch(); assert.throws(() => epoch.end(), /unbalanced/); epoch.stop();
    assert.throws(() => epoch.capture(), LocalVolumeChanged); publication.close();
});

test('reopen removes only crash-interrupted pending generations and preserves previously published snapshots', async () => {
    const { file, publication, directory } = await fixture(), volume = id(1);
    const initial = publication.prepare(volume); await publication.create(directory, initial);
    publication.publish(initial, () => {}, () => {});
    const current = publication.prepare(volume); await publication.create(directory, current);
    publication.publish(current, () => {}, () => {});
    const interrupted = publication.prepare(volume); await publication.create(directory, interrupted);
    const backing = await publication.file(directory, volume, interrupted.generation, 'index', true);
    assert.equal(backing.name, 'index.kvdb');
    assert.equal(publication.pending(volume), interrupted.generation); publication.close();
    const reopened = (await fixture(file)).publication;
    await reopened.discardPending(directory);
    assert.equal(reopened.pending(volume), null); assert.equal(reopened.current(volume), current.generation);
    assert(directory.entries.has(reopened.directory(volume, initial.generation)), 'A previously published snapshot is retained');
    assert(directory.entries.has(reopened.directory(volume, current.generation)));
    assert(!directory.entries.has(reopened.directory(volume, interrupted.generation)));
    await reopened.discardPending(directory); reopened.close();
});

test('directory collisions and asynchronous validation cannot destroy or publish a generation', async () => {
    const { publication, directory } = await fixture(), volume = id(1), prepared = publication.prepare(volume);
    const collision = publication.directory(volume, prepared.generation);
    await directory.getDirectoryHandle(collision, { create: true });
    await assert.rejects(publication.create(directory, prepared), /already exists/);
    await publication.discard(directory, volume, prepared.generation);
    assert(directory.entries.has(collision)); assert.equal(publication.pending(volume), null);
    const fresh = publication.prepare(volume); await publication.create(directory, fresh);
    assert.throws(() => publication.publish(fresh, async () => {}, () => assert.fail()), /must be synchronous/);
    assert.equal(publication.current(volume), null);
    await publication.discard(directory, volume, fresh.generation); publication.close();
});

test('a concurrent writer invalidating a File snapshot retains the bounded local-cut retry classification', async () => {
    const { publication } = await fixture(), prepared = publication.prepare(id(1));
    const source = { size: 1, slice: () => ({ async arrayBuffer() {
        prepared.epoch.begin(); prepared.epoch.end(); throw new DOMException('File changed', 'NotReadableError');
    } }) };
    await assert.rejects(copyVolumeBacking(prepared, source, new File(), async () => {}), LocalVolumeChanged);
    publication.close();
});

test('write epoch admission reserves completion steps for every concurrent writer near exhaustion', () => {
    const epoch = new VolumeWriteEpoch(); epoch.revision = Number.MAX_SAFE_INTEGER - 3;
    epoch.begin(); assert.throws(() => epoch.begin(), /exhausted/); epoch.end();
    assert.equal(epoch.capture(), Number.MAX_SAFE_INTEGER - 1);
    assert.throws(() => epoch.begin(), /exhausted/);
    const concurrent = new VolumeWriteEpoch(); concurrent.revision = Number.MAX_SAFE_INTEGER - 4;
    concurrent.begin(); concurrent.begin(); concurrent.end(); concurrent.end();
    assert.equal(concurrent.capture(), Number.MAX_SAFE_INTEGER);
    assert.throws(() => concurrent.begin(), /exhausted/);
});
