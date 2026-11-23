import assert from 'node:assert/strict';
import test from 'node:test';
import { clockNs, INDEFINITE } from './guest-deadline.ts';
import { supportsDeadline } from './guest-imports.ts';
import { guestOperations } from './guest-operations.ts';
import { prepareGuest } from './updates.ts';

// A real Wasm guest counts pb_main entries, then stores its syscall result.
// The deadline is read from memory so one executable covers every wait mode.
const name = s => [s.length, ...new TextEncoder().encode(s)];
const section = (id, data) => [id, data.length, ...data];
const code = [0, 0x41,0, 0x41,0, 0x28,2,0, 0x41,1, 0x6a, 0x36,2,0,
    0x41,8, 0x41,21, 0x42,32, 0x42,0xc0,0, 0x42,8,
    0x41,0xd0,0, 0x29,3,0, 0x42,0, 0x42,0, 0x10,0, 0x37,3,0, 0x0b];
const wasm = new Uint8Array([0,97,115,109,1,0,0,0,
    ...section(1,[2,0x60,7,0x7f,...Array(6).fill(0x7e),1,0x7e,0x60,0,0]),
    ...section(2,[1,...name('playbit'),...name('pb_syscall'),0,0]),
    ...section(3,[1,1]), ...section(5,[1,0,1]),
    ...section(7,[2,...name('memory'),2,0,...name('pb_main'),0,1]),
    ...section(10,[1,code.length,...code])]);

async function fixture(enabled) {
    let guest;
    const events = [], ready = Promise.withResolvers();
    const work = { epoch: 1n, draining: false, poll: () => events.shift() };
    const operations = guestOperations({
        memory: () => guest.exports.memory, events, work,
        waitForEvent: deadline => guest.deadline.suspend(deadline, ready.resolve),
    });
    guest = await prepareGuest(wasm, operations, enabled);
    const memory = new DataView(guest.exports.memory.buffer);
    memory.setUint32(32,24,true);
    memory.setBigInt64(8,99n,true);
    return {guest, memory, events, work, ready: ready.promise};
}

test('an undeclared guest gets Unsupported through the synchronous scalar entry', async () => {
    const {guest,memory} = await fixture(false);
    guest.exports.pb_main();
    assert.equal(guest.deadline, undefined);
    assert.equal(memory.getBigInt64(8,true), -7n);
    await assert.rejects(prepareGuest(wasm,{},true), /event.poll.deadline/);
    if (!supportsDeadline()) await assert.rejects(fixture(true), /event.poll.deadline/);
});

test('an indefinite poll resumes the original stack on an admitted event', {skip:!supportsDeadline()}, async () => {
    const {guest,memory,events,work} = await fixture(true);
    memory.setBigUint64(80,INDEFINITE,true);
    await guest.deadline.enter();
    assert.equal(guest.deadline.parked,true);
    assert.equal(guest.deadline.canEnter(false),false);
    assert.equal(memory.getBigInt64(8,true),99n);
    events.push({kind:12,target:4,data:Uint8Array.of(7,8)});
    assert.equal(guest.deadline.canEnter(true),true);
    await guest.deadline.enter();
    assert.equal(memory.getUint32(0,true),1);
    assert.equal(memory.getBigInt64(8,true),2n);
    assert.equal(memory.getUint32(48,true),12);
    assert.equal(memory.getUint32(52,true),4);
    assert.deepEqual([...new Uint8Array(memory.buffer,64,2)],[7,8]);
    assert.equal(events.length,0);
    memory.setBigUint64(80,0n,true);
    events.push({kind:2,target:3,data:new Uint8Array()});
    await guest.deadline.enter();
    assert.equal(memory.getBigInt64(8,true),0n,'queued input wins even with an expired deadline');
    assert.equal(memory.getUint32(36,true),24);
    await guest.deadline.enter();
    assert.equal(memory.getBigInt64(8,true),-6n);
    assert.equal(memory.getUint32(36,true),0,'an empty expired poll invalidates the previous event');
    work.draining = true;
    await guest.deadline.enter();
    assert.equal(memory.getBigInt64(8,true),-10n,'draining takes precedence over expiry on an empty queue');
    memory.setBigUint64(80,INDEFINITE,true);
    await guest.deadline.enter();
    assert.equal(memory.getBigInt64(8,true),-10n);
    assert.equal(guest.deadline.parked,false);
});

test('invalid output and small payload probes return before suspension and preserve the event', {skip:!supportsDeadline()}, async () => {
    const {guest,memory,events} = await fixture(true);
    memory.setBigUint64(80,INDEFINITE,true);
    memory.setUint32(32,1,true);
    await guest.deadline.enter();
    assert.equal(memory.getBigInt64(8,true),-11n);
    assert.equal(guest.deadline.parked,false);
    memory.setUint32(32,24,true);
    events.push({kind:2,target:7,data:new Uint8Array()});
    await guest.deadline.enter();
    assert.equal(memory.getUint32(36,true),24);
    events.push({kind:12,target:1,data:new Uint8Array(9)});
    memory.setUint32(32,40,true); // The last eight bytes now overlap the payload.
    await guest.deadline.enter();
    assert.equal(memory.getBigInt64(8,true),-3n);
    assert.equal(memory.getUint32(36,true),24,'an invalid span prevents every output write');
    memory.setUint32(32,24,true);
    await guest.deadline.enter();
    assert.equal(memory.getBigInt64(8,true),9n);
    assert.equal(memory.getUint32(36,true),0,'a size probe invalidates the previous event');
    assert.equal(memory.getUint32(32,true),24);
    assert.equal(events.length,1);
});

test('finite timeout releases the owner queue and retirement drops a parked stack', {skip:!supportsDeadline(),timeout:3000}, async () => {
    const {guest,memory,ready} = await fixture(true);
    const deadline = clockNs() + 30_000_000n;
    memory.setBigUint64(80,deadline,true);
    await guest.deadline.enter();
    assert.equal(memory.getBigInt64(8,true),99n);
    await ready;
    assert.ok(clockNs() >= deadline);
    await guest.deadline.enter();
    assert.equal(memory.getUint32(0,true),1);
    assert.equal(memory.getBigInt64(8,true),-6n);
    memory.setBigUint64(80,INDEFINITE,true);
    await guest.deadline.enter();
    guest.deadline.retire();
    assert.equal(guest.deadline.canEnter(true),false);
    assert.throws(() => guest.deadline.enter(), /retired/);
    assert.equal(memory.getUint32(0,true),2);
});

test('deactivation disarms a timer while preserving admitted input', {skip:!supportsDeadline(),timeout:3000}, async () => {
    const {guest,memory,events,work} = await fixture(true);
    memory.setBigUint64(80,clockNs()+15_000_000n,true);
    await guest.deadline.enter();
    work.draining = true;
    guest.deadline.deactivate();
    await new Promise(resolve=>setTimeout(resolve,25));
    assert.equal(guest.deadline.canEnter(false),false);
    events.push({kind:2,target:7,data:new Uint8Array()});
    assert.equal(guest.deadline.canEnter(true),true);
    await guest.deadline.enter();
    assert.equal(memory.getBigInt64(8,true),0n);
    assert.equal(memory.getUint32(0,true),1);
});
