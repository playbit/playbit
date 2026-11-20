import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { formatAccountKey, parseAccountKey, backupKey, accountSigningKey, loginMessage, backupProof } from './account-key.ts';
const v = JSON.parse(await readFile(new URL('../../../tests/fixtures/account-key-v1.json', import.meta.url)));
const bytes = value => Uint8Array.from(Buffer.from(value, 'hex'));
const hex = value => Buffer.from(value).toString('hex');
test('account key encoding, HKDF and signatures match native vectors', async () => {
    const seed=bytes(v.seed);
    assert.equal(formatAccountKey(seed), v.text);
    assert.deepEqual(parseAccountKey(v.text), seed);
    assert.equal(hex(await backupKey(v.account,seed)), v.backup_key);
    const {privateKey, publicKey}=await accountSigningKey(seed);
    assert.equal(hex(publicKey),v.public_key);
    assert.equal(hex(await crypto.subtle.sign('Ed25519',privateKey,loginMessage(bytes(v.nonce),bytes(v.request)))),v.login_signature);
    assert.deepEqual(await backupProof(v.account,v.operation,v.revision,v.ciphertext,seed),{public_key:v.public_key,signature:v.backup_signature});
    for (const value of [new Uint8Array(16),new Uint8Array(16).fill(255)]) {
        assert.equal(formatAccountKey(value).length,23);
        assert.deepEqual(parseAccountKey(' '+formatAccountKey(value)+'\n'),value);
    }
    for (const text of ['', 'a', 'b'+'1'.repeat(22), 'a'+'z'.repeat(22), 'a'+'0'.repeat(22), 'a'+'1'.repeat(21), 'a'+'1'.repeat(23)]) assert.throws(()=>parseAccountKey(text));
});
