import { encodeId } from './identifier.ts';
import assert from "node:assert/strict";
import test from "node:test";
import { AP, AGREEMENT, SERVICE_PROTOCOL, encodeFrame, decodeFrame, utf8 } from "./ap-codec.ts";
import { ApConnection } from "./ap-connection.ts";
import { downloadPackage, MAX_PACKAGE_BYTES } from './package-stream.ts';

// Frames emitted by the fake AP, including the mandatory authentication envelope.
function encodeReply(frame) {
    if ([AP.HELLO, AP.REGISTER, AP.KEY_CHALLENGE].includes(frame.type)) {
        frame = { ...frame, body: Uint8Array.from([2, 0, 91, 93, ...(frame.body ?? [])]) };
    }
    return encodeFrame(frame);
}

test("default transport selects its channel AP with ordinary web browser trust", async () => {
    const previous = Object.getOwnPropertyDescriptor(globalThis, "WebTransport");
    const calls = [];
    class Transport {
        constructor(...args) { calls.push(args); this.ready = Promise.resolve(); this.closed = new Promise(() => {}); }
        close() {}
    }
    Object.defineProperty(globalThis, "WebTransport", { configurable: true, value: Transport });
    try {
        const connection = new ApConnection('public');
        const selected = connection.transportFactory();
        await selected.ready;
        assert.deepEqual(calls, [["https://ap.playbit.app/"]]);
        await new ApConnection('testing').transportFactory().ready;
        assert.deepEqual(calls[1], ["https://ap.testing.playbit.app/"]);
        const fixture = {};
        assert.equal(new ApConnection('public', { transportFactory: () => fixture }).transportFactory(), fixture);
        assert.equal(calls.length, 2);
    } finally {
        if (previous) Object.defineProperty(globalThis, "WebTransport", previous);
        else delete globalThis.WebTransport;
    }
});

// A transport whose streams accept writes and never answer, so an operation can
// only end by timing out. `defer` holds stream creation open until released.
function silentTransport({ defer = false } = {}) {
    const state = { closed: false, abandoned: 0, streams: 0, written: 0, pending: [] };
    const stream = () => {
        state.streams++;
        return {
            writable: new WritableStream({
                write() {
                    state.written++;
                },
            }),
            readable: new ReadableStream({
                start() {},
                cancel() {
                    state.abandoned++;
                },
            }),
        };
    };
    state.transport = {
        close() {
            state.closed = true;
        },
        createBidirectionalStream() {
            if (!defer) return Promise.resolve(stream());
            return new Promise(resolve => state.pending.push(() => resolve(stream())));
        },
    };
    state.release = () => state.pending.splice(0).forEach(resolve => resolve());
    return state;
}

function connectedTo(state) {
    const connection = new ApConnection('public');
    connection.account = encodeId(new Uint8Array(16).fill(0x00));
    connection.identity = { instanceId: utf8.encode(encodeId(new Uint8Array(16))) };
    connection.transport = state.transport;
    connection.ready = Promise.resolve();
    connection.operationTimeout = 20;
    return connection;
}

const settled = () => new Promise(resolve => setTimeout(resolve, 50));

test("registration reads the response before closing its authentication stream", async () => {
    let incoming, delivered = false;
    const response = { type: AP.REGISTER, flags: 0, body: new Uint8Array(4) };
    const transport = {
        ready: Promise.resolve(), closed: new Promise(() => {}), close() {},
        async createBidirectionalStream() {
            return {
                readable: new ReadableStream({ start(controller) { incoming = controller; } }),
                writable: new WritableStream({
                    write() {
                        setTimeout(() => {
                            delivered = true;
                            incoming.enqueue(encodeReply(response));
                        }, 10);
                    },
                    close() { assert.equal(delivered, true, "early EOF closes the unauthenticated AP connection"); },
                }),
            };
        },
    };
    const connection = new ApConnection('public', { transportFactory: () => transport });
    assert.equal((await connection.authenticationFrame(response)).type, AP.REGISTER);
});

test("authentication response survives AP disconnect during stream cleanup", async () => {
    const response = { type: AP.REGISTER, flags: 0, body: new Uint8Array(4) };
    const transport = {
        ready: Promise.resolve(), closed: new Promise(() => {}), close() {},
        async createBidirectionalStream() {
            return {
                readable: new ReadableStream({
                    start(controller) { controller.enqueue(encodeReply(response)); },
                    cancel() { throw new Error("Connection lost."); },
                }),
                writable: new WritableStream({ close() { throw new Error("Connection lost."); } }),
            };
        },
    };
    const connection = new ApConnection('public', { transportFactory: () => transport });
    assert.equal((await connection.authenticationFrame(response)).type, AP.REGISTER);
});

test("an AP response remains authoritative when request half-close receives STOP_SENDING", async () => {
    const state = {
        transport: {
            async createBidirectionalStream() {
                return {
                    readable: new ReadableStream({ start(controller) { controller.enqueue(encodeReply(answer({ready:true}))); } }),
                    writable: new WritableStream({ close() { throw new Error("Received STOP_SENDING."); } }),
                };
            },
        },
    };
    const connection = connectedTo(state);
    assert.deepEqual((await connection.request(operation)).data, {ready:true});
});

test("a timed out operation abandons its own stream, not the account connection", async () => {
    const state = silentTransport();
    const connection = connectedTo(state);

    await assert.rejects(
        connection.request({ op: "volume_usage", operation_id: encodeId(new Uint8Array(16).fill(0xab)) }),
        /timed out/,
        "a stalled operation must fail",
    );
    assert.equal(state.abandoned, 1, "the operation's own stream is abandoned");
    assert.equal(state.closed, false, "the account connection stays open");
    assert.equal(connection.active, 0, "the operation slot is released");

    // Everything else on the connection still works afterwards.
    await assert.rejects(connection.request({ op: "volume_usage", operation_id: encodeId(new Uint8Array(16).fill(0xab)) }), /timed out/);
    assert.equal(state.streams, 2, "a later operation still opens its own stream");
    assert.equal(state.closed, false);
});

test("a stream that arrives after the deadline is abandoned unsent", async () => {
    const state = silentTransport({ defer: true });
    const connection = connectedTo(state);

    await assert.rejects(
        connection.request({ op: "volume_usage", operation_id: encodeId(new Uint8Array(16).fill(0xab)) }),
        /timed out/,
        "the caller gives up while the stream is still being created",
    );
    assert.equal(connection.active, 0, "the operation slot is released");
    assert.equal(state.streams, 0, "no stream existed yet to cancel");

    // The transport hands over the stream only now.
    state.release();
    await settled();
    assert.equal(state.written, 0, "a request is never sent for an abandoned operation");
    assert.equal(state.abandoned, 1, "the late stream is abandoned instead of used");
    assert.equal(state.closed, false, "the account connection stays open");
    assert.equal(connection.active, 0, "the late stream does not reopen the slot");
});

test("a timed out upload stops reading its source", async () => {
    const state = silentTransport();
    const connection = connectedTo(state);
    let cancelled = false;
    // A source that never produces a chunk, so the upload can only end by cancel.
    const upload = new ReadableStream({
        pull() {
            return new Promise(() => {});
        },
        cancel() {
            cancelled = true;
        },
    });

    await assert.rejects(
        connection.request({ op: "upload_app", operation_id: encodeId(new Uint8Array(16).fill(0xab)), content_length: 4 }, upload),
        /timed out/,
    );
    await settled();
    assert.equal(cancelled, true, "the upload source is cancelled, not left reading");
    assert.equal(state.closed, false, "the account connection stays open");
    assert.equal(connection.active, 0);
});

test("an upload past the web browser's byte ceiling is rejected before any bytes are sent", async () => {
    const state = silentTransport();
    const connection = connectedTo(state);
    // Never read from; a rejection before getReader() would leave this untouched.
    const upload = new ReadableStream({
        pull() {
            return new Promise(() => {});
        },
    });

    await assert.rejects(
        connection.request({ op: "upload_app", operation_id: encodeId(new Uint8Array(16).fill(0xab)), content_length: 64 * 1024 * 1024 + 1 }, upload),
        /Invalid upload length/,
    );
    assert.equal(state.written, 1, "only the operation frame is written; no upload data is sent");
    assert.equal(connection.active, 0);
});

const operation = { op: "get_blob", operation_id: encodeId(new Uint8Array(16).fill(0xab)), hash: encodeId(new Uint8Array(32).fill(0xcd)) };
const answer = data => ({ type: AP.DIRECT, flags: SERVICE_PROTOCOL, source: utf8.encode("service"), destination: utf8.encode("client"),
    body: utf8.encode(JSON.stringify({ operation_id: operation.operation_id, timing: [], ok: true, data })) });
const dataFrame = body => ({ type: AP.DIRECT, source: utf8.encode("service"), destination: utf8.encode("client"), body });

// Exercise byte framing in both directions with an in-memory reliable stream.
function scriptedTransport(onWrite) {
    const writes = [];
    const transport = {
        ready: Promise.resolve(), closed: new Promise(() => {}), close() {},
        async createBidirectionalStream() {
            let incoming;
            return {
                readable: new ReadableStream({ start(controller) { incoming = controller; } }),
                writable: new WritableStream({
                    write(bytes) {
                        writes.push(bytes.slice());
                        for (const frame of onWrite(bytes) ?? []) incoming.enqueue(encodeReply(frame));
                    },
                }),
            };
        },
    };
    return { transport, writes };
}

test("only an explicit acceptance for the shown document records a receipt", async () => {
    let accepted = false, wrongDocument = false, switchAccount = false;
    const state = scriptedTransport(bytes => {
        const frame = decodeFrame(bytes);
        assert.equal(frame.type, AP.AGREEMENT);
        assert.equal(encodeId(frame.body), AGREEMENT.sha256);
        if (frame.flags === AGREEMENT.revision) accepted = true;
        if (switchAccount) connection.account = "other";
        return [{ ...frame, flags: accepted ? AGREEMENT.revision : 0, body: wrongDocument ? new Uint8Array(32) : frame.body }];
    });
    const connection = connectedTo(state);
    const query = { ...AGREEMENT, account: connection.account, accept: false };
    assert.equal(await connection.agreement(query), false);
    await assert.rejects(connection.agreement({ ...query, account: "other" }), /signed-in account/);
    assert.equal(await connection.agreement({ ...query, accept: true }), true);
    assert.equal(await connection.agreement(query), true);
    wrongDocument = true;
    await assert.rejects(connection.agreement(query), /verify this agreement/);
    wrongDocument = false; switchAccount = true;
    await assert.rejects(connection.agreement(query), /account changed/);
});

test("an agreement click after disconnect reconnects only the account that reviewed it", async () => {
    const token = new Uint8Array(72).fill(2), account = encodeId(token.subarray(0, 16));
    const identity = { authToken: token, instanceId: utf8.encode(account), onboardingAcknowledged: 1 };
    const frames = [];
    const connection = new ApConnection("public", {
        credentialStore: async () => identity,
        transportFactory: () => {
            const { transport } = scriptedTransport(bytes => {
                const frame = decodeFrame(bytes);
                frames.push(frame);
                if (frame.type === AP.HELLO) {
                    assert.deepEqual(frame.body, token);
                    return [{ type: AP.HELLO }];
                }
                assert.equal(frame.type, AP.AGREEMENT);
                return [frame];
            });
            const closed = Promise.withResolvers();
            transport.closed = closed.promise;
            transport.close = closed.resolve;
            return transport;
        },
    });
    const disconnect = async () => {
        const transport = connection.transport;
        transport.close();
        await transport.closed;
        assert.equal(connection.status().connected, false);
    };
    await connection.connect({ account });
    const review = { ...AGREEMENT, account, accept: false };
    assert.equal(await connection.agreement(review), false);
    await disconnect();
    await connection.connect({ account });
    assert.deepEqual(frames.map(frame => [frame.type, frame.flags]), [[AP.HELLO, 0], [AP.AGREEMENT, 0], [AP.HELLO, 0]]);

    await disconnect();
    assert.equal(await connection.agreement({ ...review, accept: true }), true);
    assert.deepEqual(frames.slice(3).map(frame => [frame.type, frame.flags]), [[AP.HELLO, 0], [AP.AGREEMENT, AGREEMENT.revision]]);
    assert.equal(encodeId(frames.at(-1).body), AGREEMENT.sha256);

    await disconnect();
    identity.authToken = new Uint8Array(72).fill(3);
    await assert.rejects(connection.agreement({ ...review, accept: true }), /signed-in account differs/);
    assert.equal(frames.length, 5, "a replacement account receives no agreement request");
});

function framedConnection(frames) {
    const state = scriptedTransport(() => frames.splice(0));
    const connection = connectedTo(state);
    connection.operationTimeout = 1000;
    return { ...state, connection };
}

test("download reads framed bytes, bounds length, and checks the final acknowledgment", async () => {
    async function receive(connection) {
        const bytes = [];
        for await (const chunk of downloadPackage(connection, operation)) bytes.push(...chunk);
        return Uint8Array.from(bytes);
    }
    const bytes = Uint8Array.of(1, 2, 3, 4);
    const { connection } = framedConnection([answer({ content_length: 4 }), dataFrame(bytes.subarray(0, 1)), dataFrame(bytes.subarray(1)), answer(null)]);
    assert.deepEqual(await receive(connection), bytes);
    assert.equal(connection.active, 0);
    for (const [frames, expected] of [
        [[answer({ content_length: MAX_PACKAGE_BYTES + 1 })], /limit/],
        [[answer({ content_length: 3 }), dataFrame(bytes)], /byte count/],
        [[answer({ content_length: 4 }), dataFrame(bytes), { ...answer(null), body: utf8.encode(JSON.stringify({ timing: [], ok: false, code: "operation_failed", operation_id: operation.operation_id, error: "Upload not durable" })) }], /not durable/],
        [[{ ...answer({ content_length: 0 }), source: utf8.encode("other") }], /service data frame/],
        [[{ ...answer({ content_length: 0 }), body: utf8.encode(JSON.stringify({ timing: [], ok: true, operation_id: encodeId(new Uint8Array(16).fill(0xff)) })) }], /identity differs/],
    ]) {
        const { connection } = framedConnection(frames);
        await assert.rejects(receive(connection), expected);
        assert.equal(connection.active, 0);
    }
});

test("service replies reject incompatible versions before decoding JSON and release the stream", async () => {
    for (const flags of [0, SERVICE_PROTOCOL + 1, 255]) {
        for (const body of [answer(null).body, utf8.encode("not JSON")]) {
            const { connection } = framedConnection([{ ...answer(null), flags, body }]);
            await assert.rejects(connection.request(operation), {
                code: "ap-rejected",
                message: "Playbit and its sync service need compatible updates before they can connect. Your saved data has been kept.",
            });
            assert.equal(connection.active, 0);
            assert.equal(connection.streams.size, 0);
        }
    }
});

test("bulk data is opaque while service failures terminate the transfer", async () => {
    const body = answer(null).body;
    const good = framedConnection([dataFrame(body)]);
    const handle = await good.connection.begin(operation);
    assert.deepEqual(await good.connection.streamRead(handle), body);
    good.connection.streamClose(handle);
    for (const [frame, expected] of [
        [{ ...answer(null), body: utf8.encode(JSON.stringify({ operation_id: operation.operation_id, timing: [], ok: false, code: "operation_failed", error: "Permission removed" })) }, /Permission removed/],
        [{ ...answer(null), flags: SERVICE_PROTOCOL + 1, body: utf8.encode("not JSON") }, /compatible updates/],
        [answer(null), /Unexpected AP service reply/],
    ]) {
        const { connection } = framedConnection([frame]);
        const handle = await connection.begin(operation);
        await assert.rejects(connection.streamRead(handle), expected);
        assert.equal(connection.active, 0);
    }
});

test("bidirectional transfer follows native AP framing and half-close semantics", async () => {
    const { connection, writes } = framedConnection([answer(null), dataFrame(Uint8Array.of(9)),
        answer({ root: null, revision: 2, policy_revision: 1, source: { generation: encodeId(new Uint8Array(16).fill(0x11)), epoch: 3 } })]);
    const handle = await connection.begin({ ...operation, op: "sync_database" });
    await connection.streamResponse(handle);
    const bytes = new Uint8Array(256 * 1024 + 17).fill(7);
    await connection.streamWrite(handle, bytes);
    assert.equal(writes.length, 3);
    assert.equal(decodeFrame(writes[0]).flags, SERVICE_PROTOCOL);
    assert.equal(decodeFrame(writes[1]).flags, 0);
    assert.equal(decodeFrame(writes[1]).body.length, 256 * 1024);
    assert.equal(decodeFrame(writes[2]).body.length, 17);
    assert.equal(new TextDecoder().decode(decodeFrame(writes[1]).destination), "service");
    assert.deepEqual(await connection.streamRead(handle), Uint8Array.of(9));
    await connection.streamFinish(handle);
    const receipt = (await connection.streamResponse(handle)).data;
    assert.equal(receipt.revision, 2n);
    assert.equal(receipt.policy_revision, 1n);
    assert.equal(receipt.source.epoch, 3n);
    connection.streamClose(handle);
    assert.equal(connection.active, 0);
    await assert.rejects(connection.streamRead(handle), /closed/);
});

test("namespace evidence rejects oversized frames from the header before reading their body", async () => {
    // An oversized declaration without a body must fail immediately, while a
    // maximum-sized valid transfer still leaves the final receipt readable.
    const maximum = 8 + 4096 * 16;
    const bytes = new Uint8Array(maximum).fill(7);
    const valid = framedConnection([dataFrame(bytes), answer(null)]);
    const handle = await valid.connection.begin({ ...operation, op: "sync_database" });
    assert.deepEqual(await valid.connection.streamNamespaceEvidence(handle), bytes);
    await valid.connection.streamResponse(handle);
    valid.connection.streamClose(handle);
    const header = encodeFrame(dataFrame(new Uint8Array())).slice(0, 8);
    new DataView(header.buffer).setUint32(0, 32 + maximum + 1, true);
    let canceled = false;
    const state = {
        transport: {
            createBidirectionalStream: async () => ({
                readable: new ReadableStream({
                    start(controller) { controller.enqueue(header); },
                    cancel() { canceled = true; },
                }),
                writable: new WritableStream(),
            }),
        },
    };
    const connection = connectedTo(state);
    connection.operationTimeout = 1000;
    const oversized = await connection.begin({ ...operation, op: "sync_database" });
    await assert.rejects(connection.streamNamespaceEvidence(oversized), /body exceeds limit/);
    assert.equal(connection.active, 0);
    assert.equal(canceled, true);
});

test("a stalled transfer is bounded and does not close sibling streams", async () => {
    const state = silentTransport();
    const connection = connectedTo(state);
    const first = await connection.begin(operation);
    const second = await connection.begin(operation);
    await assert.rejects(connection.streamRead(first), /timed out/);
    assert.equal(state.closed, false);
    // Either still idle or timed out independently; it never closes the account.
    connection.streamClose(second);
    assert.equal(connection.active, 0);
});

test("unconfirmed email delivery retains enrollment identity and reports a retry", async () => {
    let saved;
    const state = scriptedTransport(() => [{ type: AP.REGISTER, flags: 4, body: new Uint8Array(4) }]);
    const connection = new ApConnection('public', {
        credentialStore: async (_key, value) => value ? (saved = value) : saved,
        transportFactory: () => state.transport,
    });
    await assert.rejects(connection.requestCode({ email: "alice@example.invalid" }), /delivery hasn’t been confirmed/);
    assert(saved.keyPair.privateKey);
    assert.equal(saved.email, "alice@example.invalid");
    assert.equal(connection.transport, null);
});

test("enrollment persists a key, requests a code once, and uses the AP canonical account", async () => {
    let saved, requests = 0, finishes = 0;
    const token = new Uint8Array(72).fill(11), canonical = encodeId(new Uint8Array(16).fill(0x0b));
    const instance = utf8.encode(encodeId(new Uint8Array(16).fill(42)));
    const registration = (flags, token = new Uint8Array()) => ({ type: AP.REGISTER, flags,
        body: Uint8Array.from([token.length, token.length ? instance.length : 0, 0, 0, ...token, ...(token.length ? instance : [])]) });
    const state = scriptedTransport(bytes => {
        const frame = decodeFrame(bytes);
        assert(saved?.keyPair.privateKey, "key committed before contacting AP");
        if (frame.type === AP.HELLO) return [{ type: AP.HELLO, body: token }];
        assert.equal(frame.type, AP.REGISTER);
        if (frame.body[3] === 0) { requests++; return [registration(0)]; }
        finishes++;
        const start = 4 + new DataView(frame.body.buffer, frame.body.byteOffset).getUint16(0, true) + frame.body[2];
        assert.equal(frame.body[3], 6);
        assert.equal(new TextDecoder().decode(frame.body.subarray(start, start + 6)), "000123");
        return [registration(1, token)];
    });
    const credentialStore = async (key, value) => {
        assert.equal(key, "active");
        if (value) saved = value;
        return saved;
    };
    const connection = new ApConnection('public', { credentialStore, transportFactory: () => state.transport });
    assert.equal((await connection.requestCode({ email: " Bob@Example.com " })).email, "bob@example.com");
    assert.equal(saved.keyPair.privateKey.extractable, false);
    assert.equal(connection.transport, null, "the email wait does not retain AP's unauthenticated first stream");
    // Simulate a page/worker restart between receiving the code and entering it.
    const resumed = new ApConnection('public', { credentialStore, transportFactory: () => state.transport });
    const status = await resumed.finishCode({ code: "000123" });
    assert.equal(status.account, canonical);
    assert.equal(status.connected, true);
    assert.equal(requests, 1, "finishing must not replace the emailed code");
    assert.equal(finishes, 1);
    assert.equal(saved.authToken.length, 72);
    await assert.rejects(resumed.connect({ account: encodeId(new Uint8Array(16).fill(0xff)) }), /another account/);
    const reconnect = new ApConnection('public', { credentialStore, transportFactory: () => state.transport });
    const tabs = await Promise.all([reconnect.connect(), reconnect.connect({ account: canonical })]);
    assert.equal(tabs[0].connectionCount, 1);
    assert.equal(tabs[1].account, canonical);
    assert.equal(requests, 1);
});

test("enrollment rejects malformed tokens without persisting a false account", async () => {
    const connection = new ApConnection('public');
    assert.throws(() => connection.bindAccount(new Uint8Array(71)), /token/);
    assert.throws(() => connection.bindAccount(new Uint8Array(72), encodeId(new Uint8Array(16).fill(0xff))), /differs/);
    assert.equal(connection.account, null);
});

// A transport whose streams acknowledge a subscription, so a channel can be
// established and then given up.
function subscribingTransport() {
    const state = { closed: false, streams: [] };
    state.transport = {
        close() {
            state.closed = true;
        },
        createBidirectionalStream() {
            const stream = { cancelled: false, aborted: false };
            let controller;
            stream.readable = new ReadableStream({
                start(each) {
                    controller = each;
                },
                cancel() {
                    stream.cancelled = true;
                },
            });
            stream.writable = new WritableStream({
                write(chunk) {
                    const frame = decodeFrame(chunk);
                    if (frame.type === AP.SUBSCRIBE) {
                        controller.enqueue(encodeReply({ type: AP.SUBSCRIBE, destination: frame.destination }));
                    }
                },
                abort() {
                    stream.aborted = true;
                },
            });
            state.streams.push(stream);
            return Promise.resolve(stream);
        },
    };
    return state;
}

test("a channel is given up once nothing is listening to it", async () => {
    const state = subscribingTransport();
    const connection = connectedTo(state);
    const first = encodeId(new Uint8Array(32).fill(0xaa));
    const reader = () => {};
    const other = () => {};

    await connection.subscribe(first, reader);
    await connection.subscribe(first, other);

    // One of two listeners leaving keeps the subscription for the other.
    connection.detach(other);
    await settled();
    assert.equal(connection.channels.size, 1, "a channel someone still reads is kept");
    assert.equal(state.streams[0].cancelled, false);

    connection.detach(reader);
    await settled();
    assert.equal(connection.channels.size, 0, "the last listener leaving releases the channel");
    assert.equal(state.streams[0].cancelled, true, "its stream ends instead of being held");
    assert.equal(state.closed, false, "the account connection stays open");

    // The released slot is usable again, which is the point: a connection may
    // hold only sixteen channels and this worker outlives every tab.
    await connection.subscribe(encodeId(new Uint8Array(32).fill(0xbb)), reader);
    assert.equal(connection.channels.size, 1, "a later subscription still works");
});

test("invitation observation receives successive states and cancels its pending read", async () => {
    let incoming;
    const operation = { op: "watch_invitation", operation_id: encodeId(new Uint8Array(16).fill(0x11)), session_id: encodeId(new Uint8Array(32).fill(0x22)), proof: Array(64).fill(1) };
    const service = value => encodeReply({ type: AP.DIRECT, flags: value.operation_id ? SERVICE_PROTOCOL : 0, source: utf8.encode("service"),
        destination: new Uint8Array(16), body: utf8.encode(JSON.stringify(value)) });
    const connection = connectedTo({ transport: {
        async createBidirectionalStream() {
            return {
                readable: new ReadableStream({ start(controller) { incoming = controller; } }),
                writable: new WritableStream({ write() {
                    incoming.enqueue(service({ operation_id: operation.operation_id, timing: [], ok: true }));
                } }),
            };
        },
    } });
    const handle = await connection.watchInvitation(operation);
    await settled();
    assert.equal(connection.active, 1);
    const pending = connection.streamInvitation(handle);
    await assert.rejects(connection.streamInvitation(handle), /already in progress/);
    await settled();
    incoming.enqueue(service({ kind: "waiting_app" }));
    assert.equal((await pending).kind, "waiting_app");
    const changed = connection.streamInvitation(handle);
    incoming.enqueue(service({ kind: "ready", build_id: encodeId(new Uint8Array(32).fill(0x44)) }));
    assert.equal((await changed).kind, "ready");
    await settled();
    const stopped = assert.rejects(connection.streamInvitation(handle), /ended early|closed/i);
    connection.streamClose(handle);
    await stopped;
    assert.equal(connection.active, 0);
});

test("512 idle account/volume subscriptions share capacity and support checkpoints during reads", async () => {
    const peers = [];
    const connection = connectedTo({ transport: { async createBidirectionalStream() {
        const peer = { controls: [] }; peers.push(peer);
        return { readable: new ReadableStream({ start(controller) { peer.incoming = controller; } }),
            writable: new WritableStream({ write(bytes) {
                const frame = decodeFrame(bytes);
                if (frame.body[0] === 123) {
                    const operation = JSON.parse(new TextDecoder().decode(frame.body));
                    peer.incoming.enqueue(encodeReply({ type: AP.DIRECT, flags: SERVICE_PROTOCOL, source: utf8.encode("service"), destination: new Uint8Array(16),
                        body: utf8.encode(JSON.stringify({ operation_id: operation.operation_id, timing: [], ok: true,
                            data: { epoch: encodeId(new Uint8Array(16).fill(7)), sequence: 0, policy_revision: 1 } })) }));
                } else peer.controls.push(Array.from(frame.body));
            } }) };
    } } });
    const subscriptions = [];
    for (let i = 0; i < 512; i++) subscriptions.push(await connection.subscribeReplica({ op: i ? "subscribe_replica" : "subscribe_account", operation_id: encodeId(Buffer.from(i.toString(16).padStart(32, "0"), 'hex')) }));
    assert.equal(connection.active, 0); assert.equal(connection.replicaStreams, 512);
    await assert.rejects(connection.subscribeReplica({ op: "subscribe_replica" }), /capacity/);
    await assert.rejects(connection.subscribeReplica({ op: "subscribe_account" }), /capacity/);
    const ordinary = await connection.begin({ op: "read", operation_id: encodeId(new Uint8Array(16).fill(0xff)) });
    assert.equal(connection.active, 1); connection.streamClose(ordinary);
    const { handle, ack } = subscriptions[0]; assert.equal(ack.policy_revision, 1n);
    const pending = connection.streamReplica(handle);
    await assert.rejects(connection.streamReplica(handle), /already in progress/);
    await connection.checkpointReplica(handle);
    assert.deepEqual(peers[0].controls, [[1, 0]]);
    await settled();
    assert.equal(connection.replicaStreams, 512, "idle reads retain no ordinary response deadline");
    const event = new Uint8Array(52); event[0] = 1; event[1] = 2; event.fill(7, 4, 20);
    const view = new DataView(event.buffer); view.setBigUint64(20, 1n, true); view.setBigUint64(28, 1n, true);
    peers[0].incoming.enqueue(encodeReply({ type: AP.DIRECT, source: utf8.encode("service"), destination: new Uint8Array(16), body: event }));
    assert.deepEqual(await pending, event);
    const stopped = assert.rejects(connection.streamReplica(handle), /ended early|closed/);
    connection.streamClose(handle); await stopped;
    for (const { handle } of subscriptions) connection.streamClose(handle);
    assert.equal(connection.replicaStreams, 0); assert.equal(connection.active, 0);
});

test("replica publication keeps exact clocks and releases admission on malformed receipts", async () => {
    let bad = false, denied = false, payloads = 0;
    const connection = connectedTo({ transport: { async createBidirectionalStream() {
        let incoming, operation;
        return { readable: new ReadableStream({ start(controller) { incoming = controller; } }),
            writable: new WritableStream({ write(bytes) {
                const frame = decodeFrame(bytes);
                if (!operation) operation = JSON.parse(new TextDecoder().decode(frame.body));
                else { assert.deepEqual(frame.body, new Uint8Array([9])); payloads++; }
            }, close() {
                if (denied) {
                    incoming.enqueue(encodeReply({ type: AP.DIRECT, flags: SERVICE_PROTOCOL, source: utf8.encode("service"), destination: new Uint8Array(16),
                        body: utf8.encode(JSON.stringify({ operation_id: operation.operation_id, timing: [], ok: false, code: 'operation_failed',
                            denial: { cause: 'participant_revoked', account_id: connection.account, volume_id: connection.account, session_id: null, build_id: null } })) }));
                    return;
                }
                const data = bad ? '{"database_clock":-1,"policy_revision":1}' : '{"database_clock":18446744073709551615,"policy_revision":9007199254740993}';
                incoming.enqueue(encodeReply({ type: AP.DIRECT, flags: SERVICE_PROTOCOL, source: utf8.encode("service"), destination: new Uint8Array(16), body: utf8.encode(`{"operation_id":"${operation.operation_id}","timing":[],"ok":true,"data":${data}}`) }));
            } }) };
    } } });
    connection.description = Promise.resolve({ features: ['bloblog-append-v1'] });
    const operation = { op: "publish_replica", operation_id: encodeId(new Uint8Array(16).fill(0xaa)) };
    assert.deepEqual(await connection.publishReplica(operation, new Uint8Array([9])), { database_clock: (1n << 64n) - 1n, policy_revision: 9007199254740993n });
    assert.deepEqual(await connection.publishReplica({ ...operation, op: "publish_account" }, new Uint8Array([9])),
        { database_clock: (1n << 64n) - 1n, policy_revision: 9007199254740993n });
    assert.deepEqual(await connection.publishReplica({ ...operation, op: "publish_bloblog" }, new Uint8Array([9])),
        { database_clock: (1n << 64n) - 1n, policy_revision: 9007199254740993n });
    bad = true; await assert.rejects(connection.publishReplica(operation, new Uint8Array([9])), /replica integer/);
    assert.equal(payloads, 4); assert.equal(connection.active, 0); assert.equal(connection.streams.size, 0);
    const append = { ...operation, op: "publish_bloblog" };
    const generation = connection.connectionCount;
    connection.connectionCount++;
    await assert.rejects(connection.publishReplica(append, new Uint8Array([9]), generation), { code: 'bloblog-offline' });
    assert.equal(payloads, 4);
    denied = true;
    await assert.rejects(connection.publishReplica(append, new Uint8Array([9]), connection.connectionCount), { code: 'bloblog-denied', serviceCode: 'operation_failed' });
    assert.equal(payloads, 5); assert.equal(connection.active, 0); assert.equal(connection.streams.size, 0);
    await assert.rejects(connection.publishReplica({ ...operation, op: "publish_creation" }, new Uint8Array([9])), /Invalid replica publication/);
});

test("a durable publication receipt completes while the transport half-close remains pending", async () => {
    let incoming, operation, closed = false, finish;
    const connection = connectedTo({ timeout: 100, transport: { async createBidirectionalStream() {
        return { readable: new ReadableStream({ start(controller) { incoming = controller; } }),
            writable: new WritableStream({
                write(bytes) {
                    const frame = decodeFrame(bytes);
                    if (frame.body[0] === 123) operation = JSON.parse(new TextDecoder().decode(frame.body));
                },
                close() {
                    closed = true;
                    // AP has consumed FIN and durably committed. The web browser may
                    // still wait for a transport acknowledgment of its half-close.
                    incoming.enqueue(encodeReply({ type: AP.DIRECT, flags: SERVICE_PROTOCOL, source: utf8.encode("service"), destination: new Uint8Array(16),
                        body: utf8.encode(JSON.stringify({ operation_id: operation.operation_id, timing: [], ok: true,
                            data: { database_clock: 1, policy_revision: 1 } })) }));
                    return new Promise(resolve => { finish = resolve; });
                },
            }) };
    } } });
    try {
        const receipt = await connection.publishReplica({ op: "publish_replica", operation_id: encodeId(new Uint8Array(16).fill(0x99)) }, new Uint8Array([1]));
        assert(closed); assert.equal(receipt.database_clock, 1n);
        assert.equal(connection.active, 0); assert.equal(connection.streams.size, 0);
    } finally { finish?.(); }
});
test("initiating half-close without a durable receipt never reports a successful publication", async () => {
    let finish;
    const connection = connectedTo({ timeout: 10, transport: { async createBidirectionalStream() {
        return { readable: new ReadableStream(), writable: new WritableStream({
            close() { return new Promise(resolve => { finish = resolve; }); },
        }) };
    } } });
    try {
        await assert.rejects(connection.publishReplica({ op: "publish_replica", operation_id: encodeId(new Uint8Array(16).fill(0x88)) }, new Uint8Array([1])), /timed out/);
        assert.equal(connection.active, 0);
    } finally { finish?.(); }
});

test('a new email abandons the saved identity while a code resend retains its pending key', async () => {
    let saved = { email: 'prior@example.invalid', authToken: new Uint8Array(72), keyPair: { old: true } };
    const previous = saved.keyPair;
    const state = scriptedTransport(() => [{ type: AP.REGISTER, flags: 0, body: new Uint8Array(4) }]);
    const connection = new ApConnection('public', { credentialStore: async (_key, value) => value ? (saved = value) : saved,
        transportFactory: () => state.transport });
    connection.account = encodeId(new Uint8Array(16).fill(0xab));
    await connection.requestCode({ email: 'next@example.invalid' });
    assert.equal(saved.email, 'next@example.invalid'); assert.notEqual(saved.keyPair, previous);
    assert.equal(saved.authToken, undefined); assert.equal(connection.account, null);
    const pendingKey = saved.keyPair;
    await connection.requestCode({ email: 'next@example.invalid' });
    assert.equal(saved.keyPair, pendingKey);
});

test('failed transport setup is retryable but rejected credentials and AP operations are terminal', async () => {
    const token=new Uint8Array(72); token.fill(1);
    const identity={authToken:token,instanceId:new Uint8Array(16)};
    let attempts=0;
    const connection=new ApConnection('public', {credentialStore:async()=>identity,transportFactory:()=>{
        attempts++;
        return {ready:Promise.reject(new DOMException('Connection lost','NetworkError')),closed:new Promise(()=>{}),close(){}};
    }});
    for(let i=0;i<3;i++) await assert.rejects(connection.connect(),{code:'ap-transport'});
    assert.equal(attempts,3); assert.equal(connection.transport,null);
    connection.authenticationFrame=async()=>({type:AP.HELLO,flags:1});
    await assert.rejects(connection.connect(),{code:'authentication-required'});
    assert.throws(()=>connection.responseBody({operation:{operation_id:'id'}},{flags:SERVICE_PROTOCOL,body:utf8.encode(JSON.stringify({operation_id:'id',timing:[],ok:false,code:'operation_failed',error:'Participant revoked'}))}),{code:'ap-rejected',message:'Participant revoked'});
});

test('disconnect between readiness and operation admission stays retryable', async () => {
    const state=silentTransport(), connection=connectedTo(state);
    const pending=connection.begin({op:'status',operation_id:encodeId(new Uint8Array(16).fill(0x01))});
    connection.disconnected(state.transport);
    await assert.rejects(pending,{code:'ap-transport'});
    assert.equal(connection.active,0);
});

test("account key login signs one challenge and reports saved credentials without requesting email", async () => {
    const { formatAccountKey, accountSigningKey, loginMessage } = await import("./account-key.ts");
    const seed = new Uint8Array(16).fill(7), nonce = new Uint8Array(32).fill(9);
    const signing = await accountSigningKey(seed);
    const publicKey = await crypto.subtle.importKey("raw", signing.publicKey, "Ed25519", false, ["verify"]);
    let saved, request, writes = 0, streams = 0, incoming;
    const token = new Uint8Array(72).fill(11);
    const instance = utf8.encode(encodeId(new Uint8Array(16).fill(42)));
    const transport = {
        ready: Promise.resolve(), closed: new Promise(() => {}), close() {},
        async createBidirectionalStream() {
            streams++;
            return {
                readable: new ReadableStream({ start(controller) { incoming = controller; } }),
                writable: new WritableStream({ async write(bytes) {
                    const frame = decodeFrame(bytes); writes++;
                    assert(saved.keyPair.privateKey, "device identity is durable before authentication");
                    if (writes === 1) {
                        assert.equal(frame.type, AP.KEY_CHALLENGE); assert.equal(frame.flags, 0); request = frame.body;
                        incoming.enqueue(encodeReply({ type: AP.KEY_CHALLENGE, body: nonce }));
                    } else if (frame.type === AP.ONBOARDING) {
                        assert.equal(frame.body[0], 4);
                        incoming.enqueue(encodeFrame(frame)); incoming.close();
                    } else {
                        assert.equal(frame.type, AP.KEY_PROOF);
                        assert(await crypto.subtle.verify("Ed25519", publicKey, frame.body, loginMessage(nonce, request)));
                        incoming.enqueue(encodeReply({ type: AP.REGISTER, flags: 1,
                            body: Uint8Array.from([72, instance.length, 0, 0, ...token, ...instance]) }));
                    }
                } }),
            };
        },
    };
    const connection = new ApConnection('public', { credentialStore: async (_key, value) => value ? (saved = value) : saved, transportFactory: () => transport });
    const status = await connection.signInWithKey({ email: " Alice@Example.invalid ", accountKey: formatAccountKey(seed) });
    assert.equal(status.account, encodeId(new Uint8Array(16).fill(0x0b))); assert.equal(status.connected, true);
    assert.equal(saved.email, "alice@example.invalid"); assert.equal(saved.authToken.length, 72);
    await connection.onboardingTask;
    assert.equal(streams, 2); assert.equal(writes, 3);
    assert.equal(saved.onboardingAcknowledged, 1);
    assert.equal("accountKey" in saved, false);
});

test("email code admission rejects malformed and eight-digit input before any I/O", async () => {
    let calls = 0;
    const connection = new ApConnection('public', {
        credentialStore: async () => { calls++; throw new Error("unexpected storage"); },
        transportFactory: () => { calls++; throw new Error("unexpected transport"); },
    });
    for (const code of ["", "12345", "1234567", "12345678", "123–456", "123-456",
        "123 456", "１２３４５６", "١٢٣٤٥٦", "abcdef", "12345\n", "123456\n", " 123456"]) {
        await assert.rejects(connection.finishCode({ code }), /six-digit/);
    }
    assert.equal(calls, 0);
});

test("onboarding persists pending receipts and retries without losing out-of-order progress", async () => {
    const saved=[], sent=[];
    let fail=true;
    const identity={instanceId:new Uint8Array(16)};
    const transport={async createBidirectionalStream(){
        let incoming;
        return {readable:new ReadableStream({start(c){incoming=c;}}),writable:new WritableStream({write(bytes){const frame=decodeFrame(bytes);sent.push(frame.body[0]);if(fail)throw Error('offline');incoming.enqueue(encodeFrame(frame));}})};
    }};
    const connection=new ApConnection('public', {credentialStore:async (_key,value)=>{if(value)saved.push(structuredClone(value));return identity;}});
    connection.identity=identity;connection.account='a'.repeat(32);connection.ready=Promise.resolve();connection.transport=transport;
    await connection.onboarding(7);await connection.onboardingTask;
    assert.equal(identity.onboardingSeen,8);assert.equal(identity.onboardingAcknowledged,undefined);
    fail=false;
    await Promise.all([connection.onboarding(6),connection.onboarding(4),connection.onboarding(5)]);
    await connection.onboardingTask;
    assert.equal(identity.onboardingSeen,15);assert.equal(identity.onboardingAcknowledged,15);
    assert.deepEqual(sent,[7,4,5,6,7]);
    await connection.onboarding(4);await connection.onboardingTask;
    assert.equal(sent.length,5,"duplicate milestones perform no request");
    assert.equal(saved.at(-1).onboardingAcknowledged,15);
    await assert.rejects(connection.onboarding(9),/milestone/);
});

test("named channel receive credits span tabs and sessions until guest consumption", () => {
    const ap = new ApConnection('public');
    const frame = session => ({type: AP.PUBLISH, flags:0, destination:Uint8Array.from([...new Uint8Array(32).fill(session),1]), source:new Uint8Array(80), body:new Uint8Array(16000)});
    const held=[];
    for(let session=1;session<=4;session++)for(let message=0;message<4;message++)held.push(ap.retainChannel(frame(session)).receipt);
    assert.throws(()=>ap.retainChannel(frame(1)), /overflow/);
    assert.throws(()=>ap.retainChannel(frame(5)), /overflow/);
    ap.channelConsumed(held[0]);
    const next=ap.retainChannel(frame(5)).receipt;
    // Controls have an independent path even while every data scope is full.
    assert.equal(ap.retainChannel({type:AP.PUBLISH,flags:0,destination:new Uint8Array(32),source:new Uint8Array(),body:new Uint8Array(100)}).receipt,undefined);
    for(const receipt of [...held,next])ap.channelConsumed(receipt);
    assert.doesNotThrow(()=>ap.retainChannel(frame(1)));
});

test('channel admission fences ciphertext to the authenticated device before opening a stream', async () => {
    const f=silentTransport(), connection=new ApConnection('public');
    connection.ready=Promise.resolve();connection.transport=f.transport;
    connection.identity={instanceId:utf8.encode(encodeId(new Uint8Array(16).fill(1)))};
    await assert.rejects(connection.beginChannel({type:AP.PUBLISH,flags:1,destination:new Uint8Array(33),source:new Uint8Array(48),body:new Uint8Array(108)},new Uint8Array(16).fill(2),0),/authentication connection changed/);
    assert.equal(f.streams,0);assert.equal(connection.streams.size,0);
});

test('channel admission decodes the enrolled base58 instance identity', async () => {
    const device = new Uint8Array(16).fill(0xab), destination = new Uint8Array(32).fill(7);
    const connection = new ApConnection('public');
    connection.identity = { instanceId: utf8.encode(encodeId(device)) };
    connection.ready = Promise.resolve();
    let written = 0;
    connection.transport = { createBidirectionalStream: async () => ({
        writable: new WritableStream({ write() { written++; } }),
        readable: new ReadableStream({ start(controller) { controller.enqueue(encodeFrame({ type: AP.SUBSCRIBE, destination })); } }),
    }) };
    assert.deepEqual(connection.status().instanceId, Array.from(device));
    const handle = await connection.beginChannel({ type: AP.SUBSCRIBE, flags: 1, destination }, device, connection.connectionCount);
    assert.equal(written, 1);
    connection.streamClose(handle);
});

test('a late channel stream is closed without sending a retired publication',async()=>{
    const f=silentTransport({defer:true}),connection=connectedTo(f);
    const frame={type:AP.PUBLISH,flags:1,destination:new Uint8Array(33),source:new Uint8Array(48),body:new Uint8Array(108)};
    await assert.rejects(connection.beginChannel(frame,new Uint8Array(16),0),/timed out/);
    f.release();await settled();
    assert.equal(f.written,0);assert.equal(f.abandoned,1);assert.equal(connection.streams.size,0);
});

test('discovery is lazy, bounded, cached per connection and preserves optional metadata', async () => {
    const connection = new ApConnection('public');
    connection.ready = Promise.resolve(); connection.identity = { instanceId: utf8.encode('instance') };
    const description = {protocol:SERVICE_PROTOCOL,operations:['describe','get_credential_vault','future_operation'],features:[],
        algorithms:{kvdb_pull:'kvdb.pull.v1',kvdb_delta:'kvdb.delta.v1',replica_events:'ap.replica.v1'},
        limits:{frame_bytes:1048576,request_bytes:262144,required_features:16,feature_name_bytes:64},future_metadata:true};
    let requests = 0;
    const transport = { async createBidirectionalStream() {
        let incoming;
        return {readable:new ReadableStream({start(controller){incoming=controller;}}),writable:new WritableStream({write(bytes){
            requests++;
            const frame=decodeFrame(bytes), request=JSON.parse(new TextDecoder().decode(frame.body));
            incoming.enqueue(encodeFrame({type:AP.DIRECT,flags:SERVICE_PROTOCOL,source:utf8.encode('service'),destination:utf8.encode('instance'),
                body:utf8.encode(JSON.stringify({timing:[],operation_id:request.operation_id,ok:true,data:description,future_metadata:42}))}));
        }})};
    }};
    connection.transport=transport;
    assert.equal(requests,0);
    const [one,two]=await Promise.all([connection.describe(),connection.describe()]);
    assert.equal(one,two);assert.equal(requests,1);
    await assert.rejects(connection.begin({op:'upload_build',operation_id:operation.operation_id,required_features:['future.format.v1']}),/unavailable/);
    assert.equal(requests,1,'unsupported features open no mutation stream');
    connection.disconnected(transport);assert.equal(connection.description,null);
});

test('future failure codes and incompatible major never erase identity or grant retry', () => {
    const connection=connectedTo(silentTransport()), identity=connection.identity;
    assert.throws(()=>connection.responseBody({operation},{flags:SERVICE_PROTOCOL,body:utf8.encode(JSON.stringify({timing:[],operation_id:operation.operation_id,ok:false,code:'future_failure',error:'Unsupported'}))}),
        {code:'ap-rejected',serviceCode:'future_failure'});
    assert.equal(connection.identity,identity);
    assert.throws(()=>connection.responseBody({operation},{flags:SERVICE_PROTOCOL,body:utf8.encode(JSON.stringify({timing:[],operation_id:operation.operation_id,ok:false,code:'future_failure',error:'Unsupported',
        denial:{kind:'participant_revoked',account_id:'1111111111111111',volume_id:'1111111111111111',session_id:null,build_id:null}}))}),error=>{
            assert.equal(error.code,'ap-rejected');assert.equal(error.denial,undefined);return true;
        });
    for (const flags of [0, SERVICE_PROTOCOL + 1, 255])
        assert.throws(()=>connection.responseBody({operation},{flags,body:utf8.encode('not JSON')}),/compatible updates/);
    assert.equal(connection.identity,identity);
});

test('web client authenticates and performs baseline operations with the independent public peer', async () => {
    const {ReferenceAp,fixtureToken} = await import('../../../tests/ap-conformance.mjs');
    const peer = new ReferenceAp();
    const connection = new ApConnection('public');
    connection.transport = {ready:Promise.resolve(),async createBidirectionalStream(){
        let incoming;
        return {readable:new ReadableStream({start(controller){incoming=controller;}}),
            writable:new WritableStream({write(bytes){incoming.enqueue(peer.accept(Buffer.from(bytes)));}})};
    }};
    const response = await connection.authenticationFrame({type:AP.HELLO,body:fixtureToken});
    assert.equal(response.flags,0);assert.deepEqual(response.body,new Uint8Array(fixtureToken));
    connection.identity={instanceId:utf8.encode('1111111111111111')}; connection.ready=Promise.resolve();
    assert.equal((await connection.describe()).protocol,SERVICE_PROTOCOL);
    assert.equal((await connection.request({op:'get_credential_vault',operation_id:'1111111111111111'})).ok,true);
    await assert.rejects(connection.request({op:'future_operation',operation_id:'1111111111111111'}),{code:'ap-rejected',serviceCode:'unsupported_operation'});
    assert.equal((await connection.request({op:'get_credential_vault',operation_id:'1111111111111111'})).ok,true);
});

test("unknown optional timing preserves successful and failed service reply semantics", () => {
    const connection = new ApConnection('public');
    const operation_id = encodeId(new Uint8Array(16).fill(7));
    const state = { operation: { operation_id } };
    for (const ok of [false, true]) {
        const frame = { type: AP.DIRECT, flags: SERVICE_PROTOCOL, source: utf8.encode("service"),
            destination: new Uint8Array(), body: utf8.encode(JSON.stringify({ operation_id, ok,
                code: "operation_failed", error: "access denied", data: { receipt: 7 },
                timing: [{ stage: "ap.future_storage", duration_ns: "18446744073709551615" }] })) };
        if (ok) assert.deepEqual(connection.responseBody(state, frame).data, { receipt: 7 });
        else assert.throws(() => connection.responseBody(state, frame),
            error => error.serviceCode === "operation_failed" && error.message === "access denied");
    }
});
