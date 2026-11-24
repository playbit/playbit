// Each simulated SharedWorker owns a separate module graph, as in the web browser.
import assert from 'node:assert/strict';
import test from 'node:test';
import { Worker, isMainThread, parentPort, workerData } from 'node:worker_threads';
import { encodeId } from './identifier.ts';
import { ApConnection } from './ap-connection.ts';
const operation = { op: 'get_blob', operation_id: encodeId(new Uint8Array(16).fill(0xab)), hash: encodeId(new Uint8Array(32).fill(0xcd)) };
const cases = [
    { name: 'SharedWorker bounds advisory work across tabs independently of its 128 ordinary requests', async run() {
        const held = Promise.withResolvers();
        let requests = 0;
        ApConnection.prototype.beginChannel = async () => 7;
        ApConnection.prototype.describe = ApConnection.prototype.channelDisplayName = () => held.promise;
        ApConnection.prototype.request = async operation => { if (operation.op === 'sync_account') requests++; return held.promise; };
        globalThis.self = { location: { href: 'https://playbit.app/assets/fixture/ap-worker.js?channel=public' } };
        await import('./ap-worker.ts');
        const tab = () => {
            const replies = new Map(), port = { postMessage(reply) { replies.set(reply.id, reply); }, start() {} };
            self.onconnect({ ports: [port] });
            let next = 0;
            return async (method, args = {}) => {
                const id = ++next;
                await port.onmessage({ data: { id, method, args } });
                assert.ok(replies.has(id));
                return replies.get(id);
            };
        };
        const first = tab(), second = tab();
        await first('beginChannel'); await second('beginChannel');
        const controls = [['describe'], ['request', { operation: { op: 'put_account_display_name' } }],
            ['channelDisplayName', { handle: 7, name: new Uint8Array(256) }]];
        const active = controls.map(args => first(...args));
        const push = { operation: { op: 'sync_account', direction: 'push' } };
        const ordinary = Array.from({ length: 128 }, (_, i) => (i % 2 ? second : first)('request', push));
        assert.equal(requests, 128);
        assert.match((await first('request', push)).error, /busy/);
        for (const args of controls) assert.equal((await second(...args)).code, 'ap-capacity');
        held.resolve({});
        for (const reply of await Promise.all([...active, ...ordinary])) assert.ok('result' in reply);
        for (const args of controls) assert.ok('result' in await second(...args));
    } },
    { name: 'SharedWorker keeps stream handles scoped to their tab and rejects account conflicts', async run() {
        const originalSelf = globalThis.self;
        const methods = ["begin", "streamRead", "streamClose", "connect"];
        const originals = Object.fromEntries(methods.map(name => [name, ApConnection.prototype[name]]));
        let reads = 0, closes = 0, bound, returned;
        try {
            ApConnection.prototype.begin = async () => 42;
            ApConnection.prototype.streamRead = async (_, maximum) => { reads++; bound = maximum; returned = Uint8Array.of(1); return returned; };
            ApConnection.prototype.streamClose = () => { closes++; };
            ApConnection.prototype.connect = async function () { this.account = encodeId(new Uint8Array(16).fill(0xab)); return this.status(); };
            globalThis.self = { location: { href: 'https://playbit.app/assets/fixture/ap-worker.js?channel=public' } };
            await import('./ap-worker.ts');
            const port = () => ({ messages: [], postMessage(message, transfer = []) { this.messages.push(structuredClone(message, { transfer })); }, start() {}, close() {} });
            const first = port(), second = port();
            self.onconnect({ ports: [first] });
            self.onconnect({ ports: [second] });
            const invoke = async (port, method, args = {}, account = null) => {
                await port.onmessage({ data: { id: 1, method, args, account } });
                return port.messages.at(-1);
            };
            assert.equal((await invoke(first, "begin", { operation })).result, 42);
            assert.match((await invoke(second, "streamRead", { handle: 42 })).error, /another tab/);
            assert.match((await invoke(second, "streamNamespaceEvidence", { handle: 42 })).error, /another tab/);
            assert.equal(reads, 0);
            assert.deepEqual((await invoke(first, "streamRead", { handle: 42, maxBody: 65536 })).result, Uint8Array.of(1));
            assert.equal(bound, 65536); assert.equal(returned.byteLength, 0, 'chunk ownership transfers without copying');
            await invoke(first, "connect");
            assert.match((await invoke(second, "status", {}, encodeId(new Uint8Array(16).fill(0xff)))).error, /another account/);
            await invoke(first, "detach");
            assert.equal(closes, 1, "detaching cancels this tab's active streams");
        } finally {
            for (const name of methods) ApConnection.prototype[name] = originals[name];
            if (originalSelf === undefined) delete globalThis.self; else globalThis.self = originalSelf;
        }
    } },
    { name: 'SharedWorker authenticates local account observers and ACKs without ordinary AP slots', async run() {
        const originalSelf = globalThis.self, connect = ApConnection.prototype.connect;
        const account = encodeId(new Uint8Array(16).fill(0xab));
        try {
            ApConnection.prototype.connect = async function () { this.account = account; return { account }; };
            globalThis.self = { location: { href: 'https://playbit.app/assets/fixture/ap-worker.js?channel=public' } };
            await import('./ap-worker.ts');
            const port = () => ({ messages: [], postMessage(message) { this.messages.push(message); }, start() {}, close() {} });
            const first = port(), second = port();
            self.onconnect({ ports: [first] }); self.onconnect({ ports: [second] });
            const invoke = (port, method, args = {}) => port.onmessage({ data: { id: 1, method, args } });
            await invoke(first, 'watchAccount', { account });
            assert.match(first.messages.at(-1).error, /another account/);
            await invoke(first, 'connect'); await invoke(first, 'watchAccount', { account });
            assert.equal(first.messages.filter(value => value.event === 'account').length, 1);
            await invoke(second, 'accountChanged', { account: encodeId(new Uint8Array(16).fill(0xcd)) });
            assert.match(second.messages.at(-1).error, /another account/);
            await invoke(second, 'accountChanged', { account });
            assert.equal(first.messages.filter(value => value.event === 'account').length, 1);
            await invoke(second, 'ackAccountNotice', { account, ticket: 1 });
            assert.equal(second.messages.at(-1).event, 'account-error');
            await invoke(first, 'ackAccountNotice', { account, ticket: 1 });
            assert.equal(first.messages.at(-1).ticket, 2);
            await invoke(first, 'detach'); const count = first.messages.length;
            await invoke(second, 'accountChanged', { account }); assert.equal(first.messages.length, count);
        } finally { ApConnection.prototype.connect = connect; globalThis.self = originalSelf; }
    } },
    { name: 'SharedWorker retires an invitation watch that opens after tab detach', async run() {
        const originalSelf = globalThis.self, methods = ["watchInvitation", "streamClose"];
        const originals = Object.fromEntries(methods.map(name => [name, ApConnection.prototype[name]]));
        const closed = [];
        let openWatch;
        try {
            ApConnection.prototype.watchInvitation = () => new Promise(resolve => { openWatch = resolve; });
            ApConnection.prototype.streamClose = handle => closed.push(handle);
            globalThis.self = { location: { href: 'https://playbit.app/assets/fixture/ap-worker.js?channel=public' } };
            await import('./ap-worker.ts');
            const port = { messages: [], postMessage(message) { this.messages.push(message); }, start() {}, close() {} };
            self.onconnect({ ports: [port] });
            const invoke = (id, method, args = {}) => port.onmessage({ data: { id, method, args } });
            const watch = invoke(5, "watchInvitation", { operation: { op: "watch_invitation" } });
            await invoke(6, "detach");
            const delivered = port.messages.length;
            openWatch(42); await watch;
            assert.deepEqual(closed, [42]);
            assert.equal(port.messages.length, delivered);
        } finally {
            for (const name of methods) ApConnection.prototype[name] = originals[name];
            if (originalSelf === undefined) delete globalThis.self; else globalThis.self = originalSelf;
        }
    } }
];
if (isMainThread) {
    for (const [index, fixture] of cases.entries()) test(fixture.name, async () => {
        const worker = new Worker(new URL(import.meta.url), { workerData: index });
        await new Promise((resolve, reject) => {
            worker.once('error', reject);
            worker.once('exit', code => code === 0 ? resolve() : reject(Error(`Worker exited with ${code}`)));
        });
    });
} else {
    await cases[workerData].run();
    parentPort.close();
}
