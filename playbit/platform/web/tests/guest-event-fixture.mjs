import test from "node:test";
import assert from "node:assert/strict";

export function guestEventCases(module, label, eventBytes = 24) {
    const optional = 0x80000000 | 999;
    const event = (kind, data = new Uint8Array(), target = 0) => ({ kind, data, target });
    const checksum = events => events.reduce((hash, event) => {
        hash = (Math.imul(hash, 31) + event.kind) >>> 0;
        for (const byte of event.data) hash = (Math.imul(hash, 31) + byte) >>> 0;
        return hash;
    }, 0);

    async function run(events) {
        const queue = [...events];
        let instance;
        const imports = { playbit: { pb_syscall(opcode, header, payload, capacity, d, e, f) {
            assert.equal(opcode, 4);
            assert.deepEqual([d, e, f], [0n, 0n, 0n]);
            const view = new DataView(instance.exports.memory.buffer);
            const pointer = Number(header);
            assert.equal(view.getUint32(pointer, true), eventBytes);
            assert.equal(view.getUint32(pointer + 4, true), 0);
            if (!queue.length) return -6n;
            const next = queue[0];
            if (next.data.length > Number(capacity)) return BigInt(next.data.length);
            view.setUint32(pointer + 4, eventBytes === 32 ? 28 : 24, true);
            view.setBigUint64(pointer + 8, 7n, true);
            view.setUint32(pointer + 16, next.kind, true);
            view.setUint32(pointer + 20, next.target, true);
            if (eventBytes === 32) view.setUint32(pointer + 24, 17, true);
            new Uint8Array(instance.exports.memory.buffer, Number(payload), next.data.length).set(next.data);
            queue.shift();
            return BigInt(next.data.length);
        } } };
        instance = await WebAssembly.instantiate(module, imports);
        const result = instance.exports.probe_events();
        return { queue, hash: Number((result >> 32n) & 0xffffffffn), status: Number(BigInt.asIntN(32, result)) };
    }

    test(`${label}: bounded optional events preserve known input and completion`, async () => {
        const known = [event(7, new Uint8Array(8).fill(7)), event(2, new Uint8Array(), 1)];
        const result = await run([known[0], event(optional), event(optional, new Uint8Array(65536).fill(3)), known[1]]);
        assert.equal(result.hash, checksum(known));
        assert.equal(result.status, 0);
        assert.equal(result.queue.length, 0);
    });

    if (eventBytes === 32) test(`${label}: accepts a typed key event from a window`, async () => {
        const bytes = new Uint8Array(60), view = new DataView(bytes.buffer);
        view.setUint16(20, 19, true); // A
        view.setUint16(22, 35, true); // Q
        view.setUint8(24, 1);
        view.setUint8(26, 1);
        view.setUint32(28, "a".codePointAt(0), true);
        const keyboard = event(23, bytes);
        const result = await run([keyboard]);
        assert.equal(result.hash, checksum([keyboard]));
        assert.equal(result.status, 0);
        assert.equal(result.queue.length, 0);
    });

    test(`${label}: mandatory unknown preserves prior completion and stops delivery`, async () => {
        const known = event(7, new Uint8Array(8).fill(7));
        const following = event(2, new Uint8Array(), 1);
        for (const kind of [999, 0x80000000 | 7]) {
            const result = await run([known, event(kind), following]);
            assert.equal(result.hash, checksum([known]));
            assert.equal(result.status, -7);
            assert.deepEqual(result.queue, [following]);
        }
        const first = await run([event(999), following]);
        assert.equal(first.hash, 0);
        assert.equal(first.status, -7);
        assert.deepEqual(first.queue, [following]);
    });
}
