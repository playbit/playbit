import { validId } from './identifier.ts';
// Recovery reads the original sealed file through one exclusive handle. Current
// authorization is checked by the host after verification, before returning rows.
import type { BlobHash, BlobHashFactory } from './blob.ts';
import { OpfsStore, type OpfsResources } from './opfs-store.ts';
import type { VolumePublication } from './volume-publication.ts';

export interface RecoveryCut {
    generation: string;
    entries: string[];
    bytes: number;
    files: Record<string, { bytes: number; digest: number[] }>;
}

export async function openRecoveryIndex(root: FileSystemDirectoryHandle, publication: VolumePublication,
    volume: string, cut: RecoveryCut, resources: OpfsResources, hash: BlobHashFactory,
    yieldReady: () => Promise<void>, check: () => void): Promise<OpfsStore> {
    return openRecoveryDatabase(root, publication, volume, cut, 'index.kvdb', resources, hash, yieldReady, check);
}

export async function openRecoveryDatabase(root: FileSystemDirectoryHandle, publication: VolumePublication,
    volume: string, cut: RecoveryCut, name: string, resources: OpfsResources, hash: BlobHashFactory,
    yieldReady: () => Promise<void>, check: () => void): Promise<OpfsStore> {
    if (name !== 'index.kvdb' && !(name.endsWith('.kvdb') && validId(name.slice(0, -5), 15))) throw new Error('Invalid retained database name');
    const file = await openRecoveryFile(root, publication, volume, cut, name, hash, yieldReady, check);
    // OpfsStore.open owns cleanup, including when an archived tail needs repair.
    return OpfsStore.open(file, resources, async () => { await yieldReady(); check(); });
}

export async function openRecoveryFile(root: FileSystemDirectoryHandle, publication: VolumePublication,
    volume: string, cut: RecoveryCut, name: string, hash: BlobHashFactory,
    yieldReady: () => Promise<void>, check: () => void): Promise<FileSystemSyncAccessHandle> {
    if (name !== 'index.kvdb' && !(name.endsWith('.kvdb') && validId(name.slice(0, -5), 15)) && !/^[0-9a-f]{64}\.blob$/.test(name)) {
        throw new Error('Invalid retained file name');
    }
    const seal = cut.files[name];
    if (!seal || !Number.isSafeInteger(seal.bytes) || seal.bytes < 0 || seal.bytes > 1024 * 1024 * 1024
        || seal.digest.length !== 32 || seal.digest.some(byte => !Number.isInteger(byte) || byte < 0 || byte > 255)) {
        throw new Error('Invalid retained index seal');
    }
    check();
    const generation = await root.getDirectoryHandle(publication.directory(volume, cut.generation));
    const archive = await generation.getDirectoryHandle('rejected');
    const file = await (await archive.getFileHandle(name)).createSyncAccessHandle();
    let owned = true, digest: BlobHash | undefined;
    try {
        check();
        if (file.getSize() !== seal.bytes) throw new Error('Retained index length differs');
        digest = hash();
        const buffer = new Uint8Array(Math.min(64 * 1024, seal.bytes));
        for (let offset = 0; offset < seal.bytes; offset += buffer.length) {
            check();
            const part = buffer.subarray(0, Math.min(buffer.length, seal.bytes - offset));
            if (file.read(part, { at: offset }) !== part.length) throw new Error('Retained index ended early');
            digest.update(part);
            await yieldReady();
        }
        check();
        const expected = seal.digest.map(byte => byte.toString(16).padStart(2, '0')).join('');
        if (digest.finish() !== expected) throw new Error('Retained index digest differs');
        digest.close(); digest = undefined;
        const immutable: FileSystemSyncAccessHandle = {
            getSize: () => file.getSize(), read: (bytes, options) => file.read(bytes, options),
            close: () => file.close(),
            write: () => { throw new Error('Retained index is immutable'); },
            truncate: () => { throw new Error('Retained index requires repair'); },
            flush: () => { throw new Error('Retained index is immutable'); },
        };
        owned = false;
        return immutable;
    } finally {
        try { digest?.close(); } finally { if (owned) file.close(); }
    }
}
