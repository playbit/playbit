import assert from "node:assert/strict";
import test from "node:test";
import { createWork } from "../tests/work-fixture.mjs";
import { ABI as L } from "./guest-layout.ts";
import { operationSupported } from "./guest-imports.ts";
import { guestOperations } from "./guest-operations.ts";

test("wake.after replaces one monotonic alarm and retires its queued credit", ({ mock }) => {
    const timers = new Map();
    let time = 0, serial = 0, ready = 0;
    mock.method(performance, "now", () => time);
    mock.method(globalThis, "setTimeout", (callback, delay) => {
        const id = ++serial;
        timers.set(id, { callback, delay });
        return id;
    });
    mock.method(globalThis, "clearTimeout", id => timers.delete(id));
    const fire = at => {
        time = at;
        const [[id, { callback }]] = timers;
        timers.delete(id);
        callback();
    };
    const events = [], work = createWork(events), memory = new WebAssembly.Memory({ initial: 1 });
    const operations = guestOperations({ memory: () => memory, host: () => null, events, work,
        log() {}, submit() {}, ready() { ready++; } });
    assert.equal(operationSupported(operations, 0xb), true);
    assert.equal(operations.pb_wake_after(1000), 0);
    assert.equal(timers.size, 1);
    const stale = timers.values().next().value.callback;
    time = 250;
    assert.equal(operations.pb_wake_after(86_400_001), -3);
    assert.equal(timers.values().next().value.callback, stale);
    assert.equal(operations.pb_wake_after(2000), 0);
    assert.equal(timers.size, 1);
    stale();
    assert.equal(ready, 0);
    fire(2249.5);
    assert.equal(timers.values().next().value.delay, 1);
    assert.equal(ready, 0);
    fire(2250);
    assert.equal(ready, 1);

    operations.queueWake(); operations.queueWake();
    assert.deepEqual(events.map(event => event.kind), [10]);
    assert.equal(operations.pb_wake_after(999), -3);
    assert.equal(events.length, 1);
    assert.equal(operations.pb_wake_after(1000), 0);
    assert.equal(events.length, 0);
    assert.equal(work.pending, false);

    fire(3250);
    const header = new DataView(memory.buffer, 8, L.event.size);
    header.setUint32(0, L.event.size, true);
    work.beginTurn();
    assert.equal(operations.pb_event_poll(8, 0, 0), 0);
    assert.equal(header.getUint32(16, true), 10);
    assert.equal(header.getUint32(20, true), 0);
    assert.equal(operations.pb_event_poll(8, 0, 0), -6);
    work.endTurn();
    assert.equal(work.pending, false);

    assert.equal(operations.pb_wake_after(1000), 0);
    const closing = timers.values().next().value.callback;
    operations.cancelWake(); work.deactivate();
    assert.equal(timers.size, 0);
    assert.equal(operations.pb_wake_after(1000), -10);
    time += 2000;
    closing(); operations.queueWake();
    assert.equal(ready, 2);
    assert.equal(events.length, 0);
});
