import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { encodePersonalRecord, decodePersonalRecord } from './account-format.ts';
const fixture = JSON.parse(await readFile(new URL('../../../tests/fixtures/account-plaintext-v1.json', import.meta.url)));
const plain = Uint8Array.from(Buffer.from(fixture.personal_hex, 'hex'));
const grants = [{ volume: fixture.volume, write: false, incarnation: fixture.incarnation }];
function extension(value, tag, body) {
    const header = new Uint8Array(6), view = new DataView(header.buffer);
    view.setUint16(0, tag, true); view.setUint32(2, body.length, true);
    return Uint8Array.from([...value, ...header, ...body]);
}
test('web personal decisions match the independent native plaintext vector', () => {
    assert.deepEqual(encodePersonalRecord(fixture.session, grants), plain);
    assert.deepEqual(decodePersonalRecord(plain).grants, grants);
    assert.deepEqual(encodePersonalRecord(fixture.session, grants, plain), plain);
    for (let size = 0; size < plain.length; size++) assert.throws(() => decodePersonalRecord(plain.subarray(0, size)));
});
test('known web edits preserve optional header bytes and refuse required semantics', () => {
    const optional = extension(plain, 99, [255, 7, 0, 9]);
    const changed = encodePersonalRecord(fixture.session, [], optional);
    assert.deepEqual(changed.subarray(-10), optional.subarray(-10));
    assert.deepEqual(decodePersonalRecord(changed).grants, []);
    for (const flags of [0x4000, 0x8000, 0xc000]) {
        assert.throws(() => encodePersonalRecord(fixture.session, [], extension(plain, 99 | flags, [255])), /newer version/);
    }
    assert.throws(() => decodePersonalRecord(extension(plain, 99, new Uint8Array(4091))), /capacity/);
    let crowded = plain;
    for (let tag = 7; tag <= 65; tag++) crowded = extension(crowded, tag, [255]);
    assert.throws(() => decodePersonalRecord(crowded), /Invalid/);
    const nested = Uint8Array.from(Buffer.from(fixture.personal_nested_optional_hex, 'hex'));
    assert.deepEqual(encodePersonalRecord(fixture.session, grants, nested), nested);
    assert.throws(() => encodePersonalRecord(fixture.session, [], nested), /Cannot preserve/);
});
