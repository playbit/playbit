import assert from 'node:assert/strict';
const empty = new Uint8Array(), encoder = new TextEncoder(), decoder = new TextDecoder();
export function files(target) {
    const host = target.host.exports;
    const stage = input => { const pointer = host.pb_input(input.length); new Uint8Array(host.memory.buffer, pointer, input.length).set(input); };
    const output = length => new Uint8Array(host.memory.buffer, host.pb_output(), length).slice();
    const roots = () => {
        const status = host.pb_web_volumes();
        assert.equal(status, 0, decoder.decode(output(host.pb_error())));
        const revision = new DataView(host.memory.buffer).getBigUint64(host.pb_web_result() + 8, true);
        const count = new DataView(host.memory.buffer).getUint32(host.pb_web_result() + 24, true);
        assert.equal(host.pb_web_volumes_acquire(revision), 0);
        const result = new DataView(output(count * 8).buffer);
        return Array.from({ length: count }, (_, i) => [result.getUint32(i * 8 + 4, true), result.getUint32(i * 8, true)]);
    };
    const begin = (file, kind, { offset = 0n, flags = 0, data = empty, length = data.length } = {}) => {
        const input = new Uint8Array(8 + data.length); input.set(data, 8); stage(input);
        const handle = host.pb_web_file_begin(...file, kind, offset, length, flags);
        return handle < 0n ? Number(handle) : handle;
    };
    const execute = token => {
        const length = host.pb_web_file_execute(token);
        return length < 0 ? length : JSON.parse(decoder.decode(output(length)));
    };
    const result = token => {
        const length = host.pb_web_file_result(token);
        return length < 0 ? length : JSON.parse(decoder.decode(output(length)));
    };
    const finish = (token, value) => {
        const engine = value.File?.handle ?? value.List?.handle ?? value.Close?.recovery ?? 0;
        assert.equal(host.pb_web_file_finish(token, BigInt(engine), Number(Boolean(value.Entry))), 0);
        const record = new DataView(output(8).buffer), handle = [record.getUint32(4, true), record.getUint32(0, true)];
        assert.equal(host.pb_web_file_release(token), 0);
        return handle;
    };
    const done = token => {
        const task = execute(token); assert.equal(task.kind, 'done');
        assert.equal(host.pb_web_file_qualify(token), 0);
        const value = result(token); return { value, file: finish(token, value) };
    };
    return { host, roots, begin, execute, result, finish, done, stage };
}

// Metadata tests report physical I/O completion explicitly. Disk-backed tests
// use FileBackend instead; this fixture never claims to persist actual bytes.
export function prepareWriter(target, path, content) {
    const api = files(target), [root] = api.roots();
    const token = api.begin(root, 23, { flags: 4, data: encoder.encode(path) });
    assert.equal(api.execute(token).kind, 'create_writer');
    assert.equal(api.host.pb_web_file_writer_ready(token), 0);
    assert.equal(api.host.pb_web_file_qualify(token), 0);
    const writer = api.finish(token, api.result(token));
    const write = api.begin(writer, 9, { data: content });
    assert.equal(api.execute(write).kind, 'write');
    assert.equal(api.host.pb_web_file_write_complete(write, 0), 0);
    assert.equal(api.host.pb_web_file_release(write), 0);
    const close = api.begin(writer, 25);
    assert.equal(api.execute(close).kind, 'close');
    const digest = target.raw('pb_blob_digest', content, content.length);
    assert(digest instanceof Uint8Array);
    api.stage(Uint8Array.from([...Array(8).fill(0), ...digest]));
    const length = api.host.pb_web_file_seal(close, BigInt(content.length)); assert(length > 0);
    const reference = JSON.parse(decoder.decode(new Uint8Array(api.host.memory.buffer, api.host.pb_output(), length)));
    return { reference, publish() {
        const bytes = encoder.encode(JSON.stringify(reference));
        api.stage(Uint8Array.from([...Array(8).fill(0), ...bytes]));
        assert.equal(api.host.pb_web_file_publish(close, bytes.length), 0);
        assert.deepEqual(api.result(close), { Close: { error: 0, publication: 1, recovery: null } });
        api.finish(close, api.result(close));
        assert.equal(api.host.pb_web_file_dispose(...root), 0);
        // The metadata fixture has no physical spool to remove.
        const count = api.host.pb_web_file_cleanup(); assert(count >= 0);
        const view = new DataView(api.host.memory.buffer, api.host.pb_output(), count);
        const retired = Array.from({ length: count / 8 }, (_, i) => view.getBigUint64(i * 8, true));
        for (const engine of retired) assert.equal(api.host.pb_web_file_cleaned(engine), 0);
    } };
}
export function selectedReference(target, path) {
    const api = files(target), [root] = api.roots();
    const reader = api.done(api.begin(root, 23, { flags: 1, data: encoder.encode(path) })).file;
    const read = api.begin(reader, 8, { length: 1 }), task = api.execute(read);
    assert.equal(task.kind, 'read');
    assert.equal(api.host.pb_web_file_io_failed(read, -5), 0);
    assert.equal(api.host.pb_web_file_release(read), 0);
    for (const file of [reader, root]) assert.equal(api.host.pb_web_file_dispose(...file), 0);
    return task.reference;
}
