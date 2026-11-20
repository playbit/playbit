import { decodeId, encodeId, validId } from './identifier.ts';
// Deterministic v1 block envelopes. Rust derives the content-scoped AES key
// and supplies incremental BLAKE3; WebCrypto authenticates each 64 KiB block.
export const BLOB_CHUNK = 65536;
export const MAX_PLAINTEXT = 64 * 1024 * 1024;
export const MAX_ENVELOPE = MAX_PLAINTEXT + 9 + 16 * Math.ceil(MAX_PLAINTEXT / BLOB_CHUNK);
export interface BlobReference {
    volume: string;
    entry: string;
    revision: string;
    len_bytes: number;
    digest: string;
    encrypted_key: string;
}
export interface BlobAcknowledgment { data?: { content_length: number; encrypted_key: string } | null }
export type BlobTransport = (method: string, ...args: unknown[]) => Promise<unknown>;
export type BlobOperation = Record<string, unknown>;
export interface BlobHash { update(bytes: Uint8Array<ArrayBuffer>): void; finish(): string; close(): void }
export type BlobHashFactory = () => BlobHash;
export type BlobFile = Pick<FileSystemSyncAccessHandle, "getSize" | "read" | "write" | "truncate" | "flush">;
const domain = new TextEncoder().encode("app.playbit.volume.blob.v1\0");
const unhex = (value: string) => Uint8Array.from(value.match(/../g)!, byte => parseInt(byte, 16));
export function envelopeLength(reference: BlobReference): number {
    const length = reference.len_bytes;
    if (!Number.isSafeInteger(length) || length < 0 || length > MAX_PLAINTEXT) throw new Error("File exceeds web transfer limit");
    if (!validId(reference.volume, 16) || !/^[0-9a-f]{64}$/.test(reference.digest)) throw new Error("Invalid encrypted file identity");
    return 9 + length + 16 * Math.max(1, Math.ceil(length / BLOB_CHUNK));
}
export function blockNonce(index: number): Uint8Array<ArrayBuffer> {
    if (!Number.isSafeInteger(index) || index < 0 || index >= 1024) throw new Error("Invalid encrypted block index");
    const nonce = new Uint8Array(12);
    new DataView(nonce.buffer).setBigUint64(4, BigInt(index));
    return nonce;
}
export function blobAAD(reference: BlobReference, index: number): Uint8Array<ArrayBuffer> {
    envelopeLength(reference); blockNonce(index);
    const bytes = new Uint8Array(91), view = new DataView(bytes.buffer);
    bytes.set(domain); bytes.set(decodeId(reference.volume), domain.length); bytes.set(unhex(reference.digest), domain.length + 16);
    view.setBigUint64(domain.length + 48, BigInt(reference.len_bytes), true);
    view.setBigUint64(domain.length + 56, BigInt(index), true);
    return bytes;
}
function header(length: number): Uint8Array<ArrayBuffer> {
    const bytes = new Uint8Array(9); bytes[0] = 1;
    new DataView(bytes.buffer).setBigUint64(1, BigInt(length), true);
    return bytes;
}
function read(file: BlobFile, offset: number, length: number): Uint8Array<ArrayBuffer> {
    const bytes = new Uint8Array(length);
    if (length && file.read(bytes, { at: offset }) !== length) throw new Error("File ended early");
    return bytes;
}
function write(file: BlobFile, offset: number, bytes: Uint8Array<ArrayBuffer>): void {
    if (bytes.length && file.write(bytes, { at: offset }) !== bytes.length) throw new Error("Could not save file on this web browser");
}
async function cipher(key: Uint8Array<ArrayBuffer>, usage: KeyUsage): Promise<CryptoKey> {
    if (key.length !== 32) throw new Error("Invalid encrypted file key");
    return crypto.subtle.importKey("raw", key, "AES-GCM", false, [usage]);
}
// Destinations are private staging. A caller must discard them on any error;
// only a complete authenticated/hash-checked result may enter the cache/index.
export async function encryptBlob(reference: BlobReference, key: Uint8Array<ArrayBuffer>, source: BlobFile, target: BlobFile, hash: BlobHashFactory): Promise<string> {
    const size = envelopeLength(reference), imported = await cipher(key, "encrypt");
    if (source.getSize() !== reference.len_bytes) throw new Error("Invalid plaintext file length");
    const plaintext = hash(), encrypted = hash();
    try {
        target.truncate(0); const prefix = header(reference.len_bytes);
        write(target, 0, prefix); encrypted.update(prefix);
        let offset = 9;
        for (let index = 0; index < Math.max(1, Math.ceil(reference.len_bytes / BLOB_CHUNK)); index++) {
            const bytes = read(source, index * BLOB_CHUNK, Math.min(BLOB_CHUNK, reference.len_bytes - index * BLOB_CHUNK));
            plaintext.update(bytes);
            const sealed = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv: blockNonce(index), additionalData: blobAAD(reference, index), tagLength: 128 }, imported, bytes));
            write(target, offset, sealed); encrypted.update(sealed); offset += sealed.length;
        }
        if (source.getSize() !== reference.len_bytes || offset !== size || plaintext.finish() !== reference.digest) throw new Error("Plaintext file does not match its declared digest");
        const digest = encrypted.finish(); target.truncate(size); target.flush(); return digest;
    } finally { plaintext.close(); encrypted.close(); }
}
export async function decryptBlob(reference: BlobReference, key: Uint8Array<ArrayBuffer>, source: BlobFile, target: BlobFile, hash: BlobHashFactory): Promise<void> {
    const size = envelopeLength(reference), imported = await cipher(key, "decrypt");
    if (source.getSize() !== size) throw new Error("Invalid encrypted file envelope");
    const prefix = read(source, 0, 9), expected = header(reference.len_bytes);
    if (prefix.some((byte, index) => byte !== expected[index])) throw new Error("Invalid encrypted file envelope");
    const plaintext = hash(), encrypted = hash();
    try {
        target.truncate(0); encrypted.update(prefix); let offset = 9;
        for (let index = 0; index < Math.max(1, Math.ceil(reference.len_bytes / BLOB_CHUNK)); index++) {
            const length = Math.min(BLOB_CHUNK, reference.len_bytes - index * BLOB_CHUNK);
            const bytes = read(source, offset, length + 16); encrypted.update(bytes); offset += bytes.length;
            let opened: Uint8Array<ArrayBuffer>;
            try { opened = new Uint8Array(await crypto.subtle.decrypt({ name: "AES-GCM", iv: blockNonce(index), additionalData: blobAAD(reference, index), tagLength: 128 }, imported, bytes)); }
            catch { throw new Error("Encrypted file could not be authenticated"); }
            if (opened.length !== length) throw new Error("Invalid authenticated block length");
            write(target, index * BLOB_CHUNK, opened); plaintext.update(opened);
        }
        if (source.getSize() !== size || encrypted.finish() !== reference.encrypted_key || plaintext.finish() !== reference.digest) throw new Error("File does not match its declared digests");
        target.truncate(reference.len_bytes); target.flush();
    } finally { plaintext.close(); encrypted.close(); }
}
function accepted(response: BlobAcknowledgment, reference: BlobReference): void {
    if (response.data?.content_length !== envelopeLength(reference) || response.data?.encrypted_key !== encodeId(unhex(reference.encrypted_key))) throw new Error("File acknowledgment does not match the authorized reference");
}
export async function transferBlob(ap: BlobTransport, operation: BlobOperation, reference: BlobReference, file: BlobFile, upload: boolean): Promise<void> {
    const expected = envelopeLength(reference);
    if (upload && file.getSize() !== expected) throw new Error("Invalid encrypted upload length");
    const handle = await ap("begin", operation);
    let timer: ReturnType<typeof setTimeout> | undefined, active = true;
    const current = () => { if (!active) throw new Error("File transfer retired"); };
    try {
        await Promise.race([(async () => {
            if (upload) {
                const begin = await ap("streamResponse", handle) as BlobAcknowledgment | null | undefined; current();
                if (begin?.data) { accepted(begin, reference); return; }
                for (let offset = 0; offset < expected; offset += BLOB_CHUNK) {
                    await ap("streamWrite", handle, read(file, offset, Math.min(BLOB_CHUNK, expected - offset))); current();
                }
                await ap("streamFinish", handle); current();
            } else {
                file.truncate(0); await ap("streamFinish", handle); current();
                accepted(await ap("streamResponse", handle) as BlobAcknowledgment, reference); current();
                let offset = 0;
                while (offset < expected) {
                    const chunk = new Uint8Array(await ap("streamRead", handle) as ArrayBuffer | ArrayLike<number>); current();
                    if (!chunk.length || chunk.length > 256 * 1024 || chunk.length > expected - offset) throw new Error("Invalid encrypted file transfer length");
                    write(file, offset, chunk); offset += chunk.length;
                }
                file.truncate(expected);
            }
            accepted(await ap("streamResponse", handle) as BlobAcknowledgment, reference); current();
            if (!upload) file.flush();
        })(), new Promise<never>((_, reject) => { timer = setTimeout(() => { active = false; reject(new Error("File transfer timed out. Local files remain saved for retry")); }, 60000); })]);
    } finally { active = false; clearTimeout(timer); await ap("streamClose", handle).catch(() => {}); }
}
export async function openCachedBlob(directory: FileSystemDirectoryHandle, digest: string): Promise<FileSystemSyncAccessHandle | null> {
    if (!/^[0-9a-f]{64}$/.test(digest)) throw new Error("Invalid file cache digest");
    try {
        const file = await (await directory.getFileHandle(`blob-${digest}`)).createSyncAccessHandle();
        if (file.getSize() > MAX_PLAINTEXT) { file.close(); throw new Error("Cached file exceeds transfer limit"); }
        return file;
    } catch (error) { if ((error as DOMException).name === "NotFoundError") return null; throw error; }
}
