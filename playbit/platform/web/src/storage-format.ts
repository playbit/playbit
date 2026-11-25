// Each owner has its own logical identity. IndexedDB's version is only the
// exclusive upgrade/retirement boundary, not an interchangeable format number.
export const METADATA_VERSION = 1;
export const INSTALLATION_DATABASE = 'playbit-installation';
export const FORMAT_STORE = 'format';
export const FORMAT_KEY = 'current';
export const FORMAT_LIMIT = 4096;
const retired = new Set<string>();
let retire: (() => void) | undefined;
const watchers = new Map<string, Promise<IDBDatabase>>();
export const storageFormatAvailable = (): boolean => retired.size === 0;
export function requireStorageFormat(): void { if (retired.size) throw incompatibleStorage(retired.has(INSTALLATION_DATABASE)); }
export function observeStorageFormat(listener: () => void): void {
    if (retire) throw Error('Storage format already has an observer in this worker');
    retire = listener;
    if (retired.size) listener();
}
function retireStorage(name: string): void {
    const first = retired.size === 0;
    retired.add(name);
    if (first) retire?.();
}
export async function watchMetadata(name: string, store: string, identity: string): Promise<void> {
    requireStorageFormat();
    let watcher = watchers.get(name);
    if (!watcher) {
        // Installation, AP credentials, personal decisions and recovery are
        // the four concrete owners. Retain one handle each to receive upgrade
        // retirement even while a runtime uses cached grants only in OPFS.
        if (watchers.size >= 4) throw Error('Storage format watcher limit exceeded');
        watcher = openMetadata(name, store, identity);
        watchers.set(name, watcher);
        watcher.catch(() => { watchers.delete(name); retireStorage(name); });
    }
    await watcher;
    requireStorageFormat();
}
// Only the AP reset owner calls this after confirmed exclusive account cleanup.
// The installation survives account reset and cannot have its fence cleared.
export async function finishWebStorageReset(): Promise<void> {
    const names = ['playbit-ap', 'playbit-personal', 'playbit-recovery'];
    if ((await indexedDB.databases()).some(database => names.includes(database.name ?? ''))) throw incompatibleStorage();
    for (const name of names) {
        const watcher = watchers.get(name);
        if (watcher) (await watcher.catch(() => undefined))?.close();
        watchers.delete(name); retired.delete(name);
    }
    requireStorageFormat();
}
export interface StorageFormat {
    format: string;
    revision: number;
    required_read_features: string[];
    required_write_features: string[];
}
export function storageFormat(format: string): StorageFormat {
    return { format, revision: 1, required_read_features: [], required_write_features: [] };
}
export function incompatibleStorage(installation = false): Error {
    if (installation) return Object.assign(Error('This browser’s saved Playbit installation requires a different version.'), { code: 'incompatible-installation-format' });
    return Object.assign(Error('This saved storage requires a different version of Playbit. Its data has been kept. Open it with a compatible version or choose new empty storage.'), { code: 'incompatible-storage-format' });
}
export function validateStorageFormat(value: unknown, identity: string): void {
    // Descriptor member order is
    // immaterial; absent fields, changed types and unallocated keys reject.
    const descriptor = value as Partial<StorageFormat> | null;
    if (!descriptor || typeof descriptor !== 'object' || Object.keys(descriptor).length !== 4
        || descriptor.format !== identity || descriptor.revision !== 1
        || !Array.isArray(descriptor.required_read_features)
        || !Array.isArray(descriptor.required_write_features)) throw incompatibleStorage(identity === 'playbit.web.installation');
    const reads = descriptor.required_read_features, writes = descriptor.required_write_features;
    if (reads.length === 0 && writes.length === 0) return;
    if (identity !== 'playbit.web.profile' || reads.length !== 1 || writes.length !== 1
        || reads[0] !== 'shared-session.v1' || writes[0] !== 'shared-session.v1') throw incompatibleStorage(identity === 'playbit.web.installation');
}
export function openMetadata(name: string, store: string, identity: string,
    initialize?: (store: IDBObjectStore) => void): Promise<IDBDatabase> {
    return new Promise((resolve, reject) => {
        requireStorageFormat();
        let failure: unknown, abandoned = false;
        const request = indexedDB.open(name, METADATA_VERSION);
        request.onupgradeneeded = event => {
            const database = request.result, tx = request.transaction!;
            // A markerless pre-release database is retained, including empty or
            // damaged identity stores. There is no implicit adoption/migration.
            if (abandoned || event.oldVersion !== 0) {
                failure = incompatibleStorage(name === INSTALLATION_DATABASE); tx.abort(); return;
            }
            try {
                const data = database.createObjectStore(store);
                database.createObjectStore(FORMAT_STORE).add(storageFormat(identity), FORMAT_KEY);
                initialize?.(data);
            } catch (error) { failure = error; tx.abort(); }
        };
        request.onblocked = () => {
            abandoned = true;
            reject(Error('Another Playbit tab is using older saved storage. Close that tab and retry. Its data has been kept.'));
        };
        request.onerror = () => reject(failure ?? (request.error?.name === 'VersionError' ? incompatibleStorage(name === INSTALLATION_DATABASE) : request.error));
        request.onsuccess = () => {
            const database = request.result;
            database.onversionchange = () => { database.close(); retireStorage(name); };
            if (abandoned) { database.close(); return; }
            try {
                const tx = database.transaction(FORMAT_STORE, 'readonly');
                const read = tx.objectStore(FORMAT_STORE).get(FORMAT_KEY);
                tx.onabort = () => { database.close(); reject(tx.error ?? incompatibleStorage(name === INSTALLATION_DATABASE)); };
                tx.oncomplete = () => {
                    try {
                        requireStorageFormat();
                        validateStorageFormat(read.result, identity); resolve(database);
                    }
                    catch (error) { database.close(); reject(error); }
                };
            } catch (error) { database.close(); reject(incompatibleStorage(name === INSTALLATION_DATABASE)); }
        };
    });
}
