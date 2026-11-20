import { encodeId } from './identifier.ts';
import test from "node:test";
import assert from "node:assert/strict";
import { validateSelection } from "./personal.ts";
const volume = encodeId(new Uint8Array(16).fill(0x11));
test("selection only admits known eligible volumes and explicit rights", () => {
    const volumes = [{ volume: { id: volume } }];
    assert.deepEqual(validateSelection([{ volume, write: false }], volumes), [{ volume, write: false }]);
    assert.deepEqual(validateSelection([], volumes), []);
    assert.throws(() => validateSelection([{ volume: encodeId(new Uint8Array(16).fill(0x44)), write: true }], volumes), /Invalid/);
    assert.throws(() => validateSelection([{ volume, write: "read" }], volumes), /Invalid/);
    assert.throws(() => validateSelection([{ volume, write: false }, { volume, write: true }], volumes), /Invalid/);
});

test('personal grants fit the 30 web personal-volume slots', () => {
    const volumes = Array.from({ length: 31 }, (_, index) => ({ volume: { id: encodeId(Buffer.from(index.toString(16).padStart(32,'0'), 'hex')) } }));
    const grants = volumes.map(item => ({ volume: item.volume.id, write: false }));
    assert.equal(validateSelection(grants.slice(0,30),volumes).length,30);
    assert.throws(() => validateSelection(grants,volumes),/capacity exceeded/);
});
