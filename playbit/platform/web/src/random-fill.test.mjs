import test from 'node:test';
import assert from 'node:assert/strict';
import { encodeId } from './identifier.ts';
import { guestOperations } from './guest-operations.ts';
import { bindOperations, operationSupported } from './guest-imports.ts';

function fixture(admitOperation = () => 0) {
    const memory = new WebAssembly.Memory({ initial: 2 });
    const operations = guestOperations({
        memory: () => memory, events: [], work: {}, admitOperation,
        host() { assert.fail('randomness must not need a data host'); },
        log() {}, submit() {},
    });
    const module = new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0));
    const imports = bindOperations(module, operations);
    return { memory, operations, fill: (pointer, length, ...slots) =>
        imports.pb_syscall(0x14, BigInt(pointer), BigInt(length), ...[...slots, 0n, 0n, 0n, 0n].slice(0, 4)) };
}

test('random.fill uses WebCrypto for the full admitted output and supports discovery', () => {
    // Real platform entropy, with sentinels verifying the exact output extent.
    const { memory, operations, fill } = fixture();
    assert.equal(operationSupported(operations, 0x14), true);
    assert.equal(operationSupported({}, 0x14), false);
    const bytes = new Uint8Array(memory.buffer).fill(42);
    assert.equal(fill(32, 65536), 0n);
    assert.ok(bytes.subarray(32, 65568).some(byte => byte !== 42));
    assert.ok(bytes.subarray(0, 32).every(byte => byte === 42));
    assert.ok(bytes.subarray(65568).every(byte => byte === 42));
});

test('random.fill rejects invalid requests before entropy and erases partial failures', t => {
    // Inject a partial source failure; invalid and empty calls must not reach it.
    const { memory, fill } = fixture();
    const bytes = new Uint8Array(memory.buffer).fill(42);
    let calls = 0;
    t.mock.method(globalThis.crypto, 'getRandomValues', output => {
        calls++;
        output.subarray(0, 8).fill(7);
        throw new Error('entropy unavailable');
    });
    assert.equal(fill(0xffffffff, 0), 0n);
    for (const [pointer, length, status] of [[0, 1, -3n], [131072, 1, -3n],
        [0xffffffff, 32, -3n], [32, 65537, -9n], [0x100000000, 0, -3n], [32, 0x100000000, -3n]]) {
        assert.equal(fill(pointer, length), status);
    }
    for (let slot = 0; slot < 4; slot++) {
        const slots = [0n, 0n, 0n, 0n]; slots[slot] = 1n;
        assert.equal(fill(32, 32, ...slots), -3n);
    }
    assert.equal(calls, 0);
    assert.ok(bytes.every(byte => byte === 42));
    assert.equal(fill(32, 32), -5n);
    assert.equal(calls, 1);
    assert.ok(bytes.subarray(32, 64).every(byte => byte === 0));
    assert.ok(bytes.subarray(0, 32).every(byte => byte === 42));
    assert.ok(bytes.subarray(64).every(byte => byte === 42));
});

test('random.fill respects guest lifecycle admission before changing memory', t => {
    // A retired guest cannot request entropy or change its retained output.
    const { memory, fill } = fixture(() => -8);
    t.mock.method(globalThis.crypto, 'getRandomValues', () => assert.fail('retired guest'));
    const bytes = new Uint8Array(memory.buffer).fill(42);
    assert.equal(fill(32, 32), -8n);
    assert.ok(bytes.every(byte => byte === 42));
});


test('random.fill remains local when account authority is quarantined', async () => {
    // Quarantine invalidates data authority before attempting durable metadata I/O.
    const { quarantineAccount } = await import('./account-outbox.ts');
    const { accountOperationStatus } = await import('./guest-authority.ts');
    const account = encodeId(new Uint8Array(16).fill(31));
    await assert.rejects(quarantineAccount(account, {
        key: 'ab'.repeat(32), clock: 4n, ciphertext: new Uint8Array(32).fill(1),
    }));
    assert.equal(accountOperationStatus(account, 0x144), -2);
    assert.equal(accountOperationStatus(account, 0x14), 0);
});
