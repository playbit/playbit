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
import { base64url } from './recovery.ts';
const source = stripTypeScriptTypes(await readFile(new URL('./runtime-worker.ts', import.meta.url), 'utf8'), { mode: 'strip' });
const turn = () => new Promise(resolve => setImmediate(resolve));
function fixture() {
    const timers = new Map(), messages = [], calls = [];
    let id = 0, failConnect = 0, starts = 0, state, hold;
    class Replica {
        constructor(options) { state = options.state; this.local = options.local; }
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
    return { context, owner: context.owner, timers, messages, calls, starts: () => starts,
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

test('central name retries leave account publication and guest renames live', async () => {
    const f = fixture(), sent = [], delivered = [];
    let started = Promise.withResolvers();
    await f.owner.connectReplication();
    Object.assign(f.context, { base64url, personalSelections: async () => ({}), deliver: name => delivered.push(name),
        put: async (method, request) => {
            const receipt = Promise.withResolvers(); sent.push({ method, request, receipt });
            started.resolve(sent.at(-1)); started = Promise.withResolvers(); await receipt.promise;
        } });
    vm.runInContext(`
        host.exports.pb_live_peek = () => 0;
        let adopted;
        accountReplica = { apply: async version => { adopted = version; return true; }, displayNameRecord: async () => adopted,
            head: async () => ({ source: 'account' }) };
        refreshPersonal = async () => { displayName = adopted.name; deliver(displayName); };
        refreshScene = async () => {}; ap = put;
        globalThis.receive = version => {
            const run = replicas.run;
            return replicas.local.apply('account', 'index', version, () => !run.stopped);
        };
        globalThis.publish = () => replicas.local.head(() => !replicas.run.stopped);
        globalThis.retire = () => { replicas.run.stopped = true; replicas.local.retired(); };
        globalThis.resume = () => { replicas.run = { stopped: false }; };
    `, f.context);
    const key = new Uint8Array(32).fill(7);
    const version = i => ({ name: 'Name ' + i, key, clock: BigInt(i), ciphertext: Uint8Array.of(255, i) });
    const first = started.promise; await f.context.receive(version(1));
    const { method, request, receipt } = await first;
    assert.equal(method, 'request'); assert.ok(validId(request.operation_id, 16));
    assert.deepEqual({ ...request }, { op: 'put_account_display_name', operation_id: request.operation_id,
        name: 'Name 1', record_key: encodeId(key), clock: '1', ciphertext: '_wE' });
    await f.context.receive(version(2)); await f.context.receive(version(3));
    assert.deepEqual(delivered, ['Name 1', 'Name 2', 'Name 3']); assert.equal(sent.length, 1);
    assert.equal((await f.context.publish()).source, 'account');
    receipt.reject(Object.assign(failure('ap-rejected', 'Profile store busy'), { serviceCode: 'operation_failed' }));
    await turn(); assert.equal((await f.context.publish()).source, 'account');
    const retry = started.promise; assert.equal(await f.fire(), 250);
    const latest = await retry; assert.equal(latest.request.name, 'Name 3');
    latest.receipt.reject(failure('ap-transport', 'Disconnected')); await turn();
    const legacy = started.promise; assert.equal(await f.fire(), 500);
    (await legacy).receipt.reject(Object.assign(failure('ap-rejected', 'Older service'), { serviceCode: 'unsupported_operation' }));
    await turn(); await f.context.receive(version(4)); assert.equal(f.timers.size, 0);
    assert.equal(sent.length, 3); assert.equal((await f.context.publish()).source, 'account');
    f.context.retire(); f.context.resume();
    const old = started.promise; await f.context.receive(version(5)); const retired = await old;
    f.context.retire(); f.context.resume();
    const next = started.promise; await f.context.receive(version(6)); assert.equal(sent.length, 4);
    retired.receipt.reject(failure('ap-transport', 'Retired'));
    const active = await next; assert.equal(active.request.name, 'Name 6'); assert.equal(f.timers.size, 0);
    active.receipt.reject(failure('ap-transport', 'Unavailable')); await turn(); assert.equal(f.timers.size, 1);
    f.context.retire(); assert.equal(f.timers.size, 0);
});

test('runtime AP replies preserve transport and service codes through the second worker boundary', async () => {
    const replies=[];
    const context=vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations,GuestWork:FixtureWork,AccountSignal,observeAccountMetadata(){},TextEncoder,TextDecoder,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" },postMessage:message=>replies.push(message)}});
    vm.runInContext(source.replace(/^import .*;$/gm,'')+'\nglobalThis.request=ap;',context);
    const pending=context.request('connect');
    const id=replies[0].ap.id;
    context.self.onmessage({data:{apReply:{id,error:'Connection lost',code:'ap-transport'}}});
    await assert.rejects(pending,error=>error.code==='ap-transport');
    const unsupported=context.request('request');
    context.self.onmessage({data:{apReply:{id:replies[1].ap.id,error:'Older service',code:'ap-rejected',serviceCode:'unsupported_operation'}}});
    await assert.rejects(unsupported,error=>error.code==='ap-rejected' && error.serviceCode==='unsupported_operation');
    const ordinary = Array.from({ length: 64 }, () => context.request('status'));
    const profile = context.request('request', { op: 'put_account_display_name' }), discovery = context.request('describe');
    const write = context.request('channelDisplayName', 7, new Uint8Array(256));
    await assert.rejects(context.request('status'), /capacity/);
    for (const { ap } of replies.slice(-67)) context.self.onmessage({data:{apReply:{id:ap.id,result:{}}}});
    await Promise.all([...ordinary, profile, discovery, write]);
});
