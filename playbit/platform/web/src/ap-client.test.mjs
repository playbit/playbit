import { encodeId } from './identifier.ts';
import test from 'node:test';
import assert from 'node:assert/strict';
import { ApClient } from './ap-client.ts';
const turn = () => new Promise(resolve => setImmediate(resolve));

test('account notice waits for its reader, ACKs despite full AP admission, and reports worker failure', async () => {
    const SharedWorker = globalThis.SharedWorker, messages = [], failures = [], account = encodeId(new Uint8Array(16).fill(0xab));
    const port = { postMessage(value) { messages.push(value); }, start() {}, close() {} };
    globalThis.SharedWorker = class { constructor() { this.port = port; } };
    let client;
    try {
        client = new ApClient('public', account);
        let release;
        const hold = new Promise(resolve => { release = resolve; });
        const watching = client.watchAccount(account, () => hold, error => failures.push(error.message));
        const watch = messages[0];
        port.onmessage({ data: { event: 'account', account, ticket: 1 } });
        port.onmessage({ data: { id: watch.id, result: null } });
        await watching; await turn(); assert.equal(messages.length, 1);
        const ordinary = Array.from({ length: 32 }, () => client.status().catch(error => error.message));
        const profile = client.request({ op: 'put_account_display_name', operation_id: account });
        const discovery = client.describe();
        const write = client.channelDisplayName(7, new Uint8Array(256));
        assert.equal(client.describe(), discovery);
        const advisory = messages.slice(-3);
        await assert.rejects(client.status(), /Too many/);
        await assert.rejects(client.request({ op: 'put_account_display_name', operation_id: account }), { code: 'ap-capacity' });
        for (const request of advisory) port.onmessage({ data: { id: request.id, result: null } });
        await Promise.all([profile, discovery, write]);
        const controls = Array.from({ length: 3 }, () => client.accountChanged(account));
        const changed = messages.slice(-3);
        const consumed = client.channelConsumed([17, 18]);
        const receipt = messages.at(-1);
        assert.equal(receipt.method, 'channelConsumed', 'credits have capacity beside ordinary requests and account controls');
        await assert.rejects(client.channelConsumed([19]), /Too many/, 'only one credit batch may be pending');
        port.onmessage({ data: { id: receipt.id, result: null } });
        await consumed;
        await assert.rejects(client.accountChanged(account), /Too many/);
        for (const request of changed) port.onmessage({ data: { id: request.id, result: null } });
        await Promise.all(controls);
        release(); await turn();
        assert.equal(messages.at(-1).method, 'ackAccountNotice');
        assert.equal(messages.at(-1).args.ticket, 1);
        assert.equal(client.pending.size, 32);
        client.fail(Error('worker disconnected'));
        await Promise.all(ordinary);
        assert.deepEqual(failures, ['worker disconnected']);
        const again = client.watchAccount(account, () => { throw Error('read failed'); }, error => failures.push(error.message));
        port.onmessage({ data: { id: messages.at(-1).id, result: null } }); await again;
        port.onmessage({ data: { event: 'account', account, ticket: 2 } }); await turn();
        assert.equal(messages.at(-1).args.ticket, 2);
        assert.equal(failures.at(-1), 'read failed');
        client.close(); const count = messages.length;
        await assert.rejects(client.status(), /closed/); client.close(); assert.equal(messages.length, count);
    } finally { client?.close(); globalThis.SharedWorker = SharedWorker; }
});

test('AP error classification, stable service code and stack survive the SharedWorker reply', async () => {
    const original = globalThis.SharedWorker;
    const stack = 'Error: Connection lost\n    at ApConnection.begin (ap-worker.js:430:15)';
    const port = { start() {}, close() {}, postMessage(value) {
        if (value.method === 'detach') return;
        queueMicrotask(() => this.onmessage({data:{id:value.id,error:'Connection lost',code:'ap-transport',serviceCode:'future_failure',stack}}));
    } };
    globalThis.SharedWorker = class { constructor() { this.port=port; } };
    let client;
    try { client=new ApClient('public'); await assert.rejects(client.connect(), {code:'ap-transport',serviceCode:'future_failure',message:'Connection lost',stack}); }
    finally { client?.close(); globalThis.SharedWorker=original; }
});
