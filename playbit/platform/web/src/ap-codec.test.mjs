import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AP, AGREEMENT, encodeFrame, decodeFrame, FrameReader, registrationFrame, registrationResponse, certificateRequest, utf8 } from "./ap-codec.ts";
import { encodeId } from "./identifier.ts";

test("public Rust frame golden vector and malformed inputs", () => {
    const frame = { type: 3, flags: 1, source: utf8.encode("sender"), destination: utf8.encode("recipes"), body: utf8.encode("hello") };
    const encoded = encodeFrame(frame);
    assert.equal(Buffer.from(encoded).toString("hex"), "250000000301060773656e64657272656369706573000000000000000000000068656c6c6f");
    assert.deepEqual(decodeFrame(encoded), frame);
    assert.throws(() => decodeFrame(encoded.subarray(1)));
    encoded[21] = 1;
    assert.throws(() => decodeFrame(encoded), /padding/);
    assert.throws(() => encodeFrame({ type: AP.DIRECT }), /destination/);
    assert.throws(() => encodeFrame({ type: 42 }), /type/);
    assert.throws(() => encodeFrame({ type: 9 }), /type/);
    assert.throws(() => encodeFrame({ type: AP.HELLO, body: new Uint8Array(1024 * 1024) }), /limit/);
});

test("stream reads preserve framing across arbitrary chunk boundaries", async () => {
    const a = encodeFrame({ type: AP.HELLO, body: utf8.encode("one") });
    const b = encodeFrame({ type: AP.HELLO, body: utf8.encode("two") });
    const combined = Uint8Array.from([...a, ...b]);
    const reader = new FrameReader(new ReadableStream({
        start(controller) {
            controller.enqueue(combined.subarray(0, 3));
            controller.enqueue(combined.subarray(3, 11));
            controller.enqueue(combined.subarray(11));
            controller.close();
        },
    }));
    assert.deepEqual((await reader.frame()).body, utf8.encode("one"));
    assert.deepEqual((await reader.frame()).body, utf8.encode("two"));
    await assert.rejects(reader.frame(), /early/);
    await reader.close();
});

test("registration fields use byte lengths and response bounds", () => {
    const request = registrationFrame("é@x", "Phone", "123", Uint8Array.of(1, 2));
    assert.deepEqual(Array.from(request.body.subarray(0, 4)), [4, 0, 5, 3]);
    assert.equal(request.flags, 0, "registration authenticates the account");
    const parsed = registrationResponse({ type: AP.REGISTER, flags: 1, body: Uint8Array.of(2, 1, 1, 0, 11, 12, 13, 14, 15) });
    assert.deepEqual(Array.from(parsed.authToken), [11, 12]);
    assert.deepEqual(Array.from(parsed.instanceId), [13]);
    assert.deepEqual(Array.from(parsed.caCertificate), [14]);
    assert.deepEqual(Array.from(parsed.instanceCertificate), [15]);
    assert.throws(() => registrationResponse({ type: AP.REGISTER, flags: 1, body: Uint8Array.of(255, 1, 1, 0) }), /Truncated/);
});

test("the agreement offered by the page identifies the exact archived bytes", () => {
    const source = new URL(`../../../assets/user-agreement-v${AGREEMENT.revision}.md`, import.meta.url);
    assert.equal(AGREEMENT.sha256, encodeId(createHash("sha256").update(readFileSync(source)).digest()));
});

test("WebCrypto PKCS10 is an independently verified Ed25519 CSR", async () => {
    const pair = await crypto.subtle.generateKey("Ed25519", false, ["sign", "verify"]);
    assert.equal(pair.privateKey.extractable, false);
    const csr = await certificateRequest(pair);
    const directory = mkdtempSync(join(tmpdir(), "playbit-web-csr-"));
    try {
        const path = join(directory, "request.der");
        writeFileSync(path, csr);
        const result = execFileSync("openssl", ["req", "-inform", "DER", "-in", path, "-noout", "-verify", "-text"], { encoding: "utf8" });
        assert.match(result, /ED25519/i);
    } finally { rmSync(directory, { recursive: true }); }
});
