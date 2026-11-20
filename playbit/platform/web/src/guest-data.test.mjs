import test from "node:test";
import assert from "node:assert/strict";
import { GuestData } from "./guest-data.ts";
import { createWork } from "../tests/work-fixture.mjs";

function fixture(events, notify, revision) {
    const work = createWork(events); work.beginTurn();
    return new GuestData(work, notify, revision);
}

const ok = () => 0;
const parts = handle => [handle >> 32n, Number(handle & 0xffffffffn)];
const ready = () => Promise.resolve({ status: 0, value: "snapshot" });
test("pending capacity and bytes reject admission before backend effects", async () => {
    let effects = 0;
    const data = fixture([], () => {}, () => "revision");
    const slots = [];
    for (let i = 0; i < 64; i++) slots.push(data.begin(4, 1, async () => { effects++; return ready(); }, ok));
    assert.equal(effects, 0);
    assert.throws(() => data.begin(4, 1, ready, ok), error => error.status === -9);
    await data.wait();
    for (const slot of slots) assert.equal(data.take(...parts(slot), 4, () => 0), 0);
    assert.equal(data.pending, 0); assert.equal(effects, 64);
    const maximum = data.begin(4, 16 * 1024 * 1024, ready, ok);
    assert.throws(() => data.begin(4, 1, ready, ok), error => error.status === -9);
    await data.wait(); assert.equal(data.take(...parts(maximum), 4, () => 0), 0);
    assert.throws(() => data.begin(4, 16 * 1024 * 1024 + 1, ready, ok), error => error.status === -9);
});

test("stale reads are fenced while known mutation acknowledgments survive revision changes", async () => {
    let revision = "first";
    const data = fixture([], () => {}, () => revision);
    const read = data.begin(4, 8, ready, ok), write = data.begin(9, 8, ready, ok);
    await new Promise(setImmediate); revision = "next";
    assert.equal(data.take(...parts(read), 4, () => { throw new Error("stale plaintext reached output"); }), -5);
    assert.equal(data.take(...parts(write), 9, value => { assert.equal(value, "snapshot"); return 0; }), 0);
    assert.throws(() => data.take(...parts(write), 9, ok), error => error.status === -8);
    assert.equal(data.failed, false, 'An invalidated read requests retry without poisoning completed work');
});

test('revision changes before completion invalidate successful reads without hiding backend errors', async () => {
    for (const status of [0, -1, -2, -8]) {
        let revision = 'first', finish;
        const data = fixture([], () => {}, () => revision);
        const read = data.begin(4, 8, () => new Promise(resolve => { finish = resolve; }), ok);
        await new Promise(setImmediate);
        revision = 'next';
        finish({ status, value: 'obsolete' });
        await data.wait();
        assert.equal(data.take(...parts(read), 4, () => { throw Error('obsolete read was delivered'); }), status || -5);
        assert.equal(data.pending, 0);
        if (!status) assert.equal(data.failed, false);
        assert.throws(() => data.admit(...parts(read), 4), error => error.status === -8);
    }
});

test('revocation before delivery takes precedence over read invalidation', async () => {
    let revision = 'first', authority = 0;
    const data = fixture([], () => {}, () => revision);
    const read = data.begin(4, 8, ready, () => authority);
    await new Promise(setImmediate);
    revision = 'next'; authority = -2;
    assert.equal(data.take(...parts(read), 4, () => { throw Error('revoked read was delivered'); }), -2);
    assert.equal(data.pending, 0);
});

test('a stale mutation receipt still retains the failed-work close guard', async () => {
    const data = fixture([], () => {}, () => 'current');
    const write = data.begin(9, 8, async () => ({ status: -8 }), ok);
    await new Promise(setImmediate);
    assert.equal(data.take(...parts(write), 9, ok), -8);
    assert.equal(data.failed, true);
});

test("revoked delivery retains accepted I/O credit until completion and never replays", async () => {
    let finish, rights = 0, effects = 0, notifications = 0;
    const events = [], data = fixture(events, () => { notifications++; }, () => "revision");
    const slot = data.begin(4, 16 * 1024 * 1024, async () => {
        effects++; return new Promise(resolve => { finish = resolve; });
    }, () => rights);
    await new Promise(setImmediate); rights = -2;
    assert.equal(data.take(...parts(slot), 4, ok), -2);
    assert.equal(data.pending, 0); assert.equal(data.waiting, true);
    assert.throws(() => data.begin(9, 1, ready, ok), error => error.status === -9);
    assert.throws(() => data.reset(), /accepted work/);
    const done = data.wait(); finish({ status: 0, value: "committed" }); await done;
    assert.equal(data.waiting, false); assert.equal(effects, 1);
    assert.equal(notifications, 0); assert.equal(events.length, 0);
    data.work.endTurn(); const epoch = data.work.epoch; data.reset(); assert.equal(data.work.epoch, epoch + 1n); data.work.beginTurn(); const next = data.begin(9, 1, ready, ok); await data.wait(); assert.equal(data.take(...parts(next), 9, ok), 0);
});

test("mutation execution is ordered and disposal suppresses obsolete readiness", async () => {
    const effects = [], events = [], data = fixture(events, () => {}, () => "revision");
    let finish;
    data.begin(9, 4, () => { effects.push("first"); return new Promise(resolve => { finish = resolve; }); }, ok);
    data.begin(9, 4, async () => { effects.push("second"); return { status: 0, value: "done" }; }, ok);
    await new Promise(setImmediate); assert.deepEqual(effects, ["first"]);
    data.dispose(); finish({ status: 0, value: "done" });
    await new Promise(setImmediate);
    assert.deepEqual(effects, ["first", "second"]); assert.equal(events.length, 0);
    assert.equal(data.pending, 0); assert.equal(data.waiting, false);
});

test("pending and known mutation receipts survive grant loss while execution still checks authority", async () => {
    let rights = 0, finish, effects = 0;
    const data = fixture([], () => {}, () => "revision");
    const slot = data.begin(9, 16, async () => {
        effects++; return new Promise(resolve => { finish = resolve; });
    }, () => rights);
    await new Promise(setImmediate); rights = -2;
    assert.equal(data.take(...parts(slot), 9, ok), -6);
    finish({ status: 0, value: "committed once" }); await data.wait();
    assert.equal(data.take(...parts(slot), 9, value => { assert.equal(value, "committed once"); return 0; }), 0);
    assert.equal(effects, 1); assert.equal(data.failed, false);
    rights = 0;
    const denied = data.begin(9, 16, async () => { effects++; return ready(); }, () => rights);
    rights = -2; await data.wait();
    assert.equal(data.take(...parts(denied), 9, ok), -2); assert.equal(effects, 1);
});

test('individual disposal retains pending quota and ordering, removes ready events and preserves failed mutations', async () => {
    const events = [], data = fixture(events, () => {}, () => 'revision');
    let finish, effects = 0;
    const slot = data.begin(9, 16 * 1024 * 1024, () => {
        effects++; return new Promise(resolve => { finish = resolve; });
    }, ok);
    await new Promise(setImmediate);
    assert.throws(() => data.disposeOperation(2n, Number(slot & 0xffffffffn)), error => error.status === -8);
    assert.equal(data.disposeOperation(...parts(slot)), 0);
    assert.throws(() => data.disposeOperation(...parts(slot)), error => error.status === -8);
    assert.throws(() => data.take(...parts(slot), 9, ok), error => error.status === -8);
    assert.throws(() => data.begin(9, 1, ready, ok), error => error.status === -9);
    assert.throws(() => data.reset(), /accepted work/);
    const done = data.wait(); finish({ status: -2 }); await done;
    assert.equal(effects, 1); assert.equal(events.length, 0); assert.equal(data.failed, true);
    const completed = data.begin(4, 1, ready, ok);
    await data.wait(); assert.equal(events.length, 1);
    assert.equal(data.disposeOperation(...parts(completed)), 0);
    assert.equal(events.length, 0); assert.equal(data.pending, 0);
});

test('owned File admission rolls back work, pins accepted sources, and releases unclaimed results exactly once', async () => {
    const data = fixture([], () => {}, () => 'revision');
    let accepted = 0, released = 0, effects = 0, finish;
    const lifetime = {
        retainedRevision: true,
        accept() { accepted++; },
        delivery: ok,
        release(value, claimed) { released++; assert.equal(value, 'resource'); assert.equal(claimed, false); },
    };
    assert.throws(() => data.begin(4, 16 * 1024 * 1024, ready, ok, {
        ...lifetime, accept() { throw new Error('No File credit'); },
    }), /No File credit/);
    assert.equal(data.pending, 0); assert.equal(data.failed, false);
    const slot = data.begin(4, 16 * 1024 * 1024, () => {
        effects++; return new Promise(resolve => { finish = resolve; });
    }, ok, lifetime);
    assert.equal(accepted, 1); assert.equal(effects, 0);
    assert.throws(() => data.begin(4, 1, ready, ok, lifetime), error => error.status === -9);
    assert.equal(accepted, 1, 'Work admission precedes resource ownership changes');
    await new Promise(setImmediate);
    data.disposeOperation(...parts(slot));
    assert.equal(released, 0, 'Disposal keeps the accepted source and result reservation pinned');
    finish({ status: 0, value: 'resource' }); await data.wait();
    assert.equal(released, 1); assert.equal(effects, 1);
    data.dispose(); assert.equal(released, 1);
});

test('retained File revisions survive unrelated changes but never revocation or failed output admission', async () => {
    let revision = 'before', authority = 0;
    const data = fixture([], () => {}, () => revision), releases = [];
    const lifetime = {
        retainedRevision: true, accept() {}, delivery: () => authority,
        release(value, claimed) { releases.push({ value, claimed }); },
    };
    const slot = data.begin(4, 100, ready, ok, lifetime);
    await data.wait(); revision = 'after';
    assert.equal(data.take(...parts(slot), 4, () => -11), -11);
    assert.deepEqual(releases, []);
    assert.equal(data.take(...parts(slot), 4, () => 0), 0);
    assert.deepEqual(releases, [{ value: 'snapshot', claimed: true }]);
    const denied = data.begin(9, 100, ready, ok, lifetime);
    await data.wait(); authority = -2;
    assert.equal(data.take(...parts(denied), 9, () => { throw new Error('Revoked capability delivered'); }), -2);
    assert.deepEqual(releases[1], { value: 'snapshot', claimed: false });
});

test('retained delivery requalifies once and rearms readiness without replay or duplicate events', async () => {
    // A stale authorization fence suspends Take, preserves its operation, and wakes it after fresh qualification.
    for (const consumeEvent of [false, true]) {
        const events = [], data = fixture(events, () => {}, () => 'revision');
        let authority = -6, qualifications = 0, effects = 0, finish, released = 0;
        const lifetime = { retainedRevision: true, accept() {}, delivery: () => authority,
            release() { released++; }, requalify() { qualifications++; return new Promise(resolve => { finish = resolve; }); } };
        const slot = data.begin(4, 100, async () => { effects++; return ready(); }, ok, lifetime);
        assert.equal(data.take(...parts(slot), 4, ok), -6); assert.equal(qualifications, 0);
        await data.wait(); assert.equal(events.length, 1);
        if (consumeEvent) data.work.poll();
        assert.equal(data.take(...parts(slot), 4, ok), -6);
        assert.equal(data.take(...parts(slot), 4, ok), -6);
        await new Promise(setImmediate); assert.equal(qualifications, 1);
        assert.equal(released, 0); assert.equal(data.waiting, true);
        const done = data.wait(); authority = 0; finish(); await done;
        assert.equal(events.length, 1); assert.equal(events[0].operation, slot);
        assert.equal(data.take(...parts(slot), 4, () => -11), -11);
        assert.equal(data.take(...parts(slot), 4, value => { assert.equal(value, 'snapshot'); return 0; }), 0);
        assert.equal(effects, 1); assert.equal(released, 1); assert.equal(events.length, 0);
    }
});

test('disposal during requalification retains ownership and quota until the authority work retires', async () => {
    // Rust and JavaScript must discard readiness together while accepted qualification retains its source and credit.
    for (const disposal of ['operation', 'owner']) {
        const events = [], data = fixture(events, () => {}, () => 'revision');
        let finish, released = 0;
        const slot = data.begin(4, 16 * 1024 * 1024, ready, ok, {
            retainedRevision: true, accept() {}, delivery: () => -6,
            release(value, claimed) { released++; assert.equal(value, 'snapshot'); assert.equal(claimed, false); },
            requalify() { return new Promise(resolve => { finish = resolve; }); },
        });
        await data.wait(); assert.equal(events.length, 1);
        assert.equal(data.take(...parts(slot), 4, ok), -6);
        await new Promise(setImmediate);
        if (disposal === 'operation') data.disposeOperation(...parts(slot));
        else data.dispose();
        assert.equal(events.length, 0, 'Abandon removes the queued readiness payload before qualification completes');
        assert.equal(released, 0); assert.equal(data.waiting, true);
        assert.throws(() => data.work.check(slot), error => error.status === -8);
        assert.throws(() => data.work.begin(4, 1), error => error.status === -9);
        data.work.queue({ kind: 2, target: 7, data: Uint8Array.of(1) });
        assert.equal(data.work.poll().target, 7, 'Unrelated input can be polled while abandoned qualification is pending');
        assert.equal(data.work.poll(), undefined);
        data.work.endTurn(); assert.equal(data.work.pending, true);
        const done = data.wait(); finish(); await done;
        assert.equal(released, 1); assert.equal(data.pending, 0); assert.equal(data.waiting, false);
        assert.equal(events.length, 0); assert.equal(data.work.pending, false); assert.equal(data.failed, false);
    }
});

test('transient qualification failure can retry and deliver its original result', async () => {
    // A failed dependency attempt must not permanently cache WouldBlock over later success.
    const { AbiError } = await import('./guest-memory.ts');
    const data = fixture([], () => {}, () => 'revision'); let authority = -6, attempts = 0;
    const slot = data.begin(4, 100, ready, ok, { retainedRevision: true, accept() {}, release() {}, delivery: () => authority,
        async requalify() { if (++attempts === 1) throw new AbiError(-6); authority = 0; } });
    await data.wait(); assert.equal(data.take(...parts(slot), 4, ok), -6); await data.wait();
    assert.equal(data.take(...parts(slot), 4, ok), -6); await data.wait();
    assert.equal(data.take(...parts(slot), 4, value => { assert.equal(value, 'snapshot'); return 0; }), 0);
    assert.equal(attempts, 2);
});


test('a log wait orders appends while cached reads and unrelated edits continue', async () => {
    const data = fixture([], () => {}, () => 'same');
    const ran = []; let release;
    const first = data.begin(13, 1, async () => { ran.push('first'); await new Promise(resolve => { release = resolve; }); return ready(); }, ok, undefined, false, 'log');
    const next = data.begin(13, 1, async () => { ran.push('next'); return ready(); }, ok, undefined, false, 'log');
    const read = data.begin(14, 1, async () => { ran.push('read'); return ready(); }, ok, undefined, true);
    const edit = data.begin(9, 1, async () => { ran.push('edit'); return ready(); }, ok);
    await new Promise(setImmediate);
    assert.deepEqual(ran, ['first', 'read', 'edit']);
    assert.equal(data.take(...parts(read), 14, ok), 0);
    assert.equal(data.take(...parts(edit), 9, ok), 0);
    release(); while (data.waiting) await data.wait();
    assert.deepEqual(ran, ['first', 'read', 'edit', 'next']);
    assert.equal(data.take(...parts(first), 13, ok), 0);
    assert.equal(data.take(...parts(next), 13, ok), 0);
});
