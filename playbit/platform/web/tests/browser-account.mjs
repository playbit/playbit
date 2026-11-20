import { ApClient } from '../src/ap-client.ts';
import { holdWebAccount, clearPlaybitStorage, prepareWebAccount, beginWebReset, reloadWebAccount, SIGN_IN_HANDOFF } from '../src/web-account.ts';
const client = new ApClient('public'), tab = crypto.randomUUID();
const reset = await prepareWebAccount(client);
await holdWebAccount(({ tab: initiating }) => {
    if (sessionStorage.getItem(SIGN_IN_HANDOFF)) { reloadWebAccount({ tab: initiating }, tab); return; }
    clearPlaybitStorage(localStorage, true); clearPlaybitStorage(sessionStorage);
    location.replace(initiating === tab ? '/tests/browser-account.html?verify' : '/s');
});
const output = document.querySelector('#result');
const stores = { 'playbit-ap': 'credentials', 'playbit-recovery': 'accounts',
    'playbit-personal': 'versions' };
async function seed() {
    for (const [name, store] of Object.entries(stores)) {
        await new Promise((resolve, reject) => {
            const request = indexedDB.open(name, 1);
            request.onupgradeneeded = () => request.result.createObjectStore(store);
            request.onerror = () => reject(request.error);
            request.onsuccess = () => {
                const database = request.result, transaction = database.transaction(store, 'readwrite');
                transaction.objectStore(store).put({ email: 'prior@example.invalid', secret: 'fixture only' }, 'active');
                transaction.oncomplete = () => { database.close(); resolve(); };
            };
        });
    }
    const root = await navigator.storage.getDirectory();
    const directory = await root.getDirectoryHandle('playbit-reset-fixture', { create: true });
    const file = await directory.getFileHandle('fixture', { create: true });
    const writer = await file.createWritable(); await writer.write(new Uint8Array(10 * 1024 * 1024)); await writer.close();
    localStorage.setItem('playbit-invitation', 'old-secret'); sessionStorage.setItem('playbit-old', 'old-secret');
    const cache = await caches.open('playbit-reset-fixture'); await cache.put('/fixture', new Response('old-secret'));
    output.textContent = 'Prepared four account databases, 10 MiB app data and cached invitation. Open a local TODO in another tab, then sign out here.';
}
document.querySelector('#seed').onclick = () => seed().catch(error => { output.textContent = error.message; });
document.querySelector('#reset').onclick = () => {
    sessionStorage.setItem('cleanup-test-start', String(Date.now()));
    void beginWebReset(client, tab).catch(error => { output.textContent = error.message; });
};
document.querySelector('#signin').onclick = () => {
    history.replaceState(null, '', '/tests/browser-account.html?verify#invitation-fixture');
    sessionStorage.setItem(SIGN_IN_HANDOFF, 'next@example.invalid');
    sessionStorage.setItem('cleanup-test-start', String(Date.now()));
    void beginWebReset(client, tab).catch(error => { output.textContent = error.message; });
};
if (reset.error) throw Error(reset.error);
if (location.search === '?verify') {
    if (location.hash) {
        if (location.hash !== '#invitation-fixture' || sessionStorage.getItem(SIGN_IN_HANDOFF) !== 'next@example.invalid') {
            throw Error('Fresh sign-in lost the invitation or new email');
        }
        sessionStorage.removeItem(SIGN_IN_HANDOFF);
    }
    const databases = (await indexedDB.databases()).filter(item => item.name.startsWith('playbit-') && item.name !== 'playbit-installation');
    const directories = []; for await (const name of (await navigator.storage.getDirectory()).keys()) if (name.startsWith('playbit-')) directories.push(name);
    const cached = (await caches.keys()).filter(name => name.startsWith('playbit-'));
    const retained = Object.keys(localStorage).concat(Object.keys(sessionStorage)).filter(name => name.startsWith('playbit-'));
    const result = { databases: databases.length, directories: directories.length, caches: cached.length, webStorageKeys: retained.length,
        invitationRetained: location.hash === '#invitation-fixture', cleanupAndReloadMs: Date.now() - Number(sessionStorage.getItem('cleanup-test-start')) };
    output.textContent = JSON.stringify(result, null, 2);
    if (databases.length || directories.length || cached.length || retained.length) throw Error('Prior account data survived sign-out');
    output.textContent = 'PASS\n' + output.textContent;
} else output.textContent = 'Ready';
