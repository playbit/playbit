import assert from "node:assert/strict";
import test from "node:test";
import { WebApTransport, WebSocketTransport, WS_PROTOCOL } from "./ap-transport.ts";

const tick = () => new Promise(resolve => setImmediate(resolve));
function transport(ready = Promise.resolve()) {
    let ended;
    return { ready, closed: new Promise(resolve => { ended = resolve; }), close() { this.stopped = true; ended(); },
        async createBidirectionalStream() { return "stream"; } };
}
test("working WebTransport is retained without opening a WebSocket", async () => {
    const wt = transport();
    const connection = new WebApTransport("https://ap.example/", () => wt, () => { throw new Error("must not run"); });
    await connection.ready;
    assert.equal(await connection.createBidirectionalStream(), "stream");
    connection.close(); assert.equal(wt.stopped, true);
});
for (const mode of ["absent", "rejected", "blackholed"]) {
    test(`${mode} WebTransport selects WebSocket before authentication`, async () => {
        const wt = transport(mode === "rejected" ? Promise.reject(new Error("blocked")) : new Promise(() => {}));
        const ws = transport(); let calls = 0;
        const connection = new WebApTransport("https://ap.example/", () => {
            if (mode === "absent") throw new ReferenceError("WebTransport");
            return wt;
        }, () => { calls++; return ws; }, 5);
        await connection.ready;
        assert.equal(calls, 1);
        if (mode !== "absent") assert.equal(wt.stopped, true);
        assert.equal(await connection.createBidirectionalStream(), "stream");
        connection.close();
    });
}
test("closure cancels negotiation without opening another transport", async () => {
    const connection = new WebApTransport("https://ap.example/", () => transport(new Promise(() => {})),
        () => { throw new Error("must not run"); });
    connection.close();
    await assert.rejects(connection.ready, /closed/);
});
test("an established connection failure does not negotiate behind authentication", async () => {
    const wt = transport(); let fallback = false;
    const connection = new WebApTransport("https://ap.example/", () => wt, () => { fallback = true; return transport(); });
    await connection.ready;
    wt.close(); await connection.closed;
    assert.equal(fallback, false);
    await assert.rejects(connection.createBidirectionalStream(), /closed/);
});
function packet(kind, id, body = new Uint8Array()) {
    const bytes = new Uint8Array(5 + body.length);
    bytes[0] = kind; new DataView(bytes.buffer).setUint32(1, id, true); bytes.set(body, 5);
    return bytes;
}
function credit(count) { const bytes = new Uint8Array(4); new DataView(bytes.buffer).setUint32(0, count, true); return bytes; }
async function fixture() {
    const socket = { protocol: WS_PROTOCOL, bufferedAmount: 0, sent: [],
        send(bytes) { this.sent.push(bytes); }, close() { this.ended = true; },
        receive(kind, id, body) { this.onmessage({ data: packet(kind, id, body).buffer }); } };
    const connection = new WebSocketTransport("wss://ap.example/", () => socket);
    socket.onopen(); await connection.ready;
    return { socket, connection };
}
test("one stalled stream consumes only its credit and sibling bytes progress", async () => {
    const { socket, connection } = await fixture();
    const first = await connection.createBidirectionalStream();
    const second = await connection.createBidirectionalStream();
    const writer = first.writable.getWriter();
    let completed = false;
    const pending = writer.write(new Uint8Array(65536)).then(() => { completed = true; });
    await tick(); assert.equal(completed, false);
    assert.equal(socket.sent.filter(p => p[0] === 1).reduce((n, p) => n + p.length - 5, 0), 32768);
    await second.writable.getWriter().write(Uint8Array.of(42));
    assert.equal(socket.sent.at(-1)[5], 42);
    socket.receive(4, 1, credit(32768)); await pending;
    connection.close();
});
test("receive credit returns only when the reader consumes bytes", async () => {
    const { socket, connection } = await fixture();
    const stream = await connection.createBidirectionalStream();
    socket.receive(1, 1, new Uint8Array(16384)); socket.receive(1, 1, new Uint8Array(16384));
    assert.equal(socket.sent.length, 1);
    const reader = stream.readable.getReader();
    assert.equal((await reader.read()).value.length, 16384);
    assert.equal(socket.sent.at(-1)[0], 4);
    await reader.cancel(); assert.equal(socket.sent.at(-1)[0], 3);
    socket.receive(2, 1); await stream.writable.getWriter().close();
    connection.close();
});
test("STOP wakes a blocked writer and acknowledges its half-close", async () => {
    const { socket, connection } = await fixture();
    const stream = await connection.createBidirectionalStream();
    const pending = stream.writable.getWriter().write(new Uint8Array(65536));
    await tick(); socket.receive(3, 1);
    await assert.rejects(pending, /closed/);
    assert.equal(socket.sent.at(-1)[0], 2);
    connection.close();
});
test("local abort cancels a write waiting for peer credit", async () => {
    const { socket, connection } = await fixture();
    const stream = await connection.createBidirectionalStream();
    const writer = stream.writable.getWriter();
    const pending = writer.write(new Uint8Array(65536));
    await tick();
    const rejected = assert.rejects(pending, /closed/);
    await writer.abort(); await rejected;
    assert.equal(socket.sent.at(-1)[0], 2);
    connection.close();
});
test("stream slots are reusable after both FINs, without reusing IDs", async () => {
    const { socket, connection } = await fixture();
    for (let id = 1; id <= 128; id++) {
        const stream = await connection.createBidirectionalStream();
        socket.receive(2, id);
        await stream.writable.getWriter().close();
        assert.equal((await stream.readable.getReader().read()).done, true);
    }
    for (let i = 0; i < 1024; i++) await connection.createBidirectionalStream();
    await assert.rejects(connection.createBidirectionalStream(), /limit/);
    connection.close();
});
for (const mode of ["credit", "data-credit", "text", "oversize", "fragments", "duplicate-fin", "future-id"]) {
    test(`malformed ${mode} terminates every stream`, async () => {
        const { socket, connection } = await fixture();
        const stream = await connection.createBidirectionalStream();
        if (mode === "credit") socket.receive(4, 1, credit(1));
        if (mode === "data-credit") for (let i = 0; i < 3; i++) socket.receive(1, 1, new Uint8Array(16384));
        if (mode === "text") socket.onmessage({ data: "bad" });
        if (mode === "oversize") socket.receive(1, 1, new Uint8Array(16385));
        if (mode === "fragments") for (let i = 0; i < 257; i++) socket.receive(1, 1, Uint8Array.of(0));
        if (mode === "duplicate-fin") { socket.receive(2, 1); socket.receive(2, 1); }
        if (mode === "future-id") socket.receive(2, 2);
        assert.equal(socket.ended, true);
        await assert.rejects(stream.readable.getReader().read(), /closed/);
    });
}
test("socket output capacity fails explicitly instead of polling for drain", async () => {
    const { socket, connection } = await fixture();
    const stream = await connection.createBidirectionalStream();
    socket.bufferedAmount = 2 * 1024 * 1024;
    await assert.rejects(stream.writable.getWriter().write(Uint8Array.of(1)), /limit/);
    assert.equal(socket.ended, true);
});

test("connection credit bounds aggregate writes independently of 1024 stream slots", async () => {
    // Sixty-four full windows exhaust connection bytes; a new stream waits.
    const {socket, connection} = await fixture();
    for (let n = 0; n < 64; n++) {
        const stream = await connection.createBidirectionalStream();
        await stream.writable.getWriter().write(new Uint8Array(32768));
    }
    const next = await connection.createBidirectionalStream();
    let finished = false;
    const pending = next.writable.getWriter().write(new Uint8Array(16)).then(() => { finished = true; });
    await tick(); assert.equal(finished, false);
    socket.receive(4, 0, credit(16));
    await pending; assert.equal(finished, true);
    socket.receive(4, 0, credit(2 * 1024 * 1024 + 1));
    assert.equal(socket.ended, true);
});
