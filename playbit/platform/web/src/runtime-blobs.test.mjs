import { encodeId } from './identifier.ts';
import { fixture, ownedFiles } from '../tests/file-io-fixture.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { DiskFile } from '../tests/opfs-fixture.mjs';
const turn = () => new Promise(resolve => setImmediate(resolve));
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
test('retained blob recovery verifies the envelope and persists shared content before entry publication', async () => {
    const f = await fixture(), content = new Uint8Array(130001).fill(37);
    let source;
    try {
        const reference = f.seed(f.file(4), content);
        await f.remote(f.file(4), content);
        const cache = path.join(f.location, `blob-${reference.digest}`);
        fs.unlinkSync(cache);
        const retained = path.join(f.location, 'retained.blob'); fs.writeFileSync(retained, content);
        source = new DiskFile(retained, false);
        const fresh = { ...reference, entry: encodeId(new Uint8Array(15).fill(0x05)), revision: encodeId(new Uint8Array(15).fill(0x06)) };
        let checks = 0;
        await f.blobs.restore(source, fresh, () => { checks++; }, write => write());
        assert(checks >= 3); assert.deepEqual(fs.readFileSync(cache), Buffer.from(content));
        assert.equal(f.current.has(fresh.entry), false, 'Content preparation does not publish an entry');
        assert.deepEqual(fs.readFileSync(retained), Buffer.from(content));
        await assert.rejects(f.blobs.restore(source, { ...fresh, encrypted_key: '00'.repeat(32) }, () => {}, write => write()), /envelope differs/);
        await assert.rejects(f.blobs.restore(source, fresh, () => { throw new Error('Authority changed'); }, write => write()), /Authority changed/);
        f.interrupt(true); fs.unlinkSync(cache);
        await assert.rejects(f.blobs.restore(source, fresh, () => {}, write => write()), /publication interruption/);
        assert.equal(fs.existsSync(cache), false);
        assert.deepEqual(fs.readFileSync(retained), Buffer.from(content));
    } finally { source?.close(); f.close(); }
});
test('selected reads deduplicate asynchronous content verification and read at most two blocks', async () => {
    // Two selected identities share one pinned cache file while verification is suspended.
    const f = await fixture();
    try {
        const content = new Uint8Array(8 * 1024 * 1024).fill(42);
        const a = { ...f.seed(f.file(1), content), offset: 65535, limit: 65536 };
        const b = { ...f.seed(f.file(2), content), limit: 1 }, hold = f.hold();
        const reads = [f.blobs.readVersion(a), f.blobs.readVersion(b)];
        await turn(); assert.equal(f.handles.length, 1); hold.resolve();
        const [first, second] = await Promise.all(reads);
        assert.equal(first.status, 0); assert.deepEqual(first.bytes, content.subarray(65535, 131071));
        assert.equal(second.status, 0); assert(f.handles[0].maxRead <= 65536);
    } finally { f.close(); }
});
test('withdrawal during preparation and every retained read uses current authority', async () => {
    const f = await fixture();
    try {
        const reference = { ...f.seed(f.file(1), new Uint8Array(70000).fill(4)), limit: 4 }, hold = f.hold();
        const read = f.blobs.readVersion(reference);
        await turn(); f.authority(false); hold.resolve();
        assert.equal((await read).status, -2); assert.equal(f.handles[0].fd, undefined);
        f.authority(true); assert.equal((await f.blobs.readVersion(reference)).status, -2);
        f.blobs.retry(); assert.equal((await f.blobs.readVersion(reference)).status, 0);
        f.authority(false); assert.equal((await f.blobs.readVersion(reference)).status, -2);
    } finally { f.close(); }
});
test('corruption invalidates all shared leases and explicit retry refetches the exact managed version', async () => {
    // Corrupt a verified file on disk, retain the failure, then retry through authenticated transfer.
    const f = await fixture();
    try {
        const content = new Uint8Array(70000).fill(5), entry = f.file(1);
        const reference = f.seed(entry, content), b = { ...f.seed(f.file(2), content), limit: 4 };
        const calls = await f.remote(entry, content), a = { ...reference, limit: 4 };
        assert.equal((await f.blobs.readVersion(a)).status, 0);
        assert.equal((await f.blobs.readVersion(b)).status, 0);
        const filename = path.join(f.location, `blob-${reference.digest}`);
        const corrupt = content.slice(); corrupt[1] = 9; fs.writeFileSync(filename, corrupt);
        assert.equal((await f.blobs.readVersion(a)).status, -5);
        assert.equal((await f.blobs.readVersion(b)).status, -5);
        assert.deepEqual(calls, [], 'Ordinary reads never retry a failed pinned file');
        f.blobs.retry();
        assert.deepEqual((await f.blobs.readVersion(a)).bytes, content.subarray(0, 4));
        assert.equal((await f.blobs.readVersion(b)).status, 0);
        assert.deepEqual(fs.readFileSync(filename), Buffer.from(content));
        assert.equal(calls.filter(method => method === 'streamResponse').length, 2);
        assert.equal(calls.at(-1), 'streamClose');
    } finally { f.close(); }
});
test('bad receipts and interrupted replacement retain the corrupt cache until explicit retry', async () => {
    for (const failure of ['receipt', 'publication']) {
        const f = await fixture();
        try {
            const entry = f.file(1), content = new Uint8Array(70000).fill(8), reference = f.seed(entry, content);
            const calls = await f.remote(entry, content, { wrongReceipt: failure === 'receipt' });
            if (failure === 'publication') f.interrupt(true);
            const selected = { ...reference, limit: 1 }, filename = path.join(f.location, `blob-${reference.digest}`);
            fs.writeFileSync(filename, Uint8Array.of(3));
            assert.equal((await f.blobs.readVersion(selected)).status, -5);
            assert.deepEqual(fs.readFileSync(filename), Buffer.from([3]));
            const count = calls.length;
            assert.equal((await f.blobs.readVersion(selected)).status, -5); assert.equal(calls.length, count);
            assert.deepEqual(fs.readdirSync(f.location), [`blob-${reference.digest}`]);
        } finally { f.close(); }
    }
});
test('withdrawal during managed refetch cannot replace retained bytes', async () => {
    const f = await fixture();
    try {
        const entry = f.file(1), content = new Uint8Array(70000).fill(4), reference = f.seed(entry, content);
        const pause = deferred(), entered = deferred(); await f.remote(entry, content, { pause, entered });
        const filename = path.join(f.location, `blob-${reference.digest}`); fs.writeFileSync(filename, Uint8Array.of(2));
        const read = f.blobs.readVersion({ ...reference, limit: 1 }); await entered.promise;
        f.authority(false); pause.resolve(); assert.equal((await read).status, -2);
        assert.deepEqual(fs.readFileSync(filename), Buffer.from([2]));
        assert.deepEqual(fs.readdirSync(f.location), [`blob-${reference.digest}`]);
    } finally { f.close(); }
});
test('bulk preparation admits two transfers and preserves ready entries on exhaustion', async () => {
    const f = await fixture();
    try {
        const references = [1, 2, 3].map(byte => ({ ...f.seed(f.file(byte), Uint8Array.of(byte)), limit: 1 }));
        const hold = f.hold(), reads = references.map(reference => f.blobs.readVersion(reference));
        assert.equal((await reads[2]).status, -9); await turn(); assert.equal(f.handles.length, 2);
        hold.resolve(); assert((await Promise.all(reads.slice(0, 2))).every(result => result.status === 0));
        assert.equal((await f.blobs.readVersion(references[0])).status, 0);
        assert.equal((await f.blobs.readVersion(references[2])).status, -9);
    } finally { f.close(); }
});
test('interrupted owned publication retains complete recovery and a new writer can succeed', async () => {
    const f = await fixture(), api = ownedFiles(f);
    try {
        const content = new Uint8Array(100000).fill(6), writer = await api.open('interrupted');
        await api.run(writer, 9, content); f.interrupt(true);
        const closed = await api.run(writer, 25);
        assert.equal(closed.result.Close.error, -5); assert.equal(closed.result.Close.publication, 0);
        assert(closed.result.Close.recovery > 0);
        assert.deepEqual((await api.run(closed.file, 8, new Uint8Array(), 0, 65535n, 4)).result.Read.bytes, content.subarray(65535, 65539));
        assert(!fs.readdirSync(f.location).some(name => name.startsWith('blob-')));
        await api.run(closed.file, 25); await api.backend.cleanup(); f.interrupt(false);
        const retry = await api.open('interrupted'); await api.run(retry, 9, content);
        assert.equal((await api.run(retry, 25)).result.Close.publication, 1);
        assert(f.handles.every(file => file.maxRead <= 65536 && file.maxWrite <= 65552));
    } finally { await api.backend.cleanup(); f.close(); }
});
test('trusted root seed publishes only after preparation and leaves no partial publication', async () => {
    const f = await fixture();
    try {
        const content = new Uint8Array(70000).fill(6), reference = f.seed(f.file(1), content);
        f.current.clear(); fs.unlinkSync(path.join(f.location, `blob-${reference.digest}`)); f.authority(false);
        const published = [], publish = prepared => {
            assert.notEqual(prepared.encrypted_key, encodeId(new Uint8Array(32).fill(0x11)));
            assert.deepEqual(fs.readFileSync(path.join(f.location, `blob-${prepared.digest}`)), Buffer.from(content));
            published.push(prepared); return 0;
        };
        f.interrupt(true); assert.equal(await f.blobs.initialize({ ...reference }, content, publish), -5);
        assert.deepEqual(published, []); assert.deepEqual(fs.readdirSync(f.location), []);
        f.interrupt(false); assert.equal(await f.blobs.initialize({ ...reference }, content, publish), 0);
        assert.equal(published.length, 1);
    } finally { f.close(); }
});
test('supplied content uploads with Write alone and rechecks authority before acknowledgment', async () => {
    const f = await fixture();
    try {
        const entry = f.file(1), content = new Uint8Array(70000).fill(7), reference = f.seed(entry, content);
        await f.remote(entry, content); f.authority(false, true);
        assert.equal((await f.blobs.readVersion({ ...reference, limit: 1 })).status, -2);
        const calls = [], received = []; let responses = 0, withdraw;
        f.services.ap = async (method, _handle, bytes) => {
            calls.push(method);
            if (method === 'begin') return 1;
            if (method === 'streamWrite') received.push(bytes.slice());
            if (method === 'streamResponse') {
                if (responses++ === 0) return { data: null };
                withdraw?.();
                return { data: { content_length: 9 + content.length + 32, encrypted_key: encodeId(Buffer.from(reference.encrypted_key, 'hex')) } };
            }
        };
        await f.blobs.upload([reference.volume]); assert.equal(f.acknowledgments.length, 1);
        assert(!calls.includes('streamRead')); assert.equal(calls.at(-1), 'streamClose');
        assert.equal(Buffer.concat(received).length, 9 + content.length + 32);
        for (const withdrawal of [() => f.authority(false, false), () => f.service(false)]) {
            f.authority(false, true); f.service(true); responses = 0; withdraw = withdrawal;
            await assert.rejects(f.blobs.upload([reference.volume]), error => error.code === -2);
            assert.equal(f.acknowledgments.length, 1, 'Withdrawal cannot retire pending content');
            assert.equal(calls.at(-1), 'streamClose');
        }
    } finally { f.close(); }
});
test('closing during preparation retires the read and suppresses scene work', async () => {
    const f = await fixture(), reference = { ...f.seed(f.file(1), new Uint8Array(70000)), limit: 1 }, hold = f.hold();
    const read = f.blobs.readVersion(reference);
    await turn(); f.blobs.close(); hold.resolve(); assert.equal((await read).status, -5);
    assert.equal(f.changes.length, 0); f.close();
});

test('owned File reads hydrate the selected revision after replacement and recheck authority', async () => {
    // Hold real content verification while the current entry changes; the retained view stays exact.
    const f = await fixture();
    try {
        const file = f.file(31), bytes = new Uint8Array(70000).fill(31);
        const selected = { ...f.seed(file, bytes), offset: 65535, limit: 65536 };
        const hold = f.hold(), read = f.blobs.readVersion(selected);
        await turn(); f.seed(file, Uint8Array.of(99)); hold.resolve();
        const result = await read;
        assert.equal(result.status, 0); assert.equal(result.total, 70000n);
        assert.deepEqual(result.bytes, bytes.subarray(65535));
        assert(f.handles.every(file => file.maxRead <= 65536));
        for (const offset of [70000, 70001, Number.MAX_SAFE_INTEGER]) {
            assert.deepEqual((await f.blobs.readVersion({ ...selected, offset })).bytes, new Uint8Array());
        }
        assert.equal((await f.blobs.readVersion({ ...selected, limit: 65537 })).status, -3);
        f.authority(false);
        assert.equal((await f.blobs.readVersion(selected)).status, -2);
        assert.equal((await f.blobs.readVersion({ ...selected, offset: 70001 })).status, -2);
    } finally { f.close(); }
});

test('owned web writer publishes flushed encrypted content and immutable readers return the selected bytes', async () => {
    // Exercise real disk-backed OPFS handles and encryption through compiled Rust publication.
    const f = await fixture(), api = ownedFiles(f);
    try {
        const writer = await api.open('owned/note');
        await api.run(writer, 9, Uint8Array.of(7, 8), 0, 65537n);
        await api.run(writer, 9, Uint8Array.of(2, 3), 0, 1n);
        assert.equal((await api.run(writer, 24)).result.Status.size, 65539);
        const close = (await api.run(writer, 25)).result;
        assert.deepEqual(close, { Close: { error: 0, publication: 1, recovery: null } });
        await api.backend.cleanup();
        assert(!fs.readdirSync(f.location).some(name => name.startsWith('file-private-')));
        const reader = await api.open('owned/note', 1);
        const first = (await api.run(reader, 8, new Uint8Array(), 0, 0n, 4)).result.Read.bytes;
        assert.deepEqual(first, Uint8Array.of(0, 2, 3, 0));
        const ending = (await api.run(reader, 8, new Uint8Array(), 0, 65536n, 16)).result.Read.bytes;
        assert.deepEqual(ending, Uint8Array.of(0, 7, 8));
        const replacement = await api.open('owned/note'); await api.run(replacement, 9, Uint8Array.of(99));
        assert.equal((await api.run(replacement, 25)).result.Close.publication, 1);
        await api.backend.cleanup();
        assert.deepEqual((await api.run(reader, 8, new Uint8Array(), 0, 1n, 2)).result.Read.bytes, Uint8Array.of(2, 3));
        assert.deepEqual((await api.run(reader, 8, new Uint8Array(), 0, 70000n, 1)).result.Read.bytes, new Uint8Array());
        await api.run(reader, 25);
    } finally { await api.backend.cleanup(); f.close(); }
});

test('exclusive publication failure retains complete spool recovery and cleanup holds credit through disk failure', async () => {
    // A competing publication returns an immutable recovery; failed deletion cannot release its credit.
    const f = await fixture(), api = ownedFiles(f);
    try {
        const a = await api.open('exclusive', 12), b = await api.open('exclusive', 12);
        await api.run(a, 9, Uint8Array.of(1)); await api.run(b, 9, Uint8Array.of(2, 3));
        assert.equal((await api.run(a, 25)).result.Close.publication, 1); await api.backend.cleanup();
        const closed = await api.run(b, 25);
        assert.equal(closed.result.Close.error, -10); assert.equal(closed.result.Close.publication, 0);
        assert(closed.result.Close.recovery > 0);
        assert.deepEqual((await api.run(closed.file, 8, new Uint8Array(), 0, 0n, 8)).result.Read.bytes, Uint8Array.of(2, 3));
        const read = api.begin(closed.file, 8, new Uint8Array(), 0, 0n, 2);
        assert.equal(api.host.pb_web_file_dispose(...closed.file), 0);
        await api.backend.cleanup(); assert.equal(api.host.pb_web_file_cleanup(), 0);
        const result = await api.backend.execute(read, new Uint8Array()); api.finish(read, result);
        const directory = f.services.directory(), remove = directory.removeEntry;
        directory.removeEntry = async name => { if (name.startsWith('file-private-')) throw new Error('Injected cleanup failure'); return remove(name); };
        await assert.rejects(api.backend.cleanup(), /cleanup failure/);
        assert.equal(api.host.pb_web_file_cleanup(), 8, 'Physical cleanup still owns the stage credit');
        directory.removeEntry = remove; await api.backend.cleanup();
        assert.equal(api.host.pb_web_file_cleanup(), 0);
        assert(!fs.readdirSync(f.location).some(name => name.startsWith('file-private-')));
    } finally { await api.backend.cleanup(); f.close(); }
});

test('notification failure preserves the known close receipt and a retryable publication notice', async () => {
    // Publication is terminal before best-effort scheduling; notification retries never publish again.
    const f = await fixture(); let fail = true, notices = 0;
    const api = ownedFiles(f, () => { notices++; if (fail) throw new Error('Injected notification failure'); });
    try {
        const writer = await api.open('notice'); await api.run(writer, 9, Uint8Array.of(1));
        const closed = await api.run(writer, 25);
        assert.deepEqual(closed.result, { Close: { error: 0, publication: 1, recovery: null } });
        assert.throws(() => api.backend.flushPublication(), /notification failure/);
        fail = false; api.backend.flushPublication(); api.backend.flushPublication(); assert.equal(notices, 2);
        const reader = await api.open('notice', 1);
        assert.deepEqual((await api.run(reader, 8, new Uint8Array(), 0, 0n, 1)).result.Read.bytes, Uint8Array.of(1));
        await api.run(reader, 25);
    } finally { await api.backend.cleanup(); f.close(); }
});

test('failed encrypted-stage deletion retains bounded ownership and blocks preparation until cleanup succeeds', async () => {
    // Fail live cleanup, then try another complete writer; no further encrypted temporaries may accumulate.
    const f = await fixture(), api = ownedFiles(f), directory = f.services.directory(), remove = directory.removeEntry;
    try {
        const writer = await api.open('cleanup-a'); await api.run(writer, 9, Uint8Array.of(1));
        directory.removeEntry = async name => { if (name.startsWith('blob-stage-')) throw new Error('Injected encrypted cleanup failure'); return remove(name); };
        const first = await api.run(writer, 25);
        assert.equal(first.result.Close.error, -5); assert.equal(first.result.Close.publication, 0);
        const retained = fs.readdirSync(f.location).filter(name => name.startsWith('blob-stage-'));
        assert(retained.length > 0 && retained.length <= 6);
        const next = await api.open('cleanup-b'); await api.run(next, 9, Uint8Array.of(2));
        const second = await api.run(next, 25); assert.equal(second.result.Close.error, -5);
        assert.deepEqual(fs.readdirSync(f.location).filter(name => name.startsWith('blob-stage-')), retained);
        await assert.rejects(f.blobs.cleanup(), /encrypted cleanup failure/);
        directory.removeEntry = remove; await f.blobs.cleanup();
        assert(!fs.readdirSync(f.location).some(name => name.startsWith('blob-stage-')));
        await api.run(first.file, 25); await api.run(second.file, 25); await api.backend.cleanup();
        const retry = await api.open('cleanup-c'); await api.run(retry, 9, Uint8Array.of(3));
        assert.equal((await api.run(retry, 25)).result.Close.publication, 1);
    } finally { directory.removeEntry = remove; await f.blobs.cleanup(); await api.backend.cleanup(); f.close(); }
});

test('owned close preserves blob preparation capacity errors and complete recovery', async () => {
    // Occupy both preparation slots with retained reads; the close must report the actual admission error.
    const f = await fixture(), api = ownedFiles(f);
    let hold;
    try {
        const writer = await api.open('capacity'); await api.run(writer, 9, Uint8Array.of(9));
        const a = { ...f.seed(f.file(51), new Uint8Array(70000).fill(51)), limit: 1 };
        const b = { ...f.seed(f.file(52), new Uint8Array(70000).fill(52)), limit: 1 };
        hold = f.hold();
        const reads = [f.blobs.readVersion(a), f.blobs.readVersion(b)]; await turn();
        const closed = await api.run(writer, 25);
        assert.equal(closed.result.Close.error, -9); assert.equal(closed.result.Close.publication, 0);
        assert(closed.result.Close.recovery > 0);
        hold.resolve(); assert((await Promise.all(reads)).every(result => result.status === 0));
        await api.run(closed.file, 25);
    } finally { hold?.resolve(); await api.backend.cleanup(); f.close(); }
});

test('execution startup removes unpublished private and encrypted crash spools while retaining published content', async () => {
    // The exclusive execution directory contains crash remnants, never resumable File capabilities.
    const f = await fixture(), api = ownedFiles(f);
    try {
        const reference = f.seed(f.file(61), Uint8Array.of(1, 2));
        for (const name of ['file-private-3.tmp', 'blob-stage-1.tmp']) fs.writeFileSync(path.join(f.location, name), Uint8Array.of(7));
        await api.backend.initialize();
        assert.deepEqual(fs.readdirSync(f.location), [`blob-${reference.digest}`]);
    } finally { await api.backend.cleanup(); f.close(); }
});


test('maximum owned File extent closes and reopens with bounded OPFS requests', async () => {
    // A one-byte tail write creates the full zero-filled extent; close must
    // encrypt its overhead without failing a previously admitted File size.
    const f = await fixture(), api = ownedFiles(f), size = 64 * 1024 * 1024;
    try {
        const writer = await api.open('maximum');
        const data = Uint8Array.of(17);
        const pointer = api.host.pb_input(9); new Uint8Array(api.host.memory.buffer, pointer + 8, 1).set(data);
        assert.equal(api.host.pb_web_file_begin(...writer, 9, BigInt(size), 1, 0), -9n);
        await api.run(writer, 9, data, 0, BigInt(size - 1));
        assert.equal((await api.run(writer, 24)).result.Status.size, size);
        assert.equal((await api.run(writer, 25)).result.Close.publication, 1);
        await api.backend.cleanup();
        const reader = await api.open('maximum', 1);
        const tail = await api.run(reader, 8, new Uint8Array(), 0, BigInt(size - 2), 2);
        assert.deepEqual(tail.result.Read.bytes, Uint8Array.of(0, 17));
        assert.equal((await api.run(reader, 24)).result.Status.size, size);
        await api.run(reader, 25);
        assert(f.handles.every(file => file.maxRead <= 65536 && file.maxWrite <= 65552));
    } finally { await api.backend.cleanup(); f.close(); }
});
