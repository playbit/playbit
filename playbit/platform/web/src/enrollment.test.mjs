import { encodeId, decodeId, validId } from './identifier.ts';
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { formatAccountKey, parseAccountKey } from "./account-key.ts";
import { AGREEMENT } from "./ap-codec.ts";
import { stripTypeScriptTypes } from "node:module";

// Types are replaced by whitespace, so the slice offsets below still hold.
const source = stripTypeScriptTypes(await readFile(new URL("./app.ts", import.meta.url), "utf8"), { mode: "strip" });
const event = { preventDefault() {} };
function page() {
    let message, request, authentication;
    const stored = new Map(), listeners = new Map();
    const element = () => ({ children: [], value: "", append(...items) { this.children.push(...items); }, focus() {} });
    const onboarding = { children: [], replaceChildren(...items) { this.children = items; } };
    const error = { textContent: "Prior authentication timeout" }, invitation = { url: "https://playbit.app/s#retained" };
    const context = vm.createContext({ encodeId, decodeId, validId, updateFrames() {}, onboarding, error, invitation, enrollment: null, parseAccountKey,
        window: { addEventListener: (name, handler) => listeners.set(name, handler) }, accountSignal: { close() {} },
        URL, history: { replaceState() {} }, document: { createElement: element }, status: value => { message = value; },
        textElement: (_tag, text) => ({ ...element(), textContent: text }),
        action: (text, onclick) => ({ ...element(), textContent: text, onclick }),
        beginWebReset: client => client.resetWebAccount(), webTab: 'tab', SIGN_IN_HANDOFF: 'handoff', sessionStorage: { setItem: (key, value) => stored.set(key, value), removeItem: key => stored.delete(key) },
        apClient: { close: () => request.reject(new Error("AP tab closed")), resetWebAccount: () => new Promise((resolve, reject) => { request = { resolve, reject }; }),
            requestCode: () => new Promise((resolve, reject) => { request = { resolve, reject }; }),
            finishCode: () => new Promise((resolve, reject) => { request = { resolve, reject }; }) },
        afterAuthentication: () => new Promise(resolve => { authentication = resolve; }),
    });
    vm.runInContext(source.slice(source.indexOf("let pageClosing ="), source.indexOf("let guestFailure"))
        + source.slice(source.indexOf("window.addEventListener('pagehide'"), source.indexOf("function showInvitationHeader"))
        + source.slice(source.indexOf("function showEmail()"), source.indexOf("async function useWeb()"))
        + "globalThis.emailForm = showEmail; globalThis.codeForm = showCode;", context);
    return { context, onboarding, error, invitation, stored, pagehide: event => listeners.get("pagehide")(event), status: () => message,
        request: () => request, authenticate: () => authentication() };
}

test("sending a code immediately clears the prior error and retains invitation through retry", async () => {
    const ui = page(); ui.context.emailForm();
    const form = ui.onboarding.children[0], button = form.children.find(child => child.type === "submit");
    const privacy = form.children.flatMap(child => child.children).find(child => child.textContent === "Privacy policy");
    assert.equal(privacy.href, "https://playbit.app/help/privacy");
    form.children[0].children[0].value = "bob@example.test";
    const failed = form.onsubmit(event);
    assert.equal(ui.status(), "Sending code…"); assert.equal(ui.error.textContent, ""); assert.ok(button.disabled);
    ui.request().reject(new Error("AP did not answer authentication in time")); await failed;
    assert.equal(button.disabled, false); assert.match(ui.status(), /Try again/);
    const retried = form.onsubmit(event); assert.equal(ui.error.textContent, "");
    ui.request().resolve(); await retried;
    assert.equal(ui.status(), "Sending code…", "fresh sign-in reloads through cleanup before sending mail");
    assert.equal(ui.invitation.url, "https://playbit.app/s#retained");
});

test("a signed-in user reviews the archived edition and can retry an unconfirmed acceptance", async () => {
    const ui = page();
    const calls = [];
    let acknowledged = false, ready = 0;
    Object.assign(ui.context, {
        AGREEMENT, enrollment: { account: "Bob" }, canJoin: false, useWeb() {},
        afterAgreement: () => { ready++; return true; },
        apClient: { async agreement(account, accept) {
            calls.push([account, accept]);
            if (accept && !acknowledged) throw Error("Offline");
            return acknowledged;
        } },
    });
    ui.context.document.querySelector = () => ({});
    vm.runInContext(source.slice(source.indexOf("async function afterAuthentication()"), source.indexOf("async function afterAgreement()")), ui.context);
    assert.equal(await ui.context.afterAuthentication(), false);
    const [link, , accept] = ui.onboarding.children;
    assert.equal(link.href, `/help/user-agreement/${AGREEMENT.revision}/`);
    assert.deepEqual(calls, [["Bob", false]]);
    await accept.onclick();
    assert.equal(ready, 0);
    assert.equal(accept.disabled, false);
    acknowledged = true;
    await accept.onclick();
    assert.equal(ready, 1);
    assert.equal(await ui.context.afterAuthentication(), true);
    assert.deepEqual(calls, [["Bob", false], ["Bob", true], ["Bob", true], ["Bob", false]]);
});

test("a blocked cleanup reports clearing, not sending, as what failed", async () => {
    const ui = page(); ui.context.emailForm();
    const form = ui.onboarding.children[0], button = form.children.find(child => child.type === "submit");
    form.children[0].children[0].value = "bob@example.test";
    const pending = form.onsubmit(event);
    ui.request().reject(new Error("Close other Playbit tabs, then try clearing web browser data again")); await pending;
    assert.equal(ui.status(), "Sign-in could not start. Try again");
    assert.equal(ui.error.textContent, "Close other Playbit tabs, then try clearing web browser data again");
    assert.equal(button.disabled, false);
    assert.equal(ui.stored.has("handoff"), false, "a genuine reset failure discards sign-in secrets");
});

test("code completion shows sign-in and private access progress without overlapping resend", async () => {
    const ui = page(); ui.context.codeForm("bob@example.test");
    const form = ui.onboarding.children[0], button = form.children[1], resend = ui.onboarding.children[1];
    const pending = form.onsubmit(event);
    assert.equal(ui.status(), "Signing in…"); assert.equal(ui.error.textContent, "");
    assert.ok(button.disabled); assert.ok(resend.disabled);
    ui.request().resolve({ account: "Bob" }); await new Promise(setImmediate);
    assert.equal(ui.status(), "Signed in. Checking private access…");
    ui.authenticate(); await pending;
    assert.equal(ui.context.enrollment.account, "Bob"); assert.equal(ui.invitation.url, "https://playbit.app/s#retained");
});

test("resending a code exposes progress and restores both actions after failure", async () => {
    const ui = page(); ui.context.codeForm("bob@example.test");
    const button = ui.onboarding.children[0].children[1], resend = ui.onboarding.children[1];
    const pending = resend.onclick();
    assert.equal(ui.status(), "Sending code…"); assert.equal(ui.error.textContent, "");
    assert.ok(button.disabled); assert.ok(resend.disabled);
    ui.request().reject(new Error("Offline")); await pending;
    assert.equal(button.disabled, false); assert.equal(resend.disabled, false); assert.equal(ui.error.textContent, "Offline");
});

test("reset reload retains email and account key when page teardown rejects its pending request", async () => {
    for (const key of ["", formatAccountKey(new Uint8Array(16).fill(7))]) {
        const ui = page(); ui.context.emailForm();
        const form = ui.onboarding.children[0];
        form.children[0].children[0].value = "bob@example.test";
        const input = form.children[1].children[0]; input.value = key;
        const pending = form.onsubmit(event);
        assert.equal(input.value, "", "the old page clears its key input");
        ui.pagehide({ persisted: false });
        await pending;
        assert.deepEqual(JSON.parse(ui.stored.get("handoff")), { email: "bob@example.test", accountKey: key });
        assert.equal(ui.error.textContent, "");
        assert.equal(ui.status(), key ? "Signing in…" : "Sending code…");
    }
});

test("a cached page still reports genuine reset failures after pagehide", async () => {
    const ui = page(); ui.context.emailForm();
    const form = ui.onboarding.children[0]; form.children[0].children[0].value = "bob@example.test";
    const pending = form.onsubmit(event);
    ui.pagehide({ persisted: true });
    ui.request().reject(new Error("Storage unavailable")); await pending;
    assert.equal(ui.stored.has("handoff"), false);
    assert.equal(ui.error.textContent, "Storage unavailable");
});
