import { encodeId } from './identifier.ts';
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { resolvedSession, prepareGuest } from "./updates.ts";
const first = encodeId(new Uint8Array(32).fill(0x11)), second = encodeId(new Uint8Array(32).fill(0x22));
const session = { id: encodeId(new Uint8Array(32).fill(0x33)), selector: { kind: "app", app: {publisher: encodeId(new Uint8Array(16).fill(0x7a)), app_id: "todo"} }, resolved_build_id: first, root_volume: encodeId(new Uint8Array(16).fill(0x55))};
test("AP resolution adopts only the session's named release and preserves identity", () => {
    const resolution = { revision: 2, selector: session.selector, resolved_build_id: second };
    const next = resolvedSession(session, resolution, { revision: 1, resolved_build_id: first });
    assert.deepEqual(next, { ...session, resolved_build_id: second });
    assert.equal(session.resolved_build_id, first);
    assert.throws(() => resolvedSession(session, { ...resolution, selector: { kind: "app", app: {publisher: encodeId(new Uint8Array(16).fill(0x7b)), app_id: "todo"} } }), /Invalid/);
    assert.throws(() => resolvedSession(session, { ...resolution, revision: 1 }, { revision: 1, resolved_build_id: first }), /backwards/);
    assert.throws(() => resolvedSession(session, { ...resolution, revision: 0 }, resolution), /backwards/);
});
test("exact selection stays pinned unless AP returns an authorized exact advancement", () => {
    const exact = { ...session, selector: { kind: "exact", build_id: first } };
    assert.deepEqual(resolvedSession(exact, { revision: 0, selector: exact.selector, resolved_build_id: first }), exact);
    assert.throws(() => resolvedSession(exact, { revision: 1, selector: session.selector, resolved_build_id: second }), /Invalid/);
    assert.throws(() => resolvedSession(exact, { revision: 1, selector: exact.selector, resolved_build_id: second }), /Invalid/);
    const next = resolvedSession(exact, { revision: 1, selector: { kind: "exact", build_id: second }, resolved_build_id: second });
    assert.equal(next.id, exact.id); assert.equal(next.resolved_build_id, second);
});
test("candidate preparation runs no guest entry point and rejects corrupt bytes before replacement", async () => {
    const bytes = new Uint8Array(await readFile(new URL("../../../../target/wasm32-unknown-unknown/release/todo.wasm", import.meta.url)));
    let calls = 0;
    const current = await prepareGuest(bytes, { pb_log: () => { calls++; return -4; } });
    assert.equal(calls, 0);
    const candidate = await prepareGuest(bytes, { pb_log: () => { calls++; return -4; } });
    assert.notEqual(current, candidate); assert.equal(calls, 0);
    await assert.rejects(prepareGuest(new Uint8Array([0, 1, 2]), {}));
    assert.ok(current.exports.memory instanceof WebAssembly.Memory);
    assert.equal(calls, 0);
});

const text = value => [value.length, ...new TextEncoder().encode(value)];
const section = (id, body) => [id, body.length, ...body];
function fileGuest({ name = "pb_syscall", parameters = [0x7f, ...Array(6).fill(0x7e)], mainResult = [], run = true, namespace = "playbit" } = {}) {
    const body = [0, ...(run ? [0x41, 0xa4, 0x02, 0x42, 7, ...Array(5).fill([0x42, 0]).flat(), 0x10, 0, 0x1a] : []), ...(mainResult.length ? [0x41, 0] : []), 0x0b];
    return new Uint8Array([0,97,115,109,1,0,0,0,
        ...section(1,[2,0x60,parameters.length,...parameters,1,0x7e,0x60,0,mainResult.length,...mainResult]),
        ...section(2,[1,...text(namespace),...text(name),0,0]),
        ...section(3,[1,1]),...section(5,[1,0,1]),
        ...section(7,[2,...text("memory"),2,0,...text("pb_main"),0,1]),
        ...section(10,[1,body.length,...body])]);
}

test("only a required pixel capability selects the larger prepared guest memory", async () => {
    const original = fileGuest();
    const plain = await prepareGuest(original, {}, ['ui.canvas.window.v1']);
    const pixels = await prepareGuest(original, {}, ['ui.canvas.pixels.v1']);
    assert.throws(() => plain.exports.memory.grow(256), RangeError);
    assert.equal(pixels.exports.memory.grow(256), 1);
    assert.throws(() => pixels.exports.memory.grow(256), RangeError);
});

test("file begin uses the bounded scalar binding without suspension", async () => {
    let calls = 0;
    const guest = await prepareGuest(fileGuest(), { pb_file_read_begin: (...args) => {
        calls++; assert.deepEqual(args, [0n, 7, 0n, 0]); return -7;
    } });
    assert.equal(calls, 0);
    guest.exports.pb_main(); assert.equal(calls, 1);
});

test("invalid signatures, namespaces and unknown imports fail before app execution", async () => {
    let calls = 0;
    const operations = { pb_file_read_begin: () => { calls++; return 0; } };
    await assert.rejects(prepareGuest(fileGuest({ parameters: [0x7f], run: false }), operations), WebAssembly.LinkError);
    await assert.rejects(prepareGuest(fileGuest({ namespace: "wasi", run: false }), operations), /only pb_syscall/);
    await assert.rejects(prepareGuest(fileGuest({ name: "pb_unrecognized", run: false }), operations), /only pb_syscall/);
    await assert.rejects(prepareGuest(fileGuest({ mainResult: [0x7f], run: false }), operations), WebAssembly.LinkError);
    assert.equal(calls, 0);
});

test("a known unavailable operation returns Unsupported without preventing instantiation", async () => {
    const guest = await prepareGuest(fileGuest(), {});
    guest.exports.pb_main();
});
