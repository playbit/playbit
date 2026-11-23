import { compatibleWorkerStorage as baseWorkerStorage } from '../tests/worker-storage-fixture.mjs';
import { MAX_TEXT_EVENT_BYTES } from './text-input.ts';
const compatibleWorkerStorage = () => ({ ...baseWorkerStorage(), MAX_TEXT_EVENT_BYTES });
import { AbiError } from "./guest-memory.ts";
import { ChannelHost } from "./channel-host.ts";
import { workerChannel } from './release-channel.ts';
import { encodeId, decodeId, validId } from './identifier.ts';
import { fixture } from '../tests/file-io-fixture.mjs';
import { envelopeLength } from "./blob.ts";
import { FixtureWork } from "../tests/work-fixture.mjs";
import { bootstrap, createHost, key, File } from "../tests/host-fixture.mjs";
import { GuestWork } from "./guest-work.ts";
import { GuestDeadline, clockNs } from "./guest-deadline.ts";
import { bindOperations, supportsDeadline } from "./guest-imports.ts";
import { guestOperations } from "./guest-operations.ts";
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import { AccountSignal } from "./account-notices.ts";
import { corruptStoreOpenMessage, retryable } from "./errors.ts";
import { resolvedSession } from "./updates.ts";
import { validateSelection, MAX_PERSONAL_VOLUMES } from "./personal.ts";
import { replicaJSON } from "./replication.ts";
import { VolumePublication, LocalVolumeChanged, PublicationUncertain } from './volume-publication.ts';
import { OpfsStore, OpfsResources } from './opfs-store.ts';
import { replicaAddress, replicaSource } from './live-replication.ts';
import { validDatabase } from './database-id.ts';
import { retainRejectedCut } from './volume-archive.ts';
import { writePackage, MAX_PACKAGE_WASM_BYTES } from './package-stream.ts';

// These lifecycle fixtures provision no storage. Metadata snapshot I/O is
// covered by the actual-host adapter tests; keep it outside these mock turns.
function lifecycleOperations(services) {
    const operations = guestOperations(services);
    operations.refreshVolumes = () => {};
    return operations;
}

const source = stripTypeScriptTypes(await readFile(new URL("./runtime-worker.ts", import.meta.url), "utf8"), { mode: "strip" });
const named = encodeId(new Uint8Array(15).fill(0x13));
const current = encodeId(new Uint8Array(32).fill(0x11)), next = encodeId(new Uint8Array(32).fill(0x22));
const session = { id: encodeId(new Uint8Array(32).fill(0x33)), selector: { kind: "app", app: {publisher: encodeId(new Uint8Array(16).fill(0x7a)), app_id: "todo"} }, resolved_build_id: current };
const resolution = { revision: 1, selector: session.selector, resolved_build_id: next };
const snapshotReceipt = (epoch = 1n, generation = encodeId(new Uint8Array(16).fill(0x11))) =>
    ({ root: null, revision: 1n, policy_revision: 1n, source: { generation, epoch } });

for (const initial of [false, true]) test(`shared backing opening (${initial ? 'initial queue owner' : 'live runtime'})`, async () => {
    // Share an exclusive opening both during initial queue ownership and after
    // initialization, when each host access must enter the local queue.
    let release, opened = 0, registered = 0;
    const held = new Promise(resolve => { release = resolve; });
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, validDatabase,
        guestOperations: lifecycleOperations, GuestWork: FixtureWork, AccountSignal, observeAccountMetadata() {},
        TextEncoder, TextDecoder, crypto, self: { location: { href: 'https://playbit.app/runtime-worker.js?channel=public' } },
        openFile: async () => { opened++; if (opened > 1) throw Error('exclusive OPFS handle already open'); await held; return {}; },
        register: () => { registered++; return 0; },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
        const volume = {id:encodeId(new Uint8Array(16).fill(0x10))}, generation = encodeId(new Uint8Array(16).fill(0x20));
        hostInput = () => {};
        hostOutput = value => ArrayBuffer.isView(value) ? value : new Uint8Array();
        host = {exports:{pb_database_make_room:()=>0,
            pb_database_backing:()=>encoder.encode(JSON.stringify({ready:true,generation})),pb_register_database:register}};
        publication = {current:()=>generation,epoch:()=>({}),file:async()=>({createSyncAccessHandle:openFile})};
        openStore = async()=>({close(){}});
        globalThis.open = ()=>openNamed(volume,${JSON.stringify(named)});
        globalThis.initialize = ()=>localStep(async()=>{
            initializing=true;
            try{return await openNamed(volume,${JSON.stringify(named)});}
            finally{initializing=false;}
        });
    `, context);
    const first = initial ? context.initialize() : context.open();
    await new Promise(setImmediate);
    assert.equal(opened, 1, 'opening must not wait on a queue turn it already owns');
    const second = context.open();
    // Observe both failures immediately: the regression must not leak an
    // unhandled rejection while the first exclusive opening is suspended.
    const done = Promise.allSettled([first, second]);
    await new Promise(setImmediate); release();
    const result = await done;
    assert.equal(opened, 1);
    assert.equal(registered, 1);
    assert.equal(result[0].status, 'fulfilled');
    assert.deepEqual(result[1], result[0]);
});

test('retiring live delivery does not fail shared guest preparation or Close', async () => {
    // Hold the live caller's file lookup while the guest joins its opening,
    // then retire only replication. Exercise real Rust Work accounting at Take.
    let release, execute, lookups = 0, opened = 0, registered = 0, applied = 0;
    const replies = [];
    const held = new Promise(resolve => { release = resolve; });
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, validDatabase,
        guestOperations(services) { execute = services.execute; return lifecycleOperations(services); },
        GuestWork: FixtureWork, AccountSignal, observeAccountMetadata() {},
        TextEncoder, TextDecoder, crypto, clearTimeout, replicaAddress, LocalVolumeChanged,
        self: { location: { href: 'https://playbit.app/runtime-worker.js?channel=public' }, postMessage: message => replies.push(message) },
        LiveReplication: class {
            constructor(options) { this.local = options.local; }
            async start() { this.run = { stopped: false }; }
            async stop() { this.run.stopped = true; }
        },
        lookup: async () => { lookups++; await held; return { createSyncAccessHandle: async () => { opened++; return {}; } }; },
        register: () => { registered++; return 0; }, apply: () => { applied++; return 0; },
        execute: callback => execute(callback),
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
        const volume = {id:encodeId(new Uint8Array(16).fill(0x10))}, generation = encodeId(new Uint8Array(16).fill(0x20));
        const address = {volume:volume.id,database:${JSON.stringify(named)}};
        let current = true, needed = true, failure = 0, input, operation;
        profile = {root:volume,session:${JSON.stringify(session)}}; managed = true;
        hostInput = bytes => { input = bytes; };
        hostOutput = value => ArrayBuffer.isView(value) ? value : new Uint8Array();
        host = {exports:{pb_database_make_room:()=>0,
            pb_database_backing:()=>encoder.encode(JSON.stringify({ready:true,generation})),pb_register_database:register,
            pb_database_needed:()=>{const result=needed?[address]:[];needed=false;return encoder.encode(JSON.stringify(result));},
            pb_database_preparation:()=>encoder.encode('"local"'),
            pb_database_failed:()=>{failure=JSON.parse(decoder.decode(input)).status;return 0;},pb_live_apply:apply}};
        publication = {current:()=>generation,epoch:()=>({}),file:lookup};
        openStore = async()=>({close(){}});
        ap = async()=>({}); openSession = async()=>{}; catalogReplicaAllowed = async()=>true;
        refreshScene = async()=>{};
        globalThis.connect = connectReplication;
        globalThis.receive = ()=>replicas.local.apply(volume.id,address.database,new Uint8Array([1]),()=>current);
        globalThis.prepare = ()=>{
            guestWork.beginTurn();
            operation=operations.data.begin(4,16,()=>execute(()=>({
                status:failure||(namedSlots.has(volume.id+'/'+address.database)?0:-6),value:{}})),()=>0);
            guestWork.endTurn();
        };
        globalThis.retire = ()=>{current=false;replicas.run.stopped=true;};
        globalThis.preparation = ()=>databasePreparations;
        globalThis.waiting = ()=>preparingDatabases.size;
        globalThis.take = ()=>{
            guestWork.beginTurn();
            while(events.length)guestWork.poll();
            const status=operations.data.take(1n,operation,4,()=>0);
            guestWork.endTurn();
            return {status,failed:guestWork.failed,pending:guestWork.pending};
        };
        retireFiles = async()=>{}; closeStorage = ()=>{};
        globalThis.close = ()=>self.onmessage({data:{id:41,method:'close',args:{}}});
    `, context);
    await context.connect();
    const delivery = Promise.allSettled([context.receive()]);
    await new Promise(setImmediate);
    context.prepare();
    await new Promise(setImmediate);
    assert.equal(context.waiting(), 1);
    context.retire(); release();
    await context.preparation(); await delivery;
    await new Promise(setImmediate);
    const result = context.take();
    assert.deepEqual({ ...result }, { status: 0, failed: false, pending: false });
    context.close(); await new Promise(setImmediate);
    assert.equal(replies.find(reply => reply.id === 41)?.result?.closed, true);
    assert.equal(lookups, 1); assert.equal(opened, 1); assert.equal(registered, 1);
    assert.equal(applied, 0, 'a retired delivery never applies its remote change');
});

test('channel credits survive a rejected acknowledgment and retry on AP progress',async()=>{
    const messages=[];
    const context=vm.createContext({ ...compatibleWorkerStorage(),workerChannel,encodeId,decodeId,validId,guestOperations:lifecycleOperations,GuestWork:FixtureWork,
        AccountSignal,observeAccountMetadata(){},TextEncoder,TextDecoder,crypto,MessageChannel,
        self:{location:{href:"https://playbit.app/runtime-worker.js?channel=public"},postMessage(message){messages.push(message);}}});
    vm.runInContext(source.replace(/^import .*;$/gm,'')+`
        globalThis.consume=consumeChannelReceipt;
        globalThis.retained=()=>[...consumedChannelReceipts];
        globalThis.connect=()=>ap('connect');
    `,context);
    const turn=()=>new Promise(resolve=>setImmediate(resolve));
    const reply=(request,result,error)=>context.self.onmessage({data:{apReply:{id:request.id,result,error}}});
    context.consume(1);context.consume(2);await turn();
    const first=messages.at(-1).ap;
    assert.deepEqual(Array.from(first.args[0]),[1,2]);
    reply(first,undefined,'temporary admission pressure');await turn();
    assert.deepEqual(Array.from(context.retained()),[1,2]);
    assert.equal(messages.length,1,'a failed ACK does not spin');
    const connected=context.connect(),connection=messages.at(-1).ap;
    reply(connection,{});await connected;await turn();
    const retry=messages.at(-1).ap;
    assert.equal(retry.method,'channelConsumed');assert.deepEqual(Array.from(retry.args[0]),[1,2]);
    context.consume(3);
    reply(retry,undefined);await turn();
    const remaining=messages.at(-1).ap;
    assert.deepEqual(Array.from(remaining.args[0]),[3]);
    reply(remaining,undefined);await turn();
    assert.deepEqual(Array.from(context.retained()),[]);
});

test('blob transport binds every captured body field for personal and session requests', () => {
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork,
        AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, MessageChannel,
        envelopeLength, self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage() {} },
        RuntimeBlobs: class { constructor(services) { this.services = services; } },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        profile = { session: { id: encodeId(new Uint8Array(32).fill(0x11)), resolved_build_id: encodeId(new Uint8Array(32).fill(0x22)) } };
        const transport = initializeBlobs();
        globalThis.request = (reference, upload, personal) => {
            personalSlots.clear();
            if (personal) personalSlots.set(reference.volume, 1);
            return transport.services.operation(reference, upload);
        };
        globalThis.close = () => transport.services.stop();
    `, context);
    const reference = { volume: encodeId(new Uint8Array(16).fill(0x12)), entry: encodeId(new Uint8Array(15).fill(0x13)), revision: encodeId(new Uint8Array(15).fill(0x14)),
        len_bytes: 123, digest: 'ab'.repeat(32), encrypted_key: 'cd'.repeat(32) };
    try {
        for (const personal of [false, true]) for (const upload of [false, true]) {
            const request = JSON.parse(JSON.stringify(context.request(reference, upload, personal)));
            assert.equal(request.op, upload ? 'upload_blob' : 'download_blob');
            assert.equal(request.session_id, personal ? null : encodeId(new Uint8Array(32).fill(0x11)));
            assert.equal(request.build_id, personal ? null : encodeId(new Uint8Array(32).fill(0x22)));
            assert.deepEqual(request.version, { address: { volume: reference.volume, entry: reference.entry },
                revision: reference.revision, len_bytes: reference.len_bytes,
                digest: Array(32).fill(0xab), encrypted_key: Array(32).fill(0xcd) });
            assert.equal(request.content_length, upload ? envelopeLength(reference) : undefined);
        }
    } finally { context.close(); }
});

test('worker authorization withdrawal during blob preparation preserves AccessDenied', async () => {
    // Exercise the actual worker's hostStatus boundary while a retained read is suspended.
    const f = await fixture();
    let allowed = true;
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork,
        AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, MessageChannel,
        envelopeLength, self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage() {} },
        RuntimeBlobs: class { constructor(services) { this.services = services; } },
        testHost: { exports: { memory: new WebAssembly.Memory({ initial: 1 }),
            pb_input: () => 8, pb_output: () => 8, pb_error: () => 0,
            pb_blob_authorize: () => allowed ? 0 : -2 } },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
        host = testHost;
        const transport = initializeBlobs();
        globalThis.authorize = transport.services.authorize;
        globalThis.close = () => transport.services.stop();
    `, context);
    try {
        f.services.authorize = context.authorize;
        const reference = { ...f.seed(f.file(1), new Uint8Array(70000).fill(4)), limit: 4 };
        const hold = f.hold(), read = f.blobs.readVersion(reference);
        await new Promise(resolve => setImmediate(resolve)); allowed = false; hold.resolve();
        const result = await read;
        assert.equal(result.status, -2); assert.equal(result.bytes, undefined);
        assert.equal(f.handles[0].fd, undefined);
        assert.equal((await f.blobs.readVersion(reference)).status, -2);
        allowed = true; f.blobs.retry();
        assert.equal((await f.blobs.readVersion(reference)).status, 0);
    } finally { context.close(); f.close(); }
});

test('append waits for its one remote attempt and preserves success when caching fails', async () => {
    for (const mode of ['transport', 'cache', 'success', 'conflict']) {
        const publications = [];
        let release, entered;
        const waiting = new Promise(resolve => { release = resolve; });
        const sending = new Promise(resolve => { entered = resolve; });
        const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork,
            AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, AbiError,
            self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage() {} },
            sent: async (operation, bytes, generation) => { publications.push({ operation, bytes: bytes.slice(), generation }); entered(); await waiting; }, mode,
        });
        vm.runInContext(source.replace(/^import .*;$/gm, '') + `
            files = { cleanup: async () => {}, flushPublication() {} };
            let cached = 0;
            profile = { session: { id: encodeId(new Uint8Array(32).fill(0x22)), resolved_build_id: encodeId(new Uint8Array(32).fill(0x33)) } };
            replicas = { run: { active: true, stopped: false } };
            hostInput = () => {}; hostOutput = bytes => bytes;
            host = { exports: { pb_live_peek: () => 0,
                pb_bloblog_cache_append: () => { if (mode === 'cache') throw Error('Cache failed'); cached++; return new Uint8Array(); },
            } };
            const prepared = new Uint8Array(40); prepared.fill(0x10, 0, 16); prepared.fill(0x11, 16, 31);
            prepared[31] = 1; prepared[32] = 120; prepared.set([1, 0, 0, 0, 9, 8, 7], 33);
            const appendId = new Uint8Array(16).fill(0x44);
            ap = async (method, operation, bytes, generation) => {
                if (method === 'status') return { connected: true, connectionCount: 3 };
                if (method !== 'publishReplica') throw Error('Unexpected transport operation');
                await sent(operation, bytes, generation);
                if (mode === 'transport') throw Error('Lost response');
                if (mode === 'conflict') throw Object.assign(Error('Conflict'), { serviceCode: 'operation_conflict' });
                return { database_clock: 1n, policy_revision: 1n };
            };
            globalThis.append = async () => appendLog(prepared, appendId, await logConnection());
            globalThis.edit = () => localStep(() => 'guest edit completed');
            globalThis.cached = () => cached;
        `, context);
        let completed = false;
        const append = context.append().finally(() => { completed = true; });
        await sending;
        assert.equal(completed, false);
        assert.equal(await context.edit(), 'guest edit completed');
        release();
        if (mode === 'transport' || mode === 'conflict') await assert.rejects(append, error => error.status === (mode === 'transport' ? -15 : -10));
        else await append;
        assert.equal(publications.length, 1);
        assert.equal(context.cached(), mode === 'success' ? 1 : 0);
        assert.equal(publications[0].operation.append_id, encodeId(new Uint8Array(16).fill(0x44)));
        assert.equal(publications[0].generation, 3);
        if (mode === 'transport') {
            await assert.rejects(context.append(), error => error.status === -15);
            assert.equal(publications.length, 2);
            assert.deepEqual(publications[1].bytes, publications[0].bytes);
            assert.equal(publications[1].operation.append_id, publications[0].operation.append_id);
        }
    }
});

test('offline discovery bounds pages, keeps guest work running and fences changed authority', async () => {
    for (const changed of [false, true]) {
        const messages = [], cursors = [];
        let release, opens = 0, closes = 0;
        const waiting = new Promise(resolve => { release = resolve; });
        const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork,
            AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto,
            self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) },
            async openRecoveryIndex(...args) {
                if (++opens === 1) await waiting;
                args.at(-1)();
                return { close() { closes++; } };
            },
            recordCursor: cursor => cursors.push(cursor),
        });
        vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
            let revision = 0, input;
            publication = { epoch: () => ({ capture: () => revision, check: expected => {
                if (revision !== expected) throw new Error('Authority changed');
            } }) };
            storageResources = {}; directory = {}; blobHash = () => {};
            hostInput = bytes => { input = JSON.parse(decoder.decode(bytes)); };
            hostOutput = value => value;
            host = { exports: {
                pb_recovery_cut: () => encoder.encode(JSON.stringify({ generation: encodeId(new Uint8Array(16).fill(0x55)) })),
                pb_recovery_entries: () => {
                    recordCursor(input.after);
                    const first = input.after === null;
                    return encoder.encode(JSON.stringify({ entries: Array.from({length: first ? 64 : 40}, (_, i) => ({i})),
                        next: first ? { cut: encodeId(new Uint8Array(16).fill(0x55)), offset: 63 } : null }));
                },
            } };
            globalThis.read = id => offlineEntries({ id, method:'offline_entries', args:{volume:encodeId(new Uint8Array(16).fill(0x10)),after:null} });
            globalThis.edit = () => localStep(() => 'guest work completed');
            globalThis.change = () => { revision++; };
            globalThis.handles = () => stores.size;
        `, context);
        const pending = context.read(1);
        await new Promise(setImmediate);
        await context.read(2);
        assert.match(messages.find(message => message.id === 2).error, /already being read/);
        assert.equal(await context.edit(), 'guest work completed');
        if (changed) context.change();
        release(); await pending;
        const reply = messages.find(message => message.id === 1);
        if (changed) {
            assert.match(reply.error, /Authority changed/);
            assert.equal(reply.result, undefined);
            assert.deepEqual(cursors, []);
        } else {
            assert.equal(reply.result.entries.length, 104);
            assert.equal(reply.result.next, encodeId(new Uint8Array(16).fill(0x55)));
            assert.deepEqual(cursors, [null, 63]);
            assert.equal(closes, 2);
        }
        assert.equal(context.handles(), 0);
    }
});

test('offline restore yields to guest work, fences edits and retains bodies after uncertain publication', async () => {
    for (const mode of ['success', 'changed', 'copy-failure', 'publish-failure', 'directory']) {
        const messages = [], effects = [];
        let release, entered;
        const waiting = new Promise(resolve => { release = resolve; });
        const copying = new Promise(resolve => { entered = resolve; });
        const store = () => { let closed = false; return { close() { if (!closed) effects.push('close'); closed = true; } }; };
        const destination = {
            async getFileHandle(name, options) {
                if (!options?.create) throw Object.assign(new Error('missing'), { name: 'NotFoundError' });
                return { createSyncAccessHandle: async () => ({}) };
            },
            async removeEntry() { effects.push('remove'); },
        };
        const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork,
            AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, console,
            self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) },
            openRecoveryIndex: async () => store(), openRecoveryDatabase: async () => { assert.notEqual(mode, 'directory'); return store(); },
            recoveryStore: store, destination, mode, effect: value => effects.push(value),
            pause: () => { entered(); return waiting; },
        });
        vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
            let revision = 0, input, pages = 0;
            publication = { current: () => encodeId(new Uint8Array(16).fill(0x44)), directory: () => 'generation',
                epoch: () => ({ capture: () => revision, check: expected => {
                    if (revision !== expected) throw new Error('Authority changed');
                } }) };
            storageResources = {}; directory = { getDirectoryHandle: async () => destination };
            blobHash = () => {}; openStore = async () => recoveryStore();
            yieldPreparation = pause; refreshScene = async () => {};
            hostInput = bytes => { input = JSON.parse(decoder.decode(bytes)); };
            hostOutput = value => value;
            host = { exports: {
                pb_recovery_cut_get: () => encoder.encode(JSON.stringify({ generation: encodeId(new Uint8Array(16).fill(0x55)) })),
                pb_recovery_begin: () => encoder.encode(JSON.stringify({ entry: encodeId(new Uint8Array(15).fill(0x66)), blob: null, database: mode !== 'directory' })),
                pb_recovery_backing: () => new Uint8Array(),
                pb_recovery_copy: () => {
                    if (++pages === 1) return encoder.encode('false');
                    if (mode === 'copy-failure') throw new Error('Copy failed');
                    return encoder.encode('true');
                },
                pb_recovery_publish: () => {
                    effect('publish');
                    if (mode === 'publish-failure') throw new Error('Commit uncertain');
                    revision++; return encoder.encode(JSON.stringify(encodeId(new Uint8Array(15).fill(0x66))));
                },
                pb_recovery_abort: () => { effect('abort'); return 0; },
            } };
            globalThis.restore = () => offlineRestore({ id:1, method:'offline_restore', args:{volume:encodeId(new Uint8Array(16).fill(0x10)),cut:encodeId(new Uint8Array(16).fill(0x55)),entry:encodeId(new Uint8Array(15).fill(0x77)),name:'Recovered/data'} });
            globalThis.edit = () => localStep(() => 'guest work completed');
            globalThis.change = () => { revision++; };
            globalThis.handles = () => stores.size;
        `, context);
        const restoring = context.restore(); if (mode !== 'directory') await copying;
        assert.equal(await context.edit(), 'guest work completed');
        if (mode === 'changed') context.change();
        release(); await restoring;
        const reply = messages.find(message => message.id === 1);
        assert.equal(context.handles(), 0);
        assert.equal(effects.filter(effect => effect === 'close').length, mode === 'directory' ? 1 : 3);
        if (mode === 'success' || mode === 'directory') {
            assert.equal(reply.result.entry, encodeId(new Uint8Array(15).fill(0x66))); assert(!effects.includes('abort'));
        } else {
            assert(reply.error); assert(effects.includes('abort'));
            assert.equal(effects.includes('remove'), mode !== 'publish-failure');
        }
        assert.equal(effects.includes('publish'), mode === 'success' || mode === 'publish-failure' || mode === 'directory');
    }
});

// Coordinator fixtures have no storage. Compiled-host tests cover durable
// admission generations; preserve the new calls here without modeling them.
function sessionAdmission(context) {
    vm.runInContext(`
        host ??= { exports:{} };
        host.exports.pb_session_authority = () => encoder.encode('{"fixtureAdmission":true}');
        host.exports.pb_session_admit = () => 0;
        const admissionInput = hostInput, admissionOutput = hostOutput;
        hostInput = bytes => {
            const value = JSON.parse(decoder.decode(bytes));
            if (value?.admission?.fixtureAdmission || (value?.id && value.selector)) return;
            admissionInput(bytes);
        };
        hostOutput = value => ArrayBuffer.isView(value) ? value : admissionOutput(value);
    `, context);
}

// Keep AP and disk waits controllable while running the actual bootstrap and
// source-cut coordinator. Compiled host/pointer admission is covered separately
// by host-publication.test.mjs; these fakes make no storage atomicity claim.
function installPipeline(context, { controls = false } = {}) {
    Object.assign(context, { LocalVolumeChanged, PublicationUncertain, replicaJSON, retainRejectedCut });
    vm.runInContext(`
        let preparedVolume, preparedSelection, preparedBase, namedSlot, sourceReceipt, pendingInput;
        const receivedSlots = new Set(), syncBeforeReceipt = syncDatabase;
        syncDatabase = async (...args) => {
            receivedSlots.delete(args[6]);
            return syncBeforeReceipt(...args);
        };
        publication = { prepare: volume => ({ volume, generation:'fresh', previous:'old', epoch:{} }),
            create: async () => {}, discard: async () => {}, check() {},
            reclaimed: [], async reclaim(_root, volume) { this.reclaimed.push(volume); },
            publish(_prepared, validate, activate) { validate(); activate(); } };
        host ??= { exports:{} };
        Object.assign(host.exports, {
            pb_authority_refresh_required: () => encoder.encode('false'),
            pb_replica_catalog_allowed: () => encoder.encode('true'),
            pb_volume_admission: () => encoder.encode('{}'), pb_volume_prepare: () => { sourceReceipt = undefined; receivedSlots.clear(); return 0; },
            pb_volume_qualify: () => {
                if (!receivedSlots.has(preparedBase) || !receivedSlots.has(preparedBase + 1)) throw new Error('Missing final control receipt');
                return host.exports.pb_volume_receipt();
            },
            pb_volume_receipt: () => {
                if (pendingInput.slot !== undefined) receivedSlots.add(pendingInput.slot);
                const receipt = pendingInput.receipt ?? pendingInput;
                if (sourceReceipt && (receipt.source.generation !== sourceReceipt.source.generation
                    || receipt.source.epoch !== sourceReceipt.source.epoch || receipt.policy_revision !== sourceReceipt.policy_revision)) {
                    throw Object.assign(new Error('Volume source changed'), {hostStatus:-10});
                }
                sourceReceipt = receipt; return 0;
            },
            pb_volume_register_backing: () => 0, pb_volume_validate: () => 0,
            pb_volume_rejected_page: () => encoder.encode('[[],null]'),
            pb_volume_publish: () => 0, pb_volume_abort: () => 0,
            pb_volume_close_backing: () => { stores.get(namedSlot)?.close(); stores.delete(namedSlot); return 0; },
            pb_select_personal: () => 0,
        });
        hostOutput = value => ArrayBuffer.isView(value) ? value : new Uint8Array();
        const previousInput = hostInput;
        hostInput = bytes => {
            const value = JSON.parse(decoder.decode(bytes));
            pendingInput = value;
            if (value?.base) { preparedVolume = value.volume; preparedSelection = value.selection; preparedBase = value.base; }
            ${controls ? 'if (value !== null) previousInput?.(bytes);' : ''}
        };
        copyPreparedBacking = async (_prepared, _database, slot) => {
            if (slot !== undefined) stores.set(slot, { attachFence() {}, close() {} });
        };
        openPreparedBacking = async (_prepared, _database, slot) => { namedSlot = slot; stores.set(slot, { close() {} }); };
        preparedCatalog = async function* (copyLocal, step) {
            if (copyLocal) return;
            const volume = [profile.root, ...personalCatalog.map(item => item.volume)].find(item => item?.id === preparedVolume);
            for await (const database of databaseCatalog(volume, step, preparedSelection)) yield [database,'kvdb'];
        };
        ${controls ? `openVolumeControls = async (_volume, base) => {
            for (let offset = 1; offset <= 2; offset++) stores.set(base + offset, {});
            return undefined;
        };` : ''}
    `, context);
    sessionAdmission(context);
}

function preparationWorker(mode) {
    const capture = gate(), calls = [], messages = [];
    let execute;
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: services => { execute = services.execute; return lifecycleOperations(services); },
        GuestWork: FixtureWork, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto,
        replicaAddress, replicaSource, clearTimeout, mode, capture: () => capture.promise,
        record: (...values) => calls.push(values), self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) } });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        managed = true; profile = { account:'account', root:{id:encodeId(new Uint8Array(16).fill(0x10))},
            session:{id:encodeId(new Uint8Array(32).fill(0x30)),resolved_build_id:encodeId(new Uint8Array(32).fill(0x40))} };
        let requested = [{ volume:profile.root.id, database:${JSON.stringify(named)} }], input, prepared = false;
        controls = new Promise(() => {}); // A draining command cannot hold preparation.
        hostInput = bytes => { input = JSON.parse(decoder.decode(bytes)); };
        hostOutput = value => ArrayBuffer.isView(value) ? value : new Uint8Array();
        host = { exports: {
            pb_database_needed: () => { const bytes = encoder.encode(JSON.stringify(requested)); requested=[]; return bytes; },
            pb_database_preparation: () => encoder.encode(JSON.stringify(mode)),
            pb_database_failed: () => { record('failed',input.status); return 0; },
        } };
        openNamed = async (volume, database) => { record('local',volume.id,database); prepared = true; return 65; };
        replicas = { run:{stopped:false,streams:[]}, add: async (scope, bootstrap) => {
            replicas.run.streams.push({ source:replicaSource(scope) }); record('subscribe',scope); await bootstrap(() => true);
        } };
        pullVolumeSnapshot = async (volume,_label,_step,_selection,entry) => {
            record('capture',volume.id,entry); await capture(); prepared = true;
        };
        globalThis.prepare = requestDatabases;
        globalThis.requestAgain = () => { requested = [{ volume:profile.root.id,database:${JSON.stringify(named)} }]; };
        globalThis.prepared = () => prepared;
        globalThis.pending = () => preparingDatabases.size;
        globalThis.retire = () => { closing=true; };
    `, context);
    return { context, calls, capture, messages, execute: callback => execute(callback) };
}

test('worker selected preparation coalesces requests, subscribes before capture and bypasses a draining command', async () => {
    const f = preparationWorker('entry');
    const first = f.context.prepare(); await new Promise(setImmediate);
    assert.equal(f.calls[0][0], 'subscribe'); assert.equal(f.calls[1][0], 'capture');
    assert.equal(f.calls[0][1].entry_scope, named); assert.equal(f.calls[1][2], named);
    assert.equal(f.context.prepared(), false); assert.equal(f.context.pending(), 1);
    f.context.requestAgain(); const second = f.context.prepare(); await new Promise(setImmediate);
    assert.equal(f.calls.length, 2);
    f.capture.resolve(); assert.equal(await first, true); assert.equal(await second, true);
    assert.equal(f.context.pending(), 0); assert.equal(f.context.prepared(), true);
});

test('worker opens an initialized backing without a freshness pull', async () => {
    const f = preparationWorker('local'); assert.equal(await f.context.prepare(), true);
    assert.deepEqual(f.calls, [['local', encodeId(new Uint8Array(16).fill(0x10)), named]]);
    assert.equal(f.context.pending(), 0);
});

test('accepted read execution resumes from the same preparation rather than retaining a permanent WouldBlock result', async () => {
    const f = preparationWorker('entry'); let attempts = 0;
    const result = f.execute(() => { attempts++; return { status:f.context.prepared() ? 0 : -6, value:9 }; });
    await new Promise(setImmediate); assert.equal(attempts, 1); assert.equal(f.context.pending(), 1);
    f.capture.resolve(); assert.deepEqual(await result, { status:0,value:9 });
    assert.equal(attempts, 2);
    await assert.rejects(f.execute(() => ({status:-6})), /without a preparation request/);
});

test('worker reconnect restores selected interests, prunes withdrawn volumes and retains scoped failures', async () => {
    // Exercise the actual connect coordinator across owner/run replacement.
    // The compiled host separately checks every selected capture's authority.
    const calls = [], messages = [];
    class Replicas {
        constructor(options) { this.options = options; this.run = null; }
        async start(scopes) {
            calls.push(['start', scopes.map(replicaSource)]);
            this.run = { stopped:false, streams:scopes.map(scope => ({scope,source:replicaSource(scope)})) };
        }
        async add(scope, bootstrap) {
            calls.push(['subscribe', replicaSource(scope), scope.build_id]);
            this.run.streams.push({scope,source:replicaSource(scope)});
            await bootstrap(() => !this.run.stopped);
            this.options.state({kind:'live',source:replicaSource(scope)});
        }
    }
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations:lifecycleOperations, GuestWork:FixtureWork,
        AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto,
        replicaSource, replicaAddress, retryable, LiveReplication:Replicas, clearTimeout,
        record:(...values) => calls.push(values), self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" },postMessage:message => messages.push(message)} });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        managed=true; profile={root:{id:encodeId(new Uint8Array(16).fill(0x10))},
            session:{id:encodeId(new Uint8Array(32).fill(0x30)),resolved_build_id:encodeId(new Uint8Array(32).fill(0x40))}};
        personalCatalog=[{volume:{id:encodeId(new Uint8Array(16).fill(0x50))}}]; activeGrants=[{volume:encodeId(new Uint8Array(16).fill(0x50))}];
        personalSlots.set(encodeId(new Uint8Array(16).fill(0x50)),4);
        ap=async () => {}; openSession=async () => {};
        catalogReplicaAllowed=async () => true;
        pullVolumeSnapshot=async (volume,_label,_step,_selection,entry) => { record('capture',volume.id,entry); };
        globalThis.reconnect=connectReplication;
        globalThis.select=volume => connectEntry(replicaRecord(volume),${JSON.stringify(named)});
        globalThis.retire=() => { replicas.run.stopped=true; };
        globalThis.withdraw=() => { activeGrants=[]; };
        globalThis.fail=source => replicationState({kind:'failed',source,code:'corrupt-store'});
        globalThis.interests=() => Array.from(entrySubscriptions);
        globalThis.recoveries=() => Array.from(entryRecoveries);
        globalThis.report=replicationState;
    `, context);
    await context.reconnect();
    await context.select(encodeId(new Uint8Array(16).fill(0x10))); await context.select(encodeId(new Uint8Array(16).fill(0x50)));
    const a = `${encodeId(new Uint8Array(16).fill(0x10))}/${named}`, b = `${encodeId(new Uint8Array(16).fill(0x50))}/${named}`;
    context.fail(b); context.retire(); await context.reconnect();
    assert.equal(calls.filter(call => call[0] === 'capture' && call[1] === encodeId(new Uint8Array(16).fill(0x10))).length, 2);
    assert.equal(calls.filter(call => call[0] === 'capture' && call[1] === encodeId(new Uint8Array(16).fill(0x50))).length, 1);
    assert.deepEqual(Array.from(context.recoveries(), values => Array.from(values)), [[b,'blocked']]);
    context.report({kind:'live'});
    assert.equal(messages.at(-1).replication.kind, 'catching_up', 'A retained failed interest prevents an all-current claim');
    context.withdraw(); context.retire(); await context.reconnect();
    assert.deepEqual(Array.from(context.interests()), [a]); assert.deepEqual(Array.from(context.recoveries()), []);
    context.report({kind:'failed',source:b});
    assert.deepEqual(Array.from(context.recoveries()), [], 'Retired source callbacks cannot grow recovery state');
});

for (const rootReadable of [false, true]) test(`bootstrap omits denied catalogs and preserves queued writes (${rootReadable ? 'readable root' : 'cold entry-only'})`, async () => {
    const calls = [], writes = ['retained local input'];
    class Replicas {
        constructor(options) { this.options = options; this.run = null; }
        async start(scopes) {
            calls.push(['start', scopes.map(replicaSource)]);
            this.run = {stopped:false, streams:scopes.map(scope => ({scope,source:replicaSource(scope)}))};
            await this.options.bootstrap(() => !this.run.stopped);
        }
        async add(scope, bootstrap) {
            calls.push(['subscribe', replicaSource(scope)]);
            this.run.streams.push({scope,source:replicaSource(scope)});
            await bootstrap(() => !this.run.stopped);
        }
    }
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations:lifecycleOperations, GuestWork:FixtureWork,
        AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, clearTimeout,
        replicaSource, replicaAddress, retryable, LiveReplication:Replicas, rootReadable,
        record:(...values) => calls.push(values), reset:() => writes.splice(0), self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" },postMessage() {}} });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        managed=true; profile={root:{id:encodeId(new Uint8Array(16).fill(0x10)),share_key_with_service:true, personal: false},

            session:{id:encodeId(new Uint8Array(32).fill(0x30)),resolved_build_id:encodeId(new Uint8Array(32).fill(0x40))}};
        let input;
        hostInput=bytes => { input=JSON.parse(decoder.decode(bytes)); };
        hostOutput=value => ArrayBuffer.isView(value) ? value : new Uint8Array();
        host={exports:{pb_replica_catalog_allowed:() => encoder.encode(JSON.stringify(rootReadable && input===profile.root.id)),
            pb_authority_refresh_required:() => encoder.encode('false'), pb_database_retry:() => 0,
            pb_live_rebase:() => { reset(); return 0; }}};
        blobs={retry() {}}; ap=async () => {}; openSession=async () => {}; refreshScene=async () => {};
        databaseCatalog=async function* (_volume,_step,_selection,protocol) { if (protocol !== 'blob_log') yield ${JSON.stringify(named)}; };
        syncDatabase=async (volume,database,direction) => record('sync',volume.id,database,direction);
        pullVolumeSnapshot=async (volume,_label,_step,_selection,entry) => record('capture',volume.id,entry);
        uploadBlobs=async () => {}; saveProfile=async () => {};
        globalThis.connect=connectReplication;
        globalThis.select=() => connectEntry(profile.root,${JSON.stringify(named)});
        globalThis.retire=() => { replicas.run.stopped=true; };
    `, context);
    await context.connect();
    assert.deepEqual([calls[0][0],Array.from(calls[0][1])], ['start',rootReadable ? [encodeId(new Uint8Array(16).fill(0x10))] : []]);
    assert(!calls.some(call => call[0] === 'sync' && call[1] === encodeId(new Uint8Array(16).fill(0x20))));
    assert(!calls.some(call => call[0] === 'capture' && call[1] === encodeId(new Uint8Array(16).fill(0x20))));
    if (!rootReadable) assert(!calls.some(call => call[0] === 'sync' || call[0] === 'capture'));
    assert.deepEqual(writes, rootReadable ? [] : ['retained local input']);
    await context.select();
    const selected = `${encodeId(new Uint8Array(16).fill(0x10))}/${named}`;
    assert.deepEqual(calls.slice(-2), [['subscribe',selected],['capture',encodeId(new Uint8Array(16).fill(0x10)),named]]);
    context.retire(); await context.connect();
    assert.equal(calls.filter(call => call[0] === 'subscribe' && call[1] === selected).length, 2);
    assert.deepEqual(writes, rootReadable ? [] : ['retained local input']);
});

test('account staging and guest work release the actor before exclusive quarantine', { timeout: 5000 }, async t => {
    // Use the production actor and real Web Locks. Stage holds shared authority
    // before its actor turn; quarantine is queued before that actor is released.
    const name = `playbit-runtime-authority-${crypto.randomUUID()}`, effects = [];
    const entered = gate(), release = gate(), staged = gate(), enqueueStage = gate();
    let quarantined = false;
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId,
        guestOperations: lifecycleOperations, GuestWork: FixtureWork, AccountSignal, observeAccountMetadata() {},
        TextEncoder, TextDecoder, crypto, clearTimeout,
        withAccountAuthority(_account, callback) {
            return navigator.locks.request(name, { mode: 'shared', signal: t.signal }, () => {
                if (quarantined) throw Object.assign(Error('Quarantined'), { code: 'unsupported-account-semantics' });
                return callback();
            });
        }, self: { location: { href: 'https://playbit.app/runtime-worker.js?channel=public' }, postMessage() {} },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
        managed = true; profile = {account:'viewer'};
        globalThis.local = localStep; globalThis.authorized = authorizedStep;
        globalThis.retire = () => { closed = true; };
    `, context);
    const blocker = context.local(async () => { entered.resolve(); await release.promise; });
    await entered.promise;
    const stage = navigator.locks.request(name, { mode: 'shared', signal: t.signal }, async () => {
        staged.resolve(); await enqueueStage.promise;
        await context.local(() => effects.push('stage'));
    });
    await staged.promise;
    const guest = context.authorized(() => effects.push('guest'));
    const quarantine = navigator.locks.request(name, { mode: 'exclusive', signal: t.signal }, () => {
        quarantined = true; effects.push('quarantine');
    });
    const late = context.authorized(() => assert.fail('Quarantined guest callback ran'));
    const rejected = assert.rejects(late, error => error.status === -2);
    enqueueStage.resolve(); release.resolve();
    await Promise.all([blocker, stage, guest, quarantine, rejected]);
    assert(effects.indexOf('stage') < effects.indexOf('quarantine'));
    assert(effects.indexOf('guest') < effects.indexOf('quarantine'));
    // Admission waiting outside the actor cannot invoke a retired runtime.
    quarantined = false;
    const held = gate(), acquired = gate();
    const exclusive = navigator.locks.request(name, async () => { acquired.resolve(); await held.promise; });
    await acquired.promise;
    const retired = context.authorized(() => assert.fail('Retired runtime callback ran'));
    const retiredResult = assert.rejects(retired, /Runtime is closed/);
    context.retire(); held.resolve(); await Promise.all([exclusive, retiredResult]);
});

for (const waiting of ['I/O', 'authority']) for (const failure of [false, true]) test(`close waits outside the worker queue for accepted ${waiting} and readback (${failure ? "failure" : "success"})`, async () => {
    let finish;
    const replies = [], effects = [];
    const acceptedWrite = () => new Promise(resolve => { finish = resolve; });
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, AccountSignal,
        observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, clearTimeout,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => replies.push(message) },
        acceptedWrite,
        withAccountAuthority(_account, callback) { return waiting === 'authority' ? acceptedWrite().then(callback) : callback(); },
        effect: value => effects.push(value),
    });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        profile = ${JSON.stringify({ account: "viewer", session })}; managed = ${waiting === 'authority'};
        scene = { root: {}, controls: new Map() };
        guest = { exports: { pb_main: () => {
            while (events.length) {
                const event = guestWork.poll();
                if (event.kind !== 16) continue;
                const handle = new DataView(event.data.buffer).getBigUint64(0, true);
                const generation = handle >> 32n, slot = Number(handle & 0xffffffffn);
                try {
                    operations.data.admit(generation, slot, 9);
                    operations.data.take(generation, slot, 9, () => {
                        effect("write committed");
                        operations.data.begin(4, 16, async () => ({ status: 0, value: {} }), () => 0);
                        return 0;
                    });
                } catch (error) {
                    if (error.status !== -8) throw error;
                    operations.data.take(generation, slot, 4, () => { effect("readback"); return 0; });
                }
            }
        } } };
        closeStorage = () => effect("storage closed");
        guestWork.beginTurn(); operations.data.begin(9, 16, ${waiting === 'authority'
            ? `() => authorizedStep(() => ({status:${failure ? -5 : 0},value:{}}))` : 'acceptedWrite'}, () => 0); guestWork.endTurn();
        globalThis.close = () => self.onmessage({ data: { id: 41, method: "close", args: {} } });
        globalThis.input = () => self.onmessage({ data: { id: 42, method: "input", args: { kind: 2, target: 1, data: [] } } });
        globalThis.state = () => ({ closed, pending: operations.data.pending, waiting: operations.data.waiting });
    `, context);
    await new Promise(setImmediate); context.close(); context.input();
    await new Promise(setImmediate);
    assert.equal(replies.find(reply => reply.id === 41), undefined);
    assert.match(replies.find(reply => reply.id === 42).error, /not accepted/);
    assert.deepEqual(effects, []);
    finish({ status: failure ? -5 : 0, value: {} });
    await new Promise(setImmediate);
    const reply = replies.find(reply => reply.id === 41);
    assert(reply);
    if (failure) {
        assert.match(reply.error, /accepted work failed/);
        assert.equal(context.state().closed, false); assert.deepEqual(effects, []);
    } else {
        assert(reply.result, JSON.stringify(reply));
        assert.equal(reply.result.closed, true);
        assert.deepEqual(effects, ["write committed", "readback", "storage closed"]);
        assert.equal(context.state().closed, true);
    }
    assert.equal(context.state().pending, 0); assert.equal(context.state().waiting, false);
});

test('a reported recovery conflict does not prevent closing completed guest work', async () => {
    const messages = [];
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork,
        AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, clearTimeout,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        closeStorage = () => {};
        globalThis.conflict = () => localStep(() => { throw new Error('Name conflict'); });
        globalThis.close = () => self.onmessage({ data: { id: 1, method: 'close' } });
    `, context);
    await assert.rejects(context.conflict(), /Name conflict/);
    context.close();
    await new Promise(setImmediate);
    assert.deepEqual(JSON.parse(JSON.stringify(messages)), [{ id: 1, result: { closed: true } }]);
});

test("Close fences an input accepted before its worker turn and rejects later input", async () => {
    const entered = gate(), release = gate(), messages = [], effects = [];
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork,
        AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, clearTimeout,
        validateInput() {}, pause: async () => { entered.resolve(); await release.promise; },
        record: value => effects.push(value), self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        profile = ${JSON.stringify({ account: "viewer", session })};
        scene = { root: {}, controls: new Map() };
        refreshPersonal = pause;
        guest = { exports: { pb_main: () => { while (guestWork.poll()) record("accepted input"); } } };
        closeStorage = () => record("storage closed");
        globalThis.dispatch = request => self.onmessage({ data: request });
        globalThis.pending = () => guestWork.pending;
    `, context);
    context.dispatch({ id: 1, method: "input", args: { kind: 2, target: 1, data: [] } });
    assert.equal(context.pending(), true);
    context.dispatch({ id: 2, method: "close" });
    context.dispatch({ id: 3, method: "input", args: { kind: 2, target: 1, data: [] } });
    await entered.promise; await new Promise(setImmediate);
    assert(!messages.some(message => message.id === 2));
    assert.match(messages.find(message => message.id === 3).error, /not accepted/);
    release.resolve(); await new Promise(setImmediate);
    assert.deepEqual(effects, ["accepted input", "storage closed"]);
    assert.equal(messages.find(message => message.id === 1).error, undefined);
    assert.equal(messages.find(message => message.id === 2).result.closed, true);
});

test("only local storage failures offer web browser data reset", () => {
    assert(corruptStoreOpenMessage("Corrupt database transaction"));
    assert(corruptStoreOpenMessage("corrupt database"));
    assert(corruptStoreOpenMessage("Corrupt local profile"));
    assert(corruptStoreOpenMessage("store error: incompatible database version"));
    assert(!corruptStoreOpenMessage("This session is already open in another tab"));
    assert(!corruptStoreOpenMessage("Corrupt invitation envelope"));
});

// Run the actual worker update loop with deferred network replies. No guest is
// replaced: the candidate represents a still-unprovisioned managed app version.
function updateWorker() {
    const replies = [], messages = [];
    let downloads = 0;
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, resolvedSession,
        personalSelections: async () => ({}),
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) },
        request: () => new Promise(resolve => replies.push(resolve)),
        download: async () => { downloads++; throw new Error("App version is not provisioned for the root volume"); },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        ap = request; downloadVersion = download; saveProfile = async () => {};
        managed = true;
        profile = ${JSON.stringify({ account: encodeId(new Uint8Array(16).fill(0x44)), session, update: { kind: "failed", build_id: next } })};
        globalThis.worker = { checkUpdate, active: () => updateTask, close: () => { closed = true; },
            stoppedReplica: callback => { replicas = { run: { stopped: true } }; connectReplication = callback; } };
    `, context);
    return { worker: context.worker, replies, messages, downloads: () => downloads };
}

test("Retry during an active failed-version check is retained and repeated clicks coalesce", async () => {
    const { worker, replies, messages, downloads } = updateWorker();
    worker.checkUpdate();
    const automatic = worker.active();
    assert.equal(replies.length, 1);
    worker.checkUpdate(true); worker.checkUpdate(true); worker.checkUpdate();
    assert.equal(messages.at(-1).update.kind, "checking", "Retry replaces the old failure before AP resolution completes");
    assert.equal(worker.active(), automatic);
    replies.shift()({ data: resolution });
    await automatic;
    const retry = worker.active();
    assert.ok(retry); assert.notEqual(retry, automatic);
    assert.equal(replies.length, 1); assert.equal(downloads(), 0);
    replies.shift()({ data: resolution });
    await retry;
    assert.ok(messages.some(message => message.update?.kind === "downloading"));
    assert.equal(downloads(), 1); assert.equal(worker.active(), null);
    assert.equal(messages.at(-1).update.kind, "failed");
    assert.match(messages.at(-1).update.message, /not provisioned/);
    worker.checkUpdate();
    const later = worker.active();
    replies.shift()({ data: resolution });
    await later;
    assert.equal(downloads(), 1, "automatic checks do not repeatedly download a failed version");
});

test("closing a worker cancels its pending explicit retry", async () => {
    const { worker, replies, downloads } = updateWorker();
    worker.checkUpdate();
    const automatic = worker.active();
    worker.checkUpdate(true); worker.close();
    replies.shift()({ data: resolution });
    await automatic;
    assert.equal(worker.active(), null); assert.equal(replies.length, 0); assert.equal(downloads(), 0);
});

test("bootstrap reports each volume database and file stage before starting its I/O", async () => {
    const messages = [], steps = [];
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) },
        record: (...args) => steps.push({ args, progress: messages.at(-1)?.progress }),
        receipt: () => ({ root: null, revision: 1n, policy_revision: 1n, source: { generation: encodeId(new Uint8Array(16).fill(0x11)), epoch: 1n } }),
    });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        managed = true;
        profile = ${JSON.stringify({ session, root: { id: "root", name: "Root", share_key_with_service: true, personal: false }})};
        ap = async () => ({});
        databaseCatalog = async function* () { yield 'XxBgdhCcvNGirZzVQCPL'; };
        syncDatabase = async (volume, database, direction) => { record(volume.id, database, direction); return receipt(); };
        uploadBlobs = async () => record("files"); saveProfile = async () => record("save");
        globalThis.bootstrap = () => synchronize(true);
    `, context);
    installPipeline(context);
    await context.bootstrap();
    assert.deepEqual(Array.from(vm.runInContext('publication.reclaimed', context)), ['root', 'root'],
        'Cleanup runs before allocation and after publication for each volume');
    assert.deepEqual(steps.map(step => step.args), [
        ["root", "index", "pull"], ["root", "revocations", "pull"], ["root", named, "pull"],
        ["root", "index", "pull"],
 ["files"], ["save"],
    ]);
    assert.deepEqual(steps.map(step => step.progress), [
        "Loading Root (1/1): catalog…", "Loading Root (1/1): permissions…",
        "Loading Root (1/1): app database 1…", "Loading Root (1/1): app database 1…",
        "Saving pending app files…", "Saving session data…",
    ]);
});

// Exercise the actual bootstrap coordinator; backing revisions may change while
// the catalog/policy revision remains identical. Failed cuts never save a profile.
function bootstrapWorker(reply) {
    const calls = [];
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal,
        observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage() {} }, reply: async (...args) => { calls.push(args); return reply(...args); },
        record: value => calls.push([value]),
    });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        managed = true;
        profile = ${JSON.stringify({ session, root: { id: "root", share_key_with_service: true, personal: false }})};
        ap = async () => ({});
        databaseCatalog = async function* () { yield '${named}'; yield '${encodeId(new Uint8Array(15).fill(0x14))}'; };
        syncDatabase = async (volume, database, direction) => reply(volume.id, database, direction);
        uploadBlobs = async () => record("files"); saveProfile = async () => record("save");
        globalThis.bootstrap = () => synchronize(true);
    `, context);
    installPipeline(context);
    return { bootstrap: context.bootstrap, calls, context };
}

test('bootstrap records the final index receipt before qualifying the prepared cut', async () => {
    const worker = bootstrapWorker(() => snapshotReceipt());
    await worker.bootstrap();
    assert.deepEqual(worker.calls.slice(-2), [['files'], ['save']]);
});

test("bootstrap retries a backing-only edit between named database snapshots", async () => {
    let changed = false;
    const worker = bootstrapWorker((volume, database) => {
        if (volume === "root" && database === encodeId(new Uint8Array(15).fill(0x14))) changed = true;
        return snapshotReceipt(changed ? 2n : 1n);
    });
    await worker.bootstrap();
    assert.deepEqual(worker.calls.filter(([volume]) => volume === "root").map(([, database]) => database),
        ["index", "revocations", named, encodeId(new Uint8Array(15).fill(0x14)), "index", "revocations", named, encodeId(new Uint8Array(15).fill(0x14)), "index"]);
    assert.deepEqual(worker.calls.slice(-2), [["files"], ["save"]]);
});

test("bootstrap bounds source replacement retries even with identical clocks", async () => {
    let calls = 0;
    const worker = bootstrapWorker(() => snapshotReceipt(0n, encodeId(Buffer.from((++calls).toString(16).padStart(32, "0"), 'hex'))));
    await assert.rejects(worker.bootstrap(), /source kept changing/);
    assert.equal(calls, 6);
    assert(!worker.calls.some(([operation]) => ["files", "save"].includes(operation)));
});

test("bootstrap never retries authorization failures or saves a partial cut", async () => {
    const worker = bootstrapWorker((_volume, database) => {
        if (database === named) throw new Error("Entry read permission denied");
        return snapshotReceipt();
    });
    await assert.rejects(worker.bootstrap(), /permission denied/);
    assert.deepEqual(worker.calls.map(([, database]) => database), ["index", "revocations", named]);
});

test('uncertain publication stops host admission, closes staged handles and preserves its original error', async () => {
    const worker = bootstrapWorker(() => snapshotReceipt());
    vm.runInContext(`
        publication.publish = () => { throw new PublicationUncertain(new Error('Pointer flush uncertainty')); };
        host.exports.pb_volume_abort = () => { throw new Error('A stopped host must not be reentered'); };
        globalThis.guardedBootstrap = () => synchronize(true,localStep);
        globalThis.stoppedState = () => ({closed,stores:stores.size,preparing:preparingVolume});
    `, worker.context);
    await assert.rejects(worker.context.guardedBootstrap(), error => error instanceof PublicationUncertain);
    assert.deepEqual(JSON.parse(JSON.stringify(worker.context.stoppedState())), {closed:true,stores:0,preparing:false});
    assert(!worker.calls.some(([operation]) => operation === 'save'));
});

test("personal bootstrap follows the pulled catalog, including empty volumes and retained slot order", async () => {
    const steps = [], selections = [], messages = [];
    const empty = { volume: { id: encodeId(new Uint8Array(16).fill(0xa1)), name: "Empty notes", share_key_with_service: true, personal: true }, credentials: { database_key: Array(32).fill(1) } };
    const populated = { volume: { id: encodeId(new Uint8Array(16).fill(0xb2)), name: "Notes", share_key_with_service: true, personal: true }, credentials: { database_key: Array(32).fill(2) } };
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, structuredClone,
        validateSelection: grants => grants,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) },
        record: (...args) => steps.push(args), selected: value => selections.push(value),
        receipt: () => snapshotReceipt(),
    });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        profile = ${JSON.stringify({ session })};
        personalCatalog = ${JSON.stringify([empty, populated])};
        directory = { getFileHandle: async () => ({ createSyncAccessHandle: async () => ({ fresh: true }) }) };
        hostInput = bytes => { const value = JSON.parse(decoder.decode(bytes)); if (value.grants) selected(value); };
        host = { exports: { pb_register_personal: () => 0 } };
        hostOutput = () => new Uint8Array();
        databaseCatalog = async function* (volume, step, selection) { record("selection catalog", volume.id); selected(selection); if (volume.id === 'P4rMbpayn6a9JeaZgpVMMj') yield 'XxBgdhCcvNGirZzVQCPL'; };
        syncDatabase = async (volume, database, direction) => { record(volume.id, database, direction); return receipt(); };
        saveProfile = async () => record("save");
        globalThis.configure = grants => preparePersonal(${JSON.stringify(session)}, grants);
    `, context);
    installPipeline(context, { controls: true });
    // Allocate retained slots in empty/populated order, then reverse chooser order.
    await context.configure([{ volume: empty.volume.id, write: false }]);
    assert.deepEqual(steps, [[empty.volume.id, "index", "pull"], [empty.volume.id, "revocations", "pull"],
        ["selection catalog", empty.volume.id], [empty.volume.id, "index", "pull"], ["save"]]);
    steps.length = 0;
    await context.configure([{ volume: populated.volume.id, write: true }, { volume: empty.volume.id, write: false }]);
    assert.deepEqual(steps, [[empty.volume.id, "index", "pull"], [empty.volume.id, "revocations", "pull"],
        ["selection catalog", empty.volume.id], [empty.volume.id, "index", "pull"],
        [populated.volume.id, "index", "pull"], [populated.volume.id, "revocations", "pull"],
        ["selection catalog", populated.volume.id], [populated.volume.id, named, "pull"],
        [populated.volume.id, "index", "pull"], ["save"]]);
    assert.equal(selections.at(-1).grants.find(grant => grant.volume === empty.volume.id).write, false);
    assert.ok(messages.some(message => message.progress === "Loading Empty notes: catalog…"));
    steps.length = 0;
    await context.configure([]);
    assert.deepEqual(steps, [["save"]], "unselected personal volumes perform no I/O");
});

test("candidate preparation keeps old admission until drain and startup failure retains the new admission", async () => {
    const calls = [], messages = [];
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, structuredClone, resolvedSession,
        personalSelections: async () => ({ [session.id]: [{ volume: "personal", write: false }] }),
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) }, record: (...args) => calls.push(args),
    });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        managed = true; profile = ${JSON.stringify({ account: encodeId(new Uint8Array(16).fill(0x44)), session, root: { id: "root" }})};
        ap = async (method, operation) => ({ data: ${JSON.stringify(resolution)} });
        let unsupported = true;
        downloadVersion = async () => {
            if (unsupported) { unsupported = false; throw new Error("unsupported package host feature: session-volume.v1"); }
            return { instance: {}, name: "Next" };
        };
        syncDatabase = async () => {};
        let hostVersion = ${JSON.stringify(current)};
        adoptSession = candidate => { hostVersion = candidate.resolved_build_id; record("begin", hostVersion); };
        let attempts = 0;
        activeGrants = [{ volume: 'personal', write: false }];
        pullPersonal = async (candidate, grants, step) => {
            record("metadata", hostVersion);
            if (++attempts === 1) throw new Error("personal metadata unavailable");
            await step(() => record("personal", candidate.resolved_build_id, hostVersion));
        };
        host = { exports: { pb_finish_adoption: commit => { record("finish", commit); if (!commit) hostVersion = ${JSON.stringify(current)}; return 0; } } };
        hostOutput = () => new Uint8Array(); selectPersonal = () => {};
        let saved;
        saveProfile = async () => { saved = profile.session.resolved_build_id; };
        globalThis.saved = () => saved;
        startGuest = async () => { record("guest"); throw new Error("candidate startup failed"); };
        globalThis.runCheck = async () => { checkUpdate(true); await updateTask; };
        globalThis.admission = () => ({ hostVersion, version: profile.session.resolved_build_id });
    `, context);
    sessionAdmission(context);
    await context.runCheck();
    assert.equal(context.saved(), current);
    assert.equal(context.admission().hostVersion, current);
    assert.match(messages.at(-1).update.message, /session-volume.v1/);
    assert.deepEqual(calls, [], 'rejected package does not interrupt or drain the running guest');
    await context.runCheck();
    assert.deepEqual(calls, [["metadata", current]], "metadata failure leaves the old host identity untouched");
    await context.runCheck();
    assert.deepEqual(calls, [["metadata", current], ["metadata", current], ["personal", next, current], ["begin", next], ["finish", 1], ["guest"]]);
    assert.equal(messages.at(-1).update.kind, "failed");
    assert.match(messages.at(-1).update.message, /candidate startup failed/);
    assert.equal(context.admission().hostVersion, next); assert.equal(context.admission().version, next);
});

test("ObjectCreateBegin rejects invalid entry input before invoking the mutating host", () => {
    let mutations = 0;
    const memory = { buffer: new ArrayBuffer(4096) };
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, Uint8Array, crypto,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage() {} }, memory,
        hostExports: { memory, pb_input: () => 512, pb_web_object_create: () => { mutations++; new Uint8Array(memory.buffer, 1024, 16).fill(7); return 0; }, pb_output: () => 1024 },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        guest = { exports: { memory } }; host = { exports: hostExports };
        globalThis.dispatchCreate = pointer => operations.pb_object_create_begin(pointer, 32, 0, 0, 40);
    `, context);
    assert.equal(context.dispatchCreate(4090), -3); assert.equal(mutations, 0);
    assert.equal(context.dispatchCreate(0), -3);
    assert.equal(context.dispatchCreate(127), -3);
    assert.equal(mutations, 0);
});


test("a failed update check never retries an independently failed replica", async () => {
    const { worker, replies } = updateWorker(); let restarts = 0;
    worker.stoppedReplica(() => { restarts++; });
    worker.checkUpdate(true); const task = worker.active();
    replies.shift()({ data: resolution }); await task;
    assert.equal(restarts, 0, "authorization, wire and storage failures require an explicit recovery decision");
});

function gate() { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; }
function preparingWorker(stage, { changedConsent = false } = {}) {
    const entered = gate(), release = gate(), messages = [], inputDelivered = gate();
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, Uint8Array, crypto, structuredClone, resolvedSession,
        validateInput() {},
        WebAssembly: { promising: callback => async () => callback() },
        pause: async name => { if (name === stage) { entered.resolve(); await release.promise; } },
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => { messages.push(message); if (message.id === 17) inputDelivered.resolve(message); } },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        managed = true; profile = ${JSON.stringify({ account: encodeId(new Uint8Array(16).fill(0x44)), session, root: { id: 'root' }})};
        let grantsRead = 0, hostVersion = ${JSON.stringify(current)}, turns = 0;
        personalSelections = async () => (++grantsRead > 1 && ${changedConsent} ? {} : { [${JSON.stringify(session.id)}]: [{ volume: 'personal', write: false }] });
        ap = async (method, operation) => { if (operation.op === 'open_session') await pause('admission'); return { data: ${JSON.stringify(resolution)} }; };
        downloadVersion = async () => ({ instance: {}, name: 'Next' });
        syncDatabase = async (volume, database, direction, version, step) => { await step(() => {}); await pause('database'); };
        activeGrants = [{ volume: 'personal', write: false }];
        pullPersonal = async (candidate, grants, step) => { await step(() => {}); await pause('personal'); };
        host = { exports: { pb_finish_adoption: commit => { if (!commit) hostVersion = ${JSON.stringify(current)}; return 0; } } };
        hostOutput = () => new Uint8Array(); selectPersonal = () => {}; saveProfile = async () => {}; refreshPersonal = async () => {};
        guest = { exports: { pb_main: () => { while (guestWork.poll()) {} turns++; } } }; scene = { root: {}, controls: new Map() };
        adoptSession = candidate => { hostVersion = candidate.resolved_build_id; };
        startGuest = async () => {};
        globalThis.runtime = { check: () => { checkUpdate(true); return updateTask; },
            version: () => hostVersion, turns: () => turns,
            input: () => self.onmessage({ data: { id: 17, method: 'input', args: { kind: 2, target: 1, data: [] } } }) };
    `, context);
    sessionAdmission(context);
    return { runtime: context.runtime, entered, release, messages, inputDelivered };
}

test('actual worker input admission remains available during every update preparation wait', { timeout: 2000 }, async () => {
    for (const stage of ['admission', 'database', 'personal']) {
        const f = preparingWorker(stage), update = f.runtime.check();
        await f.entered.promise; assert.equal(f.runtime.version(), current);
        f.runtime.input(); const reply = await f.inputDelivered.promise;
        assert(!reply.error, reply.error); assert.equal(f.runtime.turns(), 1);
        assert.equal(f.runtime.version(), current, 'preparation cannot grant candidate authority');
        f.release.resolve(); await update;
        assert.equal(f.runtime.version(), next);
        assert(f.messages.some(message => message.adopted?.build_id === next));
    }
});
test('consent changed while update preparation waits prevents the final swap', { timeout: 2000 }, async () => {
    const f = preparingWorker('personal', { changedConsent: true }), update = f.runtime.check();
    await f.entered.promise; f.runtime.input(); await f.inputDelivered.promise;
    f.release.resolve(); await update;
    assert.equal(f.runtime.version(), current);
    assert(!f.messages.some(message => message.adopted));
    assert.match(f.messages.at(-1).update.message, /access changed/);
});

test('failed updates cannot restore withdrawn mounts through autonomous blob refresh', { timeout: 10000 }, async () => {
    // Run the real worker coordinator, Rust host, guest adapter and work admission.
    // Only transport/storage waits and the guest program are controlled here.
    for (const stage of ['root', 'personal']) {
        const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x81))), personalId = encodeId(new Uint8Array(16).fill(0x83));
        const personalProfile = bootstrap(personalId, { personal: true });
        personalProfile.root.share_key_with_service = true;
        const runtime = await createHost(profile), personal = await createHost(personalProfile);
        for (let slot = 1; slot <= 2; slot++) runtime.stores.set(slot + 2, personal.stores.get(slot));
        const volume = { id: personalId, owner: profile.account, name: 'Personal', share_key_with_service: true, personal: true };
        runtime.call('pb_register_personal', { volume, key, replica_ids: [7, 8] });
        const initial = [{ volume: personalId, write: true, incarnation: encodeId(new Uint8Array(16).fill(0x86)) }];
        runtime.call('pb_select_personal', { session: profile.session.id, grants: initial });
        const entered = gate(), release = gate(), messages = [], admissions = [];
        let saved = structuredClone(initial), changed, reply;
        const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations, GuestWork, MAX_PERSONAL_VOLUMES, AccountSignal,
            observeAccountMetadata() {}, TextEncoder, TextDecoder, Uint8Array, crypto, structuredClone,
            resolvedSession, validateSelection, WebAssembly, console,
            MessageChannel: class { port1 = {}; port2 = {}; },
            RuntimeBlobs: class { constructor(services) { changed = services.changed; } },
            runtimeHost: runtime.host,
            personalSelections: async (_account, _session, grants) => {
                if (grants !== undefined) saved = structuredClone(grants);
                return { [profile.session.id]: structuredClone(saved) };
            },
            pause: async name => { if (name === stage) { entered.resolve(); await release.promise; } },
            admitted: status => admissions.push(status),
            self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => { messages.push(message); if (message.id === 1) reply?.(message); } },
        });
        vm.runInContext(source.replace(/^import .*;$/gm, '') + `
            managed = true; profile = ${JSON.stringify(profile)}; host = runtimeHost;
            personalCatalog = [{ volume: ${JSON.stringify(volume)}, credentials: { database_key: ${JSON.stringify(key)} } }];
            personalSlots.set(${JSON.stringify(personalId)}, 2); activeGrants = ${JSON.stringify(initial)};
            ap = async (_method, operation) => ({ data: operation.op === 'open_session'
                ? { session: { ...profile.session, selector: { kind: 'exact', build_id: operation.build_id }, resolved_build_id: operation.build_id } }
                : { revision: 1, selector: { kind: 'exact', build_id: '${next}' }, resolved_build_id: '${next}' } });
            downloadVersion = async () => ({ instance: {}, name: 'Next' });
            syncDatabase = async () => { await pause('root'); };
            pullVolumeSnapshot = async () => { await pause('personal'); throw new Error('personal transfer failed'); };
            saveProfile = async () => {}; connectReplication = () => {};
            const memory = new WebAssembly.Memory({ initial: 1 });
            new Uint8Array(memory.buffer, 128, 5).set(new TextEncoder().encode('todos'));
            guest = { exports: { memory, pb_main() {
                while (guestWork.poll()) {}
                admitted(operations.pb_entry_resolve_begin(1n, 2, 128, 5));
            } } };
            scene = { root: {}, controls: new Map() }; operations.refreshVolumes();
            blobs = initializeBlobs();
            const original = guest;
            globalThis.runtime = {
                update: () => { checkUpdate(true); return updateTask; },
                withdraw: () => self.onmessage({ data: { id: 1, method: 'select_personal', args: { session: profile.session.id, grants: [] } } }),
                retained: () => guest === original && profile.session.resolved_build_id === '${profile.session.resolved_build_id}',
            };
        `, context);
        assert.equal(runtime.host.exports.pb_web_volume_authorize(1n, 2, 1), 0);
        const updating = context.runtime.update();
        await entered.promise;
        const withdrawn = new Promise(resolve => { reply = resolve; });
        context.runtime.withdraw();
        assert.equal((await withdrawn).error, undefined);
        assert.deepEqual(saved, []);
        assert.equal(runtime.host.exports.pb_web_volume_authorize(1n, 2, 1), -8);
        assert.equal(admissions.at(-1), -8);
        release.resolve(); await updating;
        assert(messages.some(message => /personal transfer failed/.test(message.update?.message)));
        assert(context.runtime.retained());
        const turns = admissions.length;
        await changed();
        assert.equal(admissions.length, turns + 1, 'the retained guest executes the background refresh');
        assert.equal(runtime.host.exports.pb_web_volume_authorize(1n, 2, 1), -8, `${stage}: failed preparation preserves withdrawal`);
        assert.equal(admissions.at(-1), -8, `${stage}: autonomous refresh cannot readmit guest operations`);
        assert.deepEqual(runtime.volumes(), [profile.root.id]);
    }
});

const personalA = encodeId(new Uint8Array(16).fill(0xaa)), personalB = encodeId(new Uint8Array(16).fill(0xbb));
function selectionWorker({ pauseAt = 'preparation', fail = false } = {}) {
    const entered = gate(), release = gate(), messages = [], replies = new Map();
    const initial = [{ volume: personalA, write: true }];
    let saved = structuredClone(initial), preparations = 0;
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, Uint8Array, crypto, structuredClone, validateSelection, validateInput() {},
        WebAssembly: { promising: callback => async () => callback() },
        personalSelections: async (account, version, grants, expected) => {
            if (expected !== undefined) {
                if (pauseAt === 'commit') { entered.resolve(); await release.promise; }
                if (JSON.stringify(saved) !== JSON.stringify(expected)) throw new Error('Personal volume access changed while connecting');
            }
            if (grants !== undefined) saved = structuredClone(grants);
            return { [session.id]: structuredClone(saved) };
        },
        prepare: async step => {
            preparations++;
            await step(() => {});
            if (pauseAt === 'preparation') { entered.resolve(); await release.promise; }
            if (fail) throw new Error('personal data unavailable');
            await step(() => {});
        },
        retire: async () => { if (pauseAt === 'retirement') { entered.resolve(); await release.promise; } },
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => { messages.push(message); if (message.id) { replies.get(message.id)?.(message); replies.delete(message.id); } } },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        managed = true; profile = ${JSON.stringify({ account: encodeId(new Uint8Array(16).fill(0x44)), session, root: { id: 'root' }})};
        personalCatalog = ${JSON.stringify([personalA,personalB].map(id => ({ volume: { id } })))};
        activeGrants = ${JSON.stringify(initial)};
        personalSlots.set(${JSON.stringify(personalA)},6); personalSlots.set(${JSON.stringify(personalB)},9);
        preparePersonal = async (session, grants, step) => prepare(step);
        validateCandidate = () => {};
        selectPersonal = (version, grants) => { activeGrants = structuredClone(grants); };
        connectReplication = () => {}; replicas = { stop: retire, notify() {} };
        guest = { exports: { pb_main: () => {} } }; scene = { root: {}, controls: new Map() };
        globalThis.inspect = () => ({ grants: activeGrants, busy: Boolean(personalTask) });
        globalThis.finish = () => controls.catch(() => {});
    `, context);
    let sequence = 0;
    const request = (method, args) => new Promise(resolve => {
        const id = ++sequence; replies.set(id, resolve); context.self.onmessage({ data: { id, method, args } });
    });
    return { request, entered, release, messages, inspect: () => JSON.parse(JSON.stringify(context.inspect())),
        finish: () => context.finish(), saved: () => saved, replace: value => { saved = value; }, preparations: () => preparations };
}

test('personal preparation retains only old permitted access and cannot hold guest input or local cancellation', { timeout: 2000 }, async () => {
    const f = selectionWorker(), requested = [{ volume: personalA, write: false }, { volume: personalB, write: false }];
    const selection = f.request('select_personal', { session: session.id, grants: requested });
    await f.entered.promise;
    assert.deepEqual(f.inspect().grants, requested.slice(0,1));
    assert.deepEqual(f.saved(), requested.slice(0,1), 'withdrawn write access is already durable');
    assert(!(await f.request('input', { kind: 2, target: 1, data: [] })).error);
    for (let i = 0; i < 20; i++) assert.match((await f.request('select_personal', { session: session.id, grants: [] })).error, /still finishing/);
    assert.equal(f.preparations(),1,'repeated requests do not queue preparations');
    assert.equal((await f.request('cancel_personal')).result.canceled,true);
    assert.equal((await selection).result.canceled,true,'the caller receives cancellation before AP cleanup');
    assert.deepEqual(f.inspect().grants, requested.slice(0,1));
    f.release.resolve(); await f.finish();
    assert.deepEqual(f.saved(), requested.slice(0,1));
    assert.deepEqual(f.inspect().grants, requested.slice(0,1),'canceled work never activates additions or restores write access');
});

test('failed personal preparation and concurrent consent changes preserve the current guest', { timeout: 2000 }, async () => {
    for (const concurrent of [false,true]) {
        const f = selectionWorker({ fail: !concurrent }), selection = f.request('select_personal', { session: session.id, grants: [{ volume: personalB, write: false }] });
        await f.entered.promise;
        if (concurrent) f.replace([{ volume: personalA, write: false }]);
        f.release.resolve(); const result = await selection;
        assert.match(result.error, concurrent ? /access changed/ : /unavailable/);
        assert(!f.inspect().grants.some(grant => grant.volume === personalB));
        assert(!(await f.request('input', { kind: 2, target: 1, data: [] })).error);
    }
});

test('withdrawing access acknowledges locally while old AP subscriptions retire', { timeout: 2000 }, async () => {
    const f = selectionWorker({ pauseAt: 'retirement' });
    assert(!(await f.request('select_personal', { session: session.id, grants: [] })).error);
    await f.entered.promise;
    assert.deepEqual(f.saved(),[]); assert.deepEqual(f.inspect().grants,[]);
    assert(!(await f.request('input', { kind: 2, target: 1, data: [] })).error);
    assert.equal(f.preparations(),0);
    f.release.resolve(); await f.finish();
});

test('a cancellation arriving after consent commit starts does not falsely acknowledge cancellation', { timeout: 2000 }, async () => {
    const f = selectionWorker({ pauseAt: 'commit' });
    const selection = f.request('select_personal', { session: session.id, grants: [{ volume: personalA, write: false },{ volume: personalB, write: false }] });
    await f.entered.promise;
    const cancel = f.request('cancel_personal');
    f.release.resolve(); assert(!(await selection).error);
    assert.equal((await cancel).result.canceled,false);
    assert.equal(f.inspect().grants.length,2);
});

test('canceling detached preparation still aborts its host transaction and closes the AP stream', { timeout: 2000 }, async () => {
    const entered = gate(), release = gate(), calls = [];
    let canceled = false;
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, Uint8Array, crypto,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage() {} }, canceled: () => canceled,
        receive: async () => { entered.resolve(); await release.promise; return new Uint8Array([1]); },
        record: value => calls.push(value),
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        profile = ${JSON.stringify({ session, root: { id: 'root' } })};
        host = { exports: { pb_sync_begin: () => 0, pb_sync_step: () => { record('unexpected apply'); return 0; },
            pb_sync_finish: commit => { record(commit ? 'commit' : 'abort'); return 0; } } };
        hostOutput = () => new Uint8Array();
        ap = async method => { if (method === 'streamRead') return receive(); if (method === 'streamClose') record('close'); return {}; };
        globalThis.run = () => syncDatabase(profile.root,'index','pull',undefined,(callback,cleanup=false) => {
            if (canceled() && !cleanup) throw new Error('canceled'); return callback();
        });
    `, context);
    const transfer = context.run(); await entered.promise; canceled = true; release.resolve();
    await assert.rejects(transfer,/canceled/);
    assert.deepEqual(calls,['abort','close']);
});

test('failed storage registration closes every unregistered handle without granting access', async () => {
    const closed = [];
    const entry = { volume: { id: personalB, share_key_with_service: true, personal: true }, credentials: { database_key: [1] } };
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto, validateSelection,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage() {} }, close: slot => closed.push(slot),
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        profile = ${JSON.stringify({ session })}; personalCatalog = ${JSON.stringify([entry])};
        let slot = 0;
        directory = { getFileHandle: async () => ({ createSyncAccessHandle: async () => ++slot }) };
        OpfsStore = class { constructor(slot) { this.slot = slot; } close() { close(this.slot); } };
        OpfsStore.open = async slot => new OpfsStore(slot);
        publication = { current: () => 'generation', epoch: () => ({}),
            file: async () => ({createSyncAccessHandle:async () => ++slot}) };
        hostInput = () => {}; hostOutput = () => {};
        host = { exports: { pb_register_personal: () => { throw new Error('registration failed'); } } };
        globalThis.prepare = () => preparePersonal(profile.session,[{volume:${JSON.stringify(personalB)},write:false}]);
        globalThis.remaining = () => [stores.size,personalSlots.size,activeGrants.length];
    `,context);
    await assert.rejects(context.prepare(),/registration failed/);
    assert.deepEqual(closed,[1,2]);
    assert.deepEqual(Array.from(context.remaining()),[0,0,0]);
});

test('unlocking an open invitation coalesces account storage and replaces its volume-only subscriptions', async () => {
    const entered = gate(), release = gate(), events = [];
    let unlocked = false;
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder,
        AccountReplica: { async open() {
            events.push('open');
            if (!unlocked) return null;
            entered.resolve(); await release.promise;
            return { close() { events.push('close owner'); } };
        } },
        OpfsStore: class { static async open() { return new this(); } close() { events.push('close store'); } },
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage() {} }, record: value => events.push(value),
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        managed = true; profile = { account:'account', account_replica_id:7 };
        directory = { getFileHandle: async () => ({ createSyncAccessHandle: async () => ({}) }) };
        hostInput = () => {}; hostOutput = () => {};
        host = { exports: { pb_account_open: () => record('register') } };
        replicas = { stop: async () => record('stop'), notify: () => record('notify') };
        connectReplication = () => record('connect');
        globalThis.changed = accountChanged;
        globalThis.state = () => ({ owner:!!accountReplica, stores:stores.size });
    `, context);
    await context.changed();
    assert.deepEqual(events, ['open', 'notify', 'connect']);
    assert.equal(context.state().owner, false, 'locked credentials permit volume-only use');
    events.length = 0; unlocked = true;
    const first = context.changed(); await entered.promise;
    const second = context.changed(); release.resolve();
    await Promise.all([first, second]);
    assert.equal(events.filter(value => value === 'open').length, 1);
    assert.equal(events.filter(value => value === 'register').length, 1);
    assert.equal(events.filter(value => value === 'stop').length, 1);
    assert.equal(context.state().stores, 1);
    await context.changed();
    assert.equal(events.filter(value => value === 'register').length, 1);
});

for (const parkAgain of [true, false]) test(`the parked worker publishes its scene and ignores stale readiness after input (repark=${parkAgain})`, {skip:!supportsDeadline(),timeout:3000}, async () => {
    const replies = new Map();
    const cleanup = Promise.withResolvers(), release = Promise.withResolvers();
    const context = vm.createContext({...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId,
        guestOperations:lifecycleOperations, GuestWork:FixtureWork, GuestDeadline, AccountSignal,
        observeAccountMetadata(){}, TextEncoder, TextDecoder, crypto, clearTimeout, console, cleanup, release,
        self:{location:{href:"https://playbit.app/runtime-worker.js?channel=public"},
            postMessage: message => replies.get(message.id)?.(message)}});
    vm.runInContext(source.replace(/^import .*;$/gm,'') + `
        profile = {account:'owner',session:{creator:'owner',selector:{kind:'exact'}}};
        host = {exports:{pb_web_guest_reset:()=>globalThis.resetFails ? -5 : 0,pb_recovery_abort(){},pb_sync_finish(){}}};
        hostOutput = status=>{if(status<0) throw Error('reset failed');};
        files = {async cleanup(){if(closing) {cleanup.resolve(); await release.promise;}},flushPublication(){}};
        blobs = {async cleanup(){},close(){}}; refreshPersonal=async()=>{}; connectReplication=()=>{};
        validateInput = (_controls,kind,target) => { if(kind!==2||target!==7) throw Error('invalid input'); };
        const memory = new WebAssembly.Memory({initial:1});
        const data = new DataView(memory.buffer); data.setUint32(32,24,true);
        let entries = 0;
        const candidate = {exports:{memory},deadline:new GuestDeadline(async()=>{
            entries++;
            if (guestWork.poll()?.kind!==1) throw Error("Launch missing");
            scene = {root:{stage:'opened'}, controls:new Map()};
            const result = await operations.pb_event_poll_deadline(32,64,8,0xffffffffffffffffn);
            if (result!==0 || data.getUint32(48,true)!==2) throw Error("Click missing");
            scene = {root:{stage:'clicked'},controls:new Map()};
            if (!${parkAgain}) return;
            await operations.pb_event_poll_deadline(32,64,8,0xffffffffffffffffn);
            throw Error("Retired guest unexpectedly resumed");
        })};
        globalThis.openGuest=async()=>{ await startGuest(candidate); return presentation(); };
        globalThis.scheduleReady=()=>{ operationsReady(); return work; };
        globalThis.state=()=>({entries,parked:candidate.deadline.parked,attached:!!guest,closed});
    `,context);
    const request = (id,method,args) => new Promise(resolve=>{
        replies.set(id,resolve);
        context.self.onmessage({data:{id,method,args}});
    });
    assert.equal((await context.openGuest()).scene.stage,'opened');
    assert.equal(context.state().parked,true);
    const input = request(1,'input',{kind:2,target:7,data:[]});
    // Schedule a timer or data notification behind the already admitted input.
    const stale = context.scheduleReady();
    assert.equal((await input).result.scene.stage,'clicked');
    await stale;
    assert.equal(context.state().parked,parkAgain);
    assert.equal(context.state().entries,1);
    context.resetFails = true;
    assert.match((await request(2,'close',{})).error,/reset failed/);
    assert.equal(context.state().attached,true);
    assert.equal(context.state().parked,parkAgain);
    context.resetFails = false;
    const closing = request(3,'close',{});
    await cleanup.promise;
    assert.equal(context.state().attached,false);
    assert.equal(context.state().parked,false);
    await context.scheduleReady();
    release.resolve();
    assert.equal((await closing).result.closed,true);
    assert.equal(context.state().parked,false);
    assert.equal(context.state().entries,1);
});

for (const transition of ['update', 'close']) test(`a live guest timeout is delivered during a pending ${transition}`, {skip:!supportsDeadline(),timeout:3000}, async () => {
    const frame = Promise.withResolvers(), closed = Promise.withResolvers();
    const checking = Promise.withResolvers(), answer = Promise.withResolvers();
    const context = vm.createContext({...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId,
        guestOperations:lifecycleOperations, GuestWork:FixtureWork, GuestDeadline, clockNs, AccountSignal,
        observeAccountMetadata(){}, TextEncoder, TextDecoder, crypto, clearTimeout, console,
        requireAccountAuthority: async () => { checking.resolve(); await answer.promise; throw Error('authority unavailable'); },
        self:{location:{href:'https://playbit.app/runtime-worker.js?channel=public'},postMessage: message => {
            if (message.replicaScene) frame.resolve(message.replicaScene);
            if (message.id === 2) closed.resolve(message);
        }}});
    vm.runInContext(source.replace(/^import .*;$/gm,'') + `
        profile = {account:'owner',session:{creator:'owner',selector:{kind:'exact'}}};
        host = {exports:{}}; files = {async cleanup(){},flushPublication(){}}; connectReplication=()=>{};
        const memory = new WebAssembly.Memory({initial:1});
        new DataView(memory.buffer).setUint32(32,24,true);
        let entries = 0;
        globalThis.launch = () => localStep(async () => {
            if (${transition === 'update'}) { guestUpdating = true; guestWork.deactivate(); }
            await startGuest({exports:{memory},deadline:new GuestDeadline(async () => {
                entries++; if (guestWork.poll()?.kind !== 1) throw Error('Launch missing');
                scene = {root:{stage:'opened'},controls:new Map()};
                const result = await operations.pb_event_poll_deadline(32,64,8,clockNs()+25_000_000n);
                scene = {root:{result,entries},controls:new Map()};
            })});
        });
        globalThis.close = () => { managed = true; self.onmessage({data:{id:2,method:'close',args:{}}}); };
    `, context);
    await context.launch();
    if (transition === 'close') { context.close(); await checking.promise; }
    const {scene} = await frame.promise;
    assert.equal(scene.result,-6);
    assert.equal(scene.entries,1);
    if (transition === 'close') {
        answer.resolve();
        assert.match((await closed.promise).error,/authority unavailable/);
    }
});

test('trapped Wasm is never reentered by input or replication; developer reload replaces it', async () => {
    // (module (func (export "pb_main") unreachable)) -- a real Wasm trap.
    const trap = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array([
        0,97,115,109,1,0,0,0,1,4,1,96,0,0,3,2,1,0,
        7,11,1,7,112,98,95,109,97,105,110,0,0,10,5,1,3,0,0,11,
    ]))).exports.pb_main;
    for (const developer of [true, false]) {
        const messages = [], diagnostics = [];
        const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, MAX_PERSONAL_VOLUMES, AccountSignal, observeAccountMetadata() {},
            TextEncoder, TextDecoder, crypto, trap,
            WebAssembly: { promising: callback => callback },
            console: { error: (...values) => diagnostics.push(values) },
            self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) } });
        vm.runInContext(source.replace(/^import .*;$/gm, '') + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
            host = { exports: { pb_web_guest_reset: () => 0 } };
            profile = { account: 'owner', app_name: 'Web Crash Demo',
                session: { creator: ${developer ? "'owner'" : "'another account'"}, selector: { kind: 'exact' } } };
            let calls = 0;
            guest = { exports: { pb_main: () => { calls++; trap(); } } };
            scene = { root: {}, controls: new Map() };
            refreshPersonal = async () => {}; validateInput = () => {};
            globalThis.runtime = {
                enter: enterGuest, refresh: refreshScene, calls: () => calls,
                retained: () => Boolean(guest),
                input: () => self.onmessage({ data: { id: 12, method: 'input', args: { kind: 2, target: 1, data: [] } } }),
                reload: () => startGuest({ exports: { pb_main: () => { calls++; scene = { root: {}, controls: new Map() }; } } }),
            };
        `, context);
        const message = developer ? /Web Crash Demo is halted/ : /Web Crash Demo stopped/;
        await assert.rejects(context.runtime.enter(), message);
        assert.equal(context.runtime.retained(), developer);
        await assert.rejects(context.runtime.enter(), message);
        await context.runtime.refresh();
        await context.runtime.input();
        assert.equal(context.runtime.calls(), 1);
        assert.equal(diagnostics.length, 1);
        assert.match(messages.find(value => value.guestFailure).guestFailure, message);
        assert.match(messages.find(value => value.id === 12).error, message);
        await context.runtime.reload();
        assert.equal(context.runtime.calls(), 2);
        await context.runtime.enter();
        assert.equal(context.runtime.calls(), 3);
    }
});

test("a host panic closes the worker and cannot become a guest trap or run cleanup", async () => {
    const messages = []; let closed = 0, entered = 0;
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto,
        console:{error(){}}, checked:(memory,pointer,length)=>new Uint8Array(memory.buffer,pointer,length),
        createStorageBridge:()=>({imports:{}}),
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" },postMessage:message=>messages.push(message),close:()=>closed++},
        WebAssembly:{promising:()=>async()=>entered++} });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        host = {exports:{memory:{buffer:new TextEncoder().encode("fatal report").buffer}}};
        guest = {exports:{pb_main:()=>{}}};
        globalThis.inject = () => storageImports().engine_panic(0, 12);
        globalThis.enter = enterGuest;
        globalThis.dispatch = request => self.onmessage({ data: request });
    `,context);
    context.inject();
    assert.equal(closed,1); assert.match(messages[0].engineFailure,/Restart Playbit/);
    await assert.rejects(context.enter(),/Restart Playbit/);
    await context.dispatch({id:1,method:"input",args:{}});
    assert.equal(entered,0); assert.equal(messages.length,1);
});

test("startGuest replaces the previous owner's last-window declaration", async () => {
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork,
        AccountSignal, observeAccountMetadata() {}, TextEncoder, TextDecoder, crypto,
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage() {} },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
files = { cleanup: async () => {}, flushPublication() {} };
retireFiles = async () => {};
` + `
        profile = ${JSON.stringify({ account: "viewer", session })};
        host = { exports: { pb_web_guest_reset: () => 0 } };
        globalThis.replace = async choice => {
            await startGuest({ exports: { pb_main: () => {
                if (operations.pb_quit_on_last_window(choice) !== 0) throw new Error("Conflicting owner declaration");
                while (guestWork.poll()) {}
                scene = { root: {}, controls: new Map() };
            } } });
            return operations.quitOnLastWindow;
        };
    `, context);
    assert.equal(await context.replace(1), true);
    assert.equal(await context.replace(0), false);
    assert.equal(await context.replace(1), true);
});

test('a browser example reads the same session through identity.read and session.info', async () => {
    const account = new Uint8Array(16).fill(2), internalId = new Uint8Array(32).fill(3);
    const memory = new WebAssembly.Memory({ initial: 1 }), data = new DataView(memory.buffer);
    const info = new Uint8Array(memory.buffer, 16, 64), identity = new Uint8Array(memory.buffer, 80, 104);
    const module = new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0));
    const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, decodeId,
        guestOperations: lifecycleOperations, GuestWork: FixtureWork, AccountSignal, observeAccountMetadata() {},
        TextEncoder, TextDecoder, crypto, memory,
        self: { location: { href: 'https://playbit.app/runtime-worker.js?channel=public' }, postMessage() {} },
        read(operations) {
            const { pb_syscall } = bindOperations(module, operations);
            data.setUint32(info.byteOffset, info.length, true);
            assert.equal(pb_syscall(0x163, 16n, 0n, 0n, 0n, 0n, 0n), 0n);
            assert.equal(data.getUint32(24, true), 0);
            assert.deepEqual(info.slice(16, 48), internalId);
            assert.deepEqual(info.slice(48, 64), new Uint8Array(16));
            data.setUint32(identity.byteOffset, identity.length, true);
            assert.equal(pb_syscall(0x5, 80n, 0n, 0n, 0n, 0n, 0n), 0n);
            assert.deepEqual(identity.slice(16, 32), account);
            assert.deepEqual(identity.slice(32, 48), account);
            assert(identity.slice(48, 64).some(byte => byte !== 0));
            assert.deepEqual(identity.slice(64, 96), info.slice(16, 48));
            assert.equal(data.getUint32(176, true), 2);
        },
    });
    vm.runInContext(source.replace(/^import .*;$/gm, '') + `
        profile = { account: ${JSON.stringify(encodeId(account))}, session: { id: ${JSON.stringify(encodeId(internalId))}, selector: {kind: 'exact'} } };
        managed = false;
        files = { cleanup: async () => {} };
        globalThis.launch = () => startGuest({ exports: { memory, pb_main() {
            read(operations);
            scene = { controls: new Map() };
        } } });
    `, context);
    await context.launch();
});

test('each web guest generation registers a distinct nonzero app instance',async()=>{
    const identities=[];
    let services;
    const context=vm.createContext({ ...compatibleWorkerStorage(),workerChannel,encodeId,decodeId,validId,crypto,TextEncoder,TextDecoder,ChannelHost,
        AccountSignal,observeAccountMetadata(){},GuestWork:FixtureWork,
        self:{location:{href:"https://playbit.app/runtime-worker.js?channel=public"},postMessage(){}},
        SessionChannels:class {
            constructor(identity){this.identity=identity;identities.push(identity);}
            start(){return Promise.resolve(1);}
            close(){}
        },
        guestOperations(value){services=value;return {data:{flushReady(){}},reset(){},refreshVolumes(){},pb_session(){void value.channels().start();}};},
    });
    vm.runInContext(source.replace(/^import .*;$/gm,'')+`
        files={cleanup:async()=>{}};
        host={exports:{pb_session_channel_key:()=>0}};
        hostInput=()=>{};hostOutput=()=>new Uint8Array(32);
        managed=true;proof=Array(64).fill(1);
        profile={seed:Array(16).fill(1),account:encodeId(new Uint8Array(16).fill(2)),session:{id:encodeId(new Uint8Array(32).fill(3)),resolved_build_id:encodeId(new Uint8Array(32).fill(4)),selector:{kind:'exact'}}};
        const candidate={exports:{pb_main:()=>{scene={controls:new Map()};}}};
        globalThis.launch=()=>startGuest(candidate);
    `,context);
    await context.launch();
    assert.equal(services.channels(),services.channels());
    await context.launch();
    assert.equal(identities.length,2);
    assert(identities.every(identity=>identity.instance.some(byte=>byte!==0)));
    assert.notDeepEqual(Array.from(identities[0].instance),Array.from(identities[1].instance));
});


test('bundle admission waits for the durable complete transfer and bounds executable allocation', async () => {
    const begin = source.indexOf('async function prepareBundle(');
    const end = source.indexOf('function updateState(', begin);
    assert(begin >= 0 && end > begin);
    for (const outcome of ['accepted', 'oversized', 'unsupported']) {
        const oversized = outcome === 'oversized', unsupported = outcome === 'unsupported';
        let finished = false, flushed = false, verified = 0, compiled = 0, reads = 0, closed = 0, discarded = 0, truncated = 0, release;
        const held = new Promise(resolve => { release = resolve; });
        const metadata = { name: 'Streamed', build_id: next, length: oversized ? MAX_PACKAGE_WASM_BYTES + 1 : 8, offset: 0,
            unsupported_host_features: unsupported ? ['future.operation'] : [] };
        const file = {
            truncate() { truncated++; }, write(bytes) { return bytes.length; },
            flush() { assert(finished); flushed = true; },
            read(bytes) { reads++; assert.equal(bytes.length, 8); return bytes.length; },
            close() { closed++; },
        };
        const context = vm.createContext({ ...compatibleWorkerStorage(), writePackage, MAX_PACKAGE_WASM_BYTES, TextDecoder, authenticateBundle: async () => ({}),
            TextEncoder, Uint8Array, ArrayBuffer, Map, WeakMap,
            directory: { getFileHandle: async () => ({ createSyncAccessHandle: async () => file }) },
            host: { exports: { pb_package() { assert(finished && flushed); verified++; return 1; }, pb_package_discard() { discarded++; } } },
            hostOutput: () => new TextEncoder().encode(JSON.stringify(metadata)),
            prepareGuest: async () => { compiled++; return {}; }, operations: {}, fileYield: async () => {},
        });
        vm.runInContext(`const bundleFiles = new Map(), bundleStorage = new Map(), guestBundles = new WeakMap();
            let nextBundle = 1; const decoder = new TextDecoder();
` + source.slice(begin, end)
            + '\nglobalThis.prepare = prepareBundle;', context);
        async function* incoming() { yield Uint8Array.of(1, 2, 3); await held; finished = true; }
        const work = context.prepare(incoming(), "session", undefined, next);
        const result = Promise.allSettled([work]);
        await new Promise(setImmediate); assert.equal(verified, 0);
        release(); const [settled] = await result;
        assert.equal(verified, 1);
        if (oversized || unsupported) {
            assert.equal(settled.status, 'rejected'); assert.match(settled.reason.message, unsupported ? /unsupported host feature: future.operation/ : /exceeds 64 MiB/);
            assert.equal(compiled, 0); assert.equal(reads, 0); assert.equal(closed, 1); assert.equal(discarded, 1);
            assert.equal(truncated, unsupported ? 1 : 2, 'unsupported package bytes survive failed execution admission');
        } else {
            assert.equal(settled.status, 'fulfilled'); assert.equal(compiled, 1); assert.equal(reads, 1);
            assert.equal(closed, 0); assert.equal(discarded, 0);
        }
    }
});

test('retained initializer uses its original issuer but honors a later known revocation', async () => {
    const begin = source.indexOf('async function authenticateBundle('), end = source.indexOf('async function prepareBundle(', begin);
    const original = { issuer: [8], record: [9], status: [1] }, revoked = [2];
    let submitted;
    const context = vm.createContext({
        managed: false, profile: { session }, encoder: new TextEncoder(), decoder: new TextDecoder(),
        examplePublisher: () => assert.fail('an interrupted initializer must not depend on the new release signer'),
        hostInput(bytes) { submitted = JSON.parse(new TextDecoder().decode(bytes)); },
        hostOutput(status) { assert.equal(status, -10); throw Error('Signature revoked'); },
        async admitPublisherStatus(_publisher, _signer, status, keepPrevious) {
            assert.deepEqual(status, original.status);
            assert(keepPrevious(revoked));
            return revoked;
        },
        host: { exports: {
            pb_publisher_status() {
                assert.deepEqual(submitted.previous, revoked);
                assert.deepEqual(submitted.anchor.release_issuer, original.issuer);
                return 1;
            },
            pb_package_authenticate() {
                assert.deepEqual(submitted.retained_example, original.record);
                assert.deepEqual(submitted.status, revoked);
                return -10;
            },
        } },
    });
    vm.runInContext(source.slice(begin, end) + '\nglobalThis.authenticate = authenticateBundle;', context);
    await assert.rejects(context.authenticate(1, { publisher: session.selector.app.publisher, signer: [1] }, session.id, session.selector.app, original), /revoked/);
});

test('pending initialization reopens the retained package without overwriting it on admission failure', async () => {
    const begin = source.indexOf('async function prepareBundle('), end = source.indexOf('function updateState(', begin);
    for (const mismatch of [false, true]) {
        let compiled = 0, closed = 0;
        const file = { truncate() { assert.fail('retained package was truncated'); }, close() { closed++; },
            read(bytes) { return bytes.length; } };
        const metadata = { build_id: mismatch ? next : current, name: 'Original', length: 8, offset: 0, initializer: {}, unsupported_host_features: [] };
        const context = vm.createContext({ ...compatibleWorkerStorage(), MAX_PACKAGE_WASM_BYTES, TextDecoder, Uint8Array, authenticateBundle: async () => ({}),
            writePackage() { assert.fail('retained package was overwritten'); },
            directory: { getFileHandle: async (name, options) => {
                assert.equal(name, 'app-bundle-2.pbapp'); assert.equal(options.create, false);
                return { createSyncAccessHandle: async () => file };
            } },
            host: { exports: { pb_package: () => 1, pb_package_discard() {} } },
            hostOutput: () => new TextEncoder().encode(JSON.stringify(metadata)),
            prepareGuest: async () => { compiled++; return {}; }, operations: {}, fileYield: async () => {},
        });
        vm.runInContext(`const bundleFiles = new Map(), bundleStorage = new Map(), guestBundles = new WeakMap();
            let nextBundle = 1; const decoder = new TextDecoder();\n` + source.slice(begin, end)
            + '\nglobalThis.prepare = prepareBundle;', context);
        if (mismatch) {
            await assert.rejects(context.prepare(undefined, "session", undefined, current, 2, { record: [1] }), /does not match/);
            assert.equal(compiled, 0); assert.equal(closed, 1);
        } else {
            const prepared = await context.prepare(undefined, "session", undefined, current, 2, { record: [1] });
            assert.equal(prepared.storage, 2); assert.equal(prepared.build, current);
            assert.equal(compiled, 1); assert.equal(closed, 0);
        }
    }
});

test('interrupted local root seeding resumes its original package and never reseeds a completed root', async () => {
    function directory() {
        const entries = new Map();
        return {
            async getDirectoryHandle(name, { create = false } = {}) {
                if (!entries.has(name) && create) entries.set(name, directory());
                if (!entries.has(name)) throw new DOMException('Missing directory', 'NotFoundError');
                return entries.get(name);
            },
            async getFileHandle(name, { create = false } = {}) {
                if (!entries.has(name) && create) entries.set(name, new File());
                if (!entries.has(name)) throw new DOMException('Missing file', 'NotFoundError');
                return { createSyncAccessHandle: async () => entries.get(name) };
            },
            async *values() { for (const [name, value] of entries) yield { name, kind: value instanceof File ? 'file' : 'directory' }; },
            async removeEntry(name) { entries.delete(name); },
        };
    }
    for (const fault of ['controls', 'blob-open', 'files', 'database', 'blob', 'guest']) {
        const storage = directory(), databases = new Map(), blobs = new Map(), packages = new Map();
        const declaration = { databases: [{ id: 'first' }, { id: 'second' }], blobs: [{ id: 'file-a', bytes: [1] }, { id: 'file-b', bytes: [2] }] };
        const locals = [], launched = [];
        let available = current, failed = false, downloads = 0, resumed = 0;
        const checkpoint = stage => { if (stage === fault && !failed) { failed = true; throw new Error(`Injected ${stage} failure`); } };
        const write = (entries, id) => { if (!entries.has(id)) entries.set(id, 1); };
        async function attempt() {
            let input, selected;
            const encoder = new TextEncoder(), decoder = new TextDecoder();
            const value = () => JSON.parse(decoder.decode(input));
            const host = { exports: {
                pb_version: () => encoder.encode('fixture'),
                pb_initialize() { locals.push(value().local); return 0; },
                pb_seed_database() { write(databases, value().id); checkpoint('database'); return 0; },
                pb_seed_blob_reference() { const id = value(); return encoder.encode(JSON.stringify(blobs.has(id) ? null : { entry: id })); },
                pb_seed_blob() { write(blobs, value().entry); return 0; },
                pb_recovery_abort() {}, pb_sync_finish() {},
            } };
            const context = vm.createContext({ ...compatibleWorkerStorage(), workerChannel, encodeId, decodeId, validId, validDatabase,
                AccountSignal, observeAccountMetadata() {}, GuestWork: FixtureWork, guestOperations: () => ({}), VolumePublication, OpfsStore, OpfsResources,
                TextEncoder, TextDecoder, Uint8Array, crypto, performance, clearTimeout, console: { info() {} },
                self: { location: { href: 'https://playbit.app/runtime-worker.js?channel=public' }, postMessage() {} },
                navigator: { storage: { getDirectory: async () => ({ getDirectoryHandle: async () => storage }) },
                    locks: { request: async (_name, _options, run) => run({}) } },
                installationIdentity: async () => encodeId(new Uint8Array(16).fill(9)),
                fetch: async () => ({ arrayBuffer: async () => new ArrayBuffer(0) }),
                WebAssembly: { instantiate: async () => ({ instance: host }) },
                FileBackend: class { async initialize() { checkpoint('files'); } },
                readInput(bytes) { input = bytes; }, checkpoint,
                prepared: async (_source, _sessionId, _app, expected, retained, admission) => {
                    if (retained !== undefined) {
                        resumed++; selected = packages.get(retained);
                        assert.equal(selected.build, expected);
                        assert.deepEqual(JSON.parse(JSON.stringify(admission)), selected.example, 'resume carries the original signer evidence after release rotation');
                    }
                    else {
                        downloads++;
                        selected = { instance: {}, build: available, storage: 0, name: 'Fixture',
                            example: { issuer: [1], status: [available === current ? 1 : 2], record: [3] },
                            app: { publisher: encodeId(new Uint8Array(16).fill(2)), app_id: 'todo' },
                            world: encodeId(new Uint8Array(16).fill(3)), shared: encodeId(new Uint8Array(16).fill(4)),
                            initializer: available === current ? declaration : { databases: [{ id: 'new-seed' }], blobs: [] } };
                        packages.set(0, selected);
                    }
                    return selected;
                },
                blobBackend: () => ({ async open() { checkpoint('blob-open'); }, close() {},
                    async initialize(reference, _bytes, publish) { const status = publish(reference); checkpoint('blob'); return status; } }),
                noteLaunch: build => launched.push(build),
            });
            vm.runInContext(source.replace(/^import .*;$/gm, '') + `
                hostInput = readInput;
                hostOutput = value => ArrayBuffer.isView(value) ? value : new Uint8Array();
                storageImports = () => ({});
                yieldPreparation = fileYield = async () => {};
                fetchPackage = () => [];
                prepareBundle = prepared;
                initializeBlobs = blobBackend;
                openNamed = async () => 65;
                const originalReclaim = reclaimVolume;
                reclaimVolume = async volume => { await originalReclaim(volume); checkpoint('controls'); };
                startGuest = async () => { checkpoint('guest'); noteLaunch(profile.session.resolved_build_id); };
                presentation = () => ({});
                globalThis.start = () => open({ profile: 'fixture', app: 'todo' });
                globalThis.retire = () => closeStorage();
                globalThis.initialization = () => publication.initialization(profile.root.id);
            `, context);
            try { await context.start(); return context.initialization(); }
            finally { context.retire(); }
        }
        await assert.rejects(attempt(), /Injected/);
        available = next;
        const initialization = await attempt();
        assert.equal(initialization.build, current, fault);
        assert.equal(initialization.complete, true, fault);
        if (fault !== 'guest') {
            assert.equal(resumed, 1, fault); assert.equal(launched.at(-1), current, fault);
            await attempt();
        } else assert.equal(resumed, 0, 'a completed initializer is never resumed');
        assert.equal(launched.at(-1), next, fault);
        assert.equal(downloads, 2, fault);
        assert.equal(locals.at(-1), undefined, 'code updates do not receive seed data');
        assert.deepEqual([...databases], [['first', 1], ['second', 1]], fault);
        assert.deepEqual([...blobs], [['file-a', 1], ['file-b', 1]], fault);
    }
});
