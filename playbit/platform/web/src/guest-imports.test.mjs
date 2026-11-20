import test from "node:test";
import assert from "node:assert/strict";
import { bindOperations, operationSupported } from "./guest-imports.ts";

test('authority admission follows canonical slots and known handlers, before effects', () => {
    let admitted = 0, called = 0;
    const module = new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0));
    const imports = bindOperations(module, {
        admitOperation(opcode) { admitted++; assert.equal(opcode, 0x144); return -2; },
        pb_kv_put_begin() { called++; return 0; },
    });
    assert.equal(imports.pb_syscall(0xffff, 0n, 0n, 0n, 0n, 0n, 0n), -7n);
    assert.equal(imports.pb_syscall(0x142, 0n, 0n, 0n, 0n, 0n, 0n), -7n);
    assert.equal(imports.pb_syscall(0x144, 0x100000000n, 0n, 0n, 0n, 0n, 0n), -3n);
    assert.equal(admitted, 0);
    assert.equal(imports.pb_syscall(0x144, 0n, 0n, 0n, 0n, 0n, 0n), -2n);
    assert.equal(admitted, 1); assert.equal(called, 0);
});

test("async data entries preserve full handle bits and reject noncanonical slots before callbacks", () => {
    const module = new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0));
    const calls = [];
    const names = ["pb_object_read_begin", "pb_object_read_take", "pb_object_edit_begin",
        "pb_object_edit_take", "pb_object_query_begin", "pb_object_query_take",
        "pb_schema_read_begin", "pb_schema_read_take", "pb_schema_establish_begin", "pb_schema_establish_take",
        "pb_object_field_read_authenticated_begin", "pb_object_field_read_authenticated_take",
        "pb_entry_resolve_begin", "pb_entry_resolve_take"];
    const operations = Object.fromEntries(names.map(name => [name, (...args) => {
        calls.push([name, args]);
        return 0;
    }]));
    const imports = bindOperations(module, operations);
    const handle = 0x79abcdef_fedcba98n;
    const cases = [
        [0x130, [128n, 256n, 384n, 4n, 0n, 0n], [128, 256, 384, 4]],
        [0x131, [handle, 256n, 0n, 0n, 0n, 0n], [0x79abcdefn, 0xfedcba98, 256]],
        [0x132, [128n, 256n, 384n, 4n, 40n, 0n], [128, 256, 384, 4, 40]],
        [0x133, [handle, 0n, 0n, 0n, 0n, 0n], [0x79abcdefn, 0xfedcba98]],
        [0x134, [128n, 256n, 0n, 0n, 0n, 0n], [128, 256]],
        [0x135, [handle, 256n, 0n, 0n, 0n, 0n], [0x79abcdefn, 0xfedcba98, 256]],
        [0x102, [128n, 0n, 0n, 0n, 0n, 0n], [128]],
        [0x103, [handle, 256n, 0n, 0n, 0n, 0n], [0x79abcdefn, 0xfedcba98, 256]],
        [0x104, [128n, 256n, 4n, 48n, 0n, 0n], [128, 256, 4, 48]],
        [0x105, [handle, 0n, 0n, 0n, 0n, 0n], [0x79abcdefn, 0xfedcba98]],
        [0x136, [128n, 256n, 384n, 0n, 0n, 0n], [128, 256, 384]],
        [0x137, [handle, 512n, 0n, 0n, 0n, 0n], [0x79abcdefn, 0xfedcba98, 512]],
        [0x106, [handle, 128n, 2n, 0n, 0n, 0n], [0x79abcdefn, 0xfedcba98, 128, 2]],
        [0x107, [handle, 256n, 0n, 0n, 0n, 0n], [0x79abcdefn, 0xfedcba98, 256]],
    ];
    for (const opcode of [0x100, 0x101, 0x110, 0x111, 0x112, 0x113, 0x118]) {
        assert.equal(imports.pb_syscall(opcode, handle, 256n, 384n, 512n, 0n, 0n), -7n);
    }
    assert.equal(calls.length, 0);
    for (const [index, [opcode, args, expected]] of cases.entries()) {
        assert.equal(imports.pb_syscall(opcode, ...args), 0n);
        assert.deepEqual(calls.at(-1), [names[index], expected]);
        const count = calls.length;
        if (args[5] === 0n) {
            const unused = args.with(5, 1n);
            assert.equal(imports.pb_syscall(opcode, ...unused), -3n);
            assert.equal(calls.length, count);
        }
        const truncated = args.with(1, 0x100000000n);
        assert.equal(imports.pb_syscall(opcode, ...truncated), -3n);
        assert.equal(calls.length, count);
    }
});

test("clock reads preserve all u64 bits without account admission", () => {
    const module = new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0));
    let calls = 0;
    for (const timestamp of [0x0020000000000001n, 0x8877665544332201n]) {
        const operations = {
            admitOperation() { assert.fail("clock must not require account admission"); },
            pb_clock_read(...args) { calls++; assert.deepEqual(args, []); return timestamp; },
        };
        const imports = bindOperations(module, operations);
        assert.equal(operationSupported(operations, 0x10), true);
        assert.equal(imports.pb_syscall(0x10, 0n, 0n, 0n, 0n, 0n, 0n), BigInt.asIntN(64, timestamp));
        const count = calls;
        for (let slot = 0; slot < 6; slot++) {
            const args = Array(6).fill(0n).with(slot, 1n);
            assert.equal(imports.pb_syscall(0x10, ...args), -3n);
        }
        assert.equal(calls, count);
    }
    assert.equal(calls, 2);
});

test("clock reads are always available with a built-in monotonic clock", () => {
    const module = new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0));
    const operations = {
        admitOperation() { assert.fail("clock must not require account admission"); },
    };
    assert.equal(operationSupported({}, 0x10), true);
    const imports = bindOperations(module, operations);
    const before = BigInt(Math.trunc(performance.now() * 1_000_000));
    const first = imports.pb_syscall(0x10, 0n, 0n, 0n, 0n, 0n, 0n);
    const second = imports.pb_syscall(0x10, 0n, 0n, 0n, 0n, 0n, 0n);
    const after = BigInt(Math.trunc(performance.now() * 1_000_000));
    assert.equal(typeof first, "bigint");
    assert.ok(before <= first && first <= second && second <= after);
});

test("direct handle results preserve every positive i64 bit and reject removed output slots", () => {
    const module = new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0));
    for (const handle of [0x100000001n, 0x0020000000000001n, 0x7fffffffffffffffn]) {
        let effects = 0;
        const imports = bindOperations(module, { pb_file_open_take() { effects++; return handle; } });
        assert.equal(imports.pb_syscall(0x151, 0x100000001n, 0n, 0n, 0n, 0n, 0n), handle);
        assert.equal(imports.pb_syscall(0x151, 0x100000001n, 8n, 0n, 0n, 0n, 0n), -3n);
        assert.equal(effects, 1);
    }
});

test("audio operations remain unsupported without browser effects or authority admission", () => {
    const module = new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0));
    const operations = {
        admitOperation() { assert.fail("unsupported audio must not enter authority admission"); },
    };
    const imports = bindOperations(module, operations);
    for (let opcode = 0x1a0; opcode <= 0x1ad; opcode++) {
        assert.equal(operationSupported(operations, opcode), false);
        assert.equal(imports.pb_syscall(opcode, opcode === 0x1a0 ? 44100n : 0x7edcba9880000001n, 0n, 0n, 0n, 0n, 0n), -7n);
    }
});
