import test from "node:test";
import assert from "node:assert/strict";
import { configureInstallation } from "./install.ts";

const artifact = { platform: "macos-arm64", filename: "Playbit-macos-arm64.zip", sha256: "ab".repeat(32),
    url: "https://blob.playbit.app/" + "1".repeat(32), sequence: 1, bytes: 3 };
const feed = (value, channel = "public") => new Response(JSON.stringify({ channel, artifact: value }));

test("installer selects the exact validated immutable artifact from its channel feed", async () => {
    for (const channel of ['public', 'testing']) {
    const container = { hidden: true }, link = { href: "" };
    assert.equal(await configureInstallation(container, link, channel, async (path, options) => {
        assert.equal(path, `https://playbit.app/download/macos${channel === 'testing' ? '-testing' : ''}.json`); assert.equal(options.credentials, "omit");
        assert.equal(options.redirect, "error"); assert.ok(options.signal instanceof AbortSignal);
        return feed(artifact, channel);
    }), true);
    assert.equal(container.hidden, false);
    assert.equal(link.href, artifact.url);
    }
});

test("unavailable, mismatched or malformed feeds cannot expose an installer link", async () => {
    for (const response of [new Response("", { status: 404 }), new Response("{"),
        new Response(JSON.stringify({ channel: "testing", artifact })),
        ...[{ filename: "../private" }, { url: "https://example.invalid/" + "1".repeat(32) },
            { url: "https://blob.playbit.app/1" }, { sequence: 0 }, { bytes: 256 * 1024 * 1024 + 1 }]
            .map(change => feed({ ...artifact, ...change }))]) {
        const missing = { hidden: false }, link = { href: artifact.url };
        assert.equal(await configureInstallation(missing, link, "public", async () => response), false);
        assert.equal(missing.hidden, true);
        assert.equal(link.href, "");
    }
});

test("oversized manifest streaming is cancelled within the metadata bound", async () => {
    let cancelled = false;
    const body = new ReadableStream({
        pull(controller) { controller.enqueue(new Uint8Array(65537)); },
        cancel() { cancelled = true; },
    });
    const container = { hidden: true }, link = { href: "" };
    assert.equal(await configureInstallation(container, link, "public", async () => new Response(body)), false);
    assert.equal(cancelled, true);
    assert.equal(container.hidden, true);
});
