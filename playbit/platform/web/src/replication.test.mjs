import { encodeId } from './identifier.ts';
import { databaseBytes } from "./database-id.ts";
const named = encodeId(new Uint8Array(15).fill(0x09));
import test from 'node:test';
import assert from 'node:assert/strict';
import { replicaInteger, snapshotInteger, bloblogInteger, replicaReceipt, replicaJSON, subscriptionAck, replicaEvent, ReplicaCursor } from './replication.ts';
import { bloblogPage } from './bloblog.ts';
const ack = () => ({ epoch: encodeId(new Uint8Array(16).fill(7)), sequence: 0n, policy_revision: 1n });
test('append-log source clocks round trip without losing uint64 digits', () => {
    const volume = encodeId(new Uint8Array(16).fill(0x11)), log = encodeId(new Uint8Array(15).fill(0x22)), cursor = `1/${volume}/${log}/11111111/0/${volume}`;
    const input = `{"page":{"entries":[{"clock":9007199254740993}],"cursor":"${cursor}","more":false},"database_clock":18446744073709551615,"policy_revision":7,"source":{"generation":"${volume}","epoch":9007199254740995}}`;
    const parsed = JSON.parse(input, bloblogInteger), received = bloblogPage(parsed, volume, log);
    assert.equal(received.page.entries[0].clock, 9007199254740993n);
    assert.equal(received.receipt.revision, (1n << 64n) - 1n);
    assert.deepEqual(JSON.parse(new TextDecoder().decode(received.bytes), bloblogInteger), parsed);
    assert.throws(() => bloblogPage(parsed, encodeId(new Uint8Array(16).fill(0x33)), log), /Invalid/);
    assert.throws(() => bloblogPage({ ...parsed, ignored: true }, volume, log), /Invalid/);
    for (const number of ['-1','1.1','1e3','18446744073709551616','"3"','null']) {
        assert.throws(() => JSON.parse(`{"clock":${number}}`, bloblogInteger));
    }
});
// Parse the actual uint64 wire spelling, then mutate its source fence and shape.
test('snapshot receipts preserve every epoch digit through the Rust boundary and reject invalid wire shapes', () => {
    const receipt = replicaReceipt(JSON.parse('{"namespace":{"status":"unavailable"},"root":null,"revision":9007199254740993,"policy_revision":7,"source":{"generation":"1111111111111112","epoch":18446744073709551615}}', snapshotInteger));
    assert.equal(receipt.revision,9007199254740993n);
    assert.equal(receipt.source.epoch,(1n<<64n)-1n);
    assert.deepEqual(JSON.parse(replicaJSON(receipt), snapshotInteger), receipt);
    for (const value of [
        {...receipt,source:undefined}, {...receipt,source:{generation:encodeId(new Uint8Array(16).fill(0x00)),epoch:0n}},
        {...receipt,source:{...receipt.source,epoch:1}}, {...receipt,source:{...receipt.source,extra:true}},
        {...receipt,root:[0,[1]]}, {...receipt,extra:true},
    ]) assert.throws(()=>replicaReceipt(value));
    for (const value of ['-1','1.1','1e3','18446744073709551616','"3"','null']) {
        assert.throws(()=>JSON.parse(`{"epoch":${value}}`,snapshotInteger));
    }
});
function event(kind = 1, sequence = 1n, revision = 1n, body = new Uint8Array([9])) {
    const bytes = new Uint8Array(52 + body.length), view = new DataView(bytes.buffer);
    bytes[0] = 1; bytes[1] = kind; bytes.fill(7, 4, 20);
    view.setBigUint64(20, sequence, true); view.setBigUint64(28, revision, true);
    bytes.set(databaseBytes(kind === 1 || kind === 6 || kind === 7 ? named : "index"), 36); bytes.set(body, 52); return bytes;
}
test('replica control clocks preserve every uint64 digit', () => {
    const parsed = JSON.parse('{"database_clock":18446744073709551615,"policy_revision":9007199254740993,"sequence":0}', replicaInteger);
    assert.equal(parsed.database_clock, (1n << 64n) - 1n); assert.equal(parsed.policy_revision, 9007199254740993n);
    for (const value of ['-1', '1.1', '1e3', '18446744073709551616', '"3"', 'null']) {
        assert.throws(() => JSON.parse(`{"database_clock":${value}}`, replicaInteger));
    }
    assert.throws(() => replicaInteger('sequence', 1), /Invalid/);
});
test('replica cursor advances after durable application and distinguishes duplicates and gaps', () => {
    const cursor = new ReplicaCursor(ack()), first = replicaEvent(event());
    assert(cursor.accept(first)); assert.equal(cursor.sequence, 0n);
    cursor.applied(first); assert.equal(cursor.accept(first), false);
    assert.throws(() => cursor.accept(replicaEvent(event(1, 3n))), /sequence gap/);
    const wrong = event(1, 2n); wrong[4]++;
    assert.throws(() => cursor.accept(replicaEvent(wrong)), /epoch changed/);
    const second = replicaEvent(event(1, 2n, 2n)); cursor.applied(second);
    assert.throws(() => cursor.accept(replicaEvent(event(1, 3n, 1n))), /revision regressed/);
    assert(cursor.accept(replicaEvent(event(3, 99n, 0n, new Uint8Array()))));
    const terminal = new Uint8Array(17).fill(7); terminal[16] = 1;
    assert(cursor.accept(replicaEvent(event(4, 99n, 0n, terminal))));
});
test('replica envelopes reject malformed identities, payloads and replaced creation tags', () => {
    for (const mutate of [b => { b[0] = 2; }, b => { b[2] = 1; }, b => b.fill(0,4,20), b => b.fill(0,20,28), b => b.fill(0,28,36), b => { b[36] = 3; }]) {
        const bytes = event(); mutate(bytes); assert.throws(() => replicaEvent(bytes));
    }
    assert.throws(() => replicaEvent(event(1, 1n, 1n, new Uint8Array(65537))));
    assert.throws(() => replicaEvent(event(2)));
    assert.throws(() => replicaEvent(event(5, 1n, 1n, new Uint8Array(6))));
    for (const bad of [{ ...ack(), sequence: 1n }, { ...ack(), epoch: encodeId(new Uint8Array(16).fill(0)) }, { ...ack(), extra: true }]) assert.throws(() => subscriptionAck(bad));
});

test('bloblog inline and hint events share volume framing bounds', () => {
    for (const body of [new Uint8Array(), new Uint8Array(2048)]) {
        const value = replicaEvent(event(6,1n,1n,body));
        assert.equal(value.kind,6);
        assert.equal(value.delta.length,body.length);
    }
    assert.throws(() => replicaEvent(event(6,1n,1n,new Uint8Array(2049))));
    assert.throws(() => replicaEvent(event(6,1n,0n,new Uint8Array())));
});

test('authority terminals require an explicit volume and a known cause', () => {
    for (const cause of [1, 2, 3, 4]) {
        const body = new Uint8Array(17).fill(7); body[16] = cause;
        assert.deepEqual(replicaEvent(event(4, 99n, 0n, body)).terminal,
            { volume: encodeId(new Uint8Array(16).fill(0x07)), cause });
    }
    for (const body of [new Uint8Array(), new Uint8Array(16), new Uint8Array(18),
        Uint8Array.from([...Array(16).fill(0), 1]), Uint8Array.from([...Array(16).fill(7), 0]),
        Uint8Array.from([...Array(16).fill(7), 5])]) {
        assert.throws(() => replicaEvent(event(4, 99n, 0n, body)));
    }
});

// The web browser forwards evidence to Rust; one bounded envelope binds its scope.
test('selected namespace events split source and evidence without copying', () => {
    const body = new Uint8Array(4 + 3 + 16);
    new DataView(body.buffer).setUint32(0, 3, true);
    body.set([1, 2, 3], 4); body.fill(9, 7, 22); body[22] = 1;
    const bytes = event(7, 1n, 1n, body), decoded = replicaEvent(bytes);
    assert.equal(decoded.database, 'index');
    assert.equal(decoded.namespace.entry, named);
    assert.deepEqual(decoded.delta, new Uint8Array([1, 2, 3]));
    assert.equal(decoded.namespace.evidence.length, 16);
    assert.equal(decoded.delta.buffer, bytes.buffer);
    assert.equal(decoded.namespace.evidence.buffer, bytes.buffer);
    const invalid = bytes.slice(); new DataView(invalid.buffer).setUint32(52, 0xffffffff, true);
    assert.throws(() => replicaEvent(invalid));
    assert.throws(() => replicaEvent(event(7, 1n, 1n, body.subarray(0, 22))));
});
