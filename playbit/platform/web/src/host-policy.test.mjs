import { encodeId, decodeId } from './identifier.ts';
import { deriveSessionKeys, replicaCapability, retainedSessionKeys, setScopeAddresses } from './invitation.ts';
import { formatAccountKey, backupKey } from './account-key.ts';
import { recoveryMemory } from '../tests/recovery-memory.mjs';
import { RecoveryClient, seal, mergeCatalog, availableCatalog } from './recovery.ts';
import { personalVolumes } from './personal.ts';
import { files, prepareWriter } from '../tests/file-metadata-fixture.mjs';
import { databaseIdentity } from "./database-id.ts";
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { OpfsStore } from "./opfs-store.ts";
import { File, schema, account, key, version, bootstrap, createHost, entryId, pull } from "../tests/host-fixture.mjs";
const encoder = new TextEncoder(), decoder = new TextDecoder();
test('compiled housekeeping retains named backing locations without granting guest access', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xa8)));
    const owner = await createHost(profile);
    const request = (runtime, volume, database) => {
        const input = encoder.encode(JSON.stringify({ volume, database }));
        return runtime.raw('pb_database_retained_generation', input, input.length);
    };
    assert.equal(JSON.parse(decoder.decode(request(owner, profile.root.id, entryId))), encodeId(new Uint8Array(16).fill(0xb1)));
    const participant = await createHost({ ...profile, account: encodeId(new Uint8Array(16).fill(0xaa)), local: null }, owner.stores);
    assert.equal(JSON.parse(decoder.decode(request(participant, profile.root.id, entryId))), encodeId(new Uint8Array(16).fill(0xb1)));
    assert(participant.edit(true) < 0, 'Housekeeping cannot grant guest writes');
    assert.equal(JSON.parse(decoder.decode(request(participant, profile.root.id, encodeId(new Uint8Array(15).fill(0xab))))), null);
    assert(request(participant, encodeId(new Uint8Array(16).fill(0xac)), entryId) < 0);
    assert(request(participant, profile.root.id, 'index') < 0);
});
test("compiled host enforces session personal mounts, read-only writes and stale handles", async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x10))), personalId = encodeId(new Uint8Array(16).fill(0x30));
    profile.root.share_key_with_service = true;
    const main = await createHost(profile), personal = await createHost(bootstrap(personalId, { personal: true }));
    assert.equal(main.volumes().length,1);
    for (let slot = 1; slot <= 2; slot++) main.stores.set(slot+2,personal.stores.get(slot));
    main.call("pb_register_personal", { volume: { id: personalId, owner: account, name: "Personal", share_key_with_service: false, personal: true }, key, replica_ids: [7,8] });
    assert.equal(main.volumes().length,1, "registering storage grants no guest access");
    main.call("pb_select_personal", { session: profile.session.id, grants: [{ volume: personalId, write: false, incarnation: encodeId(new Uint8Array(16).fill(0x01)) }] });
    assert.equal(main.volumes().length,2);
    assert.ok(main.edit(false, new Uint8Array(16).fill(1), 2) < 0,"read-only selection rejects writes");
    assert(main.raw('pb_sync_begin', new Uint8Array(), 1, 0) instanceof Uint8Array);
    main.call('pb_select_personal', { session: profile.session.id, grants: [] });
    assert.equal(main.volumes().length,1, 'withdrawal does not wait for an active prepared pull');
    assert.ok(main.edit(false, new Uint8Array(16).fill(1), 2) < 0, 'a retained handle cannot bypass withdrawal during preparation');
    assert.equal(main.host.exports.pb_sync_finish(0),0);
    main.call("pb_select_personal", { session: profile.session.id, grants: [{ volume: personalId, write: true, incarnation: encodeId(new Uint8Array(16).fill(0x02)) }] });
    main.stores.set(66, personal.stores.get(65)); main.register(entryId, personalId, 66);
    assert.equal(main.edit(false, new Uint8Array(16).fill(1), 2),0);
    const next = encodeId(new Uint8Array(32).fill(0x50)), candidate = { ...profile.session, selector: { kind: "exact", build_id: next }, resolved_build_id: next };
    main.call("pb_begin_adoption",candidate);
    assert.ok(main.host.exports.pb_finish_adoption(2) < 0,"invalid finalization cannot discard the prior identity");
    assert.throws(() => main.call("pb_begin_adoption",candidate), /Host error/);
    main.call("pb_select_personal", { session: profile.session.id, grants: [] });
    assert.equal(main.host.exports.pb_finish_adoption(0),0,"failed candidate restores prior identity despite withdrawn prior access");
    assert.ok(main.edit(false, new Uint8Array(16).fill(1), 2) < 0,"rollback does not restore withdrawn permission");
    main.call("pb_select_personal", { session: profile.session.id, grants: [{ volume: personalId, write: false, incarnation: encodeId(new Uint8Array(16).fill(0x03)) }] });
    assert.equal(main.volumes().length,2,"restored identity uses current session mounts");
    main.call("pb_begin_adoption",candidate);
    assert.equal(main.host.exports.pb_finish_adoption(1),0);
    assert.ok(main.host.exports.pb_finish_adoption(0) < 0,"committed adoption cannot be rolled back later");
    assert.equal(main.volumes().length,2,"new version retains session mounts");
    assert.ok(main.edit(false, new Uint8Array(16).fill(1), 2) < 0,"stale personal handle is denied after adoption");
    assert.throws(() => main.call("pb_adopt_session",{ ...candidate, root_volume: personalId }), /immutable/);
    assert.throws(() => main.call("pb_adopt_session",{ ...candidate, selector: { kind: "app", app: {publisher: encodeId(new Uint8Array(16).fill(0x7a)), app_id: "todo"} } }), /selector/);
    main.call("pb_select_personal", { session: profile.session.id, grants: [] });
    assert.equal(main.volumes().length,1);
});

test("compiled host denies an unprovisioned candidate independently of AP admission", async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x61)));
    const owner = await createHost(profile);
    const participant = await createHost({ ...profile, account: encodeId(new Uint8Array(16).fill(0x63)), local: null }, owner.stores);
    const next = encodeId(new Uint8Array(32).fill(0x64));
    assert.throws(() => participant.call("pb_adopt_session", { ...profile.session,
        selector: { kind: "exact", build_id: next }, resolved_build_id: next }), /root version provisioning/);
});

test("each root edit reads one current policy and never reuses it across calls", async () => {
    const runtime = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0x81))));
    const root = runtime.stores.get(1), begin = root.begin.bind(root);
    let snapshots = 0;
    root.begin = write => { snapshots++; return begin(write); };
    assert.equal(runtime.edit(true), 0);
    // MetadataPolicy::read takes three named-record snapshots and one catalog
    // snapshot. An edit checks four persistent negative-evidence markers and must not repeat policy materialization.
    assert.ok(snapshots > 0 && snapshots <= 8, `bounded current policy read, observed ${snapshots} snapshots`);
    snapshots = 0;
    assert.equal(runtime.edit(false), 0);
    assert.ok(snapshots > 0 && snapshots <= 8, "the next operation must read current authority again");
    // Lose the policy records after a successful edit. A retained authorization
    // result must not allow another write against missing current authority.
    root.index.clear(); root.keys = [];
    assert.ok(runtime.edit(false) < 0);
});

test("compiled prepared pull holds no writer and merges against a newer local edit", async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x61)));
    profile.root.share_key_with_service = true;
    const received = structuredClone(profile); received.replica_ids = [21,22];
    let wall = 1800000000;
    const sender = await createHost(profile, undefined, () => wall), receiver = await createHost(received, undefined, () => wall);
    assert.equal(sender.edit(true), 0);
    let request = receiver.raw("pb_sync_begin", empty, 65, 0);
    assert(request instanceof Uint8Array);
    assert(receiver.stores.get(65).resources.writers.every(value => !value.busy));
    assert(sender.raw("pb_sync_begin", empty, 65, 1) instanceof Uint8Array);
    wall++; assert.equal(receiver.edit(false), 0);
    for (let turns = 0; !receiver.host.exports.pb_sync_done(); turns++) {
        assert(turns < 100);
        assert(sender.raw("pb_sync_step", request, request.length) instanceof Uint8Array); request = empty;
        for (;;) {
            const fragment = sender.raw('pb_sync_output', empty); assert(fragment instanceof Uint8Array);
            if (!fragment.length) break;
            const reply = receiver.raw('pb_sync_step', fragment, fragment.length); assert(reply instanceof Uint8Array);
            if (reply.length) { assert.equal(request.length, 0); request = reply; }
        }
    }
    assert(sender.raw('pb_sync_step', request, request.length) instanceof Uint8Array);
    assert.equal(sender.host.exports.pb_sync_finish(1), 0);
    assert.equal(receiver.host.exports.pb_sync_finish(1), 0);
    assert.equal(receiver.read(), false, 'The later durable local edit survives incoming merge');
});
const empty = new Uint8Array();
async function pair() {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xa1)));
    profile.root.share_key_with_service = true;
    const remote = structuredClone(profile); remote.local = null; remote.replica_ids = [31, 32];
    let wall = 1800000000;
    const source = await createHost(profile, undefined, () => wall), target = await createHost(remote, undefined, () => wall);
    await pull(source, target, 1); await pull(source, target, 2);
    target.register(entryId); await pull(source, target, 65);
    source.host.exports.pb_live_rebase(); target.host.exports.pb_live_rebase();
    return { source, target, advance() { wall++; } };
}
function head(runtime) {
    const bytes = runtime.raw('pb_live_peek', empty); assert(bytes instanceof Uint8Array, `live head ${bytes}`);
    if (!bytes.length) return null;
    assert(bytes.length >= 41);
    const incoming = new Uint8Array(bytes.length - 9); incoming.set(bytes.subarray(0, 32)); incoming.set(bytes.subarray(41), 32);
    return { incoming, ticket: new DataView(bytes.buffer).getBigUint64(32, true), database: databaseIdentity(bytes.subarray(16, 32)) };
}
test('compiled file publication captures exact versions before newer pending files', async () => {
    // Hold the first metadata head while another local file is committed.
    // The web browser uploader must not mistake the current pending queue for that head.
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x10)));
    profile.root.share_key_with_service = true;
    profile.local.databases = [];
    profile.local.blobs = [
        { id: encodeId(new Uint8Array(15).fill(0x71)), name: 'first.txt', bytes: [...encoder.encode('café 東京')] },
        { id: encodeId(new Uint8Array(15).fill(0x72)), name: 'second.txt', bytes: [...encoder.encode('短い')] },
    ];
    const runtime = await createHost(profile);
    assert.equal(runtime.host.exports.pb_live_rebase(), 0);
    const publish = entry => {
        const input = encoder.encode(JSON.stringify(entry));
        const reference = JSON.parse(decoder.decode(runtime.raw('pb_seed_blob_reference', input, input.length)));
        reference.encrypted_key = '87'.repeat(32);
        runtime.call('pb_seed_blob', reference); return reference;
    };
    const first = publish(profile.local.blobs[0].id);
    const captured = runtime.raw('pb_live_peek', empty);
    const next = publish(profile.local.blobs[1].id);
    const references = bytes => {
        const input = new Uint8Array(16 + bytes.length - 41);
        input.set(bytes.subarray(0, 16)); input.set(bytes.subarray(41), 16);
        return JSON.parse(decoder.decode(runtime.raw('pb_blob_publication', input, input.length)));
    };
    assert.deepEqual(references(captured), [first]);
    assert.equal(runtime.host.exports.pb_live_ack(new DataView(captured.buffer).getBigUint64(32, true)), 0);
    assert.deepEqual(references(runtime.raw('pb_live_peek', empty)), [next]);
    assert(runtime.raw('pb_blob_publication', new Uint8Array(15), 15) < 0);
});

test('compiled initial pull repairs derived names and exposes backing only after publication', async () => {
    const { target } = await pair();
    const name = new Uint8Array(12); name.set(encoder.encode('data'), 8);
    const handle = new DataView(target.handle().buffer).getBigUint64(8, true);
    assert.equal(target.raw('pb_web_entry_resolve', name, handle >> 32n, Number(handle & 0xffffffffn), 8, 4).length, 0);
    assert.equal(target.read(), false); assert.equal(target.query().length, 1);
});
test('failed named opening stays terminal through ordinary turns until explicit recovery', async () => {
    const { target } = await pair();
    const address = { volume: encodeId(new Uint8Array(16).fill(0xa1)), database: entryId };
    target.call('pb_database_failed', { address, status: -5 });
    for (let turn = 0; turn < 3; turn++) {
        assert.equal(target.query(), -5);
        assert.deepEqual(JSON.parse(decoder.decode(target.raw('pb_database_needed', empty))), []);
    }
    assert.throws(() => target.call('pb_database_failed', { address, status: -6 }));
    assert.throws(() => target.call('pb_database_failed', { address, status: 0 }));
    assert.equal(target.host.exports.pb_database_retry(), 0);
    assert.equal(target.query().length, 1);
});
test('independent blob initialization binds the declared bytes and publishes one persistent birth', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xa1)));
    const entry = encodeId(new Uint8Array(15).fill(0x72));
    profile.local.databases = [];
    profile.local.blobs = [{ id: entry, name: 'note.txt', bytes: [4, 5, 6] }];
    const runtime = await createHost(profile);
    const reference = () => {
        const input = encoder.encode(JSON.stringify(entry));
        return JSON.parse(decoder.decode(runtime.raw('pb_seed_blob_reference', input, input.length)));
    };
    const prepared = reference();
    assert.equal(prepared.entry, entry); assert.equal(prepared.len_bytes, 3);
    prepared.encrypted_key = '87'.repeat(32);
    for (const invalid of [{ ...prepared, digest: '99'.repeat(32) }, { ...prepared, len_bytes: 2 },
        { ...prepared, volume: encodeId(new Uint8Array(16).fill(0x74)) }, { ...prepared, entry: encodeId(new Uint8Array(15).fill(0x73)) }]) {
        assert.throws(() => runtime.call('pb_seed_blob', invalid));
        assert(reference(), 'A rejected preparation cannot publish its birth');
    }
    runtime.call('pb_seed_blob', prepared);
    assert.equal(reference(), null, 'Reopening never republishes an existing birth');
    assert.throws(() => runtime.call('pb_seed_blob', prepared));
    const reopened = await createHost(profile, runtime.stores);
    const input = encoder.encode(JSON.stringify(entry));
    assert.equal(JSON.parse(decoder.decode(reopened.raw('pb_seed_blob_reference', input, input.length))), null);
    const name = new Uint8Array(16); name.set(encoder.encode('note.txt'), 8);
    const handle = new DataView(reopened.handle().buffer).getBigUint64(8, true);
    assert.equal(reopened.raw('pb_web_entry_resolve', name, handle >> 32n, Number(handle & 0xffffffffn), 8, 8).length, 0);
});
test('failed independent blob birth leaves no named entry or outgoing publication', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xa1))), entry = encodeId(new Uint8Array(15).fill(0x71));
    profile.local.databases = [];
    profile.local.blobs = [{ id: entry, name: 'note.txt', bytes: [] }];
    const runtime = await createHost(profile);
    const input = encoder.encode(JSON.stringify(entry));
    const prepared = JSON.parse(decoder.decode(runtime.raw('pb_seed_blob_reference', input, input.length)));
    prepared.encrypted_key = '87'.repeat(32);
    runtime.host.exports.pb_live_rebase();
    const index = runtime.stores.get(1), begin = index.begin.bind(index);
    index.begin = write => {
        const tx = begin(write);
        if (write) { const end = tx.end.bind(tx); tx.end = commit => {
            end(false); if (commit) throw new Error('Injected seed publication interruption');
        }; }
        return tx;
    };
    assert.throws(() => runtime.call('pb_seed_blob', prepared));
    assert(JSON.parse(decoder.decode(runtime.raw('pb_seed_blob_reference', input, input.length))));
    assert.equal(head(runtime), null);
    index.begin = begin; runtime.call('pb_seed_blob', prepared);
    assert.equal(JSON.parse(decoder.decode(runtime.raw('pb_seed_blob_reference', input, input.length))), null);
});
test('compiled blob upload admits Append creation with ServiceRead without catalog or plaintext Read', async () => {
    const fixture = JSON.parse(await readFile(new URL('../tests/blob-authority-vectors.json', import.meta.url)));
    for (const name of ['write_only', 'append_only', 'read_only', 'no_service', 'retired']) {
        const upload = ['write_only', 'append_only'].includes(name);
        const profile = bootstrap(fixture.reference.volume, { personal: true });
        profile.local = null; profile.root.owner = encodeId(new Uint8Array(16).fill(0x09)); profile.root.share_key_with_service = true;
        const local = { ...bootstrap(profile.root.id).local, databases: [], blobs: [] };
        const initial = await createHost({ ...profile, local }), ledger = initial.stores.get(1).resources;
        initial.stores.get(1).close();
        const index = OpfsStore.empty(new File(), ledger), write = index.begin(true);
        for (const [key, value] of fixture.snapshots[name]) write.put(Uint8Array.from(key), Uint8Array.from(value));
        write.end(true); initial.stores.set(1, index);
        const runtime = await createHost(profile, initial.stores);
        const request = (operation, value) => { const input = encoder.encode(JSON.stringify(value)); return runtime.raw(operation, input, input.length); };
        const authorize = (write, service) => request('pb_blob_authorize', { reference: fixture.reference, write, service });
        assert.equal(authorize(true, true) instanceof Uint8Array, upload);
        assert.equal(authorize(false, true) instanceof Uint8Array, ['read_only', 'retired'].includes(name));
        assert.equal(authorize(false, false) instanceof Uint8Array, ['read_only', 'retired'].includes(name));
        assert.equal(authorize(true, false) instanceof Uint8Array, upload || name === 'no_service');
        assert(request('pb_catalog_page', { volume: profile.root.id, after: null, expected_root: null }) < 0,
            'An entry override never grants the volume catalog');
        assert.equal(JSON.parse(decoder.decode(request('pb_replica_catalog_allowed', profile.root.id))), false,
            'A known entry grant does not plan an automatic catalog bootstrap');
        for (const label of ['supplied.txt', 'missing']) {
            const input = new Uint8Array(8 + encoder.encode(label).length);
            input.set(encoder.encode(label), 8);
            assert(runtime.raw('pb_web_entry_resolve', input, 1n, 1, 8, input.length - 8) < 0,
                'Entry access by ID cannot disclose catalog names through lookup');
        }
        const pending = () => {
            const page = request('pb_blob_pending', { volume: profile.root.id, after: null });
            assert(page instanceof Uint8Array, `${name} pending returned ${page}`);
            return JSON.parse(decoder.decode(page));
        };
        assert.deepEqual(pending(), upload ? [[fixture.reference], null] : [[], null]);
        const acknowledged = request('pb_blob_acknowledge', fixture.reference);
        assert.equal(acknowledged instanceof Uint8Array, upload);
        if (name === 'retired') {
            // A captured version survives deletion but not current Read withdrawal.
            const current = runtime.stores.get(1);
            const write = current.begin(true);
            for (const [key, value] of fixture.snapshots.retired_write_only) {
                write.put(Uint8Array.from(key), Uint8Array.from(value));
            }
            write.end(true);
            assert(authorize(false, false) < 0);
            assert(authorize(false, true) < 0);
        }
        if (name === 'append_only') {
            assert.deepEqual(pending(), [[], null]);
            const api = files(runtime), [root] = api.roots();
            const open = api.begin(root, 23, { flags: 4, data: encoder.encode('supplied.txt') });
            assert.equal(api.execute(open), -2, 'Known-ID upload does not authorize path traversal');
            assert.equal(api.host.pb_web_file_release(open), 0);
        }
        if (name === 'write_only') {
            assert.deepEqual(pending(), [[], null]);
            assert(authorize(false, false) < 0, 'A durability acknowledgment cannot grant Read');
            const current = runtime.stores.get(1), update = rows => {
                const write = current.begin(true);
                for (const [key, value] of rows) write.put(Uint8Array.from(key), Uint8Array.from(value));
                write.end(true);
            };
            // Path access adds directory Read; the leaf still grants Write alone.
            update(fixture.snapshots.file_write);
            const prepared = prepareWriter(runtime, 'supplied.txt', encoder.encode('new supplied content'));
            prepared.reference.encrypted_key = '77'.repeat(32); prepared.publish();
            const head = runtime.raw('pb_live_peek', empty); assert(head instanceof Uint8Array && head.length > 41);
            assert(authorize(false, false) < 0, 'Publication never grants content Read');
            const ticket = new DataView(head.buffer).getBigUint64(32, true);
            assert.deepEqual(runtime.raw('pb_live_peek', empty), head, 'A held receipt retains the exact head');
            update(fixture.snapshots.file_read);
            assert(runtime.raw('pb_live_peek', empty) < 0);
            update(fixture.snapshots.file_write);
            assert.deepEqual(runtime.raw('pb_live_peek', empty), head, 'Denied publication never discards the fact');
            assert.equal(runtime.host.exports.pb_live_ack(ticket), 0);
            assert.equal(runtime.raw('pb_live_peek', empty).length, 0);
            assert(authorize(false, false) < 0);
        }
        assert(request('pb_blob_authorize', { reference: { ...fixture.reference, encrypted_key: '99'.repeat(32) }, write: true, service: true }) < 0);
    }
});
test('compiled entry authority checks persisted root revocations after reopening', async () => {
    const fixture = JSON.parse(await readFile(new URL('../tests/blob-authority-vectors.json', import.meta.url)));
    for (const name of ['write_only', 'append_only', 'read_only']) for (const [dependency, slot] of [['root', 2]]) {
        const profile = bootstrap(fixture.reference.volume, { personal: true });
        profile.local = null; profile.root.owner = encodeId(new Uint8Array(16).fill(0x09)); profile.root.share_key_with_service = true;
        const initial = await createHost(profile), resources = initial.stores.get(1).resources;
        initial.stores.get(1).close();
        const index = OpfsStore.empty(new File(), resources), write = index.begin(true);
        for (const [key, value] of fixture.snapshots[name]) write.put(Uint8Array.from(key), Uint8Array.from(value));
        write.end(true); initial.stores.set(1, index);
        const authorize = runtime => {
            const input = encoder.encode(JSON.stringify({ reference: fixture.reference, write: name !== 'read_only', service: false }));
            return runtime.raw('pb_blob_authorize', input, input.length);
        };
        assert(authorize(await createHost(profile, initial.stores)) instanceof Uint8Array);
        initial.stores.get(slot).close();
        const file = new File(), revocations = OpfsStore.empty(file, resources), transaction = revocations.begin(true);
        for (const [key, value] of fixture.revocations[dependency]) transaction.put(Uint8Array.from(key), Uint8Array.from(value));
        transaction.end(true); initial.stores.set(slot, revocations);
        assert(authorize(await createHost(profile, initial.stores)) < 0, `${name} must respect ${dependency} revocation`);
        revocations.close(); initial.stores.set(slot, await OpfsStore.open(file, resources, async () => {}));
        assert(authorize(await createHost(profile, initial.stores)) < 0, 'Reopening cannot revive the cached entry grant');
    }
});
test('compiled live edits commit before ACK, duplicate/reordered delivery converges without echo', async () => {
    const { source, target, advance } = await pair(); let older;
    for (let index = 0; index < 1000; index++) {
        advance(); const done = index % 2 === 0;
        assert.equal(source.edit(done), 0); assert.equal(source.read(), done);
        const change = head(source); assert.equal(change.database, entryId);
        assert.equal(target.raw('pb_live_apply', change.incoming, change.incoming.length).length, 0);
        assert.equal(target.raw('pb_live_apply', change.incoming, change.incoming.length).length, 0);
        if (older) assert.equal(target.raw('pb_live_apply', older, older.length).length, 0);
        assert.equal(target.read(), done); assert.equal(head(target), null);
        assert.equal(source.host.exports.pb_live_ack(change.ticket), 0); older = change.incoming;
    }
});
test('object birth publishes one complete record and survives duplicate delivery', async () => {
    const { source, target, advance } = await pair(); advance();
    const id = source.create(true); assert(id instanceof Uint8Array);
    const change = head(source); assert.equal(change.database, entryId);
    assert.equal(target.raw('pb_live_apply', change.incoming, change.incoming.length).length, 0);
    assert.equal(target.read(id), true); assert(target.query().includes(encodeId(id)));
    assert.equal(target.raw('pb_live_apply', change.incoming, change.incoming.length).length, 0);
    assert.equal(target.query().length, 2);
});
test('failed birth commit publishes no partial object or changeset', async () => {
    const { source } = await pair(), backing = source.stores.get(65), begin = backing.begin.bind(backing);
    backing.begin = write => {
        const tx = begin(write);
        if (write) { const end = tx.end.bind(tx); tx.end = commit => { end(false); if (commit) throw new Error('injected birth commit failure'); }; }
        return tx;
    };
    const before = source.query(); assert(source.create(true) < 0); assert.deepEqual(source.query(), before);
    assert.equal(head(source), null); assert(backing.resources.writers.every(writer => !writer.busy));
});
test('overflow retains committed objects and stale tickets cannot pop a rebased head', async () => {
    const { source, advance } = await pair(); source.edit(true); const old = head(source);
    for (let index = 0; index < 64; index++) { advance(); assert.equal(source.edit(false), 0); }
    assert(source.raw('pb_live_peek', empty) < 0); assert.equal(source.read(), false);
    assert(source.host.exports.pb_live_ack(old.ticket) < 0); assert.equal(source.host.exports.pb_live_rebase(), 0);
    advance(); source.edit(true); const next = head(source); assert(next.ticket > old.ticket);
    assert(source.host.exports.pb_live_ack(old.ticket) < 0); assert.deepEqual(head(source), next);
    assert.equal(source.host.exports.pb_live_ack(next.ticket), 0);
});
test('unknown volumes and lost policy deny live application and disclosure', async () => {
    const { source, target } = await pair(); source.edit(true); const change = head(source), wrong = change.incoming.slice(); wrong[0]++;
    assert(target.raw('pb_live_apply', wrong, wrong.length) < 0);
    for (const runtime of [source, target]) { runtime.stores.get(1).index.clear(); runtime.stores.get(1).keys = []; }
    assert(target.raw('pb_live_apply', change.incoming, change.incoming.length) < 0); assert(source.raw('pb_live_peek', empty) < 0);
});
test('current personal consent excludes writes and withdrawal defeats retained handles', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x10))), personalId = encodeId(new Uint8Array(16).fill(0x30));
    const personalProfile = bootstrap(personalId, { personal: true }); personalProfile.root.share_key_with_service = true;
    const main = await createHost(profile), personal = await createHost(personalProfile);
    main.stores.set(3, personal.stores.get(1)); main.stores.set(4, personal.stores.get(2));
    main.call('pb_register_personal', { volume: { id: personalId, owner: account, name: 'Personal', share_key_with_service: true, personal: true }, key, replica_ids: [7, 8] });
    assert.equal(main.volumes().length, 1);
    main.call('pb_select_personal', { session: profile.session.id, grants: [{ volume: personalId, write: false, incarnation: encodeId(new Uint8Array(16).fill(0x04)) }] });
    main.stores.set(66, personal.stores.get(65)); main.register(entryId, personalId, 66);
    assert.equal(main.volumes().length, 2); assert(main.edit(false, new Uint8Array(16).fill(1), 2) < 0);
    main.call('pb_select_personal', { session: profile.session.id, grants: [{ volume: personalId, write: true, incarnation: encodeId(new Uint8Array(16).fill(0x05)) }] });
    assert.equal(main.edit(false, new Uint8Array(16).fill(1), 2), 0);
    assert(main.raw('pb_sync_begin', empty, 66, 0) instanceof Uint8Array);
    main.call('pb_select_personal', { session: profile.session.id, grants: [] });
    assert(main.edit(true, new Uint8Array(16).fill(1), 2) < 0); assert.equal(main.host.exports.pb_sync_finish(0), 0);
    assert.equal(main.volumes().length, 1);
});
test('unprovisioned participants cannot adopt or inspect a candidate version', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x61))), owner = await createHost(profile);
    const participant = await createHost({ ...profile, account: encodeId(new Uint8Array(16).fill(0x63)), local: null }, owner.stores);
    const candidate = { ...profile.session, selector: { kind: 'exact', build_id: encodeId(new Uint8Array(32).fill(0x64)) }, resolved_build_id: encodeId(new Uint8Array(32).fill(0x64)) };
    assert.throws(() => participant.call('pb_adopt_session', candidate), /root version provisioning/);
    const bytes = encoder.encode(JSON.stringify({ volume: profile.root.id, after: null, expected_root: null, selection: { session: candidate, grants: [] } }));
    assert(participant.raw('pb_catalog_page', bytes, bytes.length) < 0);
});
test('compiled BLAKE3 and content key match the native v1 fixture and reject stale contexts', async () => {
    const fixture = JSON.parse(await readFile(new URL('../../../tests/fixtures/blob-v1.json', import.meta.url)));
    const runtime = await createHost(bootstrap(fixture.volume, { personal: true })), fromHex = value => new Uint8Array(Buffer.from(value, 'hex'));
    const content = fromHex(fixture.plaintext); assert.deepEqual(runtime.raw('pb_blob_digest', content, content.length), fromHex(fixture.digest));
    const material = new Uint8Array(80); material.set(fromHex(fixture.key)); material.set(decodeId(fixture.volume), 32); material.set(fromHex(fixture.digest), 48);
    assert.deepEqual(runtime.raw('pb_blob_cipher_key', material), fromHex(fixture.cipher_key));
    const id = runtime.host.exports.pb_blob_hash_begin(); assert(id > 0);
    for (const byte of content) assert.equal(runtime.raw('pb_blob_hash_update', Uint8Array.of(byte), id, 1).length, 0);
    assert.deepEqual(runtime.raw('pb_blob_hash_finish', empty, id), fromHex(fixture.digest));
    assert(runtime.host.exports.pb_blob_hash_finish(id) < 0); assert(runtime.host.exports.pb_blob_hash_close(id) < 0);
    const contexts = Array.from({ length: 8 }, () => runtime.host.exports.pb_blob_hash_begin()); assert(contexts.every(value => value > id));
    assert(runtime.host.exports.pb_blob_hash_begin() < 0); for (const context of contexts) assert.equal(runtime.host.exports.pb_blob_hash_close(context), 0);
});
test('metadata and named cache respect their combined 96 handle budget', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xd1)));
    const main = await createHost(profile), resources = main.stores.get(1).resources;
    for (let index = 0; index < 30; index++) {
        const id = encodeId(Buffer.from((index + 1).toString(16).padStart(32, '0'), 'hex'));
        for (let database = 0; database < 2; database++) main.stores.set(3 + index * 2 + database, OpfsStore.empty(new File(), resources));
        main.call('pb_register_personal', { volume: { id, owner: account, name: 'Personal', share_key_with_service: true, personal: true }, key, replica_ids: [1, 2] });
    }
    assert.equal(resources.stores, 63); assert.equal(main.volumes().length, 1);
    assert.throws(() => main.call('pb_register_personal', { volume: { id: encodeId(new Uint8Array(16).fill(0xd3)), owner: account, name: 'Full', share_key_with_service: true, personal: true }, key, replica_ids: [1, 2] }));
    assert.throws(() => main.call('pb_select_personal', { session: profile.session.id, grants: [], volumes: [] }), /unknown field/);
});
test('database catalog pages bound output and fence the source root across suspended work', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xd4))); profile.root.share_key_with_service = true;
    profile.local.databases = Array.from({ length: 64 }, (_, index) => ({
        id: encodeId(Buffer.from((index + 1).toString(16).padStart(30, '0'), 'hex')), name: `db/${index}`, kind: { type: 'kv_db' },
    }));
    const runtime = await createHost(profile);
    const page = request => { const input = encoder.encode(JSON.stringify({ volume: profile.root.id, after: null, expected_root: null, ...request })); return runtime.raw('pb_catalog_page', input, input.length); };
    const result = JSON.parse(new TextDecoder().decode(page({})));
    assert.equal(result.databases.length, 66); assert.equal(result.next, profile.local.databases.at(-1).id); assert.equal(result.root.length, 32);
    assert.deepEqual(result.databases.slice(0, 2), ['index', 'revocations']);
    const final = JSON.parse(new TextDecoder().decode(page({ after: profile.local.databases.at(-1).id, expected_root: result.root })));
    assert.deepEqual(final.databases, []); assert.equal(final.next, null);
    assert(page({ after: profile.local.databases.at(-1).id }) < 0, 'continuation requires its original source root');
    assert(page({ expected_root: result.root.map(byte => byte ^ 255) }) < 0, 'changed source roots never finish a prepared catalog');
    assert(runtime.stores.size <= 31, 'metadata plus at most 27 named handles reserve preparation credit');
});
test('compiled host length fuzzing rejects bad arguments and remains usable', async () => {
    const runtime = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0xe1))));
    const { fuzzWebHostLengths } = await import('../../../fuzz/fuzz_targets/web_host.mjs');
    assert.equal(fuzzWebHostLengths(runtime.host.exports, 10000), 10000); assert.equal(runtime.volumes().length, 1);
    assert.equal(runtime.raw('pb_blob_digest', empty, 0).length, 32); assert.equal(runtime.host.exports.pb_input(64 * 1024 * 1024 + 1), 0);
});

test('owned root grants retain persisted incarnations and reject stale metadata acquisition', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0x10)));
    const personalId = encodeId(new Uint8Array(16).fill(0x30)), main = await createHost(profile);
    const personal = await createHost(bootstrap(personalId, { personal: true }));
    main.stores.set(3, personal.stores.get(1)); main.stores.set(4, personal.stores.get(2));
    main.call('pb_register_personal', { volume: { id: personalId, owner: account, name: 'Personal', share_key_with_service: false, personal: true }, key, replica_ids: [7, 8] });
    const select = (incarnation, write = false) => main.call('pb_select_personal', {
        session: profile.session.id, grants: incarnation ? [{ volume: personalId, write, incarnation }] : [],
    });
    const metadata = () => {
        const host = main.host.exports;
        assert.equal(host.pb_web_volumes(), 0);
        const data = new DataView(host.memory.buffer), pointer = host.pb_web_result();
        const records = data.getUint32(pointer + 16, true), count = data.getUint32(pointer + 24, true);
        return { revision: data.getBigUint64(pointer + 8, true), count,
            grant: count === 2 ? Buffer.from(host.memory.buffer, records + data.getUint32(pointer + 28, true) + 64, 16).toString('hex') : null };
    };
    select(encodeId(new Uint8Array(16).fill(0xa1))); const first = metadata();
    assert.equal(main.host.exports.pb_web_volumes_acquire(first.revision), 0);
    const handles = new Uint32Array(main.host.exports.memory.buffer, main.host.exports.pb_output(), 4).slice();
    select(encodeId(new Uint8Array(16).fill(0xa1))); assert.deepEqual(metadata(), first);
    select(encodeId(new Uint8Array(16).fill(0xa2)));
    assert.equal(main.host.exports.pb_web_volumes_acquire(first.revision), -10);
    const replaced = metadata(); assert.notEqual(replaced.grant, first.grant);
    // The old owned handles remain releasable after revocation.
    for (let i = 0; i < 4; i += 2) assert.equal(main.host.exports.pb_web_file_dispose(handles[i + 1], handles[i]), 0);
    select(null); select(encodeId(new Uint8Array(16).fill(0xa3))); // No refresh between removal and re-addition.
    const readded = metadata(); assert.notEqual(readded.grant, replaced.grant);
    select(encodeId(new Uint8Array(16).fill(0xa4)), true);
    assert.notEqual(metadata().grant, readded.grant);
    assert.throws(() => select(encodeId(new Uint8Array(16).fill(0x00))));
});

test('an admitted browser guest can receive and publish the Shared database with its original invitation', async () => {
    const addressHost = await createHost(bootstrap(encodeId(new Uint8Array(16).fill(0x81))));
    const original = bootstrap(addressHost.volumes()[0]);
    const { session_id } = await deriveSessionKeys(new Uint8Array(16).fill(0x49));
    const request = encoder.encode(JSON.stringify({ app: original.session.app, session: session_id }));
    const { world, shared } = JSON.parse(decoder.decode(addressHost.raw('pb_scope_addresses', request, request.length)));
    original.root.id = shared;
    original.root.share_key_with_service = true;
    Object.assign(original.session, { id: session_id, root_volume: world, shared_volume: shared });
    const owner = await createHost(original);

    // Deliver the same native provisioning fact used by the blob authority
    // fixtures, in a delta for this Shared Index's database fingerprint.
    const { snapshots } = JSON.parse(await readFile(new URL('../tests/blob-authority-vectors.json', import.meta.url)));
    const [storageKey, stored] = snapshots.read_only.find(([key]) => key[0] === 0 && key[1] === 3 && key[2] === 18);
    const key = Uint8Array.from(storageKey.slice(1));
    assert.equal(decoder.decode(key.subarray(2)), original.session.resolved_build_id);
    const hello = owner.raw('pb_sync_begin', new Uint8Array(), 1, 0);
    assert(hello instanceof Uint8Array);
    assert.equal(owner.host.exports.pb_sync_finish(0), 0);
    const delta = new Uint8Array(32 + 6 + key.length + stored.length), view = new DataView(delta.buffer);
    view.setUint32(0, delta.length, true); view.setUint16(4, 6, true); view.setUint16(8, 1, true);
    delta[10] = hello[10]; delta.set(hello.subarray(12, 20), 12); delta.set(stored.slice(2, 10), 20);
    view.setUint32(28, 1, true); view.setUint16(32, key.length, true); view.setUint32(34, stored.length, true);
    delta.set(key, 38); delta.set(stored, 38 + key.length);
    const provision = new Uint8Array(32 + delta.length);
    provision.set(decodeId(shared, 16)); provision.set(delta, 32);
    assert(owner.raw('pb_live_apply', provision, provision.length) instanceof Uint8Array);

    const guestProfile = { ...original, account: encodeId(new Uint8Array(16).fill(0x66)), local: null };
    const guestStores = new Map();
    for (const [slot, store] of owner.stores) {
        const replica = new File();
        replica.bytes = store.handle.bytes.slice();
        guestStores.set(slot, await OpfsStore.open(replica, owner.resources, async () => {}));
    }
    const guest = await createHost(guestProfile, guestStores);
    guest.register(entryId);
    assert.deepEqual(guest.volumes(), [shared]);
    assert.equal(guest.read(), false);
    assert.equal(guest.edit(true), 0);
    const deliver = (from, to) => {
        const outgoing = from.raw('pb_live_peek', new Uint8Array());
        assert(outgoing instanceof Uint8Array && outgoing.length > 41);
        assert.equal(databaseIdentity(outgoing.subarray(16, 32)), entryId);
        const apply = new Uint8Array(outgoing.length - 9);
        apply.set(outgoing.subarray(0, 32)); apply.set(outgoing.subarray(41), 32);
        assert(to.raw('pb_live_apply', apply, apply.length) instanceof Uint8Array);
    };
    assert.equal(owner.read(), false);
    deliver(guest, owner);
    assert.equal(owner.read(), true);
    assert.equal(owner.edit(false), 0);
    assert.equal(guest.read(), true);
    deliver(owner, guest);
    assert.equal(guest.read(), false);

    const ordinary = await createHost({ ...guestProfile, session: { ...original.session, shared_volume: null, root_volume: shared } }, owner.stores);
    const volumeRequest = encoder.encode(JSON.stringify(shared));
    assert.equal(JSON.parse(decoder.decode(ordinary.raw('pb_replica_catalog_allowed', volumeRequest, volumeRequest.length))), false);
    assert.throws(() => ordinary.register(entryId), /lookup failed/);
    const other = await deriveSessionKeys(new Uint8Array(16).fill(0x48));
    await assert.rejects(createHost({ ...guestProfile, session: { ...original.session, id: other.session_id } }, owner.stores), /Shared session binding/);
});

test('Shared invitation and recovery retain only the native session capability and its original scope', async () => {
    const profile = bootstrap(encodeId(new Uint8Array(16).fill(0xa8))), host = await createHost(profile);
    const resolve = async request => {
        const bytes = encoder.encode(JSON.stringify(request));
        return JSON.parse(decoder.decode(host.raw('pb_scope_addresses', bytes, bytes.length)));
    };
    setScopeAddresses(resolve);
    const seed = new Uint8Array(16).fill(24), { session_id } = await deriveSessionKeys(seed);
    const publisher = encodeId(new Uint8Array(16).fill(88));
    const app = { publisher, app_id: 'signed-note' };
    const { world, shared } = await resolve({ app, session: session_id });
    assert.notEqual(world, shared);
    assert.deepEqual(await resolve({ app }), { world });
    const invitation = {
        seed: Array.from(seed), session: { ...profile.session, creator: profile.account, id: session_id, app, root_volume: world, shared_volume: shared },
        shared: { volume: { ...profile.root, id: shared, personal: false, share_key_with_service: true },
            credentials: { database_key: Array(32).fill(31), blob_key: Array(32).fill(32) } },
    };
    await retainedSessionKeys(invitation);
    for (const change of [
        value => { value.root = profile.root; },
        value => { value.root_credentials = value.shared.credentials; },
        value => { value.shared.volume.owner = publisher; },
        value => { value.shared.volume.personal = true; },
        value => { value.shared.volume.share_key_with_service = false; },
        value => { value.session.app.app_id = 'different-app'; },
        value => { value.session.root_volume = shared; },
        value => { value.shared.volume.id = value.session.shared_volume = profile.root.id; },
    ]) {
        const invalid = structuredClone(invitation);
        change(invalid);
        await assert.rejects(retainedSessionKeys(invalid));
    }
    const empty = { account: profile.account, volumes: [], invitations: [] };
    const scope = { kind: 'session', id: session_id, creation_build: profile.session.resolved_build_id, user_volume: null };
    const catalog = { ...empty, scopes: [scope], volumes: [replicaCapability(invitation)], invitations: [invitation] };
    assert.deepEqual(await mergeCatalog(catalog, empty), catalog);
    assert.deepEqual(await personalVolumes(catalog), []);
    const accountKey = new Uint8Array(16).fill(14);
    const ciphertext = await seal(profile.account, 1, await backupKey(profile.account, accountKey), encoder.encode(JSON.stringify(catalog)));
    const vault = { request: async () => ({ data: { revision: 1, ciphertext } }) };
    const restored = new RecoveryClient(vault, profile.account, recoveryMemory(), callback => callback());
    assert.equal((await restored.recover(formatAccountKey(accountKey))).kind, 'ready');
    assert.deepEqual((await restored.state()).catalog, catalog);
    const updated = structuredClone(catalog); updated.scopes[0].creation_build = encodeId(new Uint8Array(32).fill(99));
    await assert.rejects(mergeCatalog(catalog, updated), /scope conflicts/);
    assert.deepEqual(await availableCatalog(catalog, [shared]), empty);

    const worldScope = { kind: 'world', app, build: profile.session.resolved_build_id, initialization: Array(32).fill(19) };
    const worldCapability = { ...invitation.shared, volume: { ...invitation.shared.volume, id: world, owner: publisher } };
    const published = { account: publisher, volumes: [worldCapability], invitations: [], scopes: [worldScope] };
    assert.deepEqual(await mergeCatalog(published, { account: publisher, volumes: [], invitations: [] }), published);
    const changed = structuredClone(published); changed.scopes[0].initialization[0]++;
    await assert.rejects(mergeCatalog(published, changed), /scope conflicts/);
    assert.deepEqual(await availableCatalog(published, [world]), { account: publisher, volumes: [], invitations: [] });
});
