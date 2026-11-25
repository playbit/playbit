import { AbiError } from './guest-memory.ts';
import { decodeId, encodeId, validId } from './identifier.ts';
import type { VolumeCredentials } from './invitation.ts';
import type { OpfsResources } from './opfs-store.ts';
import { incompatibleStorage, requireStorageFormat } from './storage-format.ts';
import { VolumePublication } from './volume-publication.ts';

const HEADER = 'volume-format', PENDING = 'volume-format.pending', POINTERS = 'volume-generations.kvdb';
// PBAUTO01 | volume[16] | owner[16] | original build[32] | database and blob keys[32 each] | two LE u32 replica IDs | SHA-256 of these 144 bytes.
const MAGIC = Uint8Array.of(80, 66, 65, 85, 84, 79, 48, 49), BODY_BYTES = 144, HEADER_BYTES = BODY_BYTES + 32;
interface AutomaticDescriptor {
    credentials: VolumeCredentials;
    replica_ids: [number, number];
    creation_build: string;
}
export interface AutomaticStorage extends AutomaticDescriptor {
    directory: FileSystemDirectoryHandle;
    publication: VolumePublication;
    close(): Promise<void>;
}

function readDescriptor(bytes: Uint8Array, volume: string, account: string): AutomaticDescriptor {
    if (bytes.length !== HEADER_BYTES || !MAGIC.every((byte, i) => bytes[i] === byte)
        || encodeId(bytes.subarray(8, 24)) !== volume || encodeId(bytes.subarray(24, 40)) !== account) throw incompatibleStorage();
    const database = bytes.subarray(72, 104), blob = bytes.subarray(104, 136);
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.length);
    const replica_ids: [number, number] = [view.getUint32(136, true), view.getUint32(140, true)];
    if (!database.some(Boolean) || !blob.some(Boolean) || database.every((byte, i) => byte === blob[i])
        || !replica_ids[0] || !replica_ids[1] || replica_ids[0] === replica_ids[1]
        || !bytes.subarray(40, 72).some(Boolean)) throw incompatibleStorage();
    return { creation_build: encodeId(bytes.subarray(40, 72)), credentials: {
        database_key: Array.from(database), blob_key: Array.from(blob),
    }, replica_ids };
}

// The exclusive volume lock covers both first publication and every later use of its files.
async function descriptor(directory: FileSystemDirectoryHandle, volume: string, account: string, build: string): Promise<AutomaticDescriptor> {
    requireStorageFormat();
    let file: FileSystemFileHandle | undefined;
    try { file = await directory.getFileHandle(HEADER); }
    catch (cause) { if ((cause as DOMException).name !== 'NotFoundError') throw cause; }
    if (file) {
        const saved = await file.getFile();
        if (saved.size !== HEADER_BYTES) throw incompatibleStorage();
        const bytes = new Uint8Array(await saved.arrayBuffer());
        const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', bytes.subarray(0, BODY_BYTES)));
        if (!digest.every((byte, index) => byte === bytes[BODY_BYTES + index])) throw incompatibleStorage();
        return readDescriptor(bytes, volume, account);
    }

    // The only artifacts an interrupted first publication can leave are its spool and the empty pointer file.
    for await (const item of directory.values()) {
        if (item.kind === 'file' && (item.name === PENDING || item.name === POINTERS && !(await item.getFile()).size)) continue;
        throw incompatibleStorage();
    }
    const bytes = new Uint8Array(HEADER_BYTES);
    crypto.getRandomValues(bytes.subarray(72, BODY_BYTES));
    bytes.set(MAGIC); bytes.set(decodeId(volume), 8); bytes.set(decodeId(account), 24); bytes.set(decodeId(build, 32), 40);
    bytes.set(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes.subarray(0, BODY_BYTES))), BODY_BYTES);
    const result = readDescriptor(bytes, volume, account);
    requireStorageFormat();
    await directory.getFileHandle(POINTERS, { create: true });
    const pending = await directory.getFileHandle(PENDING, { create: true });
    const handle = await pending.createSyncAccessHandle();
    try {
        requireStorageFormat(); handle.truncate(0);
        if (handle.write(bytes, { at: 0 }) !== bytes.length) throw Error('Automatic volume storage is full');
        handle.flush();
    } finally { handle.close(); }
    requireStorageFormat();
    await (pending as FileSystemFileHandle & { move(to: string): Promise<void> }).move(HEADER);
    return result;
}

export function openAutomaticStorage(volume: string, account: string, build: string,
    resources: OpfsResources, yieldReady: () => Promise<void>): Promise<AutomaticStorage> {
    return new Promise((resolve, reject) => {
        if (!validId(volume) || !validId(account) || !validId(build, 32)
            || /^1+$/.test(volume) || /^1+$/.test(account) || /^1+$/.test(build)) throw Error('Invalid automatic volume identity');
        const ownership = navigator.locks.request(`playbit-automatic:${volume}`, { ifAvailable: true }, async lock => {
            // Waiting for another session's lifetime lock can deadlock when two apps use several scopes.
            if (!lock) throw new AbiError(-12); // Busy. The operation may be retried after the other tab closes.
            requireStorageFormat();
            const root = await navigator.storage.getDirectory();
            const directory = await root.getDirectoryHandle(`playbit-automatic-${volume}`, { create: true });
            const record = await descriptor(directory, volume, account, build);
            const pointers = await directory.getFileHandle(POINTERS);
            requireStorageFormat();
            const publication = await VolumePublication.open(await pointers.createSyncAccessHandle(), resources,
                () => encodeId(crypto.getRandomValues(new Uint8Array(16))), yieldReady);
            let attached = true;
            const detach = () => { if (attached) { attached = false; publication.close(); } };
            try {
                await publication.discardPending(directory);
                // Only this volume's lock owner can have created a private blob stage.
                for await (const item of directory.values()) {
                    if (item.kind === 'file' && /^blob-stage-[0-9]+\.tmp$/.test(item.name)) {
                        requireStorageFormat();
                        await directory.removeEntry(item.name);
                    }
                    await yieldReady();
                }
                requireStorageFormat();
                await new Promise<void>(release => resolve({ ...record, directory, publication, close() {
                    try { detach(); } finally { release(); }
                    return ownership;
                } }));
            } finally { detach(); }
        });
        ownership.catch(reject);
    });
}
