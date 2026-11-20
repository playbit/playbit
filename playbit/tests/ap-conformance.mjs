// Small independent AP fixture. Run: node playbit/tests/ap-conformance.mjs
// Uses only the published wire contract and Node built-ins, never service/ or
// Playbit codecs. The caller supplies verified transport; this in-memory peer
// tests token authentication and operation admission, not TLS or enrollment.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { createHmac, timingSafeEqual } from 'node:crypto';

const vectors = JSON.parse(readFileSync(new URL('./fixtures/ap-baseline.json', import.meta.url)));
// Fixed fixture clock and public test-only signing key keep vectors deterministic.
const nowSeconds = 1793127600;
const secret = Buffer.alloc(32, 0x55);
const payload = Buffer.alloc(40);
payload.fill(7, 0, 16); payload.writeBigUInt64LE(BigInt(nowSeconds + 30 * 86400), 32);
export const fixtureToken = Buffer.concat([payload, createHmac('sha256', secret).update(payload).digest()]);
const token = fixtureToken;
const instance = '1111111111111111';
const description = { protocol: 1, operations: ['describe', 'get_credential_vault'], features: [],
    algorithms: { kvdb_pull: 'kvdb.pull.v1', kvdb_delta: 'kvdb.delta.v1', replica_events: 'ap.replica.v1' },
    limits: { frame_bytes: 1048576, request_bytes: 262144, required_features: 16, feature_name_bytes: 64 } };

export function encode(type, flags, source, destination, body) {
    source = Buffer.from(source); destination = Buffer.from(destination); body = Buffer.from(body);
    const offset = (8 + source.length + destination.length + 15) & ~15;
    assert(source.length <= 255 && destination.length <= 255 && offset + body.length <= 1048576);
    const bytes = Buffer.alloc(offset + body.length);
    bytes.writeUInt32LE(bytes.length); bytes.set([type, flags, source.length, destination.length], 4);
    bytes.set(source, 8); bytes.set(destination, 8 + source.length); bytes.set(body, offset);
    return bytes;
}
export function decode(bytes) {
    assert(bytes.length >= 16 && bytes.length <= 1048576 && bytes.readUInt32LE() === bytes.length);
    const names = 8 + bytes[6] + bytes[7], offset = (names + 15) & ~15;
    assert(offset <= bytes.length && bytes.subarray(names, offset).every(value => value === 0));
    return { type: bytes[4], flags: bytes[5], source: bytes.subarray(8, 8 + bytes[6]).toString(),
        destination: bytes.subarray(8 + bytes[6], names).toString(), body: bytes.subarray(offset) };
}

// Keep operation values raw until the bounded support envelope is admitted.
// A flat scanner avoids constructing a tree for an unsupported operation body.
function fields(source) {
    let index = 0;
    const whitespace = () => { while (/\s/.test(source[index] ?? '') && index < source.length) index++; };
    const stringEnd = () => {
        assert(source[index++] === '"');
        while (index < source.length) {
            const byte = source[index++];
            if (byte === '\\') index++;
            else if (byte === '"') return index;
        }
        throw Error('unterminated JSON string');
    };
    whitespace(); assert(source[index++] === '{'); whitespace();
    const result = new Map();
    while (source[index] !== '}') {
        const begin = index, end = stringEnd(), key = JSON.parse(source.slice(begin, end));
        assert(key.length <= 64 && result.size < 64 && !result.has(key));
        whitespace(); assert(source[index++] === ':'); whitespace();
        const start = index, stack = [];
        while (index < source.length) {
            const byte = source[index];
            if (byte === '"') { stringEnd(); continue; }
            if (!stack.length && (byte === ',' || byte === '}')) break;
            if (byte === '{' || byte === '[') stack.push(byte);
            if (byte === '}' || byte === ']') assert(stack.pop() === (byte === '}' ? '{' : '['));
            assert(stack.length <= 128); index++;
        }
        assert(!stack.length && index > start);
        result.set(key, source.slice(start, index)); whitespace();
        if (source[index] !== ',') break;
        index++; whitespace();
    }
    assert(source[index++] === '}'); whitespace(); assert(index === source.length);
    return result;
}

function validIdentity(id) {
    const alphabet = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';
    if (typeof id !== 'string' || !id.length || id.length > 22) return false;
    let value = 0n;
    for (const byte of id) {
        const digit = alphabet.indexOf(byte); if (digit < 0) return false;
        value = value * 58n + BigInt(digit);
    }
    let bytes = 0; while (value) { bytes++; value >>= 8n; }
    return bytes + (id.match(/^1*/)[0].length) === 16;
}

export class ReferenceAp {
    authenticated = false;
    accept(bytes) {
        const frame = decode(bytes);
        if (frame.type === 0) {
            assert(frame.flags === 0 && !frame.source && !frame.destination);
            this.authenticated = frame.body.length === 72
                && frame.body.subarray(0, 32).equals(token.subarray(0, 32))
                && frame.body.readBigUInt64LE(32) > BigInt(nowSeconds)
                && timingSafeEqual(frame.body.subarray(40), createHmac('sha256', secret).update(frame.body.subarray(0, 40)).digest());
            // Mandatory binary auth timing envelope precedes the renewed 72-byte token.
            return encode(0, this.authenticated ? 0 : 2, '', '', Buffer.concat([Buffer.from([2, 0, 91, 93]), this.authenticated ? token : Buffer.alloc(0)]));
        }
        assert(this.authenticated, 'authenticate before opening operation streams');
        assert(frame.type === 2 && frame.destination === 'service');
        if (frame.flags !== 1) return encode(200, 4, 'service', instance, 'Incompatible AP protocol');
        assert(frame.body.length <= 262144);
        const envelope = fields(frame.body.toString('utf8'));
        const id = JSON.parse(envelope.get('operation_id')), op = JSON.parse(envelope.get('op'));
        assert(validIdentity(id) && /^[a-z0-9_]{1,64}$/.test(op));
        const reply = (code, data = null) => encode(2, 1, 'service', instance, JSON.stringify({
            timing: [], operation_id: id, ok: code === null, code, error: code, denial: null, data, future_metadata: true }));
        // The fixture implements a small baseline subset; unknown ops are correlated.
        if (!['describe', 'get_credential_vault', 'upload_build'].includes(op)) return reply('unsupported_operation');
        const required = JSON.parse(envelope.get('required_features') ?? '[]');
        assert(Array.isArray(required) && required.length <= 16 && new Set(required).size === required.length
            && required.every(name => typeof name === 'string' && /^[a-z0-9._-]{1,64}$/.test(name)));
        if (required.length) return reply('unsupported_feature');
        if (op === 'upload_build' || [...envelope.keys()].some(key => !['op', 'operation_id', 'required_features'].includes(key)))
            return reply('invalid_request');
        return reply(null, op === 'describe' ? description : null);
    }
}

export function runConformance() {
    assert.equal(vectors.protocol, 1);
    const peer = new ReferenceAp();
    const hello = encode(0, 0, '', '', token), auth = decode(peer.accept(hello));
    assert.equal(hello.toString("hex"), vectors.reference_wire.hello_request);
    assert.equal(peer.accept(hello).toString("hex"), vectors.reference_wire.hello_response);
    assert.equal(auth.flags, 0); assert.deepEqual(auth.body, Buffer.concat([Buffer.from([2, 0, 91, 93]), token]));
    const refused = new ReferenceAp(); assert.equal(decode(refused.accept(encode(0, 0, '', '', Buffer.alloc(32)))).flags, 2);
    for (const vector of vectors.cases) {
        const request = encode(2, vector.major, instance, 'service', vector.request);
        if (vector.expected === 'invalid_envelope') { assert.throws(() => peer.accept(request)); continue; }
        const response = decode(peer.accept(request));
        if (vector.expected === 'incompatible_protocol') { assert.equal(response.type, 200); assert.equal(response.flags, 4); continue; }
        const value = JSON.parse(response.body);
        assert.equal(value.operation_id, vectors.operation_id);
        assert.equal(value.code, vector.expected === 'accepted' ? null : vector.expected, vector.name);
    }
    // Failed operations never revoke the authenticated connection.
    assert.equal(JSON.parse(decode(peer.accept(encode(2, 1, instance, 'service', vectors.cases[0].request))).body).ok, true);
    console.log(`AP public conformance: authentication and ${vectors.cases.length} baseline/admission vectors passed`);
}
if (import.meta.url === pathToFileURL(process.argv[1]).href) runConformance();
