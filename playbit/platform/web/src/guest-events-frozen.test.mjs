import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { guestEventCases } from "../tests/guest-event-fixture.mjs";

// Retained compiler output is independent of today's SDK and build directory.
const root = new URL("../../../tests/fixtures/guest-abi-rc2/", import.meta.url);
const raw = await readFile(new URL("manifest.json", root));
assert.equal(createHash("sha256").update(raw).digest("hex"),
    "7c90dad9e096db75d6c9a5e7df99d20eee95274db11ceef79d70a885bb0628e3");
const manifest = JSON.parse(raw);
assert.equal(manifest.status, "release_candidate");
const record = manifest.artifacts.find(record => record.file === "events-rust.wasm");
const bytes = await readFile(new URL(record.file, root));
assert.equal(bytes.length, record.bytes);
assert.equal(createHash("sha256").update(bytes).digest("hex"), record.sha256);
guestEventCases(await WebAssembly.compile(bytes), "retained v1 second release candidate SDK");
