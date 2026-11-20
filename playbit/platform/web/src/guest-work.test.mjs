import test from "node:test";
import assert from "node:assert/strict";
import { createWork } from "../tests/work-fixture.mjs";
import { GuestData } from "./guest-data.ts";

const ok = () => 0;
const parts = handle => [handle >> 32n, Number(handle & 0xffffffffn)];
test("accepted input reservations bound payloads and survive deactivation until queued", () => {
    const events = [], work = createWork(events), roots = [];
    for (let i = 0; i < 256; i++) roots.push(work.input(0));
    assert.throws(() => work.input(0), error => error.status === -9);
    work.deactivate();
    assert.throws(() => work.input(0), error => error.status === -10);
    assert.throws(() => work.reset(), error => error.status === -10);
    for (const root of roots) work.queueAccepted(root, { kind: 2, target: 1, data: new Uint8Array() });
    work.beginTurn(); while (work.poll()) {} work.endTurn();
    assert.equal(work.pending, false); assert.equal(work.failed, false);
    work.reset();
    for (let i = 0; i < 15; i++) work.input(65536);
    assert.throws(() => work.input(65536), error => error.status === -9);
    assert.throws(() => work.input(65537), error => error.status === -9);
});

test("a full event queue defers readiness without repeating accepted I/O", async () => {
    const events = [], work = createWork(events), data = new GuestData(work, () => {}, () => "revision");
    let executions = 0;
    work.beginTurn();
    const slot = data.begin(4, 32, async () => { executions++; return { status: 0, value: "read" }; }, ok);
    for (let i = 0; i < 256; i++) work.queue({ kind: 2, target: 1, data: new Uint8Array() });
    await data.wait(); assert.equal(events.length, 256); assert.equal(executions, 1);
    work.poll(); data.flushReady(); data.flushReady();
    assert.equal(events.length, 256); assert.equal(events.filter(event => event.operation === slot).length, 1);
    while (work.poll()) {}
    assert.equal(data.take(...parts(slot), 4, () => 0), 0);
    work.endTurn(); assert.equal(work.pending, false);
});

test("completion take restores ancestry and dependent drain steps end at the shared Rust limit", async () => {
    const work = createWork(), data = new GuestData(work, () => {}, () => "revision");
    let effects = 0, denied = 0;
    const execute = async () => { effects++; return { status: 0, value: "receipt" }; };
    work.beginTurn(); let slot = data.begin(9, 8, execute, ok); work.endTurn();
    work.deactivate();
    // A full 64-MiB read and readback at 64-KiB guest chunks, plus metadata.
    const steps = 2 * 1024 + 128;
    for (let i = 0; i <= steps; i++) {
        await data.wait(); work.beginTurn();
        // Direct Take must also restore the root; polling a different accepted
        // event or losing a ready notification cannot create a new drain budget.
        let next;
        assert.equal(data.take(...parts(slot), 9, () => {
            try { next = data.begin(9, 8, execute, ok); }
            catch (error) { assert.equal(error.status, -9); denied++; }
            return 0;
        }), 0);
        work.endTurn(); slot = next;
    }
    assert.equal(effects, steps + 1); assert.equal(denied, 1);
    assert.equal(work.pending, false); assert.equal(work.failed, true);
    const epoch = work.epoch; data.reset(); assert.equal(work.epoch, epoch + 1n); work.beginTurn();
    const next = data.begin(9, 8, execute, ok); assert(next > 0n);
    await data.wait(); assert.equal(data.take(...parts(next), 9, () => 0), 0); work.endTurn();
});

test("deactivation suppresses queued frame/wake work and refuses an unconsumed event", () => {
    const work = createWork();
    for (const kind of [10, 13, 2]) work.queue({ kind, target: 0, data: new Uint8Array() });
    work.deactivate(); assert.deepEqual(work.events.map(event => event.kind), [2]);
    work.beginTurn(); work.endTurn();
    assert.equal(work.failed, true); assert.equal(work.pending, true);
});

test("Rust operation ownership rejects early cleanup and preserves abandoned work until completion", () => {
    const work = createWork();
    work.beginTurn();
    const handle = work.begin(9, 16);
    assert.throws(() => work.release(handle, -2, false), error => error.status === -12);
    work.check(handle, 9);
    work.abandon(handle);
    assert.throws(() => work.check(handle, 9), error => error.status === -8);
    assert.throws(() => work.release(handle, 0, false), error => error.status === -12);
    work.complete(handle, -2);
    assert.throws(() => work.complete(handle, -2), error => error.status === -10);
    work.release(handle, -2, false);
    work.endTurn();
    assert.equal(work.pending, false);
    assert.equal(work.failed, true);
});
