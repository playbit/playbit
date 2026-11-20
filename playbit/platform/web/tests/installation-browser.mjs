import { encodeId } from '../src/identifier.ts';
import { validId } from '../src/identifier.ts';
import { METADATA_VERSION } from '../src/storage-format.ts';
// Run only on an isolated test origin: this probe deliberately corrupts and
// deletes its installation database to exercise fail-closed initialization.
import { installationIdentity, INSTALLATION_DATABASE } from '../src/installation.ts';
import { deleteWebAccount } from '../src/web-account.ts';
const require = (condition, message) => { if (!condition) throw Error(message); };
const remove = () => new Promise((resolve, reject) => {
    const request = indexedDB.deleteDatabase(INSTALLATION_DATABASE);
    request.onsuccess = resolve;
    request.onerror = () => reject(request.error);
    request.onblocked = () => reject(Error('Installation probe blocked'));
});
async function freshIdentity(zero = false) {
    const worker = new Worker(new URL('./installation-browser.js', import.meta.url), { type: 'module' });
    try { return await new Promise((resolve, reject) => {
        worker.onmessage = ({ data }) => resolve(data);
        worker.onerror = event => reject(Error(event.message));
        worker.postMessage({ identityOnly: true, zero });
    }); } finally { worker.terminate(); }
}
const change = (value, erase = false) => new Promise((resolve, reject) => {
    const request = indexedDB.open(INSTALLATION_DATABASE, METADATA_VERSION);
    request.onerror = () => reject(request.error);
    request.onsuccess = () => {
        const database = request.result;
        const transaction = database.transaction('identity', 'readwrite');
        const store = transaction.objectStore('identity');
        if (erase) store.delete('installation');
        else store.put(value, 'installation');
        transaction.oncomplete = () => { database.close(); resolve(); };
        transaction.onabort = () => { database.close(); reject(transaction.error); };
    };
});
export async function run() {
    await remove();
    try {
        const identities = await Promise.all(Array.from({ length: 32 }, () => installationIdentity()));
        const identity = identities[0];
        require(validId(identity, 16) && !/^1+$/.test(identity), 'Invalid new identity');
        require(identities.every(value => value === identity), 'Concurrent initialization split identity');
        require(await installationIdentity() === identity, 'Reopening changed identity');
        await deleteWebAccount({ indexedDB, storage: navigator.storage, caches });
        require(await installationIdentity() === identity, 'Account reset changed installation identity');
        for (const value of [undefined, encodeId(new Uint8Array(16).fill(0x00)), 'bad', 42]) {
            await change(value, value === undefined);
            for (let attempt = 0; attempt < 2; attempt++) {
                let rejected = false;
                try { await installationIdentity(); } catch { rejected = true; }
                require(rejected, 'Damaged installation identity was silently replaced');
            }
        }
        await remove();
        let rejected = false;
        try { await installationIdentity(); } catch { rejected = true; }
        require(rejected, 'An old owner continued after installation retirement');
        require((await freshIdentity(true)).error, 'Zero random identity was admitted');
        require(!((await indexedDB.databases()).some(db => db.name === INSTALLATION_DATABASE)),
            'Aborted initialization left a damaged installation');
        const { identity: fresh, error } = await freshIdentity();
        require(!error && validId(fresh, 16), 'A fresh owner could not initialize');
        require(fresh !== identity, 'Fresh installation reused the removed identity');
        return { passed: true, concurrentInitializers: identities.length,
            cases: ['creation', 'reopen', 'concurrent creation', 'account reset', 'missing identity',
                'zero identity', 'malformed identity', 'wrong type', 'aborted creation'] };
    } finally { await remove(); }
}
if (!('document' in globalThis)) self.onmessage = async ({ data }) => {
    if (data.runInstallation) {
        try { self.postMessage({ result: await run() }); }
        catch (error) { self.postMessage({ error: error.message }); }
        self.close(); return;
    }
    if (!data.identityOnly) return;
    if (data.zero) crypto.getRandomValues = bytes => { bytes.fill(0); return bytes; };
    try { self.postMessage({ identity: await installationIdentity() }); }
    catch (error) { self.postMessage({ error: error.message }); }
    self.close();
};
