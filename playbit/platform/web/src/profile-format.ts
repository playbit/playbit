import { storageFormat, validateStorageFormat, incompatibleStorage, requireStorageFormat, FORMAT_LIMIT } from './storage-format.ts';

const PROFILE_FILE = 'profile-format.json', PENDING = 'profile-format.pending';
const PROFILE_IDENTITY = 'playbit.web.profile';

function validate(bytes: Uint8Array): boolean {
    try {
        const text = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
        const descriptor = JSON.parse(text);
        validateStorageFormat(descriptor, PROFILE_IDENTITY);
        // The supported descriptor is flat with four members and feature
        // arrays. Count separators outside strings to reject duplicate members
        // rather than letting JSON.parse hide an earlier requirement.
        let quoted = false, escaped = false, members = 0;
        for (const byte of text) {
            if (escaped) escaped = false;
            else if (quoted && byte === '\\') escaped = true;
            else if (byte === '"') quoted = !quoted;
            else if (!quoted && byte === ':') members++;
        }
        if (members !== 4) throw incompatibleStorage();
        return descriptor.required_read_features.length > 0;
    } catch { throw incompatibleStorage(); }
}
export async function admitWebProfile(directory: FileSystemDirectoryHandle, shared = false): Promise<boolean> {
    requireStorageFormat();
    try {
        const file = await (await directory.getFileHandle(PROFILE_FILE)).getFile();
        if (!file.size || file.size > FORMAT_LIMIT) throw incompatibleStorage();
        const bytes = new Uint8Array(await file.arrayBuffer());
        const admitted = validate(bytes);
        requireStorageFormat();
        if (admitted || !shared) return admitted;
    } catch (error) {
        if ((error as Error).name !== 'NotFoundError') throw error;
        // The caller owns the runtime's exclusive lifetime Web Lock. Only this
        // unpublished header spool can exist before the first format publication.
        for await (const name of (directory as FileSystemDirectoryHandle & { keys(): AsyncIterableIterator<string> }).keys()) {
            if (name !== PENDING) throw incompatibleStorage();
        }
    }
    const descriptor = storageFormat(PROFILE_IDENTITY);
    if (shared) {
        descriptor.required_read_features = ['shared-session.v1'];
        descriptor.required_write_features = ['shared-session.v1'];
    }
    const profileBytes = new TextEncoder().encode(JSON.stringify(descriptor));
    const pending = await directory.getFileHandle(PENDING, { create: true });
    const file = await pending.createSyncAccessHandle();
    try {
        requireStorageFormat();
        file.truncate(0);
        if (file.write(profileBytes, { at: 0 }) !== profileBytes.length) throw Error('Saved storage is full');
        file.flush();
    } finally { file.close(); }
    requireStorageFormat();
    await (pending as FileSystemFileHandle & { move(name: string): Promise<void> }).move(PROFILE_FILE);
    return shared;
}
