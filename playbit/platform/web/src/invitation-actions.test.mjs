import { replicaCapability } from "./invitation.ts";
import { encodeId, decodeId, validId } from './identifier.ts';
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";

// Types are replaced by whitespace, so the slice offsets below still hold.
const source = stripTypeScriptTypes(await readFile(new URL("./app.ts", import.meta.url), "utf8"), { mode: "strip" });
// Execute the actual join, failure actions, and pause/resume handlers with AP
// responses and worker errors at their boundary. These are UI logic tests;
// packaged-web demonstrations remain separate release evidence.
function page(states, failure) {
    const onboarding = { children: [], replaceChildren(...children) { this.children = children; } };
    const error = {}, saved = [], copied = [], failures = [];
    let requests = 0, reads = 0, headers = 0, opens = 0, terminated = 0, status;
    const context = vm.createContext({ encodeId, decodeId, validId, replicaCapability,
        document: { createElement: tag => ({ tag }) }, navigator: { clipboard: { writeText: async text => copied.push(text) } },
        onboarding, error, console: { error: (...args) => failures.push(args) }, status: text => { status = text; },
        recoveryListAccount: undefined, joining: false, paused: false, opened: false, canJoin: true, attempt: 0, syncTimer: undefined,
        invitation: { url: "https://playbit.app/s#example", uri: "playbit:session:public:example", session_id: "session", privateKey: {},
            invitation: { session: { id: "session" }, root: { id: "root", share_key_with_service: false, personal: false }} },
        enrollment: { account: "bob-account" }, personalCatalog: [], runtimeVersion: undefined,
        apClient: { connect: async () => ({ account: "bob-account" }), watchInvitation: async request => {
            assert.ok(validId(request.operation_id, 16));
            assert.equal(request.op, "watch_invitation"); assert.equal(request.session_id, "session"); requests++; return requests;
        }, streamInvitation: async () => states.shift() ?? { kind: "waiting_peer" }, streamClose: async () => {},
        request: async request => { assert.ok(validId(request.operation_id, 16)); assert.equal(request.op, "read_invitation"); assert.equal(request.session_id, "session"); reads++; return { data: { envelope: "sealed" } }; } },
        openEnvelope: async (keys, envelope) => { assert.equal(envelope, "sealed"); return { session: { id: "session" }, root: { id: "root", name: "Shared tasks", share_key_with_service: true, personal: false }}; },
        showInvitationHeader() { headers++; },
        recoveryClient: { catalog: async () => ({}) }, personalVolumes: async () => [],
        sessionProof: async () => [1, 2, 3], crypto,
        worker: { terminate: () => { terminated++; } }, createWorker() {},
        call: async method => { assert.equal(method, "open"); opens++; throw new Error(failure); },
        pending: new Map(), streams: new Set(), clearTimeout,
        localStorage: { setItem: (_, value) => saved.push(JSON.parse(value)) },
    });
    vm.runInContext(source.slice(source.indexOf("function textElement"), source.indexOf("const updateNotice ="))
        + source.slice(source.indexOf("function persistInvitation("), source.indexOf("async function afterAuthentication("))
        + source.slice(source.indexOf("const waiting ="), source.indexOf("async function refreshAccountPending()")), context);
    return { join: () => context.join(), action: label => {
        const item = onboarding.children.find(item => item.textContent === label);
        assert.ok(item, `missing action ${label}`); return item;
    }, context, error, saved, copied, failures, text: () => onboarding.children.map(item => item.textContent).join(" "),
    status: () => status, requests: () => requests, reads: () => reads, headers: () => headers, opens: () => opens, terminated: () => terminated };
}

test("unsupported scene has author guidance and conditional installed-app action, retaining pause/resume", async () => {
    const ui = page([{ kind: "ready" }], "Unsupported guest view operation 11");
    await ui.join();
    assert.equal(ui.opens(), 1);
    assert.equal(ui.status(), "This app cannot run on the web");
    assert.match(ui.text(), /Ask the app’s author/);
    assert.match(ui.text(), /support depends on the app/);
    assert.equal(ui.action("Try in Playbit").href, "playbit:session:public:example");
    ui.action("Cancel for now").onclick();
    assert.equal(ui.saved.at(-1).paused, true);
    assert.equal(ui.saved.at(-1).url, "https://playbit.app/s#example");
    await ui.action("Resume joining").onclick();
    // Resume callback schedules join; allow connect/proof/request to settle.
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(ui.requests(), 2);
    assert.equal(ui.saved.at(-1).paused, false);
});

test("joining retires the recovery worker and its streams before checking invitation readiness", async () => {
    const ui = page([{ kind: 'waiting_peer' }]);
    const closed = [];
    ui.context.recoveryListAccount = 'bob-account';
    ui.context.streams.add(42);
    ui.context.apClient.streamClose = async handle => closed.push(handle);
    await ui.join();
    assert.equal(ui.terminated(), 1);
    assert.deepEqual(closed, [42, 1]);
    assert.equal(ui.requests(), 1); assert.equal(ui.opens(), 0);
});

test("denied invitation supplies account-identification action and retries access without sharing secrets", async () => {
    const ui = page([{ kind: "denied", message: "root version provisioning denied" }, { kind: "ready" }], "Network interrupted");
    await ui.join();
    assert.equal(ui.opens(), 0);
    assert.equal(ui.status(), "Access to this session is unavailable");
    assert.equal(ui.error.textContent, "root version provisioning denied");
    assert.match(ui.text(), /sender to check access/);
    await ui.action("Copy account ID").onclick();
    assert.deepEqual(ui.copied, ["bob-account"]);
    await ui.action("Check access again").onclick();
    assert.equal(ui.requests(), 2);
    assert.equal(ui.opens(), 1);
    assert.equal(ui.status(), "Could not join yet. Your invitation is saved");
});

test("joining with a different account checks its own agreement before opening the invitation", async () => {
    const ui = page([]);
    let reviewed;
    ui.context.apClient.connect = async () => ({ account: "alice-account" });
    ui.context.afterAuthentication = async () => { reviewed = ui.context.enrollment.account; };
    await ui.join();
    assert.equal(reviewed, "alice-account");
    assert.equal(ui.requests(), 0);
    assert.equal(ui.opens(), 0);
    assert.equal(ui.context.joining, false);
});

test("withheld service key explains peer catch-up and offers installed handoff", async () => {
    const ui = page([{ kind: "waiting_peer" }]);
    await ui.join();
    assert.equal(ui.requests(), 1);
    assert.equal(ui.opens(), 0);
    assert.equal(ui.context.joining, false);
    assert.equal(ui.status(), "This session needs an online peer");
    assert.match(ui.text(), /key has not been shared/);
    assert.match(ui.text(), /Web peer catch-up is not available yet/);
    assert.match(ui.text(), /participant’s device is online/);
    assert.equal(ui.action("Open in Playbit").href, "playbit:session:public:example");
    await ui.action("Check again").onclick();
    assert.equal(ui.requests(), 2);
    assert.equal(ui.saved.at(-1).joined, false);
    ui.action("Cancel for now").onclick();
    assert.equal(ui.saved.at(-1).paused, true);
});

test("access revoked during bootstrap uses denial actions while transient failures remain retryable", async () => {
    const ui = page([{ kind: "ready" }], "session participant revoked");
    await ui.join();
    assert.equal(ui.terminated(), 1);
    assert.equal(ui.status(), "Access to this session is unavailable");
    ui.action("Check access again");
    const transient = page([{ kind: "ready" }], "Connection interrupted");
    await transient.join();
    transient.action("Try again");
    assert.equal(transient.status(), "Could not join yet. Your invitation is saved");
    assert.equal(transient.failures[0][0], "Could not join invitation");
    assert.equal(transient.failures[0][1].message, "Connection interrupted");
    assert.match(transient.failures[0][1].stack, /Connection interrupted/);
});


test("an absent AP replica explains unfinished sharing", async () => {
    const ui = page([{ kind: "waiting_peer" }]);
    ui.context.invitation.invitation.root.share_key_with_service = true;
    ui.context.invitation.invitation.root.share_key_with_service = true;
    await ui.join();
    assert.equal(ui.status(), "Waiting for shared data to become available");
    assert.match(ui.text(), /finish sharing this session/);
    ui.action("Check again");
});

test("waiting for AP shows progress and cancellation ignores late readiness", async () => {
    const ui = page([]);
    let deliver;
    const closed = [];
    ui.context.apClient.streamInvitation = () => new Promise(resolve => { deliver = resolve; });
    ui.context.apClient.streamClose = async handle => { closed.push(handle); };
    const joining = ui.join();
    assert.equal(ui.status(), "Checking invitation…");
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(ui.requests(), 1);
    ui.action("Cancel").onclick();
    assert.equal(ui.status(), "Joining paused. Your invitation is saved");
    assert.equal(ui.saved.at(-1).paused, true);
    assert.equal(ui.context.streams.size, 0);
    assert.ok(closed.includes(1));
    deliver({ kind: "ready" });
    await joining;
    assert.equal(ui.opens(), 0);
    assert.equal(ui.status(), "Joining paused. Your invitation is saved");
    ui.action("Resume joining");
});

test("the creator's envelope is read once the session is registered and its details replace the placeholder", async () => {
    const ui = page([{ kind: "waiting_registration" }, { kind: "waiting_app" }, { kind: "ready" }], "Network interrupted");
    ui.context.invitation.invitation = null;
    await ui.join();
    assert.equal(ui.reads(), 1, "one read after registration, none while waiting for it");
    assert.equal(ui.headers(), 1);
    assert.equal(ui.context.invitation.invitation.root.name, "Shared tasks");
    assert.equal(ui.context.invitation.envelope, "sealed");
    assert.equal(ui.saved.find(item => item.envelope)?.envelope, "sealed", "the envelope is kept for the next visit");
    assert.equal(ui.opens(), 1);
});
