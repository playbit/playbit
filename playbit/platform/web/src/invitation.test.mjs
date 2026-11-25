import { encodeId } from './identifier.ts';
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { parseInvitation, parseSessionLink, sessionCode, sessionLink, sessionUri, openEnvelope, retainedInvitation, validateInvitation, sessionProof, hex, unhex } from "./invitation.ts";

const vector = JSON.parse(await readFile(new URL("../../../tests/fixtures/session-link-v1.json", import.meta.url), "utf8"));

test("session codes are fixed-width base58 and every link form names the same seed", () => {
    const seed = unhex(vector.seed);
    assert.equal(sessionCode(seed), vector.code);
    assert.equal(sessionLink(seed, "public"), `https://playbit.app/s#${vector.code}`);
    assert.equal(sessionUri(seed, "public"), `playbit:session:public:${vector.code}`);
    for (const text of [sessionLink(seed, "public"), sessionUri(seed, "public"), `session:${vector.code}`, vector.code]) assert.equal(hex(parseSessionLink(text, "public")), vector.seed);
    for (const bytes of [new Uint8Array(16), new Uint8Array(16).fill(255), new Uint8Array(16).fill(7)]) {
        assert.equal(sessionCode(bytes).length, 22);
        assert.equal(hex(parseSessionLink(sessionCode(bytes), "public")), hex(bytes));
    }
    assert.equal(sessionCode(new Uint8Array(16)), "1".repeat(22));
    for (const invalid of ["", "0".repeat(22), "1".repeat(21), "z".repeat(22), `https://other.example/s#${vector.code}`, "x".repeat(300)]) {
        assert.throws(() => parseSessionLink(invalid, "public"), /incomplete/, invalid);
    }
});

test("web crypto derives the native session identity, proof and envelope", async () => {
    const parsed = await parseInvitation(sessionLink(unhex(vector.seed), "public"), "public");
    assert.equal(parsed.session_id, vector.session_id);
    assert.equal(parsed.invitation, null);
    assert.equal(hex(await sessionProof(parsed.privateKey, vector.account)), vector.proof);
    const invitation = await openEnvelope(parsed, vector.envelope);
    assert.deepEqual(invitation, { seed: Array.from(unhex(vector.seed)), ...JSON.parse(vector.plaintext) });
    const retained = await retainedInvitation(invitation, "public");
    assert.equal(retained.url, parsed.url);
    assert.deepEqual(retained.invitation, invitation);
    // Another seed's keys cannot open it, and any damage is rejected.
    const other = await parseInvitation(sessionCode(new Uint8Array(16).fill(9)), "public");
    await assert.rejects(openEnvelope(other, vector.envelope), /does not belong/);
    const bytes = Buffer.from(vector.envelope, "base64url");
    for (const index of [0, 1, 13, bytes.length - 1]) {
        const corrupt = Buffer.from(bytes); corrupt[index] ^= 1;
        await assert.rejects(openEnvelope(parsed, corrupt.toString("base64url")));
    }
    await assert.rejects(openEnvelope(parsed, bytes.subarray(0, 20).toString("base64url")), /damaged/);
    await assert.rejects(openEnvelope(parsed, ""), /damaged/);
});

test("a personal session link with a trailing slash stays in its own environment", async () => {
    const link = `https://julia.testing.playbit.app/s/#${vector.code}`;
    const parsed = await parseInvitation(link, 'testing-julia');
    assert.equal(parsed.url, `https://julia.testing.playbit.app/s#${vector.code}`);
    assert.equal(parsed.uri, null);
    assert.throws(() => parseSessionLink(link, 'testing'), /belongs/);
});

test("invitation validation rejects cross-bound identities and substituted seeds", async () => {
    const invitation = { seed: Array.from(unhex(vector.seed)), ...JSON.parse(vector.plaintext) };
    validateInvitation(invitation);
    for (const edit of [
        value => value.seed = value.seed.slice(1),
        value => value.session.root_volume = encodeId(new Uint8Array(16).fill(0x12)),
        value => value.root_credentials.blob_key = value.root_credentials.database_key,
        value => value.session.selector.build_id = encodeId(new Uint8Array(32).fill(0x12)),
        value => value.extra = true,
        value => delete value.root,
    ]) {
        const changed = structuredClone(invitation); edit(changed);
        assert.throws(() => validateInvitation(changed), /invalid/);
    }
    const foreign = structuredClone(invitation); foreign.seed[0] ^= 1;
    await assert.rejects(retainedInvitation(foreign, "public"), /does not match/);
});
