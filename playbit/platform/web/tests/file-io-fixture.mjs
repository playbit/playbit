import { encodeId, decodeId } from '../src/identifier.ts';
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { RuntimeBlobs } from '../src/runtime-blobs.ts';
import { FileBackend } from '../src/file-backend.ts';
import { encryptBlob } from '../src/blob.ts';
import { DiskFile } from './opfs-fixture.mjs';
import { bootstrap, createHost } from './host-fixture.mjs';
const turn = () => new Promise(resolve => setImmediate(resolve));
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
const denied = () => { throw Object.assign(new Error('Authority withdrawn'), { code: -2 }); };
export const cacheService = runtime => request => {
    const bytes = new TextEncoder().encode(JSON.stringify(request));
    const output = runtime.raw('pb_blob_cache', bytes, bytes.length);
    if (typeof output === 'number') throw Object.assign(new Error('Cache operation failed'), { code: output });
    return output.length ? JSON.parse(new TextDecoder().decode(output)) : undefined;
};
export async function fixture(options = {}) {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x10)));
    if (options.connected) { profile.root.share_key_with_service = true; }
    const host = await createHost(profile);
    const location = fs.mkdtempSync(path.join(os.tmpdir(), 'playbit-blob-leases-'));
    const handles = [], locked = new Set(), current = new Map(), changes = [], acknowledgments = [];
    let serial = 1, readable = true, writable = true, serviceReadable = true, failMove = false, holdYield;
    const hash = () => {
        const id = host.host.exports.pb_blob_hash_begin(); assert(id > 0); let active = true;
        return { update(bytes) { assert(bytes.length <= 256 * 1024); assert.equal(host.raw('pb_blob_hash_update', bytes, id, bytes.length).length, 0); },
            finish() { const digest = host.raw('pb_blob_hash_finish', new Uint8Array(), id); assert(digest instanceof Uint8Array); active = false; return Buffer.from(digest).toString('hex'); },
            close() { if (active) { assert.equal(host.host.exports.pb_blob_hash_close(id), 0); active = false; } } };
    };
    const digest = content => { const h = hash(); for (let offset = 0; offset < content.length; offset += 65536) h.update(content.subarray(offset, offset + 65536)); return h.finish(); };
    const directory = {
        async *[Symbol.asyncIterator]() { for (const name of fs.readdirSync(location)) yield [name, {}]; },
        async getFileHandle(name, options) {
            const filename = path.join(location, name);
            if (!fs.existsSync(filename)) {
                if (!options?.create) throw Object.assign(new Error('missing'), { name: 'NotFoundError' });
                fs.closeSync(fs.openSync(filename, 'wx', 0o600));
            }
            return {
                async createSyncAccessHandle() {
                    assert(!locked.has(name), `Duplicate OPFS handle for ${name}`); locked.add(name);
                    const file = new DiskFile(filename, false), close = file.close.bind(file);
                    file.close = () => { assert(file.fd !== undefined, 'A handle closes exactly once'); close(); locked.delete(name); };
                    handles.push(file); return file;
                },
                async move(target) { assert(!locked.has(name)); if (failMove) throw new Error('Injected publication interruption'); fs.renameSync(filename, path.join(location, target)); },
            };
        },
        async removeEntry(name) { try { fs.unlinkSync(path.join(location, name)); } catch (error) { if (error.code === 'ENOENT') error.name = 'NotFoundError'; throw error; } },
    };
    const services = {
        cache: cacheService(host), failure() {},
        directory: () => directory,
        key(reference) {
            const material = new Uint8Array(80); material.fill(9, 0, 32); material.set(Buffer.from(decodeId(reference.volume)), 32); material.set(Buffer.from(reference.digest, 'hex'), 48);
            return host.raw('pb_blob_cipher_key', material);
        }, hash,
        ap: async () => { throw new Error('Network access is forbidden in this local fixture'); },
        operation: (reference, upload) => ({ upload, reference }),
        authorize(reference, write, service) { if ((write ? !writable : !readable) || service && !serviceReadable) denied(); },
        pending: () => [[...current.values()], null], acknowledge: reference => acknowledgments.push(reference),
        changed: async () => { changes.push(1); }, yield: async () => { if (holdYield) await holdYield.promise; else await turn(); }, stop() { holdYield?.resolve(); }, managed: () => false,
    };
    const blobs = new RuntimeBlobs(services);
    const file = byte => new Uint8Array(15).fill(byte);
    const seed = (entry, content) => {
        const reference = { volume: encodeId(new Uint8Array(16).fill(0x10)), entry: encodeId(entry), revision: encodeId(Buffer.from((serial++).toString(16).padStart(30, '0'), 'hex')), len_bytes: content.length, digest: digest(content), encrypted_key: '11'.repeat(32), offset: 0, limit: 0 };
        current.set(reference.entry, reference); fs.writeFileSync(path.join(location, `blob-${reference.digest}`), content); return reference;
    };
    const settled = async () => { for (let i = 0; i < 1000; i++) { await turn(); if (changes.length) return; } assert.fail('Preparation did not finish'); };
    const remote = async (entry, content, options = {}) => {
        const reference = current.get(encodeId(entry));
        const sourceName = path.join(location, 'remote-plain.tmp'), targetName = path.join(location, 'remote-envelope.tmp');
        fs.writeFileSync(sourceName, content);
        const source = new DiskFile(sourceName, false), target = new DiskFile(targetName);
        let envelope;
        try {
            reference.encrypted_key = await encryptBlob(reference, services.key(reference), source, target, hash);
            envelope = fs.readFileSync(targetName);
        } finally { source.close(); target.close(); fs.unlinkSync(sourceName); fs.unlinkSync(targetName); }
        const calls = []; let offset = 0;
        services.managed = () => true;
        services.ap = async method => {
            calls.push(method);
            if (method === 'begin') { offset = 0; return 1; }
            if (method === 'streamResponse') return { data: { content_length: envelope.length,
                encrypted_key: options.wrongReceipt ? encodeId(new Uint8Array(32)) : encodeId(Buffer.from(reference.encrypted_key, 'hex')) } };
            if (method === 'streamRead') {
                if (options.pause) { options.entered.resolve(); await options.pause.promise; }
                const part = envelope.subarray(offset, offset + 65536); offset += part.length; return part;
            }
        };
        return calls;
    };
    return { host, profile, blobs, services, current, changes, acknowledgments, handles, file, seed, settled, location,
        remote,
        hold() { holdYield = deferred(); return holdYield; }, authority(read, write = read) { readable = read; writable = write; }, service(value) { serviceReadable = value; }, interrupt(value) { failMove = value; },
        close() { blobs.close(); assert.equal(locked.size, 0); fs.rmSync(location, { recursive: true, force: true }); } };
}
export function ownedFiles(f, published = () => {}) {
    const host = f.host.host.exports, encoder = new TextEncoder();
    const backend = new FileBackend({ host: () => host, directory: f.services.directory, blobs: f.blobs, hash: f.services.hash,
        step: async callback => callback(), yield: turn, published });
    assert.equal(host.pb_web_volumes(), 0);
    const revision = new DataView(host.memory.buffer).getBigUint64(host.pb_web_result() + 8, true);
    assert.equal(host.pb_web_volumes_acquire(revision), 0);
    const root = [new DataView(host.memory.buffer).getUint32(host.pb_output() + 4, true), new DataView(host.memory.buffer).getUint32(host.pb_output(), true)];
    const begin = (file, kind, data = new Uint8Array(), flags = 0, offset = 0n, length = data.length) => {
        const p = host.pb_input(8 + data.length); new Uint8Array(host.memory.buffer, p + 8, data.length).set(data);
        const token = host.pb_web_file_begin(...file, kind, offset, length, flags); assert(token > 0); return token;
    };
    const finish = (token, result) => {
        const engine = result.File?.handle ?? result.List?.handle ?? result.Close?.recovery ?? 0;
        assert.equal(host.pb_web_file_finish(token, BigInt(engine), Number(Boolean(result.Entry))), 0);
        const view = new DataView(host.memory.buffer, host.pb_output(), 8);
        const file = [view.getUint32(4, true), view.getUint32(0, true)];
        assert.equal(host.pb_web_file_release(token), 0); return file;
    };
    const run = async (file, kind, data = new Uint8Array(), flags = 0, offset = 0n, length = data.length) => {
        const token = begin(file, kind, data, flags, offset, length);
        const result = await backend.execute(token, data);
        return { result, file: finish(token, result) };
    };
    const open = async (path, flags = 4) => (await run(root, 23, encoder.encode(path), flags)).file;
    return { host, backend, begin, finish, run, open, root };
}
