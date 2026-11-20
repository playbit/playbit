import { compatibleWorkerStorage } from '../tests/worker-storage-fixture.mjs';
import { MAX_TEXT_EVENT_BYTES } from './text-input.ts';
import { workerChannel } from './release-channel.ts';
import { encodeId, decodeId, validId } from './identifier.ts';
import { FixtureWork } from "../tests/work-fixture.mjs";
import { guestOperations } from "./guest-operations.ts";
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import { AccountSignal } from "./account-notices.ts";
import { corruptStoreOpenMessage } from "./errors.ts";
import { prepareGuest } from "./updates.ts";
import { decodeScene, validateInput } from "./scene.ts";

// These lifecycle fixtures provision no storage. Metadata snapshot I/O is
// covered by the actual-host adapter tests; keep it outside these mock turns.
function lifecycleOperations(services) {
    const operations = guestOperations(services);
    operations.refreshVolumes = () => {};
    return operations;
}

const source = stripTypeScriptTypes(await readFile(new URL("./runtime-worker.ts", import.meta.url), "utf8"), { mode: "strip" });
// Build this public fixture explicitly; the web bundle includes only TODO and Micro Kart.
execFileSync("cargo", ["build", "--release", "--target", "wasm32-unknown-unknown", "-p", "ui-gallery"], {
    cwd: fileURLToPath(new URL("../../../../", import.meta.url)), stdio: "inherit",
});
const gallery = new Uint8Array(await readFile(new URL("../../../../target/wasm32-unknown-unknown/release/ui_gallery.wasm", import.meta.url)));
async function worker(wasm = gallery, rejectPresentation = false) {
    const messages = [];
    const context = vm.createContext({ ...compatibleWorkerStorage(), MAX_TEXT_EVENT_BYTES, workerChannel, encodeId, decodeId, validId, guestOperations: lifecycleOperations, GuestWork: FixtureWork, AccountSignal, corruptStoreOpenMessage, observeAccountMetadata() {}, TextEncoder, TextDecoder, WebAssembly, crypto, prepareGuest, validateInput,
        decodeScene(bytes) {
            if (!rejectPresentation) return decodeScene(bytes);
            // Exercise decoder failure inside a real guest's presentation syscall,
            // independently of which controls the gallery currently demonstrates.
            const unsupported = bytes.slice();
            unsupported[4] = 11;
            return decodeScene(unsupported);
        },
        self: { location: { href: "https://playbit.app/runtime-worker.js?channel=public" }, postMessage: message => messages.push(message) }, wasm,
    });
    vm.runInContext(source.replace(/^import .*;$/gm, "") + `
        files = { cleanup: async () => {}, flushPublication() {} };
        retireFiles = async () => {};
        host = { exports: { pb_web_guest_reset: () => 0 } };
        profile = { account: "viewer", app_name: "Gallery", session: { creator: "author", selector: { kind: "exact" } } };
        globalThis.initialize = async () => { guest = await prepareGuest(wasm, operations); };
        open = async () => startGuest(guest);
        refreshPersonal = async () => {};
        scene = { controls: new Map([[1, { kind: "button", id: 1, disabled: false }]]) };
        globalThis.dispatch = request => handle(request, request.method === "input" ? acceptInput(request.args) : undefined);
    `, context);
    await context.initialize();
    return { messages, dispatch: context.dispatch };
}

test("compiled desktop Gallery reaches its unsupported menu after decoding basic controls", async () => {
    const ui = await worker();
    await ui.dispatch({ id: 1, method: "open", args: {} });
    assert.match(ui.messages.at(-1).error, /Gallery stopped.*Unsupported guest view operation 11/);
    assert.equal(ui.messages.at(-1).id, 1);
});

test("guest startup preserves an injected unsupported operation through the worker reply", async () => {
    const ui = await worker(gallery, true);
    await ui.dispatch({ id: 1, method: "open", args: {} });
    assert.match(ui.messages.at(-1).error, /Gallery stopped.*Unsupported guest view operation 11/);
    assert.equal(ui.messages.at(-1).id, 1);
});

test("guest input preserves an injected unsupported presentation instead of the rejection trap", async () => {
    const ui = await worker(gallery, true);
    await ui.dispatch({ id: 2, method: "input", args: { kind: 2, target: 1, data: [] } });
    assert.match(ui.messages.at(-1).error, /Gallery stopped.*Unsupported guest view operation 11/);
    assert.equal(ui.messages.at(-1).id, 2);
});

test("an unrelated real Wasm trap is not classified as unsupported presentation", async () => {
    const text = value => [value.length, ...new TextEncoder().encode(value)];
    const section = (id, bytes) => [id, bytes.length, ...bytes];
    const wasm = new Uint8Array([0,97,115,109,1,0,0,0,
        ...section(1,[1,0x60,0,0]),
        ...section(3,[1,0]), ...section(5,[1,0,1]),
        ...section(7,[2,...text("memory"),2,0,...text("pb_main"),0,0]),
        ...section(10,[1,3,0,0,0x0b])]);
    const ui = await worker(wasm);
    await ui.dispatch({ id: 3, method: "open", args: {} });
    assert.match(ui.messages.at(-1).error, /unreachable/);
    assert.doesNotMatch(ui.messages.at(-1).error, /Unsupported/);
});
