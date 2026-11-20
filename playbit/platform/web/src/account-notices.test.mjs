import { encodeId } from './identifier.ts';
import test from 'node:test';
import assert from 'node:assert/strict';
import { AccountNotices, AccountSignal } from './account-notices.ts';
const account = encodeId(new Uint8Array(16).fill(0xab)), other = encodeId(new Uint8Array(16).fill(0xcd));
const port = () => ({ messages: [], postMessage(value) { this.messages.push(value); } });
const turn = () => new Promise(resolve => setImmediate(resolve));

test('local account notices retain one frame and one dirty bit through 1,000 commits', () => {
    const registry = new AccountNotices(), first = port(), second = port(), foreign = port();
    registry.watch(first, account); registry.watch(second, account); registry.watch(foreign, other);
    registry.acknowledge(second, 1); registry.acknowledge(foreign, 1);
    for (let index = 0; index < 1000; index++) registry.changed(account);
    assert.equal(first.messages.length, 1); assert.equal(second.messages.length, 2);
    assert.equal(foreign.messages.length, 1);
    registry.acknowledge(first, 1);
    assert.equal(first.messages.length, 2);
    assert.deepEqual(first.messages[1], { event: 'account', account, ticket: 2 });
    registry.changed(account); // A commit made during the reader's snapshot.
    registry.acknowledge(first, 2);
    assert.equal(first.messages.length, 3);
    registry.acknowledge(first, 3);
    assert.equal(first.messages.length, 3);
    assert.throws(() => registry.acknowledge(first, 3), /acknowledgment/);
    assert.throws(() => registry.acknowledge(second, 1), /acknowledgment/);
    registry.acknowledge(second, 2);
    registry.acknowledge(second, 3);
    registry.watch(first, account); // Explicit retry also refreshes current state.
    assert.equal(first.messages.length, 4);
});

test('account observer capacity, account binding, failed ports and detach remain bounded', () => {
    const registry = new AccountNotices(), ports = Array.from({ length: 32 }, port);
    for (const value of ports) registry.watch(value, account);
    assert.throws(() => registry.watch(port(), account), /Too many/);
    assert.throws(() => registry.watch(ports[0], other), /another account/);
    assert.throws(() => registry.watch(port(), 'bad'), /identity/);
    registry.detach(ports[0]); registry.changed(account);
    assert.equal(ports[0].messages.length, 1);
    registry.watch({ postMessage() { throw Error('port closed'); } }, account);
    assert.equal(registry.observers.size, 31);
    registry.watch(port(), account); assert.equal(registry.observers.size, 32);
    for (const state of registry.observers.values()) assert.ok(state.busy && typeof state.dirty === 'boolean');
});

test('metadata sender coalesces a held request and preserves a wake after failure', async () => {
    let release, calls = 0;
    const failures = [];
    const sender = new AccountSignal(value => {
        assert.equal(value, account); calls++;
        return new Promise((resolve, reject) => { release = { resolve, reject }; });
    }, error => failures.push(error.message));
    sender.notify(account);
    for (let index = 0; index < 1000; index++) sender.notify(account);
    assert.equal(calls, 1);
    sender.notify(other); assert.match(failures[0], /another account/);
    release.resolve(); await turn(); assert.equal(calls, 2);
    release.reject(Error('disconnected')); await turn();
    assert.equal(calls, 2); assert.equal(sender.busy, false); assert.equal(sender.dirty, true);
    sender.notify(account); assert.equal(calls, 3);
    sender.notify(account); sender.close(); release.resolve(); await turn();
    sender.notify(account); assert.equal(calls, 3); assert.equal(sender.busy, false);
});
