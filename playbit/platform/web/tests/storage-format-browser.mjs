// Isolated-origin web browser evidence: real IndexedDB upgrades, OPFS and WebCrypto.
import { numberedId } from './identifier-fixture.mjs';
import { openMetadata, storageFormat, watchMetadata, storageFormatAvailable, finishWebStorageReset, FORMAT_STORE, FORMAT_KEY, METADATA_VERSION } from '../src/storage-format.ts';
import { resetWebAccount, ACCOUNT_STORAGE_LOCK } from '../src/web-account.ts';
import { admitWebProfile } from '../src/profile-format.ts';
import { openAccountMetadata, readAccountMutation, commitAccountMutation, pendingAccountRecord } from '../src/account-outbox.ts';
import { personalSelections } from '../src/personal.ts';
import { accountOperationStatus } from '../src/guest-authority.ts';
import { bindOperations } from '../src/guest-imports.ts';
import { OpfsStore, OpfsResources } from '../src/opfs-store.ts';
import { ApConnection } from '../src/ap-connection.ts';
const check = (value, message) => { if (!value) throw Error(message); };
const idb = (name, version, upgrade) => new Promise((resolve, reject) => {
    const request = version === undefined ? indexedDB.open(name) : indexedDB.open(name, version);
    request.onupgradeneeded = () => upgrade?.(request.result, request.transaction);
    request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error);
});
const transaction = (database, store, mode, run) => new Promise((resolve, reject) => {
    const tx = database.transaction(store, mode), output = run(tx.objectStore(store));
    tx.oncomplete = () => resolve(output?.result); tx.onabort = () => reject(tx.error);
});
const remove = name => new Promise((resolve, reject) => {
    const request = indexedDB.deleteDatabase(name); request.onsuccess = resolve; request.onerror = () => reject(request.error);
});
const rejected = async run => { try { await run(); } catch { return; } throw Error('Incompatible storage was admitted'); };
export async function webStorageFormatCases() {
    const owner = new Worker(new URL('./installation-browser.js', import.meta.url), { type: 'module' });
    const installation = await new Promise((resolve, reject) => {
        owner.onmessage = ({ data }) => data.error ? reject(Error(data.error)) : resolve(data.result);
        owner.onerror = event => reject(Error(event.message));
        owner.postMessage({ runInstallation: true });
    }).finally(() => owner.terminate());
    const names = [], root = await navigator.storage.getDirectory(), folder = `format-probe-${crypto.randomUUID()}`;
    const directory = await root.getDirectoryHandle(folder, { create: true });
    const handles = [];
    let database, store;
    try {
        const oldName = `playbit-format-old-${crypto.randomUUID()}`; names.push(oldName);
        const old = await idb(oldName, 1, db => db.createObjectStore('data').put('retained', 'identity')); old.close();
        await rejected(() => openMetadata(oldName, 'data', 'fixture.old'));
        const retained = await idb(oldName);
        check(retained.version === 1 && !retained.objectStoreNames.contains(FORMAT_STORE), 'Failed adoption changed old DB version/shape');
        check(await transaction(retained, 'data', 'readonly', data => data.get('identity')) === 'retained', 'Failed adoption lost identity'); retained.close();
        const failedName = `playbit-format-failure-${crypto.randomUUID()}`; names.push(failedName);
        await rejected(() => openMetadata(failedName, 'data', 'fixture.failed', () => { throw new DOMException('Injected full disk', 'QuotaExceededError'); }));
        check(!(await indexedDB.databases()).some(db => db.name === failedName), 'Failed identity creation published its format');
        await admitWebProfile(directory);
        const header = await (await directory.getFileHandle('profile-format.json')).getFile();
        const original = await header.text();
        const pending = await directory.getFileHandle('pending-intent', { create: true });
        const writer = await pending.createWritable(); await writer.write('retained intent'); await writer.close();
        const format = await directory.getFileHandle('profile-format.json');
        const future = await format.createWritable(); await future.write(JSON.stringify({ ...storageFormat('playbit.web.profile'), required_write_features: ['future.authority'] })); await future.close();
        await rejected(() => admitWebProfile(directory));
        check(await (await pending.getFile()).text() === 'retained intent', 'Profile refusal cleared retained intent');
        const restore = await format.createWritable(); await restore.write(original); await restore.close(); await admitWebProfile(directory);
        const file = async name => { const result = await (await directory.getFileHandle(name, { create: true })).createSyncAccessHandle(); handles.push(result); return result; };
        const resources = new OpfsResources([await file('writer0'), await file('writer1')], [await file('stage0'), await file('stage1')]);
        store = await OpfsStore.open(await file('data'), resources, async () => {});
        const first = store.begin(true); first.put(Uint8Array.of(1), Uint8Array.of(7)); first.end(true);
        const before = store.handle.getSize();
        const accepted = store.begin(true); accepted.put(Uint8Array.of(1), Uint8Array.of(8));
        const account = numberedId(77), session = numberedId(88, 32), volume = numberedId(99);
        const recovery = await openAccountMetadata('recovery');
        const snapshot = await readAccountMutation(recovery, account);
        await commitAccountMutation(recovery, account, snapshot.control, { secret: Array(16).fill(7), acknowledged: true, revision: 1, catalog: { account, volumes: [], invitations: [] } }); recovery.database.close();
        await personalSelections(account, session, [{ volume, write: true }]);
        const cached = (await personalSelections(account))[session];
        const intent = await pendingAccountRecord('personal', account);
        let mutations = 0;
        const guest = bindOperations(new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0)), {
            admitOperation: opcode => accountOperationStatus(account, opcode),
            pb_kv_put_begin() { check(cached[0].write, 'Expected retained grant'); mutations++; return 0; },
        });
        check(guest.pb_syscall(0x144, 0n, 0n, 0n, 0n, 0n, 0n) === 0n && mutations === 1, 'Compatible cached grant was unusable');
        database = await idb('playbit-personal', METADATA_VERSION + 1, (db, tx) => {
            tx.objectStore(FORMAT_STORE).put({ ...storageFormat('playbit.web.personal'), required_write_features: ['future.authority'] }, FORMAT_KEY);
        });
        check(!storageFormatAvailable(), 'Upgrade did not retire cached authority');
        let writes = 0;
        const connection = new ApConnection('public');
        connection.streams.set(1, { operation: { op: 'describe', operation_id: numberedId(1) }, handle: 1, busy: false, closed: false, finished: false,
            writer: new WritableStream({ write() { writes++; } }).getWriter() });
        await rejected(() => connection.streamWrite(1, Uint8Array.of(7)));
        check(writes === 0, 'Cached AP stream wrote after metadata retirement');
        check(guest.pb_syscall(0x144, 0n, 0n, 0n, 0n, 0n, 0n) === -2n && mutations === 1, 'Guest used cached grant after upgrade');
        await rejected(() => personalSelections(account));
        await rejected(() => openMetadata('playbit-personal', 'versions', 'playbit.web.personal'));
        await rejected(() => watchMetadata('playbit-personal', 'versions', 'playbit.web.personal'));
        await rejected(() => Promise.resolve(accepted.end(true)));
        check(store.handle.getSize() === before, 'Accepted OPFS transaction committed after metadata retirement');
        await rejected(() => Promise.resolve(store.begin(true)));
        const saved = await transaction(database, 'versions', 'readonly', data => data.get([account, 'record', intent.key]));
        check(saved.clock === intent.clock && saved.ciphertext.every((byte, index) => byte === intent.ciphertext[index]), 'Upgrade/refusal lost original pending ciphertext');
        await rejected(() => finishWebStorageReset());
        database.close(); database = undefined;
        await resetWebAccount({ resume: true }, async () => {}, globalThis, async () => {
            const shared = await navigator.locks.request(ACCOUNT_STORAGE_LOCK, { mode: 'shared', ifAvailable: true }, lock => Boolean(lock));
            check(!shared, 'Fence reset ran after releasing exclusive cleanup ownership');
            await finishWebStorageReset();
        });
        check(storageFormatAvailable(), 'Explicit account reset left the retained worker permanently fenced');
        const fresh = await openAccountMetadata('personal');
        check(!(await readAccountMutation(fresh, account)).state, 'Explicit account reset revived cached grants'); fresh.database.close();
        names.push('playbit-installation');
        const futureInstallation = await idb('playbit-installation', METADATA_VERSION + 1, db => {
            db.createObjectStore('identity').put('retained installation', 'installation');
            db.createObjectStore(FORMAT_STORE).put({ ...storageFormat('playbit.web.installation'), required_read_features: ['future.installation'] }, FORMAT_KEY);
        }); futureInstallation.close();
        await rejected(() => watchMetadata('playbit-installation', 'identity', 'playbit.web.installation'));
        check(!storageFormatAvailable(), 'Already newer metadata failed to fence watcher initialization');
        await rejected(() => resetWebAccount({ resume: true }, async () => {}, globalThis, finishWebStorageReset));
        check(!storageFormatAvailable(), 'Account reset cleared installation incompatibility');
        return { installation, checks: 'markerless version retained; aborted creation; OPFS update refusal; real IDB versionchange; cached grant syscall denied; cached AP writes denied; lower-version open refused; accepted OPFS commit refused; pending ciphertext retained; explicit reset completes under exclusive lock and same worker opens empty account state; incompatible installation remains fenced', retained_record_bytes: intent.ciphertext.length + 40 };
    } finally {
        database?.close(); store?.close();
        for (const handle of handles) { try { handle.close(); } catch { /* Resource owner already closed it. */ } }
        for (const name of names) await remove(name);
        await root.removeEntry(folder, { recursive: true });
    }
}
