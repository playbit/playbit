import { encodeId, decodeId } from './identifier.ts';
import test from "node:test";
import assert from "node:assert/strict";
import { bootstrap, createHost, entryId, account, key } from "../tests/host-fixture.mjs";
import { GuestWork } from "./guest-work.ts";
import { guestOperations } from "./guest-operations.ts";

const kvId = encodeId(new Uint8Array(15).fill(0x71));
function profile(root = encodeId(new Uint8Array(16).fill(0x10)), session = encodeId(new Uint8Array(16).fill(0x20))) {
    const value = bootstrap(root);
    value.local.databases.push({ id: kvId, name: "settings", kind: { type: "kv_db" } });
    return value;
}
function adapter(database, slot = 1) {
    const memory = new WebAssembly.Memory({ initial: 4 });
    const events = [], work = new GuestWork(events, () => database.host.exports); work.beginTurn();
    const ops = guestOperations({ memory: () => memory, events, work, host: () => database.host.exports,
        log() {}, submit() {} });
    ops.refreshVolumes();
    let view = new DataView(memory.buffer);
    view.setUint32(8, 32, true); view.setUint32(16, slot, true); view.setUint32(20, 1, true);
    new Uint8Array(memory.buffer, 24, 15).set(decodeId(kvId, 15));
    let latest;
    for (const name of Object.keys(ops).filter(name => name.endsWith("_begin"))) {
        const begin = ops[name]; ops[name] = (...args) => { latest = begin(...args); return latest; };
    }
    const handle = () => [latest >> 32n, Number(latest & 0xffffffffn)];
    const settle = async () => { while (ops.data.waiting) await ops.data.wait(); };
    let outputCapacity = 0;
    const buffer = capacity => { outputCapacity = capacity; new DataView(memory.buffer).setUint32(520, 0, true); };
    const takeArgs = () => [1024, outputCapacity, 520];
    const output = () => new Uint8Array(memory.buffer, 1024, new DataView(memory.buffer).getUint32(520, true)).slice();
    const put = async (bytes, value) => {
        new Uint8Array(memory.buffer, 128, bytes.length).set(bytes);
        new Uint8Array(memory.buffer, 192, value.length).set(value);
        assert.equal(ops.pb_kv_put_begin(8, 128, bytes.length, 192, value.length) > 0n, true);
        await settle(); return ops.pb_kv_put_take(...handle());
    };
    const keys = async () => {
        new Uint8Array(memory.buffer, 48, 56).fill(0);
        new DataView(memory.buffer).setUint32(48, 56, true); new DataView(memory.buffer).setUint32(96, 64, true);
        assert.equal(ops.pb_kv_keys_begin(8, 48, 0, 0) > 0n, true);
        await settle(); buffer(33895);
        assert.equal(ops.pb_kv_keys_take(...handle(), ...takeArgs()), 0);
        return output();
    };
    const read = (bytes, root) => {
        new Uint8Array(memory.buffer, 128, bytes.length).set(bytes);
        new Uint8Array(memory.buffer, 48, 64).fill(0);
        const view = new DataView(memory.buffer); view.setUint32(48, 64, true); view.setUint32(64, 4096, true);
        if (root) {
            view.setUint32(68, 1, true); view.setUint32(72, root[1], true);
            new Uint8Array(memory.buffer, 76, 32).set(root.subarray(2, 34));
        }
        return ops.pb_kv_read_begin(8, 128, bytes.length, 48);
    };
    return { ops, memory, handle, settle, buffer, takeArgs, output, put, keys, read };
}

test("compiled web KV owns binary input, retains short snapshots and persists independently", async () => {
    const initial = profile(), database = await createHost(initial), a = adapter(database);
    const bytes = Uint8Array.of(0, 255, 1), value = Uint8Array.of(0, 128, 255);
    new Uint8Array(a.memory.buffer, 128, 3).set(bytes);
    new Uint8Array(a.memory.buffer, 192, 3).set(value);
    assert.equal(a.ops.pb_kv_put_begin(0, 128, 3, 192, 3), -3);
    assert.equal(a.ops.pb_kv_put_begin(8, 128, 3, 192, 3) > 0n, true);
    const pending = a.handle();
    new Uint8Array(a.memory.buffer, 128, 3).fill(9);
    new Uint8Array(a.memory.buffer, 192, 3).fill(9);
    await a.settle();
    assert.equal(a.ops.pb_kv_delete_take(...pending), -8);
    assert.equal(a.ops.pb_kv_put_take(...pending), 0);
    const page = await a.keys();
    assert.equal(new DataView(page.buffer).getUint32(34, true), 1);
    assert.equal(new DataView(page.buffer).getBigUint64(42, true), 3n);
    assert.deepEqual([...page.subarray(50)], [...bytes]);
    assert(a.read(bytes, page) > 0n); await a.settle();
    const handle = a.handle(); a.buffer(4);
    new Uint8Array(a.memory.buffer, 1024, 64).fill(0xa5);
    assert.equal(a.ops.pb_kv_read_take(...handle, ...a.takeArgs()), -11);
    assert.equal(new DataView(a.memory.buffer).getUint32(520, true), 52);
    assert(new Uint8Array(a.memory.buffer, 1024, 64).every(byte => byte === 0xa5));
    a.memory.grow(1); a.buffer(64);
    assert.equal(a.ops.pb_kv_read_take(...handle, ...a.takeArgs()), 0);
    assert.deepEqual([...a.output().subarray(49)], [...value]);
    assert.equal(a.ops.pb_kv_read_take(...handle, ...a.takeArgs()), -8);
    const reopened = await createHost({ ...initial, local: null }, database.stores);
    reopened.register(kvId, initial.root.id, 66);
    const b = adapter(reopened);
    assert(b.read(bytes) > 0n); await b.settle(); b.buffer(64);
    assert.equal(b.ops.pb_kv_read_take(...b.handle(), ...b.takeArgs()), 0);
    assert.deepEqual([...b.output().subarray(49)], [...value]);
    assert.equal(b.ops.pb_kv_delete_begin(8, 128, 3) > 0n, true); await b.settle();
    assert.equal(b.ops.pb_kv_delete_take(...b.handle()), 0);
    assert.equal(new DataView((await b.keys()).buffer).getUint32(34, true), 0);
});

test("compiled web KV validates roots and refuses chunks across another write", async () => {
    const database = await createHost(profile()), a = adapter(database);
    assert.equal(await a.put(Uint8Array.of(1), Uint8Array.of(2)), 0);
    const root = await a.keys();
    assert.equal(await a.put(Uint8Array.of(1), Uint8Array.of(3)), 0);
    assert(a.read(Uint8Array.of(1), root) > 0n); await a.settle(); a.buffer(64);
    assert.equal(a.ops.pb_kv_read_take(...a.handle(), ...a.takeArgs()), -10);
    assert.equal(a.ops.pb_kv_read_begin(8, 128, 0, 48), -3);
    new DataView(a.memory.buffer).setUint32(72, 256, true);
    assert.equal(a.ops.pb_kv_read_begin(8, 128, 1, 48), -3);
    new Uint8Array(a.memory.buffer, 24, 15).set(Buffer.from(decodeId(entryId, 15)));
    assert(a.read(Uint8Array.of(1)) > 0n); await a.settle(); a.buffer(64);
    assert.equal(a.ops.pb_kv_read_take(...a.handle(), ...a.takeArgs()), -3);
});

test("current personal withdrawal blocks a retained compiled KV result without disclosure", async () => {
    const personalId = encodeId(new Uint8Array(16).fill(0x30)), personalProfile = profile(personalId, encodeId(new Uint8Array(16).fill(0x40)));
    personalProfile.root.personal = true;
    const personal = await createHost(personalProfile), writer = adapter(personal);
    assert.equal(await writer.put(Uint8Array.of(1), Uint8Array.of(2)), 0);
    const mainProfile = bootstrap(encodeId(new Uint8Array(16).fill(0x10))), main = await createHost(mainProfile);
    main.stores.set(3, personal.stores.get(1)); main.stores.set(4, personal.stores.get(2));
    main.call("pb_register_personal", { volume: { id: personalId, owner: account, name: "Personal", share_key_with_service: false, personal: true }, key, replica_ids: [7, 8] });
    main.call("pb_select_personal", { session: mainProfile.session.id, grants: [{ volume: personalId, write: false, incarnation: encodeId(new Uint8Array(16).fill(0x01)) }] });
    main.stores.set(67, personal.stores.get(66)); main.register(kvId, personalId, 67);
    const a = adapter(main, 2);
    assert(a.read(Uint8Array.of(1)) > 0n); await a.settle(); a.buffer(64);
    const pending = a.handle();
    main.call("pb_select_personal", { session: mainProfile.session.id, grants: [] });
    a.ops.refreshVolumes();
    new Uint8Array(a.memory.buffer, 1024, 64).fill(0xa5);
    assert.equal(a.ops.pb_kv_read_take(...pending, ...a.takeArgs()), -8);
    assert(new Uint8Array(a.memory.buffer, 1024, 64).every(byte => byte === 0xa5));
});
