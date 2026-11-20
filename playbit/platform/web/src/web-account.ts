import { INSTALLATION_DATABASE } from './installation.ts';
// All app pages hold this shared lock until unload. Cleanup holds it exclusively,
// after asking pages to reload, so no old worker can recreate deleted storage.
export const ACCOUNT_STORAGE_LOCK = 'playbit-web-storage';
export const ACCOUNT_RESET_CHANNEL = 'playbit-web-reset';
export const RESET_PENDING = 'playbit-reset-pending';
export const SIGN_IN_HANDOFF = 'playbit-new-sign-in';
export const SIGN_IN_AFTER_RESET = 'playbit-sign-in-after-reset';

// The Web Storage surface this module uses; DOM Storage satisfies it.
export interface KeyValueStorage {
    readonly length: number;
    key(index: number): string | null;
    getItem(key: string): string | null;
    setItem(key: string, value: string): void;
    removeItem(key: string): void;
}
// Broadcast on ACCOUNT_RESET_CHANNEL by the tab that initiated a reset.
export interface ResetMessage { tab: string }
export interface ResetRequest { tab: string; resume?: boolean }
// The AP worker's reply to resetState/resetWebAccount.
export interface ResetState { error: string | null }
export interface WebAccountClient {
    resetState(token: string | null): Promise<ResetState>;
    resetWebAccount(tab: string, token: string): Promise<ResetState>;
}
export interface WebStorage { indexedDB: IDBFactory; storage: StorageManager; caches: CacheStorage }
export interface ResetChannel { postMessage(message: ResetMessage): void; close(): void }
// What resetWebAccount needs from its global scope (globalThis in a worker).
export interface ResetEnvironment {
    BroadcastChannel: new (name: string) => ResetChannel;
    navigator: { locks: LockManager; storage: StorageManager };
    indexedDB: IDBFactory;
    caches: CacheStorage;
}

// This module is also compiled for the AP SharedWorker, whose library has no
// Web Storage, History or reloadable Location. The page-only functions below
// use these module-scoped declarations of the page globals they already used.
declare const localStorage: KeyValueStorage;
declare const sessionStorage: KeyValueStorage;
declare const history: { replaceState(data: unknown, unused: string, url?: string | URL | null): void };
declare const location: { reload(): void };

export function clearPlaybitStorage(storage: KeyValueStorage, preserveReset = false): void {
    for (let index = storage.length - 1; index >= 0; index--) {
        const key = storage.key(index);
        if (key?.startsWith('playbit-') && !(preserveReset && key === RESET_PENDING)) storage.removeItem(key);
    }
}

export function reloadWebAccount({ tab }: ResetMessage, webTab: string): void {
    const next = tab === webTab ? sessionStorage.getItem(SIGN_IN_HANDOFF) : null;
    const signIn = tab === webTab ? sessionStorage.getItem(SIGN_IN_AFTER_RESET) : null;
    clearPlaybitStorage(localStorage, true); clearPlaybitStorage(sessionStorage);
    if (next) sessionStorage.setItem(SIGN_IN_HANDOFF, next);
    if (signIn) sessionStorage.setItem(SIGN_IN_AFTER_RESET, signIn);
    if (!next) history.replaceState(null, '', '/s');
    // replace(location.href) can be a same-document navigation for invitation
    // fragments, leaving this document holding the lock cleanup needs. Always
    // reload the document, retaining only the initiating invitation and one-use sign-in form.
    location.reload();
}

export async function deleteWebAccount({ indexedDB, storage, caches }: WebStorage): Promise<void> {
    // Include the fixed names even when an interrupted opening is not yet listed.
    const names = new Set(['playbit-ap', 'playbit-recovery', 'playbit-personal']);
    // The installation origin belongs to this web browser, independently of accounts.
    for (const { name } of await indexedDB.databases()) {
        if (name?.startsWith('playbit-') && name !== INSTALLATION_DATABASE) names.add(name);
    }
    for (const name of names) {
        await new Promise((resolve, reject) => {
            const request = indexedDB.deleteDatabase(name);
            request.onsuccess = resolve;
            request.onerror = () => reject(request.error);
            request.onblocked = () => reject(Error('Close other Playbit tabs, then try clearing web browser data again'));
        });
    }
    const root = await storage.getDirectory();
    for await (const [name] of root.entries()) {
        if (name.startsWith('playbit-')) await root.removeEntry(name, { recursive: true });
    }
    for (const name of await caches.keys()) if (name.startsWith('playbit-')) await caches.delete(name);
}

export function holdWebAccount(onReset: (message: ResetMessage) => void): Promise<void> {
    const channel = new BroadcastChannel(ACCOUNT_RESET_CHANNEL);
    channel.onmessage = ({ data }: MessageEvent<ResetMessage>) => onReset(data);
    return new Promise<void>((resolve, reject) => {
        // The web browser releases the lock only when this document is destroyed.
        // Do not release on pagehide before old workers/async writers have died.
        navigator.locks.request(ACCOUNT_STORAGE_LOCK, { mode: 'shared' }, async () => {
            resolve();
            await new Promise(() => {});
        }).catch(reject);
    });
}

export async function resetWebAccount({ tab, resume = false }: ResetRequest, drain: () => Promise<unknown>, environment: ResetEnvironment = globalThis,
    afterCleanup: () => Promise<void> = async () => {}): Promise<void> {
    const channel = new environment.BroadcastChannel(ACCOUNT_RESET_CHANNEL);
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), 10000);
    // Queue the exclusive lock before broadcasting: reloaded pages must wait
    // behind cleanup before opening any database or runtime worker.
    const cleanup = environment.navigator.locks.request(ACCOUNT_STORAGE_LOCK, { signal: abort.signal }, async () => {
        clearTimeout(timer);
        await drain();
        await deleteWebAccount({ indexedDB: environment.indexedDB,
            storage: environment.navigator.storage, caches: environment.caches });
        await afterCleanup();
    });
    if (!resume) channel.postMessage({ tab });
    try { await cleanup; }
    catch (error) {
        // The lock request rejects with a DOMException; only its name is inspected.
        if ((error as DOMException).name === 'AbortError') throw Error('Close other Playbit tabs, then try clearing web browser data again');
        throw error;
    } finally { clearTimeout(timer); channel.close(); }
}

// SharedWorkers can be terminated when all old documents unload. Keep a small,
// non-secret reset intent until a new document observes successful cleanup.
export async function prepareWebAccount(client: WebAccountClient): Promise<ResetState> {
    const token = localStorage.getItem(RESET_PENDING);
    const state = await client.resetState(token);
    if (token && !state.error) {
        if (localStorage.getItem(RESET_PENDING) === token) clearPlaybitStorage(localStorage);
        const next = sessionStorage.getItem(SIGN_IN_HANDOFF);
        const signIn = sessionStorage.getItem(SIGN_IN_AFTER_RESET);
        clearPlaybitStorage(sessionStorage);
        if (next) sessionStorage.setItem(SIGN_IN_HANDOFF, next);
        if (signIn) sessionStorage.setItem(SIGN_IN_AFTER_RESET, signIn);
    }
    return state;
}

export async function beginWebReset(client: WebAccountClient, tab: string): Promise<void> {
    const token = localStorage.getItem(RESET_PENDING) ?? crypto.randomUUID();
    localStorage.setItem(RESET_PENDING, token);
    const result = await client.resetWebAccount(tab, token);
    if (result.error) throw Error(result.error);
}
