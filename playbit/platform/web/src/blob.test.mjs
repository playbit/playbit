import { encodeId } from './identifier.ts';
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { fixture as diskFixture } from "../tests/opfs-fixture.mjs";
import { blobAAD, blockNonce, envelopeLength, encryptBlob, decryptBlob, transferBlob, MAX_PLAINTEXT, MAX_ENVELOPE } from "./blob.ts";
const fixture = JSON.parse(await readFile(new URL("../../../tests/fixtures/blob-v1.json", import.meta.url)));
const bytes = hex => new Uint8Array(Buffer.from(hex, "hex"));
const sha = bytes => createHash("sha256").update(bytes).digest("hex");
// This oracle recognizes the exact native BLAKE3 vector via SHA-256. Other
// codec cases use SHA-256 as their injected incremental hash. Compiled-host
// tests separately exercise the real BLAKE3 export; no JS hash implementation.
const oracle = new Map([[sha(bytes(fixture.plaintext)), fixture.digest], [sha(bytes(fixture.envelope)), fixture.encrypted]]);
function hash() {
    const value = createHash("sha256"); let closed = false;
    return { update(bytes) { assert(!closed); assert(bytes.length <= 256 * 1024); value.update(bytes); },
        finish() { assert(!closed); closed = true; const digest = value.digest("hex"); return oracle.get(digest) ?? digest; }, close() { closed = true; } };
}
const reference = { volume: fixture.volume, entry: encodeId(new Uint8Array(15).fill(0x11)), revision: encodeId(new Uint8Array(15).fill(0x12)), len_bytes: bytes(fixture.plaintext).length, digest: fixture.digest, encrypted_key: fixture.encrypted };
function files() {
    const disk = diskFixture(), handles = [];
    return { file(content) { const file = disk.file(); handles.push(file); if (content) file.write(content, { at: 0 }); return file; },
        close() { for (const file of handles) file.close(); disk.cleanup(); } };
}
function contents(file) { const value = new Uint8Array(file.getSize()); assert.equal(file.read(value, { at: 0 }), value.length); return value; }
test("WebCrypto matches the native v1 content key, nonce, AAD and envelope", async () => {
    const disk = files();
    try {
        assert.deepEqual(blockNonce(0), bytes(fixture.nonce));
        assert.deepEqual(blobAAD(reference, 0), bytes(fixture.aad));
        const source = disk.file(bytes(fixture.plaintext)), envelope = disk.file(), opened = disk.file();
        assert.equal(await encryptBlob(reference, bytes(fixture.cipher_key), source, envelope, hash), fixture.encrypted);
        assert.deepEqual(contents(envelope), bytes(fixture.envelope));
        await decryptBlob(reference, bytes(fixture.cipher_key), envelope, opened, hash);
        assert.deepEqual(contents(opened), bytes(fixture.plaintext));
    } finally { disk.close(); }
});
test("block boundaries and maximum File content use bounded file reads and writes", async () => {
    const disk = files();
    try {
        for (const length of [0, 1, 15, 16, 17, 65535, 65536, 65537, 8 * 1024 * 1024, MAX_PLAINTEXT]) {
            const content = new Uint8Array(length).fill(42), source = disk.file(content), envelope = disk.file(), opened = disk.file();
            const version = { ...reference, len_bytes: length, digest: oracle.get(sha(content)) ?? sha(content) };
            version.encrypted_key = await encryptBlob(version, bytes(fixture.cipher_key), source, envelope, hash);
            assert.equal(envelope.getSize(), envelopeLength(version));
            await decryptBlob(version, bytes(fixture.cipher_key), envelope, opened, hash);
            assert(source.maxRead <= 65536); assert(envelope.maxRead <= 65552);
            assert(envelope.maxWrite <= 65552); assert(opened.maxWrite <= 65536);
            assert.deepEqual(contents(opened), content);
        }
        assert.equal(envelopeLength({ ...reference, len_bytes: MAX_PLAINTEXT }), MAX_ENVELOPE);
        assert.throws(() => envelopeLength({ ...reference, len_bytes: MAX_PLAINTEXT + 1 }), /limit/);
        assert.throws(() => envelopeLength({ ...reference, len_bytes: Number.MAX_SAFE_INTEGER }), /limit/);
    } finally { disk.close(); }
});
test("every small envelope byte, wrong identity/key, truncation and extension are rejected", async () => {
    const disk = files(), key = bytes(fixture.cipher_key), envelope = bytes(fixture.envelope);
    try {
        for (let index = 0; index < envelope.length; index++) {
            const bad = envelope.slice(); bad[index] ^= 1;
            await assert.rejects(decryptBlob(reference, key, disk.file(bad), disk.file(), hash));
        }
        for (const identity of [{ ...reference, volume: encodeId(new Uint8Array(16).fill(0x42)) }, { ...reference, digest: "43".repeat(32) }, { ...reference, encrypted_key: "44".repeat(32) }]) {
            await assert.rejects(decryptBlob(identity, key, disk.file(envelope), disk.file(), hash));
        }
        await assert.rejects(decryptBlob(reference, new Uint8Array(32), disk.file(envelope), disk.file(), hash), /authenticated/);
        await assert.rejects(decryptBlob(reference, key, disk.file(envelope.subarray(0, -1)), disk.file(), hash), /envelope/);
        await assert.rejects(decryptBlob(reference, key, disk.file(new Uint8Array([...envelope, 0])), disk.file(), hash), /envelope/);
    } finally { disk.close(); }
});
test("blocks cannot be reordered or spliced; failed output remains private staging", async () => {
    const disk = files();
    try {
        const content = new Uint8Array(2 * 65536).fill(4); content.fill(5, 65536);
        const version = { ...reference, len_bytes: content.length, digest: sha(content) }, source = disk.file(content), envelope = disk.file();
        version.encrypted_key = await encryptBlob(version, bytes(fixture.cipher_key), source, envelope, hash);
        const bad = contents(envelope), first = bad.slice(9, 9 + 65552), second = bad.slice(9 + 65552);
        bad.set(second, 9); bad.set(first, 9 + 65552);
        await assert.rejects(decryptBlob(version, bytes(fixture.cipher_key), disk.file(bad), disk.file(), hash), /authenticated/);
        const target = disk.file(); target.write = () => 0;
        await assert.rejects(decryptBlob(version, bytes(fixture.cipher_key), envelope, target, hash), /save/);
        const noFlush = disk.file(); noFlush.failFlush = true;
        await assert.rejects(decryptBlob(version, bytes(fixture.cipher_key), envelope, noFlush, hash), /flush/);
    } finally { disk.close(); }
});
function transport({ upload = false, ack = true, stored = false, wrongDigest = false, oversized = false } = {}) {
    const envelope = bytes(fixture.envelope), calls = []; let responses = 0, offset = 0;
    const ap = async (method, ...args) => {
        calls.push(method);
        if (method === "begin") return 1;
        if (method === "streamRead") { if (oversized) return new Uint8Array(256 * 1024 + 1); const part = envelope.slice(offset, offset + 3); offset += part.length; return part; }
        if (method === "streamWrite") { assert.deepEqual(args[1], envelope.subarray(offset, offset + args[1].length)); offset += args[1].length; }
        if (method === "streamResponse") {
            if (upload && !stored && responses++ === 0) return { data: null };
            return { data: { content_length: ack || responses++ === 0 ? envelope.length : envelope.length - 1, encrypted_key: wrongDigest ? encodeId(new Uint8Array(32).fill(0x00)) : encodeId(bytes(reference.encrypted_key)) } };
        }
    };
    return { ap, calls };
}
test("streamed transfers require exact length/digest durability receipts and always close", async () => {
    const disk = files();
    try {
        const download = transport(), target = disk.file();
        await transferBlob(download.ap, { op: "download_blob" }, reference, target, false);
        assert.deepEqual(contents(target), bytes(fixture.envelope));
        assert.equal(download.calls.filter(call => call === "streamResponse").length, 2);
        assert.equal(download.calls.at(-1), "streamClose");
        for (const option of [{ ack: false }, { wrongDigest: true }, { oversized: true }]) {
            const failed = transport(option);
            await assert.rejects(transferBlob(failed.ap, { op: "download_blob" }, reference, disk.file(), false));
            assert.equal(failed.calls.at(-1), "streamClose");
        }
        for (const stored of [false, true]) {
            const upload = transport({ upload: true, stored });
            await transferBlob(upload.ap, { op: "upload_blob" }, reference, disk.file(bytes(fixture.envelope)), true);
            assert(!upload.calls.includes("streamRead"));
            assert.equal(upload.calls.filter(call => call === "streamWrite").length, Number(!stored));
            assert.equal(upload.calls.at(-1), "streamClose");
        }
    } finally { disk.close(); }
});
