import test from 'node:test';
import assert from 'node:assert/strict';
import { downloadPackage, fetchPackage, writePackage, MAX_PACKAGE_BYTES, PACKAGE_CHUNK_BYTES } from './package-stream.ts';

const operation = { op: 'get_blob', operation_id: 'fixture', hash: 'fixture' };
function transfer(length, options = {}) {
    const state = { reads: 0, replies: 0, closed: 0 };
    const bytes = new Uint8Array(PACKAGE_CHUNK_BYTES);
    const transport = {
        async begin() { return 17; },
        async streamFinish(handle) { assert.equal(handle, 17); },
        async streamResponse() {
            if (++state.replies === 1) return { data: { content_length: length } };
            if (options.rejectFinal) throw new Error('Rejected final acknowledgment');
            return {};
        },
        async streamRead(handle, maximum) {
            assert.equal(handle, 17); assert.equal(maximum, PACKAGE_CHUNK_BYTES);
            options.beforeRead?.(state.reads);
            state.reads++;
            return options.chunk ?? bytes;
        },
        async streamClose() { state.closed++; },
    };
    return { state, transport };
}

test('512 MiB transfers retain one chunk and wait for OPFS before reading again', async () => {
    let written = 0, yields = 0, flushed = 0, release;
    const held = new Promise(resolve => { release = resolve; });
    const { state, transport } = transfer(MAX_PACKAGE_BYTES, {
        beforeRead(reads) { assert.equal(written, reads * PACKAGE_CHUNK_BYTES); },
    });
    const file = {
        truncate(length) { assert.equal(length, 0); },
        write(bytes, { at }) {
            assert.equal(at, written); assert(bytes.length <= PACKAGE_CHUNK_BYTES);
            written += bytes.length; return bytes.length;
        },
        flush() { assert.equal(state.replies, 2, 'final AP acknowledgment precedes verification'); flushed++; },
    };
    const work = writePackage(file, downloadPackage(transport, operation), async () => {
        if (++yields === 1) await held;
    });
    await new Promise(setImmediate);
    assert.equal(state.reads, 1); assert.equal(written, PACKAGE_CHUNK_BYTES);
    release(); await work;
    assert.equal(written, MAX_PACKAGE_BYTES); assert.equal(flushed, 1); assert.equal(state.closed, 1);
});

test('bad lengths, oversized frames and failed completion close the download without flushing', async () => {
    for (const [length, options] of [
        [MAX_PACKAGE_BYTES + 1, {}],
        [PACKAGE_CHUNK_BYTES, { chunk: new Uint8Array() }],
        [PACKAGE_CHUNK_BYTES, { chunk: new Uint8Array(PACKAGE_CHUNK_BYTES + 1) }],
        [1, { chunk: new Uint8Array(2) }],
        [PACKAGE_CHUNK_BYTES, { rejectFinal: true }],
    ]) {
        const { state, transport } = transfer(length, options);
        const file = { truncate() {}, write(bytes) { return bytes.length; }, flush() { assert.fail('failed transfer flushed'); } };
        await assert.rejects(writePackage(file, downloadPackage(transport, operation), async () => {}));
        assert.equal(state.closed, 1);
        if (length > MAX_PACKAGE_BYTES) assert.equal(state.reads, 0);
    }
});

test('HTTP examples use a bounded BYOB reader and release the fetch on storage failure', async () => {
    const originalFetch = globalThis.fetch;
    let received = 0, cancelled = 0, signal;
    globalThis.fetch = async (url, options) => {
        assert.equal(url, './fixture.pbapp'); signal = options.signal;
        const body = new ReadableStream({
            type: 'bytes',
            pull(controller) {
                const request = controller.byobRequest;
                assert(request); assert(request.view.byteLength <= PACKAGE_CHUNK_BYTES);
                request.view.fill(7); received += request.view.byteLength;
                request.respond(request.view.byteLength);
            },
            cancel() { cancelled++; },
        });
        return { ok: true, body };
    };
    try {
        let writes = 0;
        const file = { truncate() {}, write(bytes) {
            assert.equal(bytes[0], 7);
            if (++writes === 2) throw Object.assign(new Error('quota'), { name: 'QuotaExceededError' });
            return bytes.length;
        }, flush() { assert.fail('failed transfer flushed'); } };
        await assert.rejects(writePackage(file, fetchPackage('./fixture.pbapp'), async () => {}), /Not enough web browser storage/);
        assert.equal(received, 2 * PACKAGE_CHUNK_BYTES); assert.equal(cancelled, 1); assert.equal(signal.aborted, true);
    } finally { globalThis.fetch = originalFetch; }
});

test('streamed writes enforce the aggregate cap and reject short OPFS writes', async () => {
    const chunk = new Uint8Array(PACKAGE_CHUNK_BYTES);
    async function* tooLarge() {
        for (let offset = 0; offset <= MAX_PACKAGE_BYTES; offset += chunk.length) yield chunk;
    }
    let written = 0;
    const file = { truncate() {}, write(bytes) { written += bytes.length; return bytes.length; }, flush() { assert.fail('oversized transfer flushed'); } };
    await assert.rejects(writePackage(file, tooLarge(), async () => {}), /exceeds download limit/);
    assert.equal(written, MAX_PACKAGE_BYTES);
    async function* one() { yield chunk; }
    await assert.rejects(writePackage({ ...file, write() { return 1; } }, one(), async () => {}), /Incomplete app write/);
});
