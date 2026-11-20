import { encodeId } from './identifier.ts';
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { accountCipher, MAX_ACCOUNT_RECORD_BYTES } from "./account-record.ts";

const fixture = JSON.parse(await readFile(new URL("../../../tests/fixtures/account-record-v1.json", import.meta.url), "utf8"));
const unhex = value => Uint8Array.from(Buffer.from(value, "hex"));
const encode = value => new TextEncoder().encode(value);

test("web and native share encrypted account record bytes and authenticate account/key/clock", async () => {
    const secret = unhex(fixture.secret), key = unhex(fixture.key), clock = BigInt(fixture.clock);
    const cipher = await accountCipher(fixture.account, secret);
    const envelope = unhex(fixture.envelope);
    assert.deepEqual(await cipher.recordKey(encode(fixture.logical_key)), key);
    assert.deepEqual(await cipher.open(key, clock, envelope), encode(fixture.plaintext));
    const sealed = await cipher.seal(key, clock, encode(fixture.plaintext));
    assert.deepEqual(await cipher.open(key, clock, sealed), encode(fixture.plaintext));
    assert.notDeepEqual(sealed, await cipher.seal(key, clock, encode(fixture.plaintext)));
    await assert.rejects((await accountCipher(encodeId(new Uint8Array(16).fill(0xcd)), secret)).open(key, clock, envelope));
    await assert.rejects((await accountCipher(fixture.account, new Uint8Array(16))).open(key, clock, envelope));
    await assert.rejects(cipher.open(new Uint8Array(32), clock, envelope));
    await assert.rejects(cipher.open(key, clock + 1n, envelope));
    for (let index = 0; index < envelope.length; index++) {
        const damaged = envelope.slice(); damaged[index] ^= 1;
        await assert.rejects(cipher.open(key, clock, damaged));
        await assert.rejects(cipher.open(key, clock, envelope.subarray(0, index)));
    }
});

test("account record bounds and full-width kvdb clocks agree with native", async () => {
    const cipher = await accountCipher(fixture.account, unhex(fixture.secret));
    const key = unhex(fixture.key), plain = new Uint8Array(MAX_ACCOUNT_RECORD_BYTES).fill(7);
    const clock = 0xfffffffffffffffcn;
    const envelope = await cipher.seal(key, clock, plain);
    assert.deepEqual(await cipher.open(key, clock, envelope), plain);
    await assert.rejects(cipher.seal(key, clock, new Uint8Array(MAX_ACCOUNT_RECORD_BYTES + 1)));
    await assert.rejects(cipher.open(key, clock, new Uint8Array(MAX_ACCOUNT_RECORD_BYTES + 30)));
    for (const invalid of [0n, -1n, 0xfffffffffffffffdn, 0xfffffffffffffffen, 0xffffffffffffffffn, 0x10000000000000000n, 1, NaN]) {
        await assert.rejects(cipher.seal(key, invalid, plain));
        await assert.rejects(cipher.open(key, invalid, envelope));
    }
    for (const invalid of [new Uint8Array(), new Uint8Array(512)]) await assert.rejects(cipher.recordKey(invalid));
    await assert.rejects(cipher.seal(key, clock, new Uint8Array()));
    await assert.rejects(accountCipher(fixture.account.toUpperCase(), unhex(fixture.secret)));
    await assert.rejects(accountCipher(fixture.account, new Uint8Array(31)));
});
