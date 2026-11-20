import { numberedId } from './identifier-fixture.mjs';
import { encodeId } from '../src/identifier.ts';
// Matched before/after consent benchmark using actual IndexedDB. For before,
// bundle the selected revision's personal.js as target/web/personal-before.js:
// esbuild --bundle --format=esm <revision>/src/personal.js --outfile=target/web/personal-before.js
import { openAccountMetadata } from '../src/account-outbox.ts';
import { personalSelections as currentSelections } from '../src/personal.ts';
self.onmessage = async ({ data }) => {
    self.onmessage = null;
    const account = encodeId(crypto.getRandomValues(new Uint8Array(16))), version = encodeId(new Uint8Array(32).fill(0xab)), volume = encodeId(new Uint8Array(16).fill(0xcd));
    let result;
    try {
        if (!Number.isInteger(data.samples) || data.samples < 1 || data.samples > 1000) throw Error('Use1–1000 samples');
        const personalSelections = data.before ? (await import(new URL('/personal-before.js', location.href).href)).personalSelections : currentSelections;
        const versions = Object.fromEntries(Array.from({ length: 128 }, (_, index) => [numberedId(index, 32), [{ volume, write: false }]]));
        const recovery = { catalog: { account, volumes: [], invitations: [] }, secret: Array(16).fill(7), acknowledged: true, revision: 1 };
        for (const [kind, value] of [['personal', versions], ['recovery', recovery]]) {
            const { database, store } = await openAccountMetadata(kind);
            try { await new Promise((resolve, reject) => {
                const tx = database.transaction(store, 'readwrite', { durability: 'strict' });
                tx.objectStore(store).put(value, account); tx.oncomplete = resolve; tx.onabort = () => reject(tx.error);
            }); } finally { database.close(); }
        }
        await personalSelections(account, version, []);
        const timings = [];
        for (let index = 0; index < data.samples; index++) {
            const start = performance.now();
            await personalSelections(account, version, index % 2 ? [] : [{ volume, write: false }]);
            timings.push(performance.now() - start);
        }
        const retained = await personalSelections(account);
        if (Object.keys(retained).length !== 129 || !Object.entries(versions).every(([key, value]) => JSON.stringify(retained[key]) === JSON.stringify(value))) throw Error('Existing versions changed');
        const ordered = timings.slice().sort((a, b) => a - b);
        result = { implementation: data.before ? 'personal-before.js' : 'personal.js',
            boundary: 'personalSelections entry through durable local consent completion; after also includes encrypted account intent',
            userAgent: navigator.userAgent, crossOriginIsolated, retained_versions: 129, samples: timings.length,
            p50_ms: ordered[Math.ceil(ordered.length * .5) - 1], p99_ms: ordered[Math.ceil(ordered.length * .99) - 1], max_ms: ordered.at(-1), timings_ms: timings };
    } catch (error) { result = { error: error.stack || error.message }; }
    finally {
        for (const kind of ['personal', 'recovery']) {
            const { database, store } = await openAccountMetadata(kind);
            try { await new Promise((resolve, reject) => {
                const tx = database.transaction(store, 'readwrite', { durability: 'strict' }), entries = tx.objectStore(store);
                entries.delete(account); entries.delete(IDBKeyRange.bound([account], [account, []]));
                tx.oncomplete = resolve; tx.onabort = () => reject(tx.error);
            }); } finally { database.close(); }
        }
        self.postMessage(result); self.close();
    }
};
