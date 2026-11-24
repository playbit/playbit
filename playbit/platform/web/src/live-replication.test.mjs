import { encodeId } from './identifier.ts';
import { databaseBytes } from "./database-id.ts";
const named = encodeId(new Uint8Array(15).fill(0x09));
import test from 'node:test';
import assert from 'node:assert/strict';
import { LiveReplication, replicaSource, replicaAddress, replicationDiagnostic } from './live-replication.ts';
const turn = () => new Promise(resolve => setImmediate(resolve));
function deferred() { let resolve, reject; const promise = new Promise((ok, fail) => { resolve = ok; reject = fail; }); return { promise, resolve, reject }; }
const scope = { kind: 'volume', session_id: encodeId(new Uint8Array(32).fill(0x01)), build_id: encodeId(new Uint8Array(32).fill(0x02)), volume_id: encodeId(new Uint8Array(16).fill(0x03)) };
function envelope(sequence, kind, delta = new Uint8Array(), database = "index") {
    const bytes = new Uint8Array(52 + delta.length), view = new DataView(bytes.buffer);
    bytes[0] = 1; bytes[1] = kind; bytes.fill(7, 4, 20);
    view.setBigUint64(20, BigInt(sequence), true); view.setBigUint64(28, 1n, true);
    bytes.set(databaseBytes(database), 36); bytes.set(delta, 52); return bytes;
}
function fixture(options = {}) {
    const calls = [], states = [], applied = [], heads = [], readers = new Map(), sequences = new Map();
    let nextHandle = 0;
    const ap = async (method, ...args) => {
        calls.push([method, ...args]);
        if (method === 'subscribeReplica') { await options.subscribe?.(); const handle = ++nextHandle; sequences.set(handle, 0); return { handle, ack: { epoch: encodeId(new Uint8Array(16).fill(7)), sequence: 0n, policy_revision: 1n } }; }
        if (method === 'streamReplica') { assert(!readers.has(args[0]), 'one in-flight frame'); const read = deferred(); readers.set(args[0], read); return read.promise; }
        if (method === 'checkpointReplica') {
            if (options.checkpoint) return options.checkpoint(args[0]);
            await turn(); send(args[0], 2); return;
        }
        if (method === 'publishReplica') { if (options.publish) return options.publish(...args); return { database_clock: 1n, policy_revision: 1n }; }
        if (method === 'streamClose') { readers.get(args[0])?.reject(new Error('closed')); readers.delete(args[0]); return; }
        assert.fail(`Unexpected AP method ${method}`);
    };
    const send = (handle, kind, delta, database) => {
        const read = readers.get(handle); assert(read, `read pending on ${handle}`); readers.delete(handle);
        const sequence = sequences.get(handle) + 1; sequences.set(handle, sequence);
        read.resolve(envelope(sequence, kind, delta, database));
    };
    const actor = new LiveReplication({ ap, operationId: () => encodeId(new Uint8Array(16).fill(0x04)), timeout: options.timeout ?? 1000,
        bootstrap: options.bootstrap ?? (async () => {}), state: state => states.push(state),
        local: { head: async () => heads[0], ack: async ticket => { assert.equal(heads[0].ticket, ticket); heads.shift(); },
            published: options.published,
            terminal: options.terminal ?? (async () => {}),
            apply: async (...args) => { applied.push(args.slice(0, 3)); await options.apply?.(...args); },
            catchUpLog: async (...args) => { calls.push(['catchUpLog', ...args.slice(0, 2)]); await options.catchUpLog?.(...args); } } });
    const edit = (database = named, source = scope.volume_id) => {
        const volume = source.split('/')[0];
        const publication = source === 'account' ? { kind: 'account' } : { ...scope, volume_id:volume };
        heads.push({ ticket: BigInt(heads.length + 1), source:volume, publication, database, bytes: new Uint8Array([1,2,3]) }); actor.notify();
    };
    return { actor, calls, states, applied, heads, readers, send, edit };
}

test('failure diagnostics distinguish subscription, catch-up, publication and checkpoint', async () => {
    for (const stage of ['subscription', 'catch_up', 'publication', 'checkpoint']) {
        const fail = async () => { throw Object.assign(Error('private remote detail'), { code: 'ap-transport' }); };
        const f = fixture({
            ...(stage === 'subscription' ? { subscribe: fail } : {}),
            ...(stage === 'catch_up' ? { bootstrap: fail } : {}),
            ...(stage === 'publication' ? { publish: fail } : {}),
            ...(stage === 'checkpoint' ? { checkpoint: fail } : {}),
        });
        if (stage === 'publication') f.edit();
        await f.actor.start([scope]);
        await turn(); await turn();
        assert.equal(f.states.at(-1).kind, 'reconnecting');
        assert.equal(f.states.at(-1).stage, stage);
        assert.equal(f.states.at(-1).code, 'ap-transport');
        assert.equal(replicationDiagnostic(f.states.at(-1)), `${stage}: The connection was interrupted or exceeded its deadline (ap-transport)`);
        await f.actor.stop();
    }
    assert.equal(replicationDiagnostic({ kind: 'failed', stage: 'delivery', code: 'private code', message: 'private data' }),
        'delivery: Synchronization failed (unknown)');
});
test('temporary server capacity during bootstrap retries with pending account changes intact', async () => {
    let attempts = 0;
    const f = fixture({ bootstrap() {
        if (++attempts === 1) throw Object.assign(Error('AP request capacity exhausted'), { code: 'ap-capacity' });
    } });
    f.edit('index', 'account');
    await f.actor.start([{ kind: 'account' }]);
    assert.equal(f.states.at(-1).kind, 'reconnecting');
    assert.equal(f.states.at(-1).stage, 'catch_up');
    assert.equal(f.heads.length, 1);
    await f.actor.start([{ kind: 'account' }]);
    await f.actor.run.publisher;
    assert.equal(f.states.at(-1).kind, 'live');
    assert.equal(f.heads.length, 0);
    await f.actor.stop();
});
test('file upload finishes before releasing its publication ticket while incoming changes continue', async () => {
    // Hold the immutable upload after metadata acknowledgment, then deliver
    // an incoming change. Neither upload completion nor a polling turn is faked.
    const upload = deferred(), started = deferred();
    const f = fixture({ published: async (head, current) => {
        assert.equal(head.database, 'index'); assert(current());
        started.resolve(); await upload.promise;
    } });
    await f.actor.start([scope]); await turn(); await turn();
    f.edit('index'); await started.promise;
    assert.equal(f.heads.length, 1, 'file upload retains the exact metadata ticket');
    const handle = [...f.readers.keys()][0];
    f.send(handle, 1, new Uint8Array([7]), named); await turn(); await turn();
    assert.equal(f.applied.length, 1, 'incoming changes do not wait for an upload');
    upload.resolve(); await turn(); await turn();
    assert.equal(f.heads.length, 0);
    await f.actor.stop();
});

test('file upload failure retains the metadata ticket for recovery', async () => {
    const f = fixture({ published: async () => { throw new Error('upload interrupted'); } });
    await f.actor.start([scope]); await turn(); await turn();
    f.edit('index'); await turn(); await turn();
    assert.equal(f.heads.length, 1);
    assert.equal(f.states.at(-1).kind, 'failed');
    await f.actor.stop();
});

test('live actor publishes actual committed bytes and checks receiver progress without a pull per edit', async () => {
    const f = fixture(); await f.actor.start([scope]); await turn(); await turn();
    assert.equal(f.states.at(-1).kind, 'live');
    for (let i = 0; i < 1000; i++) { f.edit(i % 2 ? "index" : named); await turn(); await turn(); }
    assert.equal(f.heads.length, 0);
    const publications = f.calls.filter(call => call[0] === 'publishReplica');
    assert.equal(publications.length, 1000);
    assert.equal(publications[0][1].op, 'publish_replica'); assert.equal(publications[1][1].op, 'publish_replica');
    assert.deepEqual(publications[0][2], new Uint8Array([1,2,3]));
    assert.equal(f.calls.filter(call => call[0] === 'subscribeReplica').length, 1);
    assert.equal(f.states.at(-1).kind, 'live'); await f.actor.stop();
});
test('held publication retains its head while local work and bounded incoming apply continue', async () => {
    const publication = deferred(), apply = deferred();
    const f = fixture({ publish: () => publication.promise, apply: () => apply.promise });
    await f.actor.start([scope]); await turn(); await turn();
    f.edit(); await turn(); f.edit();
    assert.equal(f.heads.length, 2); assert.equal(f.calls.filter(call => call[0] === 'publishReplica').length, 1);
    f.send(1, 1, new Uint8Array([9]), named); await turn();
    assert.equal(f.applied.length, 1); assert.equal(f.readers.size, 0, 'no next frame while apply is held');
    assert.equal(f.states.at(-1).kind, 'catching_up');
    apply.resolve(); await turn(); assert.equal(f.readers.size, 1);
    publication.resolve(); await turn(); await turn(); await turn();
    assert.equal(f.heads.length, 0); assert.equal(f.states.at(-1).kind, 'live'); await f.actor.stop();
});
test('idle 512-stream actor has no freshness operations until a real change', async () => {
    const f = fixture(); const scopes = Array.from({ length: 512 }, (_, i) => ({ ...scope, volume_id: encodeId(Buffer.from((i + 1).toString(16).padStart(32, '0'), 'hex')) }));
    await f.actor.start(scopes);
    for (let i = 0; i < 520; i++) await turn();
    assert.equal(f.readers.size, 512); assert.equal(f.states.at(-1).kind, 'live');
    const count = f.calls.length; await new Promise(resolve => setTimeout(resolve, 30));
    assert.equal(f.calls.length, count); await f.actor.stop(); assert.equal(f.readers.size, 0);
    await assert.rejects(f.actor.start([...scopes, scope]), /Invalid/);
});
test('cold entry-only connection stays idle until an address is supplied and subscribes before capture', async () => {
    const f = fixture();
    await f.actor.start([]); await turn(); await turn();
    assert.equal(f.calls.length, 0);
    assert.equal(f.actor.run.active, true);
    const selected = { ...scope, kind:'entry', entry_scope:named };
    let captured = false;
    await f.actor.add(selected, async current => {
        assert.equal(f.calls.at(-1)[0], 'subscribeReplica');
        assert.equal(f.calls.at(-1)[1].entry_scope, named);
        assert(current()); captured = true;
    });
    assert(captured);
    assert.equal(f.actor.run.streams.length, 1);
    assert.equal(f.states.at(-1).kind, 'live');
    assert.equal(f.calls.filter(call => call[0] === 'subscribeReplica').length, 1);
    await f.actor.stop();
});
test('entry-only and write-only connections publish without a catalog subscription', async () => {
    for (const readEntry of [false, true]) {
        const publication = deferred(), f = fixture({ publish: () => publication.promise });
        await f.actor.start([]); await turn();
        if (readEntry) await f.actor.add({ ...scope, kind:'entry', entry_scope:named }, async () => {});
        f.edit(); await turn();
        assert.equal(f.heads.length, 1, 'An unacknowledged fact retains its credit');
        const sent = f.calls.find(call => call[0] === 'publishReplica');
        assert.equal(sent[1].volume_id, scope.volume_id);
        assert.equal(sent[1].session_id, scope.session_id);
        assert.equal(sent[1].build_id, scope.build_id);
        assert.equal(sent[1].database_id, named);
        assert.equal('entry_scope' in sent[1], false);
        assert.equal(f.calls.filter(call => call[0] === 'subscribeReplica').length, Number(readEntry));
        publication.resolve(); await turn(); await turn(); await turn();
        assert.equal(f.heads.length, 0);
        await f.actor.stop();
    }
});
test('a malformed outbound scope cannot borrow a sibling publication binding', async () => {
    const f = fixture(); await f.actor.start([scope]); await turn(); await turn();
    f.heads.push({ source: scope.volume_id, publication: { ...scope, volume_id:encodeId(new Uint8Array(16).fill(0xff)) },
        database:named, ticket:1n, bytes:new Uint8Array([1]) });
    f.actor.notify(); await turn(); await turn();
    assert.equal(f.heads.length, 1);
    assert.equal(f.calls.filter(call => call[0] === 'publishReplica').length, 0);
    assert.match(f.states.at(-1).message, /Invalid publication scope/);
    await f.actor.stop();
});
test('empty publications validate their binding before acknowledging without an AP request', async () => {
    for (const publication of [{ kind: 'account' }, scope]) {
        const f = fixture(); await f.actor.start([]); await turn();
        let operationIds = 0;
        f.actor.operationId = () => { operationIds++; return encodeId(new Uint8Array(16).fill(0x04)); };
        f.heads.push({ source: replicaSource(publication), publication, database:'index',
            ticket:1n, bytes:new Uint8Array() });
        f.actor.notify(); await turn();
        assert.equal(f.heads.length, 0);
        assert.equal(operationIds, 0, 'An empty publication does not reserve an operation ID');
        assert.equal(f.calls.filter(call => call[0] === 'publishReplica').length, 0);
        assert.equal(f.states.at(-1).kind, 'live');
        await f.actor.stop();
    }
    for (const head of [
        { source:'account', publication:{kind:'account'}, database:named },
        { source:scope.volume_id, publication:{...scope, volume_id:encodeId(new Uint8Array(16).fill(0xff))}, database:named },
    ]) {
        const f = fixture(); await f.actor.start([]); await turn();
        f.heads.push({ ...head, ticket:1n, bytes:new Uint8Array() });
        f.actor.notify(); await turn();
        assert.equal(f.heads.length, 1, 'Invalid binding cannot release the local head');
        assert.equal(f.calls.filter(call => call[0] === 'publishReplica').length, 0);
        assert.equal(f.states.at(-1).kind, 'failed');
        assert.match(f.states.at(-1).message, /Invalid (account replica database|publication scope)/);
        await f.actor.stop();
    }
});
test('held checkpoint is pending rather than live and its deadline retires streams', async () => {
    const f = fixture({ checkpoint: async () => {}, timeout: 10 });
    await f.actor.start([scope]); await new Promise(resolve => setTimeout(resolve, 30));
    assert.equal(f.states.at(-1).kind, 'reconnecting'); assert.match(f.states.at(-1).message, /deadline/);
    assert.equal(f.readers.size, 0); assert(!f.states.some(state => state.kind === 'live')); await f.actor.stop();
});
test('stream gaps, revocation and apply failure cannot acknowledge unread state', async () => {
    for (const kind of [3, 4, 1]) {
        const f = fixture({ apply: async () => { throw new Error('disk full'); } });
        await f.actor.start([scope]); await turn(); await turn();
        f.send(1, kind, kind === 1 ? new Uint8Array([1]) : kind === 4 ? Uint8Array.from([...Array(16).fill(3), 2]) : undefined, kind === 1 ? named : "index"); await turn();
        assert.equal(f.states.at(-1).kind, kind === 3 ? 'reconnecting' : kind === 4 ? 'action_required' : 'failed'); assert.equal(f.readers.size, 0);
        assert.equal(f.actor.run.streams[0].cursor.sequence, 1n, 'cursor retains last durable checkpoint'); await f.actor.stop();
    }
});
test('retiring a held publisher never acknowledges the old head or bootstraps before it settles', async () => {
    const publication = deferred(); let bootstraps = 0;
    const f = fixture({ publish: () => publication.promise, bootstrap: async () => { bootstraps++; } });
    await f.actor.start([scope]); await turn(); await turn(); f.edit(); await turn();
    const restart = f.actor.start([scope]); await turn(); assert.equal(bootstraps, 1); assert.equal(f.heads.length, 1);
    publication.resolve(); await restart; await turn(); await turn();
    assert.equal(bootstraps, 2); assert.equal(f.heads.length, 0); await f.actor.stop();
});

test('account and volume changes share one driver without aliasing their wire operations', async () => {
    let bootstraps = 0;
    const f = fixture({ bootstrap: async () => { bootstraps++; } });
    await f.actor.start([{ kind: 'account' }, scope]); await turn(); await turn(); await turn();
    const subscriptions = f.calls.filter(call => call[0] === 'subscribeReplica').map(call => call[1]);
    assert.deepEqual(subscriptions[0], { op: 'subscribe_account', operation_id: encodeId(new Uint8Array(16).fill(0x04)) });
    assert.equal(subscriptions[1].op, 'subscribe_replica');
    assert.equal(subscriptions[1].volume_id, scope.volume_id);
    assert(!Object.hasOwn(subscriptions[1], 'kind'));
    for (let i = 0; i < 1000; i++) {
        f.edit(i % 2 ? named : 'index', i % 2 ? scope.volume_id : 'account');
        await turn(); await turn(); await turn();
    }
    const published = f.calls.filter(call => call[0] === 'publishReplica');
    assert.equal(published.length, 1000);
    assert.deepEqual(published[0][1], { op: 'publish_account', operation_id: encodeId(new Uint8Array(16).fill(0x04)) });
    assert.equal(published[1][1].op, 'publish_replica');
    assert.equal(published[1][1].database_id, named);
    assert.equal(f.heads.length, 0); assert.equal(bootstraps, 1, 'ordinary edits never pull');
    f.send(1, 1, new Uint8Array([8]), "index"); f.send(2, 1, new Uint8Array([9]), named); await turn();
    assert.deepEqual(f.applied, [['account', 'index', new Uint8Array([8])], [scope.volume_id, named, new Uint8Array([9])]]);
    await f.actor.stop();
});

test('account shares the 512-source bound and invalid scopes cannot retire a working run', async () => {
    const f = fixture();
    const scopes = [{ kind: 'account' }, ...Array.from({ length: 511 }, (_, i) =>
        ({ ...scope, volume_id: encodeId(Buffer.from((i + 1).toString(16).padStart(32, '0'), 'hex')) }))];
    await f.actor.start(scopes);
    for (let i = 0; i < 520; i++) await turn();
    assert.equal(f.readers.size, 512);
    const run = f.actor.run;
    for (const invalid of [[...scopes, scope], [{ kind: 'account' }, { kind: 'account' }],
        [{ kind: 'account', volume_id: scope.volume_id }], [{ ...scope, volume_id: 'account' }]]) {
        await assert.rejects(f.actor.start(invalid), /Invalid replica subscription scope/);
        assert.equal(f.actor.run, run);
    }
    await f.actor.stop();
});

test('account rejects volume-only database changes before applying or publishing them', async () => {
    for (const database of ['revocations', named]) {
        const f = fixture(); await f.actor.start([{ kind: 'account' }]); await turn(); await turn();
        f.send(1, 1, new Uint8Array([9]), database); await turn();
        assert.equal(f.applied.length, 0); assert.match(f.states.at(-1).message, /Invalid account replica database/);
        await f.actor.stop();
    }
});

test('retired account publication retains its exact head while a volume frame applies', async () => {
    const publication = deferred();
    const f = fixture({ publish: () => publication.promise });
    await f.actor.start([{ kind: 'account' }, scope]); await turn(); await turn(); await turn();
    f.edit('index', 'account'); await turn();
    const head = f.heads[0];
    f.send(2, 1, new Uint8Array([9]), named); await turn();
    assert.equal(f.applied.length, 1, 'held account ACK does not block volume receipt');
    const stopped = f.actor.stop(); await turn();
    publication.resolve(); await stopped;
    assert.equal(f.heads[0], head, 'old run cannot acknowledge a committed intent');
    assert.equal(f.readers.size, 0);
});

test('log publications apply inline data or catch up the selected log before advancing sequence', async () => {
    const f = fixture(); await f.actor.start([scope]); await turn(); await turn();
    f.send(1,6,new Uint8Array([1,2,3]),named); await turn();
    assert.equal(f.applied.length,1);
    f.send(1,6,new Uint8Array(),named); await turn();
    f.send(1,1,new Uint8Array([9]),named); await turn();
    assert.equal(f.applied.length,2);
    assert.deepEqual(f.calls.filter(call => call[0] === 'catchUpLog'), [['catchUpLog', scope.volume_id, named]]);
    assert.equal(f.states.at(-1).kind,'live');
    await f.actor.stop();
});

test('transport loss retains pending app/account writes for a fresh subscription and bootstrap', async () => {
    let offline=true, bootstraps=0;
    const f=fixture({bootstrap:async()=>bootstraps++,publish:async()=>{
        if(offline) throw Object.assign(new Error('Connection lost'),{code:'ap-transport'});
    }});
    await f.actor.start([{kind:'account'},scope]); await turn(); await turn(); await turn();
    f.edit('index','account'); f.edit(named,scope.volume_id); await turn();
    assert.equal(f.states.at(-1).kind,'reconnecting'); assert.equal(f.heads.length,2);
    offline=false; await f.actor.start([{kind:'account'},scope]); await turn(); await turn(); await turn();
    assert.equal(bootstraps,2); assert.equal(f.heads.length,0); assert.equal(f.states.at(-1).kind,'live');
    assert.equal(f.readers.size,2); await f.actor.stop(); assert.equal(f.readers.size,0);
});

test('retiring a run fences work already admitted to an asynchronous local apply', async () => {
    const held = deferred(); let current;
    const f = fixture({ apply: async (_source, _database, _delta, admitted) => {
        current = admitted; assert.equal(current(), true); await held.promise;
        assert.equal(current(), false);
    } });
    await f.actor.start([{ kind: 'account' }]); await turn(); await turn();
    f.send(1, 1, new Uint8Array([9]), "index"); await turn();
    assert.equal(current(), true);
    const stopped = f.actor.stop();
    assert.equal(current(), false);
    held.resolve(); await stopped;
});

test('authority terminal persistence precedes reconnect or participant withdrawal', async () => {
    for (const cause of [1, 2, 3]) {
        const persistence = deferred(), received = [];
        const f = fixture({ terminal: async (...args) => { received.push(args.slice(0, 2)); await persistence.promise; } });
        await f.actor.start([scope]); await turn(); await turn();
        const sequence = f.actor.run.streams[0].cursor.sequence;
        f.send(1, 4, Uint8Array.from([...Array(16).fill(3), cause])); await turn();
        assert.deepEqual(received, [[scope.volume_id, { volume: scope.volume_id, cause }]]);
        assert.equal(f.actor.run.streams[0].cursor.sequence, sequence);
        assert(!['reconnecting', 'action_required'].includes(f.states.at(-1).kind));
        persistence.resolve(); await turn(); await turn();
        assert.equal(f.states.at(-1).kind, cause === 2 ? 'action_required' : 'reconnecting');
        assert.equal(f.actor.run.streams[0].cursor.sequence, sequence);
        await f.actor.stop();
    }
});
test('a volume stream rejects an entry-specific terminal without changing local authority', async () => {
    const received = [];
    const f = fixture({ terminal: async (...args) => received.push(args) });
    await f.actor.start([scope]); await turn(); await turn();
    f.send(1, 4, Uint8Array.from([...Array(16).fill(3), 4])); await turn(); await turn();
    assert.equal(f.states.at(-1).kind, 'failed');
    assert.match(f.states.at(-1).message, /Invalid authority terminal scope/);
    assert.deepEqual(received, []);
    await f.actor.stop();
});

test('a failed durable authority fence is reported and never reconnects as qualified', async () => {
    const f = fixture({ terminal: async () => { throw new Error('disk full'); } });
    await f.actor.start([scope]); await turn(); await turn();
    f.send(1, 4, Uint8Array.from([...Array(16).fill(3), 1])); await turn(); await turn();
    assert.equal(f.states.at(-1).kind, 'failed'); assert.match(f.states.at(-1).message, /disk full/);
    assert.equal(f.actor.run.streams[0].cursor.sequence, 1n);
    await f.actor.stop();
});

const selected = { ...scope, kind: 'entry', entry_scope: named };
const sibling = { ...selected, entry_scope: encodeId(new Uint8Array(15).fill(0x0a)) };
test('entry sources are distinct from the volume and siblings, and publication omits the pull selector', async () => {
    assert.deepEqual(replicaAddress(replicaSource(selected)), { volume: scope.volume_id, entry: named });
    for (const invalid of [{ ...selected, entry_scope: '0'.repeat(30) }, { ...selected, entry_scope: '01' },
        { ...scope, entry_scope: named }]) assert.throws(() => replicaSource(invalid), /Invalid/);
    const f = fixture(); await f.actor.start([scope, selected, sibling]);
    for (let i = 0; i < 5; i++) await turn();
    assert.equal(f.readers.size, 3);
    assert.equal(f.calls.filter(call => call[0] === 'subscribeReplica')[1][1].entry_scope, named);
    f.edit(named, replicaSource(selected)); await turn(); await turn(); await turn();
    const publication = f.calls.find(call => call[0] === 'publishReplica')[1];
    assert.equal(publication.volume_id, scope.volume_id); assert.equal(publication.database_id, named);
    assert.equal(Object.hasOwn(publication, 'entry_scope'), false);
    await f.actor.stop();
});

for (const cause of [3, 4]) test(`selected terminal ${cause} persists before retiring only its stream`, async () => {
    const persistence = deferred(), received = [];
    const f = fixture({ terminal: async (...args) => { received.push(args.slice(0, 2)); await persistence.promise; } });
    await f.actor.start([selected, sibling]); for (let i = 0; i < 4; i++) await turn();
    const run = f.actor.run, sequence = run.streams[0].cursor.sequence;
    f.send(1, 4, Uint8Array.from([...Array(16).fill(3), cause])); await turn();
    assert.deepEqual(received, [[replicaSource(selected), { volume: scope.volume_id, cause }]]);
    assert.equal(run.streams[0].stopped, false);
    f.send(2, 1, Uint8Array.of(9), sibling.entry_scope); await turn();
    assert.equal(f.applied.length, 1);
    persistence.resolve(); await turn(); await turn();
    assert.equal(run.stopped, false); assert.equal(run.streams[0].stopped, true);
    assert.equal(run.streams[0].cursor.sequence, sequence); assert.equal(f.readers.size, 1);
    assert(f.states.some(state => state.source === replicaSource(selected) && state.kind === 'reconnecting'));
    f.edit(sibling.entry_scope, replicaSource(sibling)); for (let i = 0; i < 4; i++) await turn();
    assert.equal(f.heads.length, 0, 'Healthy sibling publication continues');
    await f.actor.stop();
});

for (const cause of [1, 2]) test(`selected terminal ${cause} retains its volume/participant effect`, async () => {
    const received = [];
    const f = fixture({ terminal: async (...args) => received.push(args.slice(0, 2)) });
    await f.actor.start([selected, sibling]); for (let i = 0; i < 4; i++) await turn();
    f.send(1, 4, Uint8Array.from([...Array(16).fill(3), cause])); await turn(); await turn();
    assert.equal(received.length, 1); assert.equal(f.actor.run.stopped, true); assert.equal(f.readers.size, 0);
    await f.actor.stop();
});

test('adding and recovering one selected stream subscribes before capture without replacing siblings', async () => {
    const captured = deferred(); let current, bootstraps = 0;
    const f = fixture(); await f.actor.start([sibling]); await turn(); await turn();
    const added = f.actor.add(selected, async admitted => { bootstraps++; current = admitted; await captured.promise; });
    await turn();
    assert.equal(bootstraps, 1); assert.equal(current(), true);
    assert.equal(f.calls.filter(call => call[0] === 'subscribeReplica').length, 2);
    assert.equal(f.readers.size, 1, 'Selected delivery waits for durable capture');
    const duplicate = f.actor.add(selected, async () => assert.fail('duplicate capture'));
    f.send(1, 1, Uint8Array.of(9), sibling.entry_scope); await turn(); assert.equal(f.applied.length, 1);
    captured.resolve(); await added; await duplicate;
    assert.equal(f.readers.size, 2);
    f.send(2, 3); await turn(); await turn();
    assert.equal(current(), false); assert.equal(f.actor.run.stopped, false);
    await f.actor.add(selected, async admitted => { bootstraps++; assert.equal(admitted(), true); });
    assert.equal(bootstraps, 2); assert.equal(f.actor.run.streams.length, 2); assert.equal(f.readers.size, 2);
    assert.equal(f.calls.filter(call => call[0] === 'streamClose').some(call => call[1] === 1), false);
    await f.actor.stop();
});

test('selected delivery rejects a sibling database before any local application', async () => {
    const f = fixture(); await f.actor.start([selected, sibling]); for (let i = 0; i < 4; i++) await turn();
    f.send(1, 1, Uint8Array.of(9), sibling.entry_scope); await turn(); await turn();
    assert.equal(f.applied.length, 0); assert.equal(f.actor.run.stopped, false);
    assert.equal(f.actor.run.streams[0].stopped, true); assert.equal(f.readers.size, 1);
    await f.actor.stop();
});

test('selected setup shares the 512 reservations and retiring capture fences its late completion', async () => {
    const f = fixture(), capture = deferred(); let current;
    const scopes = Array.from({ length: 511 }, (_, index) => ({ ...sibling, entry_scope: encodeId(Buffer.from((index + 1).toString(16).padStart(30, '0'), 'hex')) }));
    await f.actor.start(scopes); for (let i = 0; i < 520; i++) await turn();
    const addition = f.actor.add(selected, async admitted => { current = admitted; await capture.promise; });
    await turn(); assert.equal(f.actor.run.streams.length, 512); assert.equal(current(), true);
    await assert.rejects(f.actor.add({ ...selected, entry_scope: encodeId(new Uint8Array(15).fill(0x0b)) }, async () => {}), /capacity/);
    assert.equal(f.calls.filter(call => call[0] === 'subscribeReplica').length, 512);
    const stopped = f.actor.stop(); await turn(); assert.equal(current(), false);
    capture.resolve(); await addition; await stopped;
    assert.equal(f.readers.size, 0);
    assert.equal(f.calls.filter(call => call[0] === 'streamReplica').some(call => call[1] === 512), false);
});

test('a selected checkpoint deadline retires that source while a sibling keeps receiving', async () => {
    let f;
    f = fixture({ timeout:10, checkpoint: async handle => {
        if (handle === 1) return;
        await turn(); f.send(handle, 2);
    } });
    await f.actor.start([selected, sibling]); await new Promise(resolve => setTimeout(resolve, 30));
    assert.equal(f.actor.run.stopped, false); assert.equal(f.actor.run.streams[0].stopped, true);
    assert.equal(f.readers.size, 1);
    f.send(2,1,Uint8Array.of(9),sibling.entry_scope); await turn();
    assert.equal(f.applied.length,1);
    assert(f.states.some(state => state.source === replicaSource(selected) && state.code === 'replica-gap'));
    await f.actor.stop();
});

// Namespace evidence must share the selected identity and durable apply fence.
test('selected namespace evidence waits for durable apply and rejects sibling scope', async () => {
    const done = deferred(); let received;
    const f = fixture({ apply: async (...args) => { received = args; await done.promise; } });
    const selected = { ...scope, kind: 'entry', entry_scope: named };
    await f.actor.start([selected]); await turn(); await turn();
    const payload = new Uint8Array(4 + 1 + 16);
    new DataView(payload.buffer).setUint32(0, 1, true);
    payload[4] = 8; payload.fill(7, 5, 20); payload[20] = 1;
    const cursor = f.actor.run.streams[0].cursor;
    const sequence = cursor.sequence;
    f.send(1, 7, payload, named); await turn();
    assert.equal(received[0], `${scope.volume_id}/${named}`);
    assert.equal(received[1], 'index');
    assert.deepEqual(received[2], new Uint8Array([8]));
    assert.equal(received[4].length, 16);
    assert.equal(cursor.sequence, sequence);
    done.resolve(); await turn(); await turn();
    assert.equal(cursor.sequence, sequence + 1n);
    f.send(1, 7, payload, encodeId(new Uint8Array(15).fill(0x08))); await turn(); await turn();
    assert.equal(f.applied.length, 1);
    assert.equal(cursor.sequence, sequence + 1n);
    assert.equal(f.states.at(-1).kind, 'failed');
    await f.actor.stop();
});
