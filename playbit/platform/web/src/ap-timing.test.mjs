import test from "node:test";
import assert from "node:assert/strict";
import { AP_TIMING_STAGES, authenticationReply, validateApTiming } from "./ap-timing.ts";

test("AP durations retain full u64 precision within the eight-stage reply allowance", () => {
    const timing = AP_TIMING_STAGES.map(stage => ({ stage, duration_ns: "18446744073709551615" }));
    validateApTiming(timing);
    assert.ok(new TextEncoder().encode(JSON.stringify({ timing })).length <= 1024);
    assert.equal(BigInt(timing[0].duration_ns), (1n << 64n) - 1n);
    validateApTiming([]); // No measurement; never synthetic zero.
});

test("malformed, private, duplicate and missing timing metadata is rejected", () => {
    const entry = { stage: "ap.operation", duration_ns: "1" };
    for (const bad of [undefined, null, {}, [null], [entry, entry],
        [{ ...entry, account: "private" }], [{ ...entry, stage: "private.account" }],
        ...[1, "", "01", "-1", "+1", "1.0", "1e3", "1\n", "18446744073709551616", "9".repeat(21)]
            .map(duration_ns => [{ ...entry, duration_ns }])]) {
        assert.throws(() => validateApTiming(bad), /Invalid AP timing/);
    }
});

test("unknown optional stage names remain bounded and never affect reply payloads", () => {
    const entries = [{ stage: "ap.future_storage", duration_ns: "18446744073709551615" },
        { stage: "ap.operation", duration_ns: "7" }];
    validateApTiming(entries);
    validateApTiming(Array.from({ length: 8 }, (_, index) => ({
        stage: "ap." + "x".repeat(60) + index, duration_ns: "18446744073709551615" })));
    for (const bad of [
        [{ stage: "ap." + "x".repeat(62), duration_ns: "1" }],
        [entries[0], entries[0]], Array.from({ length: 9 }, (_, index) => ({ stage: "ap.future" + index, duration_ns: "1" })),
        [{ stage: "ap.futüré", duration_ns: "1" }], [{ stage: "ap.future\n", duration_ns: "1" }], [{ stage: "ap.future", duration_ns: "01" }],
    ]) assert.throws(() => validateApTiming(bad), /Invalid AP timing/);
    const metadata = new TextEncoder().encode(JSON.stringify(entries));
    for (const [type, flags] of [[0, 0], [1, 1], [5, 2]]) {
        const payload = Uint8Array.of(7, 8, 9);
        const frame = { type, flags, source: new Uint8Array(), destination: new Uint8Array(),
            body: Uint8Array.from([metadata.length & 255, metadata.length >> 8, ...metadata, ...payload]) };
        const decoded = authenticationReply(frame);
        assert.equal(decoded.flags, flags);
        assert.deepEqual(decoded.body, payload);
    }
});

test("authentication metadata is mandatory, bounded and excluded from retained credentials", () => {
    const timing = new TextEncoder().encode('[{"stage":"ap.operation","duration_ns":"18446744073709551615"}]');
    const body = Uint8Array.from([timing.length, 0, ...timing, ...new Uint8Array(72).fill(7)]);
    const frame = { type: 0, flags: 0, source: new Uint8Array(), destination: new Uint8Array(), body };
    const reply = authenticationReply(frame);
    assert.deepEqual(reply.body, new Uint8Array(72).fill(7));
    assert.equal(reply.body.buffer.byteLength, 72, "IndexedDB must not retain diagnostic prefix bytes with credentials");
    for (const invalid of [new Uint8Array(), new Uint8Array(72).fill(7),
        Uint8Array.of(0, 0), Uint8Array.of(1, 0, 48), Uint8Array.of(255, 255),
        Uint8Array.of(2, 0, 123, 125), new Uint8Array(4097)]) {
        assert.throws(() => authenticationReply({ ...frame, body: invalid }));
    }
    const exact = Uint8Array.from([2, 0, 91, 93, ...new Uint8Array(4092)]);
    assert.equal(authenticationReply({ ...frame, body: exact }).body.length, 4092);
    assert.throws(() => authenticationReply({ ...frame, type: 2 }));
    assert.throws(() => authenticationReply({ ...frame, source: Uint8Array.of(1) }));
});
