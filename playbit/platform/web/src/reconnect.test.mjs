import { compatibleWorkerStorage } from '../tests/worker-storage-fixture.mjs';
import { workerChannel } from './release-channel.ts';
import { encodeId, decodeId, validId } from './identifier.ts';
import { guestOperations } from "./guest-operations.ts";
import { FixtureWork } from "../tests/work-fixture.mjs";
import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import { stripTypeScriptTypes } from 'node:module';
import { readFile } from 'node:fs/promises';
import { retryable, failure } from './errors.ts';
import { AccountSignal } from './account-notices.ts';
const source = stripTypeScriptTypes(await readFile(new URL('./runtime-worker.ts', import.meta.url), 'utf8'), { mode: 'strip' });
const turn = () => new Promise(resolve => setImmediate(resolve));
function fixture() {
    const timers = new Map(), messages = [], calls = [];
    let id = 0, failConnect = 0, starts = 0, state, hold;
    class Replica {
        constructor(options) { state = options.state; }
        async start() { starts++; this.run = { stopped: false }; state({ kind: 'live' }); }
        async stop() { this.run = null; }
    }
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations, GuestWork: FixtureWork, retryable, AccountSignal, observeAccountMetadata() {},
        TextEncoder, TextDecoder, crypto, LiveReplication: Replica,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: value => messages.push(value) },
        setTimeout(callback, delay) { const key = ++id; timers.set(key, {callback,delay}); return key; },
        clearTimeout(key) { timers.delete(key); },
        request: async method => { calls.push(method); if (method === 'connect') {
            if (hold) await hold;
            if (failConnect-- > 0) throw failure('ap-transport', 'offline');
        } return {}; },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
        files = { cleanup: async () => {}, flushPublication() {} };
        retireFiles = async () => {};
        ap = request; managed = true; proof = 'fixture';
        host = { exports: { pb_session_authority: () => 0, pb_session_admit: () => 0,
            pb_replica_catalog_allowed: () => encoder.encode('true') } };
        hostInput = () => {}; hostOutput = value => ArrayBuffer.isView(value) ? value : encoder.encode('{}');
        profile = { root: {id:'a'},  session: {id:'c', resolved_build_id:'d'} };
        globalThis.owner = { connectReplication, replicationState,
            disconnect(value) { replicas.run.stopped = true; replicationState(value); },
            close() { self.onmessage({data:{method:'close',id:9}}); },
            block() { updateTask = Promise.resolve(); },
            unblock() { updateTask = null; if (recoveryPending) connectReplication(); },
        };
    `, context);
    return { owner: context.owner, timers, messages, calls, starts: () => starts,
        fail(count) { failConnect = count; }, hold(promise) { hold = promise; },
        async fire() { assert.equal(timers.size,1); const [key,timer] = timers.entries().next().value; timers.delete(key); timer.callback(); await turn(); return timer.delay; } };
}
test('transport failure recovers without an online event and retries cap at one 30s timer', async () => {
    const f = fixture(); await f.owner.connectReplication(); assert.equal(f.starts(),1);
    f.fail(9); f.owner.disconnect({kind:'reconnecting',code:'ap-transport'});
    const delays = []; for (let i=0;i<10;i++) delays.push(await f.fire());
    assert.deepEqual(delays,[250,500,1000,2000,4000,8000,16000,30000,30000,30000]);
    assert.equal(f.starts(),2); assert.equal(f.timers.size,0); assert.equal(f.messages.at(-1).replication.kind,'live');
    await f.owner.connectReplication(); assert.equal(f.starts(),2,'resume does not duplicate live subscriptions');
    f.owner.disconnect({kind:'reconnecting',code:'replica-gap'}); assert.equal(await f.fire(),250,'successful recovery resets backoff');
});
test('held connection coalesces wakeups and close prevents late subscriptions or retries', async () => {
    const f = fixture(); let release; f.hold(new Promise(resolve => release=resolve));
    const first = f.owner.connectReplication(); f.owner.connectReplication(); f.owner.connectReplication();
    assert.equal(f.calls.length,1); f.owner.close(); release(); await first; await turn();
    assert.equal(f.starts(),0); assert.equal(f.timers.size,0);
});
test('close cancels a scheduled reconnect and terminal failures never retry on resume', async () => {
    const f = fixture(); f.fail(1); await f.owner.connectReplication(); assert.equal(f.timers.size,1);
    f.owner.close(); await turn(); assert.equal(f.timers.size,0);
    for (const code of ['authentication-required','access-revoked','ap-rejected',undefined]) {
        const g=fixture(); await g.owner.connectReplication();
        g.owner.disconnect({kind: code ? 'action_required':'failed',code,message:'specific failure'});
        await g.owner.connectReplication(); assert.equal(g.starts(),1); assert.equal(g.timers.size,0);
        assert.equal(g.messages.at(-1).replication.message,'specific failure');
    }
});
test('a retry deferred by app update is retained until the update retires', async () => {
    const f=fixture(); await f.owner.connectReplication();
    f.owner.disconnect({kind:'reconnecting',code:'ap-transport'}); f.owner.block();
    await f.fire(); assert.equal(f.starts(),1); assert.equal(f.timers.size,0);
    f.owner.unblock(); await turn(); assert.equal(f.starts(),2);
});

test('runtime AP replies preserve transport classification through the second worker boundary', async () => {
    const replies=[];
    const context=vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations,GuestWork:FixtureWork,AccountSignal,observeAccountMetadata(){},TextEncoder,TextDecoder,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" },postMessage:message=>replies.push(message)}});
    vm.runInContext(source.replace(/^import .*;$/gm,'')+'\nglobalThis.request=ap;',context);
    const pending=context.request('connect');
    const id=replies[0].ap.id;
    context.self.onmessage({data:{apReply:{id,error:'Connection lost',code:'ap-transport'}}});
    await assert.rejects(pending,error=>error.code==='ap-transport');
});
