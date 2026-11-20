import test from 'node:test';
import assert from 'node:assert/strict';
import { clearPlaybitStorage, deleteWebAccount, resetWebAccount, ACCOUNT_STORAGE_LOCK } from './web-account.ts';

const turn = () => new Promise(setImmediate);
test('cleanup removes account keys, databases, app files and caches, preserving unrelated origin data', async () => {
    const values = new Map([['playbit-invitation', 'secret'], ['playbit-new-sign-in', 'old'], ['other', 'keep']]);
    clearPlaybitStorage({ get length() { return values.size; }, key: i => [...values.keys()][i], removeItem: key => values.delete(key) });
    assert.deepEqual([...values], [['other', 'keep']]);
    const databases = [], files = [], caches = [];
    await deleteWebAccount({
        indexedDB: { databases: async () => [{ name: 'playbit-extra' }, { name: 'unrelated' }, { name: 'playbit-installation' }],
            deleteDatabase(name) { databases.push(name); const request = {}; queueMicrotask(() => request.onsuccess()); return request; } },
        storage: { getDirectory: async () => ({ async *entries() { yield ['playbit-session']; yield ['unrelated']; },
            async removeEntry(name, options) { assert.equal(options.recursive, true); files.push(name); } }) },
        caches: { keys: async () => ['playbit-content', 'unrelated'], delete: async name => caches.push(name) },
    });
    assert.deepEqual(databases, ['playbit-ap', 'playbit-recovery', 'playbit-personal', 'playbit-extra']);
    assert.deepEqual(files, ['playbit-session']); assert.deepEqual(caches, ['playbit-content']);
});

test('blocked deletion fails visibly rather than admitting a new account', async () => {
    await assert.rejects(deleteWebAccount({ indexedDB: { databases: async () => [], deleteDatabase() {
        const request = {}; queueMicrotask(() => request.onblocked()); return request;
    } } }), /Close other Playbit tabs/);
});

test('reset queues exclusive cleanup before reloading pages and drains old AP writes before deletion', async () => {
    const order = []; let release;
    const environment = {
        BroadcastChannel: class { postMessage(message) { assert.equal(message.tab, 'tab'); order.push('reload'); } close() { order.push('close'); } },
        navigator: { locks: { request(name, options, work) {
            assert.equal(name, ACCOUNT_STORAGE_LOCK); assert(options.signal);
            order.push('queued'); return new Promise(resolve => { release = async () => { await work(); resolve(); }; });
        } }, storage: { getDirectory: async () => ({ async *entries() {} }) } },
        indexedDB: { databases: async () => { order.push('delete'); return []; }, deleteDatabase() {
            const request = {}; queueMicrotask(() => request.onsuccess()); return request;
        } }, caches: { keys: async () => [] },
    };
    const reset = resetWebAccount({ tab: 'tab' }, async () => { order.push('drained'); }, environment,
        async () => { order.push('format-fence'); });
    await turn(); assert.deepEqual(order, ['queued', 'reload']);
    await release(); await reset;
    assert.deepEqual(order, ['queued', 'reload', 'drained', 'delete', 'format-fence', 'close']);
});

test('interrupted cleanup resumes before new sign-in and failure retains its retry marker', async () => {
    const { prepareWebAccount, RESET_PENDING, SIGN_IN_HANDOFF, SIGN_IN_AFTER_RESET } = await import('./web-account.ts');
    const original = { localStorage: globalThis.localStorage, sessionStorage: globalThis.sessionStorage };
    const storage = entries => {
        const values = new Map(entries);
        return { get length() { return values.size; }, key: i => [...values.keys()][i],
            getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value), removeItem: key => values.delete(key) };
    };
    try {
        globalThis.localStorage = storage([[RESET_PENDING, 'interrupted'], ['playbit-invitation', 'old'], ['other', 'keep']]);
        globalThis.sessionStorage = storage([['playbit-secret', 'old'], [SIGN_IN_HANDOFF, 'new@example.invalid'], [SIGN_IN_AFTER_RESET, '1']]);
        let failure = 'Storage unavailable';
        const client = { resetState: async token => { assert.equal(token, 'interrupted'); return { error: failure }; } };
        assert.equal((await prepareWebAccount(client)).error, failure);
        assert.equal(localStorage.getItem(RESET_PENDING), 'interrupted');
        failure = null; await prepareWebAccount(client);
        assert.equal(localStorage.getItem(RESET_PENDING), null);
        assert.equal(localStorage.getItem('playbit-invitation'), null);
        assert.equal(sessionStorage.getItem('playbit-secret'), null);
        assert.equal(sessionStorage.getItem(SIGN_IN_HANDOFF), 'new@example.invalid');
        assert.equal(sessionStorage.getItem(SIGN_IN_AFTER_RESET), '1');
        assert.equal(localStorage.getItem('other'), 'keep');
    } finally {
        for (const [key, value] of Object.entries(original)) {
            if (value === undefined) delete globalThis[key]; else globalThis[key] = value;
        }
    }
});

test('fresh sign-in forces document reload with its invitation, while other tabs discard their handoff', async () => {
    const { reloadWebAccount, RESET_PENDING, SIGN_IN_HANDOFF, SIGN_IN_AFTER_RESET } = await import('./web-account.ts');
    const names = ['localStorage', 'sessionStorage', 'location', 'history'];
    const original = Object.fromEntries(names.map(name => [name, globalThis[name]]));
    const storage = entries => {
        const values = new Map(entries);
        return { get length() { return values.size; }, key: index => [...values.keys()][index],
            getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value), removeItem: key => values.delete(key) };
    };
    let reloads = 0, path = '/s#invitation';
    try {
        globalThis.localStorage = storage([[RESET_PENDING, 'cleanup'], ['playbit-invitation', 'old-account']]);
        globalThis.sessionStorage = storage([[SIGN_IN_HANDOFF, 'next@example.invalid'], [SIGN_IN_AFTER_RESET, '1'], ['playbit-key', 'old-key']]);
        globalThis.location = { reload() { reloads++; } };
        globalThis.history = { replaceState(_state, _title, value) { path = value; } };
        reloadWebAccount({ tab: 'initiator' }, 'initiator');
        assert.equal(reloads, 1); assert.equal(path, '/s#invitation');
        assert.equal(sessionStorage.getItem(SIGN_IN_HANDOFF), 'next@example.invalid');
        assert.equal(sessionStorage.getItem(SIGN_IN_AFTER_RESET), '1');
        assert.equal(sessionStorage.getItem('playbit-key'), null);
        assert.equal(localStorage.getItem('playbit-invitation'), null);
        assert.equal(localStorage.getItem(RESET_PENDING), 'cleanup');
        sessionStorage.removeItem(SIGN_IN_HANDOFF);
        reloadWebAccount({ tab: 'initiator' }, 'initiator');
        assert.equal(path, '/s', 'reset sign-in does not reuse an invitation from the corrupt profile');
        assert.equal(sessionStorage.getItem(SIGN_IN_AFTER_RESET), '1');
        reloadWebAccount({ tab: 'initiator' }, 'other-tab');
        assert.equal(reloads, 3); assert.equal(path, '/s');
        assert.equal(sessionStorage.getItem(SIGN_IN_HANDOFF), null);
        assert.equal(sessionStorage.getItem(SIGN_IN_AFTER_RESET), null);
    } finally {
        for (const name of names) {
            if (original[name] === undefined) delete globalThis[name]; else globalThis[name] = original[name];
        }
    }
});
