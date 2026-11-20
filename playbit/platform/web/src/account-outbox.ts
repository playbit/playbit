import { validId } from './identifier.ts';
// Account publication intent lives beside the local state it describes. Array
// keys share the existing store with its string-keyed account row; no migration
// or second metadata database is needed. Network work never enters these txns.
import { MAX_ACCOUNT_RECORD_BYTES } from './account-record.ts';
import { unsupportedAccount } from './account-format.ts';
import { openMetadata, watchMetadata } from './storage-format.ts';

// Which local metadata database: 'personal' holds per-version volume selections,
// 'recovery' the credential catalog and account key.
export type AccountMetadataKind = 'personal' | 'recovery';
// An open metadata database. `S` is the shape of the account's string-keyed
// state row (PersonalSelections or RecoveryState); its owner validates the row.
export interface AccountMetadata<S = unknown> { database: IDBDatabase; store: string }
// The `[account]` row: how many local commits happened and the newest record clock.
export interface AccountMetadataControl { revision: bigint; clock: bigint }
// One encrypted account record awaiting or surviving publication; `key` is the
// hex record key, `ciphertext` the sealed envelope.
export interface AccountOutboxRecord { key: string; clock: bigint; ciphertext: Uint8Array<ArrayBuffer> }
// One readonly snapshot: the state row, its control and the requested record head.
export interface AccountMutationSnapshot<S = unknown> { state: S | undefined; control: AccountMetadataControl; record: AccountOutboxRecord | null }
// Thrown by commit/adopt when the revision moved since the snapshot was read.
export interface AccountMetadataChangedError extends Error { code: 'account-metadata-changed' }
type RecordKey = [string, 'record', string];
type DirtyKey = [string, 'dirty', string];
type TransactionRun<T> = (store: IDBObjectStore, done: (value: T) => void, fail: (error: unknown) => void) => void;

const id = (value: unknown): value is string => typeof value === 'string' && validId(value, 16);
const opaque = (value: unknown): value is string => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
const maximumClock = 0xfffffffffffffffcn;
const recordKey = (account: string, key: string): RecordKey => [account, 'record', key];
const dirtyKey = (account: string, key: string): DirtyKey => [account, 'dirty', key];
const quarantineKey = (account: string): [string, string] => [account, 'unsupported-account'];
const pausedAccounts = new Set<string>();
let authorityCacheFull = false;
export const accountAuthorityAllowed = (account: string): boolean => !authorityCacheFull && !pausedAccounts.has(account);
function pauseAuthority(account: string): void {
    if (pausedAccounts.size >= 32 && !pausedAccounts.has(account)) authorityCacheFull = true;
    else pausedAccounts.add(account);
}
const sameBytes = (a: Uint8Array, b: Uint8Array): boolean => a.length === b.length && a.every((byte, index) => byte === b[index]);
let observer: ((account: string) => void) | undefined;
export function observeAccountMetadata(listener: (account: string) => void): void {
    if (observer) throw Error('Account metadata already has an observer in this worker');
    observer = listener;
}
// Validates a stored or offered record; the parameter is only a candidate shape.
function record(value: Partial<AccountOutboxRecord> | null | undefined): AccountOutboxRecord {
    if (!value || !opaque(value.key) || typeof value.clock !== 'bigint' || value.clock <= 0n || value.clock > maximumClock
        || !(value.ciphertext instanceof Uint8Array) || value.ciphertext.length <= 29
        || value.ciphertext.length > MAX_ACCOUNT_RECORD_BYTES + 29 || value.ciphertext[0] !== 1) throw Error('Invalid account outbox record');
    return value as AccountOutboxRecord;
}
function control(value: Partial<AccountMetadataControl> | null | undefined): AccountMetadataControl {
    value ??= { revision: 0n, clock: 0n };
    if (typeof value.revision !== 'bigint' || value.revision < 0n || value.revision > maximumClock
        || typeof value.clock !== 'bigint' || value.clock < 0n || value.clock > maximumClock) throw Error('Invalid account metadata clock');
    return value as AccountMetadataControl;
}
function metadataChanged(): AccountMetadataChangedError {
    const error = Error('Account state changed while preparing this update. Review and try again') as AccountMetadataChangedError;
    error.code = 'account-metadata-changed';
    return error;
}
export function nextAccountClock(previous: bigint, seconds: number = Math.floor(Date.now() / 1000)): bigint {
    if (typeof previous !== 'bigint' || previous < 0n || previous >= maximumClock || !Number.isSafeInteger(seconds)) throw Error('Account clock exhausted or invalid');
    const physical = BigInt(Math.max(1, Math.min(0xffffffff, seconds - 1728000000))) << 32n;
    const next = physical > previous ? physical : previous + 1n;
    if (next > maximumClock) throw Error('Account clock exhausted');
    return next;
}
export async function openAccountMetadata<S = unknown>(kind: AccountMetadataKind): Promise<AccountMetadata<S>> {
    const store = kind === 'personal' ? 'versions' : kind === 'recovery' ? 'accounts' : null;
    if (!store) throw Error('Invalid account metadata store');
    await watchMetadata(`playbit-${kind}`, store, `playbit.web.${kind}`);
    const database = await openMetadata(`playbit-${kind}`, store, `playbit.web.${kind}`);
    return { database, store };
}
function transaction<T>(database: IDBDatabase, name: string, mode: IDBTransactionMode, run: TransactionRun<T>): Promise<T> {
    return new Promise<T>((resolve, reject) => {
        const tx = database.transaction(name, mode, { durability: 'strict' }), store = tx.objectStore(name);
        let result: T | undefined, failure: unknown;
        const fail = (error: unknown): void => { failure = error; try { tx.abort(); } catch { /* An aborted request can already have retired the transaction. */ } };
        tx.oncomplete = () => resolve(result as T); // Every `run` reaches `done` before its requests can complete.
        tx.onabort = () => reject(failure ?? tx.error ?? Error('Account metadata transaction aborted'));
        tx.onerror = () => {}; // Abort reports the final error, never a prefix.
        try { run(store, value => { result = value; }, fail); } catch (error) { fail(error); }
    });
}
// Negative authenticated evidence is independent of the recovery/catalog row,
// so recovery writes and reconnect cannot accidentally clear it. No current
// required account features are supported; this writer has no clear operation.
export async function quarantineAccount(account: string, incoming: AccountOutboxRecord): Promise<void> {
    pauseAuthority(account);
    record(incoming);
    const metadata = await openAccountMetadata('recovery');
    try {
        await navigator.locks.request(`playbit-account-authority:${account}`, { mode: 'exclusive' }, () => transaction(metadata.database, metadata.store, 'readwrite', (store, done, fail) => {
            const retained = store.get(quarantineKey(account));
            retained.onsuccess = () => {
                try { if (retained.result === undefined) store.put(incoming, quarantineKey(account)); done(undefined); }
                catch (error) { fail(error); }
            };
        }));
    } finally { metadata.database.close(); }
    observer?.(account);
}
export async function requireAccountAuthority(account: string): Promise<void> {
    if (!id(account)) throw Error('Invalid account authority identity');
    const metadata = await openAccountMetadata('recovery');
    try {
        const quarantined = await transaction<boolean>(metadata.database, metadata.store, 'readonly', (store, done) => {
            const retained = store.get(quarantineKey(account));
            retained.onsuccess = () => done(retained.result !== undefined);
        });
        if (quarantined) pauseAuthority(account);
        if (!accountAuthorityAllowed(account)) throw unsupportedAccount();
    } finally { metadata.database.close(); }
}
export function withAccountAuthority<T>(account: string, run: () => Promise<T>): Promise<T> {
    return navigator.locks.request(`playbit-account-authority:${account}`, { mode: 'shared' }, async () => {
        await requireAccountAuthority(account);
        return run();
    });
}
export function readAccountMutation<S = unknown>({ database, store }: AccountMetadata<S>, account: string, key?: string): Promise<AccountMutationSnapshot<S>> {
    if (!id(account) || (key !== undefined && !opaque(key))) return Promise.reject(Error('Invalid account metadata identity'));
    return transaction<AccountMutationSnapshot<S>>(database, store, 'readonly', (store, done, fail) => {
        const state = store.get(account), clock = store.get([account]), current = key === undefined ? null : store.get(recordKey(account, key));
        // Requests are queued in order; this final request sees their results in
        // the same readonly snapshot, including older string-keyed account rows.
        (current ?? clock).onsuccess = () => {
            try {
                const previous = current?.result === undefined ? null : record(current.result);
                if (previous && previous.key !== key) throw Error('Account record identity mismatch');
                done({ state: state.result, control: control(clock.result), record: previous });
            }
            catch (error) { fail(error); }
        };
    });
}
export async function commitAccountMutation<S = unknown>({ database, store }: AccountMetadata<S>, account: string, expected: AccountMetadataControl, state: S, pending?: AccountOutboxRecord): Promise<S> {
    if (!id(account)) return Promise.reject(Error('Invalid account metadata identity'));
    try { control(expected); if (pending) record(pending); } catch (error) { return Promise.reject(error); }
    const result = await withAccountAuthority(account, () => transaction<S>(database, store, 'readwrite', (store, done, fail) => {
        const request = store.get([account]);
        request.onsuccess = () => {
            try {
                const current = control(request.result);
                if (current.revision !== expected.revision) throw metadataChanged();
                if (current.revision === maximumClock) throw Error('Account metadata revision exhausted');
                if (pending && pending.clock <= current.clock) throw Error('Account mutation clock did not advance');
                store.put(state, account);
                if (pending) {
                    store.put(pending, recordKey(account, pending.key));
                    store.put(pending.clock, dirtyKey(account, pending.key));
                }
                store.put({ revision: current.revision + 1n, clock: pending?.clock ?? current.clock }, [account]);
                done(state);
            } catch (error) { fail(error); }
        };
    }));
    observer?.(account);
    return result;
}
// The caller authenticates and projects the record before write admission.
// This transaction rechecks its original snapshot and the current encrypted
// head. An exact local replay remains dirty until a publication acknowledgment.
export async function adoptAccountMutation<S = unknown>({ database, store }: AccountMetadata<S>, account: string, expected: AccountMetadataControl, state: S, incoming: AccountOutboxRecord, admit: () => boolean = () => true): Promise<boolean> {
    if (!id(account)) throw Error('Invalid account metadata identity');
    control(expected); record(incoming);
    const changed = await withAccountAuthority(account, () => transaction<boolean>(database, store, 'readwrite', (store, done, fail) => {
        const head = store.get(recordKey(account, incoming.key)), clock = store.get([account]);
        clock.onsuccess = () => {
            try {
                if (!admit()) { done(false); return; }
                const current: AccountOutboxRecord | undefined = head.result && record(head.result), version = control(clock.result);
                if (current && current.key !== incoming.key) throw Error('Account record identity mismatch');
                if (current) {
                    let order = current.clock < incoming.clock ? -1 : current.clock > incoming.clock ? 1 : 0;
                    if (!order) {
                        const count = Math.min(current.ciphertext.length, incoming.ciphertext.length);
                        for (let index = 0; index < count && !order; index++) order = current.ciphertext[index] - incoming.ciphertext[index];
                        if (!order) order = current.ciphertext.length - incoming.ciphertext.length;
                    }
                    if (order >= 0) { done(false); return; }
                }
                if (version.revision !== expected.revision) throw metadataChanged();
                if (version.revision === maximumClock) throw Error('Account metadata revision exhausted');
                store.put(state, account);
                store.put(incoming, recordKey(account, incoming.key));
                store.delete(dirtyKey(account, incoming.key));
                store.put({ revision: version.revision + 1n, clock: incoming.clock > version.clock ? incoming.clock : version.clock }, [account]);
                done(true);
            } catch (error) { fail(error); }
        };
    }));
    if (changed) observer?.(account);
    return changed;
}
export async function pendingAccountRecord(kind: AccountMetadataKind, account: string, after: string = ''): Promise<AccountOutboxRecord | null> {
    if (!id(account) || (after && !opaque(after))) throw Error('Invalid account outbox cursor');
    const { database, store } = await openAccountMetadata(kind);
    try {
        return await transaction<AccountOutboxRecord | null>(database, store, 'readonly', (store, done, fail) => {
            const range = IDBKeyRange.bound(dirtyKey(account, after), dirtyKey(account, '￿'), Boolean(after));
            const request = store.openCursor(range);
            request.onsuccess = () => {
                const cursor = request.result;
                if (!cursor) { done(null); return; }
                // The range only spans this account's dirty markers.
                const key = (cursor.key as DirtyKey)[2], read = store.get(recordKey(account, key));
                read.onsuccess = () => {
                    try { const value = record(read.result); if (value.key !== key || value.clock !== cursor.value) throw Error('Account outbox marker mismatch'); done(value); }
                    catch (error) { fail(error); }
                };
            };
        });
    } finally { database.close(); }
}
export async function acknowledgeAccountRecord(kind: AccountMetadataKind, account: string, expected: AccountOutboxRecord, admit: () => boolean = () => true): Promise<boolean> {
    if (!id(account)) throw Error('Invalid account outbox identity');
    record(expected);
    const { database, store } = await openAccountMetadata(kind);
    try {
        const acknowledged = await transaction<boolean>(database, store, 'readwrite', (store, done, fail) => {
            const head = store.get(recordKey(account, expected.key)), dirty = store.get(dirtyKey(account, expected.key));
            dirty.onsuccess = () => {
                try {
                    if (!admit()) { done(false); return; }
                    const current: AccountOutboxRecord | undefined = head.result && record(head.result);
                    if (current && current.key !== expected.key) throw Error('Account record identity mismatch');
                    const exact = current && current.clock === expected.clock && sameBytes(current.ciphertext, expected.ciphertext) && dirty.result === expected.clock;
                    if (exact) store.delete(dirtyKey(account, expected.key));
                    done(Boolean(exact));
                } catch (error) { fail(error); }
            };
        });
        if (acknowledged) observer?.(account);
        return acknowledged;
    } finally { database.close(); }
}
