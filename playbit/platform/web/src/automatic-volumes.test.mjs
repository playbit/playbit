import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { bootstrap, createHost, entryId, File, pull } from '../tests/host-fixture.mjs';
import { OpfsStore } from './opfs-store.ts';
import { prepareWriter } from '../tests/file-metadata-fixture.mjs';
import { encodeId, decodeId } from './identifier.ts';

const fixture = JSON.parse(await readFile(new URL('../tests/automatic-volume-package.json', import.meta.url)));
const contents = Buffer.from(fixture.bundle, 'base64');
const projection = JSON.parse(await readFile(new URL('../tests/automatic-world-index.json', import.meta.url)));
const encoder = new TextEncoder(), decoder = new TextDecoder();
const id = (byte, count = 16) => encodeId(new Uint8Array(count).fill(byte));
const empty = { permissions: { grants: [] }, databases: [], blobs: [] };
const app = { publisher: id(1), app_id: 'scope-fixture' };
function invoke(engine, name, value, ...args) {
    const input = value === undefined ? new Uint8Array() : encoder.encode(JSON.stringify(value));
    return engine.raw(name, input, ...args, ...(value === undefined ? [] : [input.length]));
}
function data(engine, name, value, ...args) {
    const result = invoke(engine, name, value, ...args);
    if (typeof result === 'number') {
        const host = engine.host.exports, length = host.pb_error();
        assert.fail(`${name} failed with ${result}: ${decoder.decode(new Uint8Array(host.memory.buffer, host.pb_output(), length))}`);
    }
    return result.length ? JSON.parse(decoder.decode(result)) : null;
}
async function initialize(sessionId, account, installation, existing, userVolume, local = empty, current = contents, rootBacked = false, creator = account) {
    const resolver = await createHost(bootstrap(id(80)));
    const scope = data(resolver, 'pb_scope_addresses', { app, session: sessionId, account, installation });
    const primary = rootBacked ? scope.world : scope.shared;
    const profile = bootstrap(primary);
    Object.assign(profile, { account, installation, local, user_volume: userVolume });
    const build = encodeId(resolver.raw('pb_blob_digest', current, current.length));
    profile.root = { id: primary, owner: rootBacked ? app.publisher : creator, name: rootBacked ? 'World' : 'Shared', personal: false, share_key_with_service: true };
    profile.session = { ...profile.session, app, id: sessionId, root_volume: scope.world, shared_volume: rootBacked ? undefined : scope.shared, creator,
        resolved_build_id: build, selector: { kind: 'exact', build_id: build } };
    const packages = new Map([[1, current], [2, contents]]);
    const engine = await createHost(profile, existing, () => fixture.now, imports => ({
        ...imports, bundle_length: slot => packages.get(slot).length,
        bundle_read(slot, start, pointer, length) {
            new Uint8Array(engine.host.exports.memory.buffer, pointer, length).set(packages.get(slot).subarray(start, start + length));
            return length;
        }, bundle_close() {},
    }));
    authenticate(engine, profile, 1);
    assert.equal(engine.host.exports.pb_package_adopt(1), 0);
    return { engine, profile, scope, build };
}
function authenticate(engine, profile, slot) {
    const host = engine.host.exports;
    let size;
    do { size = host.pb_package(slot); } while (size === 0);
    if (size < 0) {
        const length = host.pb_error();
        assert.fail(decoder.decode(new Uint8Array(host.memory.buffer, host.pb_output(), length)));
    }
    const parsed = JSON.parse(decoder.decode(new Uint8Array(host.memory.buffer, host.pb_output(), size)));
    assert.deepEqual(parsed.unsupported_host_features, []);
    data(engine, 'pb_package_authenticate', {
        anchor: { release_issuer: Array.from(Buffer.from(fixture.issuer, 'base64')) },
        status: Array.from(Buffer.from(fixture.status, 'base64')), session_id: profile.session.id, expected_app: app, bound_session: profile.session,
    }, slot);
}
function select(engine, role) {
    const host = engine.host.exports;
    assert.equal(host.pb_web_volumes(), 0);
    const view = new DataView(host.memory.buffer), header = host.pb_web_result();
    const pointer = view.getUint32(header + 16, true), count = view.getUint32(header + 24, true);
    for (let i = 0; i < count; i++) {
        const position = pointer + i * view.getUint32(header + 28, true);
        if (view.getUint32(position + 80, true) === role)
            return view.getBigUint64(position + 8, true);
    }
    assert.fail(`role ${role} absent`);
}
function resolve(engine, handle, name) {
    const bytes = encoder.encode(name);
    const input = new Uint8Array(8 + bytes.length); input.set(bytes, 8);
    const status = engine.raw('pb_web_entry_resolve', input, handle >> 32n, Number(handle & 0xffffffffn), 8, bytes.length);
    if (typeof status === 'number') return status;
    const host = engine.host.exports;
    return new Uint8Array(host.memory.buffer, host.pb_output(), 32).slice();
}
function controls(engine, base) {
    for (const slot of [base + 1, base + 2]) {
        if (!engine.stores.has(slot)) engine.stores.set(slot, OpfsStore.empty(new File(), engine.resources));
    }
}

function registerLocal(engine, scope, account, build) {
    controls(engine, 2);
    return {
        role: 'local',
        volume: { id: scope.local, owner: account, name: 'Local', personal: false, share_key_with_service: false },
        base: 2, key: Array(32).fill(91), replica_ids: [7, 8], creation_build: build,
    };
}

function put(engine, entry, key, byte) {
    const input = new Uint8Array(48);
    input.set(entry, 8);
    input[40] = key;
    input[41] = byte;
    return engine.raw('pb_web_kv_put', input, 8, 40, 1, 41, 1);
}

async function openWorld(existing, local, account = id(2), registerBackings = true) {
    const { engine, scope } = await initialize(id(3, 32), account, id(43), existing, undefined, local);
    const world = select(engine, 1);
    assert.equal(resolve(engine, world, 'world-state'), -6);
    const volume = { id: scope.world, owner: app.publisher, name: 'World', personal: false, share_key_with_service: true };
    const registration = { role: 'root', volume, base: 2, replica_ids: [7, 8] };
    controls(engine, 2);
    if (account !== app.publisher) assert.equal(invoke(engine, 'pb_automatic_register', { ...registration, key: Array(32).fill(17) }), -2);
    if (!existing) {
        // Snapshot of a canonical World projection with a provisioned build and three different entry grants.
        const write = engine.stores.get(3).begin(true);
        for (const [key, value] of projection.rows) write.put(Buffer.from(key, 'base64'), Buffer.from(value, 'base64'));
        write.end(true);
    }
    data(engine, 'pb_automatic_register', registration);
    data(engine, 'pb_automatic_ready', { role: 'root', volume: scope.world });
    if (registerBackings) {
        for (const [entry, slot] of [[81, 66], [82, 67], [83, 68]]) engine.register(id(entry, 15), scope.world, slot);
    }
    return { engine, world, scope, registration };
}

test('the signed app lists all role addresses without preparing their storage', async () => {
    const { engine, scope, profile } = await initialize(id(3, 32), id(1), id(42));
    assert.deepEqual(engine.volumes().slice(0, 6),
        [scope.world, scope.shared, scope.user, scope.user_session, scope.local, scope.local_session]);
    assert.deepEqual(data(engine, 'pb_automatic_needed'), []);
    assert.deepEqual([...engine.stores.keys()], [1, 2]);
    const other = (session, account, installation) => data(engine, 'pb_scope_addresses', { app, session, account, installation });
    const next = other(id(4, 32), profile.account, profile.installation);
    for (const name of ['world', 'user', 'local']) assert.equal(next[name], scope[name]);
    for (const name of ['shared', 'user_session', 'local_session']) assert.notEqual(next[name], scope[name]);
    const device = other(profile.session.id, profile.account, id(43));
    assert.equal(device.user, scope.user);
    assert.notEqual(device.local, scope.local);
    const another = other(profile.session.id, id(2), profile.installation);
    assert.equal(another.shared, scope.shared);
    assert.notEqual(another.user, scope.user);
    assert.notEqual(another.local, scope.local);
});

test('private first-use can resume after a lock and restart without reapplying the signed initializer', async () => {
    const session = id(3, 32), account = id(1), device = id(42);
    const { engine, scope, build } = await initialize(session, account, device);
    const local = select(engine, 5);
    assert.equal(resolve(engine, local, 'local-data'), -6);
    const address = { role: 'local', volume: scope.local };
    const registration = registerLocal(engine, scope, account, build);
    assert.deepEqual(data(engine, 'pb_automatic_needed'), [address]);
    const foreign = { ...registration, volume: { ...registration.volume, owner: id(2) } };
    assert.equal(invoke(engine, 'pb_automatic_register', foreign), -2);
    const initializer = data(engine, 'pb_automatic_register', registration);
    const [seed] = initializer.databases;
    engine.register(seed.id, scope.local, 66);
    assert.equal(invoke(engine, 'pb_automatic_ready', address), -1);
    const backings = [3, 4, 66].map(slot => [slot, engine.stores.get(slot).handle]);
    data(engine, 'pb_automatic_failed', { ...address, status: -12 });
    assert.deepEqual(data(engine, 'pb_automatic_needed'), []);
    assert.equal(resolve(engine, local, 'local-data'), -12);
    assert.deepEqual(data(engine, 'pb_automatic_needed'), []);
    assert.equal(resolve(engine, local, 'local-data'), -6);
    for (const [slot, file] of backings) engine.stores.set(slot, await OpfsStore.open(file, engine.resources, async () => {}));
    assert.deepEqual(data(engine, 'pb_automatic_register', registration), initializer);
    engine.register(seed.id, scope.local, 66);
    data(engine, 'pb_seed_database', { volume: scope.local, seed });
    data(engine, 'pb_automatic_ready', address);
    const entry = resolve(engine, local, 'local-data');
    assert(put(engine, entry, 3, 27) instanceof Uint8Array);
    assert.equal(engine.host.exports.pb_live_peek(), 0);
    const reloaded = (await initialize(session, account, device, engine.stores)).engine;
    const same = select(reloaded, 5);
    assert.equal(resolve(reloaded, same, 'local-data'), -6);
    assert.equal(data(reloaded, 'pb_automatic_register', { ...registration, creation_build: undefined }), null);
    data(reloaded, 'pb_automatic_ready', address);
    assert.deepEqual(resolve(reloaded, same, 'local-data'), entry);
});

test('only the Shared copy of identical File content acquires upload protection', async () => {
    const { engine, scope, build, profile } = await initialize(id(3, 32), id(1), id(42));
    assert.equal(resolve(engine, select(engine, 5), 'local-data'), -6);
    const registration = registerLocal(engine, scope, profile.account, build);
    const { databases: [seed] } = data(engine, 'pb_automatic_register', registration);
    engine.register(seed.id, scope.local, 66);
    data(engine, 'pb_seed_database', { volume: scope.local, seed });
    data(engine, 'pb_automatic_ready', { role: 'local', volume: scope.local });

    const localKeys = (slot, prefix) => engine.stores.get(slot).keys.filter(key =>
        key.startsWith('81' + Buffer.from(prefix).toString('hex')));
    for (const volume of [scope.shared, scope.local]) {
        const writer = prepareWriter(engine, 'hello.txt', Uint8Array.of(7, 8), engine.volumes().indexOf(volume));
        assert.equal(writer.reference.volume, volume);
        writer.reference.encrypted_key = '51'.repeat(32);
        writer.publish();
    }
    assert.equal(localKeys(1, 'blob-protection/').length, 1);
    assert.equal(localKeys(1, 'blob-pending/').length, 1);
    assert.equal(localKeys(3, 'blob-pending/').length, 0);
});

test('a retained private descriptor resumes its original signed initializer after an app update with no index yet', async () => {
    const session = id(3, 32), account = app.publisher, device = id(42), creator = id(2);
    const first = await initialize(session, account, device, undefined, undefined, empty, contents, true, creator);
    const updated = Buffer.from(fixture.updated_bundle, 'base64');
    for (const [role, tag] of [['user', 3], ['user-session', 4], ['local', 5], ['local-session', 6]]) {
        const original = select(first.engine, tag);
        assert.equal(resolve(first.engine, original, 'local-data'), -6);
        // The browser persisted this descriptor but exited before registering index/revocations.
        const address = { role, volume: first.scope[role.replace('-', '_')] };
        const record = { id: address.volume, owner: account, name: role, personal: false, share_key_with_service: false };
        const stored = { role, volume: record, base: 2, key: Array(32).fill(91), replica_ids: [7, 8], creation_build: first.build };
        const { engine, profile, build } = await initialize(session, account, device, undefined, undefined, empty, updated, true, creator);
        assert.notEqual(build, first.build);
        const handle = select(engine, tag);
        assert.equal(resolve(engine, handle, 'local-data'), -6);
        controls(engine, 2);
        assert.equal(invoke(engine, 'pb_automatic_register', stored), -2, 'the current package cannot stand in for the retained build');
        controls(engine, 2);
        authenticate(engine, profile, 2);
        const initializer = data(engine, 'pb_automatic_register', { ...stored, creation_slot: 2 });
        assert.deepEqual(initializer.databases.map(seed => seed.name), role === 'local' ? ['local-data'] : []);
        for (const seed of initializer.databases) {
            engine.register(seed.id, address.volume, 66);
            data(engine, 'pb_seed_database', { volume: address.volume, seed });
        }
        data(engine, 'pb_automatic_ready', address);
        assert.equal(resolve(engine, handle, 'updated-' + role + '-data'), -1);
        if (role === 'local') assert.ok(resolve(engine, handle, 'local-data') instanceof Uint8Array);
    }
});

test('World entry overrides authorize the writable KV and protect its read-only sibling across catch-up', async () => {
    const { engine, world, scope } = await openWorld();
    const guest = engine.host.exports;
    assert.equal(guest.pb_web_volume_check(world >> 32n, Number(world & 0xffffffffn), 2), -2);
    assert.equal(guest.pb_web_volume_check(world >> 32n, Number(world & 0xffffffffn), 8), 0);
    assert(put(engine, resolve(engine, world, 'world-state'), 1, 9) instanceof Uint8Array);
    assert.equal(put(engine, resolve(engine, world, 'sibling'), 1, 9), -2);
    assert(guest.pb_live_peek() > 0, 'authorized World mutation is eligible for publication');
    assert.deepEqual(data(engine, 'pb_upload_catalog_page', { volume: scope.world }).databases, [id(81, 15)]);
    assert(guest.pb_sync_begin(66, 1) >= 0);
    assert.equal(guest.pb_sync_finish(0), 0);
    for (const slot of [3, 4, 67, 68]) assert.equal(guest.pb_sync_begin(slot, 1), -2);
});

test('projected publisher and guest share the KV ceiling while the credentialed publisher Root retains schema rights', async () => {
    const allowed = (engine, handle, rights) => engine.host.exports.pb_web_volume_authorize(handle >> 32n, Number(handle & 0xffffffffn), rights);
    for (const account of [app.publisher, id(2)]) {
        const { engine, world } = await openWorld(undefined, undefined, account);
        assert.equal(allowed(engine, world, 1), 0);
        assert.equal(allowed(engine, world, 2), -2);
        assert.equal(allowed(engine, world, 4), -2);
        assert.equal(engine.edit(true), -2);
        assert(put(engine, resolve(engine, world, 'world-state'), 2, 10) instanceof Uint8Array);
    }
    const { engine } = await initialize(id(3, 32), app.publisher, id(43), undefined, undefined, bootstrap(id(90)).local, contents, true);
    assert.equal(allowed(engine, select(engine, 1), 7), 0);
    assert.equal(engine.edit(true), 0);
});

test('withdrawing the admitted World fences cached handles and leaves private Shared changes live', async () => {
    const local = { ...empty, databases: [{ id: id(77, 15), name: 'room', kind: { type: 'kv_db' } }] };
    const { engine, world, scope, registration } = await openWorld(undefined, local);
    const shared = select(engine, 2), room = resolve(engine, shared, 'room');
    const entry = resolve(engine, world, 'world-state');
    const read = () => {
        const input = new Uint8Array(112), view = new DataView(input.buffer);
        input.set(entry, 8); input[40] = 1;
        view.setUint32(48, 64, true); view.setUint32(64, 1, true);
        const result = engine.raw('pb_web_kv_read', input, 8, 40, 1, 48);
        return result instanceof Uint8Array ? result[49] : result;
    };
    assert(put(engine, entry, 1, 9) instanceof Uint8Array);
    assert.equal(read(), 9);
    assert(put(engine, room, 1, 7) instanceof Uint8Array);
    const deny = (volume, revoked) => invoke(engine, 'pb_automatic_world_denied', { volume, revoked });
    const backings = [3, 4, 66].map(slot => [slot, engine.stores.get(slot).handle]);
    assert.equal(deny(scope.shared, true), -2);
    for (const revoked of [false, true]) {
        assert(deny(scope.world, revoked) instanceof Uint8Array);
        assert(deny(scope.world, revoked) instanceof Uint8Array);
        assert.equal(resolve(engine, world, 'world-state'), -2);
        assert.equal(read(), -2);
        assert.equal(put(engine, entry, 1, 10), -2);
        assert.deepEqual(resolve(engine, shared, 'room'), room);
        assert(put(engine, room, 1, 8) instanceof Uint8Array);
        const next = engine.raw('pb_live_peek', new Uint8Array());
        assert.deepEqual(next.slice(0, 16), decodeId(scope.shared));
        for (const [slot, file] of backings) engine.stores.set(slot, await OpfsStore.open(file, engine.resources, async () => {}));
        data(engine, 'pb_automatic_register', registration);
        const address = { role: 'root', volume: scope.world };
        if (revoked) {
            assert.equal(data(engine, 'pb_authority_refresh_required', scope.world), true);
            assert.equal(invoke(engine, 'pb_automatic_ready', address), -2);
        } else {
            data(engine, 'pb_automatic_ready', address);
            engine.register(id(81, 15), scope.world, 66);
            assert.equal(read(), 9, 'unpublished World edit survived the withdrawn attachment');
        }
    }
});

test('a temporarily unavailable first World open remains requested across reconnect', async () => {
    const { engine, scope } = await initialize(id(3, 32), id(2), id(43));
    const world = select(engine, 1), address = {role:'root', volume:scope.world};
    assert.equal(resolve(engine, world, 'world-state'), -6);
    data(engine, 'pb_automatic_failed', {...address, status:-5});
    assert.equal(resolve(engine, world, 'world-state'), -5);
    assert.equal(engine.host.exports.pb_database_retry(), 0);
    assert.deepEqual(data(engine, 'pb_automatic_needed'), [address]);
});

test('a World subscriber scopes live participant denial to the changed volume', async () => {
    const { engine, world, scope } = await openWorld();
    const shared = select(engine, 2);
    const entry = resolve(engine, world, 'world-state');
    const terminal = new Uint8Array(33);
    terminal.set(decodeId(scope.world)); terminal.set(decodeId(scope.world), 16);
    terminal[32] = 2; // TerminalCause::ParticipantDenied.
    assert(engine.raw('pb_live_terminal', terminal, terminal.length) instanceof Uint8Array);
    assert.equal(data(engine, 'pb_authority_refresh_required', scope.world), true);
    assert.equal(resolve(engine, world, 'world-state'), -2);
    assert.equal(put(engine, entry, 1, 10), -2);
    assert.equal(resolve(engine, shared, 'unused'), -1);
    // A terminal for this same subscription's Shared dependency has session scope.
    terminal.set(decodeId(scope.shared), 16);
    assert(engine.raw('pb_live_terminal', terminal, terminal.length) instanceof Uint8Array);
    assert.equal(resolve(engine, shared, 'unused'), -2);
});

test('a stopped worker recovers the exact locally authored World append and cannot submit the rest of its backing', async () => {
    const { engine, world, scope } = await openWorld();
    const entry = resolve(engine, world, 'append-only');
    const database = id(83, 15), address = { volume: scope.world, database };
    assert(put(engine, entry, 1, 9) instanceof Uint8Array);
    assert.equal(put(engine, entry, 1, 10), -2);
    const before = invoke(engine, 'pb_world_append_next', address);
    assert(before instanceof Uint8Array && before.length > 16);
    const clock = Array.from(before.subarray(8, 16));
    const reloaded = (await openWorld(engine.stores)).engine;
    assert.equal(reloaded.host.exports.pb_live_peek(), 0);
    assert.deepEqual(invoke(reloaded, 'pb_world_append_next', address), before);
    assert.deepEqual(data(reloaded, 'pb_world_append_catalog_page', { volume: scope.world }).databases, [id(81, 15), database]);
    assert.equal(invoke(reloaded, 'pb_world_append_next', { ...address, database: id(82, 15) }), -2);
    assert.equal(invoke(reloaded, 'pb_world_append_ack', { ...address, clock }), -10);
    assert.deepEqual(invoke(reloaded, 'pb_world_append_next', address), before);
    data(reloaded, 'pb_world_append_ack', { ...address, clock });
    assert.equal(invoke(reloaded, 'pb_world_append_next', address).length, 0);
});

test('only the durable receipt for a live World append clears its exact birth', async () => {
    const { engine, world, scope } = await openWorld();
    const entry = resolve(engine, world, 'append-only');
    const address = { volume: scope.world, database: id(83, 15) };
    for (const key of [1, 2]) assert(put(engine, entry, key, key) instanceof Uint8Array);
    const firstBirth = invoke(engine, 'pb_world_append_next', address);
    const rejected = engine.raw('pb_live_peek', new Uint8Array());
    const sequence = frame => new DataView(frame.buffer, frame.byteOffset).getBigUint64(32, true);
    assert.equal(engine.host.exports.pb_live_ack(sequence(rejected)), 0);
    const accepted = engine.raw('pb_live_peek', new Uint8Array()), bytes = accepted.subarray(41);
    assert(bytes.length > 0);
    assert(engine.raw('pb_world_append_published', bytes, 68, bytes.length) instanceof Uint8Array);
    assert.equal(engine.host.exports.pb_live_ack(sequence(accepted)), 0);
    assert.equal(engine.host.exports.pb_live_peek(), 0);
    const first = invoke(engine, 'pb_world_append_next', address);
    assert.deepEqual(first.subarray(8, 16), firstBirth.subarray(8, 16));
    assert.equal(invoke(engine, 'pb_world_append_next', { ...address, through: Array.from(first.subarray(0, 8)),
        after: Array.from(first.subarray(8, 16)) }).length, 0);
});

test('rearming an overflowed publication feed keeps World KV snapshots and exact append births recoverable', async () => {
    const { engine, world, scope } = await openWorld();
    const host = engine.host.exports, change = resolve(engine, world, 'world-state');
    const append = resolve(engine, world, 'append-only');
    assert(put(engine, append, 8, 91) instanceof Uint8Array);
    const address = { volume: scope.world, database: id(83, 15) };
    const birth = invoke(engine, 'pb_world_append_next', address);
    assert(birth.length > 16);
    const before = engine.raw('pb_live_peek', new Uint8Array());
    assert.equal(host.pb_live_rebase(1), 0);
    assert.deepEqual(engine.raw('pb_live_peek', new Uint8Array()), before);
    for (let i = 0; i < 65; i++) assert(put(engine, change, i, i) instanceof Uint8Array);
    assert(host.pb_live_peek() < 0);
    assert.equal(host.pb_live_rebase(1), 0);
    assert.equal(host.pb_live_peek(), 0);
    assert.deepEqual(data(engine, 'pb_upload_catalog_page', {volume: scope.world}).databases, [id(81, 15)]);
    assert(host.pb_sync_begin(66, 1) >= 0); assert.equal(host.pb_sync_finish(0), 0);
    assert.deepEqual(invoke(engine, 'pb_world_append_next', address), birth);
    assert(put(engine, change, 66, 66) instanceof Uint8Array);
    assert(host.pb_live_peek() > 0);
});

function accept(engine, world, entry, encoded) {
    const delta = Buffer.from(encoded, 'base64'), bytes = new Uint8Array(32 + delta.length);
    bytes.set(decodeId(world));
    if (entry) {
        bytes[16] = 2;
        bytes.set(decodeId(entry, 15), 17);
    }
    bytes.set(delta, 32);
    const result = engine.raw('pb_live_apply', bytes, bytes.length);
    assert(result instanceof Uint8Array, `AP delivery returned ${result}`);
}

test('World mutation-only grants reopen qualified KV backings without granting Read', async () => {
    const { engine, world, scope } = await openWorld();
    const volume = scope.world;
    const [write, noService, append] = [81, 82, 83].map(byte => id(byte, 15));
    assert(put(engine, resolve(engine, world, 'world-state'), 1, 10) instanceof Uint8Array);
    assert(put(engine, resolve(engine, world, 'append-only'), 1, 11) instanceof Uint8Array);
    assert.equal(put(engine, resolve(engine, world, 'sibling'), 1, 12), -2);
    accept(engine, volume, null, projection.blind_mutation);
    const { engine: resumed } = await openWorld(engine.stores, undefined, undefined, false);
    for (const [database, slot] of [[write, 66], [noService, 67], [append, 68]]) {
        const handle = resumed.handle(1, 1, database), address = { volume, database };
        assert.equal(put(resumed, handle, 2, 20), -6);
        assert.deepEqual(data(resumed, 'pb_database_needed'), [address]);
        assert.equal(data(resumed, 'pb_database_preparation', address), 'local');
        resumed.register(database, volume, slot);
        assert(put(resumed, handle, 2, 20) instanceof Uint8Array);
        const input = new Uint8Array(112);
        input.set(handle, 8); input[40] = 1;
        const view = new DataView(input.buffer);
        view.setUint32(48, 64, true); view.setUint32(64, 1, true);
        assert.equal(resumed.raw('pb_web_kv_read', input, 8, 40, 1, 48), -2);
    }
    assert(put(resumed, resumed.handle(1, 1, write), 1, 21) instanceof Uint8Array);
    assert.equal(put(resumed, resumed.handle(1, 1, append), 1, 21), -2);
    assert.deepEqual(data(resumed, 'pb_catalog_page', { volume }).databases, ['index', 'revocations']);
    const uploads = data(resumed, 'pb_upload_catalog_page', { volume });
    assert.deepEqual(uploads.databases, [write]); assert.deepEqual(uploads.blind, [write]);
    assert.deepEqual(data(resumed, 'pb_world_append_catalog_page', { volume }).databases, [write, append]);
    assert.equal(resumed.host.exports.pb_sync_begin(66, 0), -2);
    assert.equal(resumed.host.exports.pb_sync_begin(66, 1), -2);
    assert(resumed.host.exports.pb_sync_begin(66, 2) >= 0);
    assert.equal(resumed.host.exports.pb_sync_finish(0), 0);
    assert.equal(resumed.host.exports.pb_sync_begin(67, 2), -2, 'Service Read still applies');
    assert.equal(resumed.host.exports.pb_sync_begin(68, 2), -2, 'Append cannot submit the whole backing');
    assert(invoke(resumed, 'pb_world_append_next', { volume, database: append }).length > 16);
});

test('first-time World mutation-only grants do not initialize an unreceived backing', async () => {
    const { engine, scope } = await openWorld(undefined, undefined, undefined, false);
    const volume = scope.world, unreceived = [81, 83];
    // Keep the received index and exact source identities, without local
    // backing-readiness records or physical backing files.
    const index = engine.stores.get(3).begin(true);
    for (const byte of unreceived) {
        index.put(Uint8Array.of(0x81, 3, 6, ...new Uint8Array(15).fill(byte)), null);
    }
    index.end(true);
    accept(engine, volume, null, projection.blind_mutation);
    for (const byte of unreceived) {
        const database = id(byte, 15), address = { volume, database };
        assert.equal(put(engine, engine.handle(1, 1, database), 1, 10), -1);
        assert.equal(invoke(engine, 'pb_database_preparation', address), -2);
    }
    assert.deepEqual(data(engine, 'pb_database_needed'), []);
    assert.deepEqual(data(engine, 'pb_upload_catalog_page', { volume }).databases, []);
    assert.deepEqual(data(engine, 'pb_world_append_catalog_page', { volume }).databases, []);
    assert.deepEqual([...engine.stores.keys()], [1, 2, 3, 4]);
});

test('World catch-up drops stale append payloads and immediately narrows its snapshot catalog after AP revocation', async () => {
    const { engine, world, scope } = await openWorld();
    const volume = scope.world, database = id(83, 15), address = { volume, database };
    assert(put(engine, resolve(engine, world, 'append-only'), 1, 9) instanceof Uint8Array);
    const offered = invoke(engine, 'pb_world_append_next', address);
    assert(offered.length > 16);
    const page = data(engine, 'pb_upload_catalog_page', { volume });
    accept(engine, volume, database, projection.replacement);
    const replaced = invoke(engine, 'pb_world_append_next', address);
    assert.equal(replaced.length, 16);
    assert.deepEqual(replaced.subarray(8), offered.subarray(8, 16));
    accept(engine, volume, null, projection.revocation);
    const blocked = engine.raw('pb_live_peek', new Uint8Array());
    assert.equal(blocked.length, 41, 'local revocation exposes no old publication bytes');
    assert.equal(blocked[40], 1);
    assert.equal(invoke(engine, 'pb_world_append_next', address), -2);
    assert.equal(invoke(engine, 'pb_world_append_ack', { ...address, clock: Array.from(offered.subarray(8, 16)) }), -2);
    assert.deepEqual(data(engine, 'pb_upload_catalog_page', { volume }).databases, []);
    assert.deepEqual(data(engine, 'pb_world_append_catalog_page', { volume }).databases, []);
    assert.equal(typeof invoke(engine, 'pb_upload_catalog_page', { volume, expected_root: page.root }), 'number');
    assert.equal(engine.host.exports.pb_sync_begin(66, 1), -2);
});

test('a recovered User can push its saved database after overflow without an individual chooser grant', async () => {
    const volume = id(93), profile = bootstrap(volume, { personal: true });
    profile.root.share_key_with_service = true;
    const saved = await createHost(profile);
    const remote = await createHost({ ...profile, replica_ids: [31, 32] });
    const { engine } = await initialize(id(3, 32), profile.account, id(42), undefined, volume);
    engine.stores.set(3, saved.stores.get(1)); engine.stores.set(4, saved.stores.get(2));
    const user = select(engine, 3);
    assert.equal(resolve(engine, user, 'unused'), -6);
    data(engine, 'pb_automatic_register', { role: 'user', volume: profile.root, key: profile.root_key,
        base: 2, replica_ids: profile.replica_ids });
    data(engine, 'pb_automatic_ready', { role: 'user', volume });
    engine.stores.set(66, saved.stores.get(65)); engine.register(entryId, volume, 66);
    for (let i = 0; i < 65; i++) assert.equal(engine.edit(i % 2 === 0, undefined, 3), 0);
    assert(engine.host.exports.pb_live_peek() < 0);
    assert.equal(engine.host.exports.pb_live_rebase(1), 0);
    assert.deepEqual(data(engine, 'pb_upload_catalog_page', { volume }).databases, [entryId]);
    assert.equal(remote.read(), false);
    pull(engine, remote, 66, 65);
    assert.equal(remote.read(), true);
});

test('a User override and a selected personal grant share their mount in either order', async () => {
    for (const personalFirst of [false, true]) {
        const volume = id(93), session = id(3, 32);
        const profile = bootstrap(volume, { personal: true });
        const personal = await createHost(profile);
        const { engine } = await initialize(session, profile.account, id(42), undefined, volume);
        engine.stores.set(3, personal.stores.get(1));
        engine.stores.set(4, personal.stores.get(2));
        const mount = { volume: profile.root, key: profile.root_key, base: 2, replica_ids: profile.replica_ids };
        const grant = () => {
            data(engine, 'pb_register_personal', mount);
            data(engine, 'pb_select_personal', { session, grants: [{ volume, write: false, incarnation: id(16) }] });
        };
        if (personalFirst) grant();
        const user = select(engine, 3);
        assert.equal(resolve(engine, user, 'unused'), -6);
        data(engine, 'pb_automatic_register', { ...mount, role: 'user' });
        data(engine, 'pb_automatic_ready', { volume, role: 'user' });
        if (!personalFirst) grant();
        assert.equal(engine.volumes().filter(item => item === volume).length, 1);
        const handle = select(engine, 3);
        assert.equal(engine.host.exports.pb_web_volume_authorize(handle >> 32n, Number(handle & 0xffffffffn), 2), 0);
        assert.equal(invoke(engine, 'pb_automatic_user_deleted', id(94)), -2);
        data(engine, 'pb_automatic_user_deleted', volume);
        assert.equal(engine.host.exports.pb_web_volume_authorize(handle >> 32n, Number(handle & 0xffffffffn), 2), -2);
        assert.equal(resolve(engine, handle, 'unused'), -2);
        assert.equal(invoke(engine, 'pb_replica_catalog_allowed', volume), -2);
        assert.equal(invoke(engine, 'pb_register_personal', mount), -2, 'stale chooser cannot restore deleted controls');
        assert.equal(invoke(engine, 'pb_select_personal', { session, grants: [{ volume, write: true, incarnation: id(17) }] }), -3);
        assert.equal(resolve(engine, select(engine, 2), 'unused'), -1, 'Shared stays available');
    }
});

test('an account tombstone fences an explicit User mount awaiting registration or catalog capture', async () => {
    for (const registering of [true, false]) {
        const volume = id(93), profile = bootstrap(volume), personal = await createHost(profile);
        const { engine } = await initialize(id(3, 32), app.publisher, id(42), undefined, volume);
        const user = select(engine, 3), address = { role: 'user', volume };
        assert.equal(resolve(engine, user, 'unused'), -6);
        const mount = { role: 'user', volume: profile.root, key: profile.root_key, base: 2, replica_ids: [7, 8] };
        engine.stores.set(3, personal.stores.get(1));
        engine.stores.set(4, personal.stores.get(2));
        if (!registering) data(engine, 'pb_automatic_register', mount);
        data(engine, 'pb_automatic_user_deleted', volume);
        if (registering) assert.equal(invoke(engine, 'pb_automatic_register', mount), -10);
        assert.equal(invoke(engine, 'pb_automatic_ready', address), -2);
        data(engine, 'pb_automatic_failed', { ...address, status: -5 }); // A retired open cannot replace the tombstone.
        assert.equal(resolve(engine, user, 'unused'), -2);
        assert.deepEqual(data(engine, 'pb_automatic_needed'), []);
    }
});
