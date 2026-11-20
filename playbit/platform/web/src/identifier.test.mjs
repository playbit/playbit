import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { encodeId, decodeId, validId, compareId } from './identifier.ts';
const vectors = JSON.parse(await readFile(new URL('../../../tests/fixtures/identifiers.json', import.meta.url)));
test('identifier text agrees with native vectors at every leading-zero boundary', () => {
    for (const {bytes, text} of vectors) {
        assert.equal(encodeId(Uint8Array.from(bytes)), text);
        assert.deepEqual(Array.from(decodeId(text, bytes.length)), bytes);
        assert(!validId('1' + text, bytes.length));
    }
    for (const text of ['', '0', 'O', 'I', 'l', 'é', '1'.repeat(17), 'z'.repeat(22), 'ab'.repeat(16)]) assert(!validId(text));
    for (const bytes of [{length:-1}, {length:0.5}, [], [256], [-1], [0.5], new Uint8Array(33)]) assert.throws(() => encodeId(bytes));
});

test('catalog ordering compares decoded identities across base58 digit boundaries', () => {
    const a = new Uint8Array(16), b = new Uint8Array(16);
    a[15] = 57; b[15] = 58;
    const first = encodeId(a), second = encodeId(b);
    assert(first > second);
    assert(compareId(first, second) < 0);
    assert(compareId(second, first) > 0);
    assert.equal(compareId(first, first), 0);
});
