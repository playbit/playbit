import { NativeTextEdits, encodeTextInput, decodeTextInput, MAX_TEXT_BYTES, MAX_TEXT_EVENT_BYTES } from "./text-input.ts";
import { encodeId, decodeId, validId } from './identifier.ts';
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import { surfaceBytes, validateInput as validateSceneInput } from "./scene.ts";
const validateInput = (...args) => validateSceneInput(...args, decodeTextInput);
import { drawCanvas } from "./canvas.ts";
import { canvasExtent } from "./guest-window.ts";

// Types are replaced by whitespace, so the slice offsets below still hold.
const source = stripTypeScriptTypes(await readFile(new URL("./app.ts", import.meta.url), "utf8"), { mode: "strip" });
const tick = () => new Promise(resolve => setImmediate(resolve));
function page(twoFields = false, nativeMove = true) {
    const metrics = { moves: 0, insertions: 0, replacements: 0, maps: 0 };
    const document = { activeElement: null, createElement: tag => new Element(tag), querySelector: () => ({ textContent: "" }), addEventListener() {} };
    class Element {
        constructor(tag) {
            this.tagName = tag; this.dataset = {}; this.style = { removeProperty(name) { delete this[name]; } };
            this.children = []; this.listeners = {}; this.value = "";
        }
        getContext() { return { clearRect() {}, fillRect() {}, setTransform() {} }; }
        getBoundingClientRect() { return { left: 0, top: 0, width: 320, height: 240 }; }
        setPointerCapture(id) { this.capturedPointer = id; }
        hasPointerCapture(id) { return this.capturedPointer === id; }
        releasePointerCapture(id) {
            assert.equal(this.capturedPointer, id);
            delete this.capturedPointer;
            this.onlostpointercapture?.({ pointerId: id });
        }
        addEventListener(name, fn) { this.listeners[name] = fn; }
        showPopover() { this.open = true; }
        hidePopover() { this.open = false; this.listeners.toggle?.(); }
        matches(selector) { return selector === ":popover-open" && this.open; }
        querySelector(selector) { return this.querySelectorAll(selector)[0] ?? null; }
        append(...children) { for (const child of children) { child.parent = this; this.children.push(child); } }
        replaceChildren(...children) { this.children = []; this.append(...children); }
        replaceWith(next) {
            metrics.replacements++;
            if (this.contains(document.activeElement)) document.activeElement = null;
            const index = this.parent.children.indexOf(this); this.parent.children[index] = next;
            next.parent = this.parent; this.parent = null;
        }
        remove() {
            if (!this.parent) return;
            if (this.contains(document.activeElement)) document.activeElement = null;
            this.parent.children.splice(this.parent.children.indexOf(this), 1); this.parent = null;
        }
        insertBefore(child, before) {
            if (child.parent) {
                assert.equal(child.parent, this, "Reconciliation is limited to siblings");
                assert.equal(child.contains(document.activeElement), false, "Never detach the focused control or its ancestor");
                metrics.moves++;
                this.children.splice(this.children.indexOf(child), 1);
                // Chrome clears scroll state on ordinary detach/reinsert moves.
                for (const viewport of [child, ...child.querySelectorAll(".guest-scroll")]) {
                    if (viewport.className === "guest-scroll") { viewport.scrollTop = 0; viewport.scrollLeft = 0; }
                }
            } else metrics.insertions++;
            this.children.splice(before ? this.children.indexOf(before) : this.children.length, 0, child); child.parent = this;
        }
        moveBefore(child, before) {
            assert.equal(child.parent, this, "Reconciliation is limited to siblings");
            metrics.moves++;
            this.children.splice(this.children.indexOf(child), 1);
            this.children.splice(before ? this.children.indexOf(before) : this.children.length, 0, child);
            // Chrome may scroll a moved focused descendant back into view.
            if (child.contains(document.activeElement)) {
                for (const viewport of [child, ...child.querySelectorAll(".guest-scroll")]) {
                    if (viewport.className === "guest-scroll") { viewport.scrollTop = 0; viewport.scrollLeft = 0; }
                }
            }
        }
        focus() { document.activeElement = this; }
        contains(node) { return node === this || this.children.some(child => child.contains(node)); }
        setAttribute(key, value) { this[key] = value; }
        toggleAttribute(key, present) { if (present) this.setAttribute(key, ""); else delete this[key]; }
        closest() { return null; }
        get parentElement() { return this.parent; }
        get nextElementSibling() { return this.parent?.children[this.parent.children.indexOf(this) + 1] ?? null; }
        get firstElementChild() { return this.children[0]; }
        get lastElementChild() { return this.children.at(-1); }
        querySelectorAll(selector) {
            return this.children.flatMap(child => [
                ...((selector === "canvas[data-control]"
                    ? child.tagName === "canvas" && child.dataset.control !== undefined
                    : selector === ".guest-scroll" ? child.className === "guest-scroll"
                    : child.tagName === "button" || child.type === "checkbox") ? [child] : []),
                ...child.querySelectorAll(selector),
            ]);
        }
    }
    if (!nativeMove) delete Element.prototype.moveBefore;
    const frames = new Map(); let nextFrame = 0;
    const calls = [], main = new Element("main"), error = { actions: [], after(button) { this.actions.push(button); } }, status = {};
    const windowListeners = {};
    let surface = surfaceBytes(320, 240, 1, 3);
    let captureSurfaces = false;
    let terminated = 0, recoveryIdentity;
    const context = vm.createContext({ NativeTextEdits, MAX_TEXT_EVENT_BYTES, encodeId, decodeId, validId, drawCanvas, canvasExtent, TextEncoder, encoder: new TextEncoder(), Uint8Array, console, error, main, app: "todo",
        guestFailure: undefined, engineFailure: undefined, pageClosing: false,
        requestAnimationFrame: callback => { frames.set(++nextFrame, callback); return nextFrame; },
        cancelAnimationFrame: id => frames.delete(id),
        words: (...values) => { const data = new Uint8Array(values.length * 4), view = new DataView(data.buffer); values.forEach((n, i) => view.setUint32(i * 4, n, true)); return data; },
        document, window: { addEventListener(name, callback) { windowListeners[name] = callback; } }, status: value => { status.textContent = value; }, WebAssembly: { Suspending() {}, promising() {} }, Map: class extends Map { constructor(...args) { super(...args); metrics.maps++; } },
        applyGuestWindow() {}, layoutGuestWindow: () => surface, queueMicrotask,
        pending: new Map(), streams: new Set(), clearTimeout, worker: { terminate: () => terminated++ },
        createWorker: () => { context.worker = { terminate: () => terminated++ }; },
        action: (label, callback) => ({ label, callback }),
        showOfflineEdits: (_container, identity) => { recoveryIdentity = identity; },
        call: (method, args) => method === "surface" && !captureSurfaces ? Promise.resolve(undefined)
            : new Promise((resolve, reject) => calls.push({ worker: context.worker, method, args, resolve, reject })),
        URL, Image: class { async decode() {} }, webAccountClient: {}, initialReset: {},
        sessionStorage: { getItem: () => null }, SIGN_IN_HANDOFF: "test", SIGN_IN_AFTER_RESET: "test-reset", parameters: { has: name => name === "app" }, profile: "test",
    });
    vm.runInContext(source.slice(source.indexOf("const submissions ="), source.indexOf("const statusElement"))
        + source.slice(source.indexOf("function replaceWorker"), source.indexOf("function cancelJoin"))
        + "globalThis.submit = input; globalThis.present = render; globalThis.framesChanged = updateFrames; globalThis.close = closeApp; globalThis.replaceRuntime = replaceWorker;", context);
    const scene = (value, disabled = false, checkState = 0) => ({ kind: "column", children: [
        { kind: "row", children: [{ kind: "input", id: 1, value, placeholder: "New task", disabled: false },
            { kind: "button", id: 2, label: "Add", state: 0, disabled },
            ...(twoFields ? [{kind:"input",id:4,value:"Second",placeholder:"Second field",disabled:false}] : [])] },
        { kind: "checkbox", id: 3, label: "Task", state: checkState, disabled: false },
    ] });
    const field = () => main.children[0].children[0].children[0];
    const button = () => main.children[0].children[0].children[1];
    const checkbox = () => main.children[0].children[1].children[0];
    context.present({ scene: scene("", true) });
    return { calls, frames, setFrameState: values => { Object.assign(context, values); context.framesChanged(); },
        display: time => { assert.equal(frames.size, 1); const [id, callback] = frames.entries().next().value; frames.delete(id); return callback(time); },
        close: context.close, replaceRuntime: context.replaceRuntime, terminated: () => terminated, recoveryIdentity: () => recoveryIdentity,
        error, status, field, button, checkbox, main, document, metrics, scene,
        renderScene: (scene, window) => context.present({ scene, window }),
        captureSurfaces() { captureSurfaces = true; },
        resolveScene: (index, scene) => calls[index].resolve({ scene }),
        resize(width, height) { surface = surfaceBytes(width, height, 1, 3); windowListeners.resize(); },
        start: ({ suspending = true, promising = true } = {}) => {
            if (!suspending) delete context.WebAssembly.Suspending;
            if (!promising) delete context.WebAssembly.promising;
            return vm.runInContext("let apClient;" + source.slice(source.indexOf("async function start()"), source.indexOf("const ready = start()")).replaceAll("import.meta.url", "'https://localhost/app.js'") + "start()", context);
        },
        toggle: () => { const check = checkbox(); check.checked = !check.checked; check.indeterminate = false; check.listeners.change(); },
        present: (value, disabled, checkState) => context.present({ scene: scene(value, disabled, checkState) }),
        submit: () => context.submit(3, 1, new TextEncoder().encode(field().value)),
        click: () => context.submit(2, 2, new Uint8Array()),
        raw: context.submit,
        edit: (value, isComposing = false) => { field().value = value; field().listeners.input({ isComposing }); },
        editSecond: value => { const second = main.children[0].children[0].children[2]; second.value = value; second.listeners.input({isComposing:false}); },
        finish: (index, value, disabled = false, checkState = 0) => calls[index].resolve({ scene: scene(value, disabled, checkState) }),
    };
}

test("committed edits reach the guest, retain DOM identity and preserve newer native selection", async () => {
    const ui = page(), original = ui.field();
    ui.edit("One"); await tick(); ui.edit("Two");
    original.selectionStart = 0; original.selectionEnd = 2; original.selectionDirection = "backward";
    ui.finish(0, "One"); await tick();
    assert.equal(ui.field(), original); assert.equal(original.value, "Two");
    assert.equal(original.selectionEnd, 2); assert.equal(original.selectionDirection, "backward");
    ui.finish(1, "Two"); await tick();
    assert.equal(ui.calls[0].args.kind, 14); assert.equal(ui.field().value, "Two");
});

for (const nativeMove of [false, true]) test(`scroll updates preserve viewport offsets and focused descendants (${nativeMove ? "native move" : "portable reorder"})`, async () => {
    // Update content and reorder a viewport without replacing state owned by the web browser.
    const ui = page(false, nativeMove), content = ui.scene("");
    const viewport = { kind: "scroll", id: 40, complexBackground: false, children: [content] };
    const button = { kind: "button", id: 41, label: "Other", state: 0, disabled: false };
    ui.renderScene({ kind: "column", children: [viewport, button] });
    const root = ui.main.firstElementChild, scroll = root.children[0], field = scroll.children[0].children[0].children[0];
    assert.equal(scroll.className, "guest-scroll");
    scroll.scrollTop = 80; scroll.scrollLeft = 20;
    field.focus(); field.listeners.compositionstart(); field.value = "日本語";
    field.selectionStart = 1; field.selectionEnd = 2;
    const next = { kind: "column", children: [button, { ...viewport,
        complexBackground: true, children: [ui.scene("reply", false, 1)] }] };
    ui.renderScene(next);
    assert.equal(root.children[1], scroll);
    assert.equal(scroll.scrollTop, 80); assert.equal(scroll.scrollLeft, 20);
    assert.equal(scroll.dataset.complexBackground, "true");
    assert.equal(ui.document.activeElement, field); assert.equal(field.value, "日本語");
    assert.equal(field.selectionStart, 1); assert.equal(field.selectionEnd, 2);
    assert.equal(ui.calls.length, 0); assert.equal(ui.frames.size, 0);
    field.listeners.compositionend(); await tick();
    assert.equal(ui.calls[0].args.kind, 14); assert.equal(ui.calls[0].args.target, 1);
    ui.resolveScene(0, next); await tick();
    const check = scroll.children[0].children[1].children[0];
    check.listeners.change(); await tick();
    assert.equal(ui.calls[1].args.kind, 2); assert.equal(ui.calls[1].args.target, 3);
    ui.resolveScene(1, next); await tick();
    assert.equal(root.children[1], scroll); assert.equal(scroll.scrollTop, 80);
    // The portable move must also preserve state when no focused descendant anchors it.
    ui.document.activeElement = null;
    ui.renderScene({ ...next, children: [next.children[1], button] });
    ui.renderScene(next);
    assert.equal(root.children[1], scroll);
    assert.equal(scroll.scrollTop, 80); assert.equal(scroll.scrollLeft, 20);
});

test("portable moves preserve offsets of every viewport in the moved subtree", () => {
    // An outer viewport's DOM move also detaches its nested scrolling content.
    const ui = page(false, false), inner = { kind: "scroll", id: 42,
        complexBackground: false, children: [ui.scene("")] };
    const outer = { ...inner, id: 40, children: [inner] };
    const button = { kind: "button", id: 41, label: "Other", state: 0, disabled: false };
    ui.renderScene({ kind: "column", children: [outer, button] });
    const root = ui.main.firstElementChild, scroll = root.children[0], nested = scroll.children[0];
    scroll.scrollTop = 90; scroll.scrollLeft = 10; nested.scrollTop = 50; nested.scrollLeft = 7;
    ui.renderScene({ kind: "column", children: [button, outer] });
    assert.equal(root.children[1], scroll); assert.equal(scroll.children[0], nested);
    assert.equal(scroll.scrollTop, 90); assert.equal(scroll.scrollLeft, 10);
    assert.equal(nested.scrollTop, 50); assert.equal(nested.scrollLeft, 7);
});

for (const nativeMove of [false, true]) test(`focus-then-scroll offsets survive nested viewport reordering (${nativeMove ? "native move" : "portable reorder"})`, () => {
    // Scroll away from the focused first checkbox before moving its viewport ahead of a sibling.
    const ui = page(false, nativeMove), inner = { kind: "scroll", id: 42,
        complexBackground: false, children: [{ kind: "column", children: [
            { kind: "checkbox", id: 3, label: "First task", state: 0, disabled: false },
        ] }] };
    const outer = { ...inner, id: 40, children: [inner] };
    const button = { kind: "button", id: 41, label: "Other", state: 0, disabled: false };
    ui.renderScene({ kind: "column", children: [button, outer] });
    const root = ui.main.firstElementChild, scroll = root.children[1], nested = scroll.children[0];
    const check = nested.children[0].children[0].children[0];
    check.focus(); scroll.scrollTop = 90; scroll.scrollLeft = 100; nested.scrollTop = 150;
    ui.renderScene({ kind: "column", children: [outer, button] });
    assert.equal(root.children[0], scroll); assert.equal(scroll.children[0], nested);
    assert.equal(ui.document.activeElement, check);
    assert.equal(scroll.scrollTop, 90); assert.equal(scroll.scrollLeft, 100); assert.equal(nested.scrollTop, 150);
});

for (const failure of [false, true]) test(`Close fences page input and preserves the reviewed interface (${failure ? "failure" : "success"})`, async () => {
    const ui = page(), field = ui.field();
    ui.edit("Retained private draft"); await tick();
    const close = ui.close(); assert.equal(ui.close(), close);
    assert.equal(ui.calls.length, 1); assert.equal(field.readOnly, true);
    await assert.rejects(ui.raw(14, 1, new Uint8Array()), /not accepted/);
    ui.finish(0, "Retained private draft"); await tick();
    assert.equal(ui.calls[1].method, "close");
    assert.equal(ui.field(), field); assert.equal(field.value, "Retained private draft");
    assert.equal(ui.button().disabled, true); assert.equal(ui.checkbox().disabled, true);
    if (failure) {
        ui.calls[1].reject(new Error("Some accepted work failed. Review the app before closing it."));
        await assert.rejects(close, /accepted work failed/);
        assert.equal(ui.terminated(), 0); assert.match(ui.error.textContent, /accepted work failed/);
        assert.equal(ui.status.textContent, "App needs attention");
    } else {
        ui.calls[1].resolve({ closed: true }); assert.equal((await close).closed, true);
        assert.equal(ui.terminated(), 1); assert.match(ui.status.textContent, /App closed/);
    }
    assert.equal(ui.field(), field); assert.equal(field.value, "Retained private draft");
    assert.equal(ui.error.actions.at(-1).label, "Reopen app");
});

test("Close locks every current interactive canvas and any replacement scene", async () => {
    const ui = page();
    const surface = id => ({ kind: "canvas", id, width: 320, height: 240, frames: 1, items: [] });
    const scene = { kind: "column", children: [surface(17), surface(18), surface(0)] };
    ui.renderScene(scene);
    assert.equal(ui.frames.size, 1);
    const canvases = ui.main.children[0].children;
    const close = ui.close();
    assert.equal(ui.frames.size, 0);
    for (const canvas of canvases.slice(0, 2)) {
        assert.equal(canvas.inert, true);
        assert.equal(canvas.tabIndex, -1);
    }
    assert.equal(canvases[2].inert, undefined, "decorative canvas has no input authority");
    await assert.rejects(ui.raw(11, 17, new Uint8Array()), /not accepted/);
    ui.renderScene({ kind: "column", children: [surface(19)] });
    assert.equal(ui.main.children[0].children[0].inert, true);
    assert.equal(ui.frames.size, 0);
    await tick();
    assert.equal(ui.calls[0].method, "close");
    ui.calls[0].resolve({ closed: true });
    await close;
});

test("Close and its already accepted inputs have no worker-termination timeout", () => {
    const pending = new Map(), timers = [], sent = [];
    const context = vm.createContext({ NativeTextEdits, MAX_TEXT_EVENT_BYTES, encodeId, decodeId, validId, pending, timers, sent, engineFailure: undefined,
        sequence: 0, invitation: undefined, appClosing: false,
        worker: { postMessage: request => sent.push(request), terminate: () => { throw new Error("Timed shutdown"); } },
        setTimeout: (callback, delay) => { timers.push({ callback, delay }); return timers.length; },
    });
    vm.runInContext(source.slice(source.indexOf("function call"), source.indexOf("const submissions =")) + "globalThis.call = call;", context);
    void context.call("input", { kind: 14 }); assert.equal(timers.length, 1);
    context.appClosing = true;
    void context.call("input", { kind: 2 }); void context.call("close");
    assert.equal(timers.length, 1);
    assert.equal(pending.get(2).timer, undefined); assert.equal(pending.get(3).timer, undefined);
    assert.deepEqual(sent.map(request => request.method), ["input", "input", "close"]);
});

test("Return coalesces and an acknowledgment preserves editing back to the submitted value", async () => {
    const ui = page(); ui.edit("One"); await tick(); ui.finish(0, "One"); await tick();
    const first = ui.submit(), repeat = ui.submit(); await tick();
    ui.edit("Temporary"); ui.edit("One"); ui.finish(1, ""); await Promise.all([first, repeat]); await tick();
    assert.equal(ui.field().value, "One"); assert.equal(ui.calls[1].args.kind, 3);
    ui.finish(2, "Temporary"); await tick(); ui.finish(3, "One"); await tick();
    const last = ui.submit(); await tick(); ui.finish(4, ""); await last;
    assert.equal(ui.field().value, "");
});

test("marked composition survives unrelated render and emits only after commit", async () => {
    const ui = page(), original = ui.field(); original.listeners.compositionstart();
    ui.edit("に", true); ui.present("", true);
    assert.equal(ui.field(), original); assert.equal(original.value, "に"); assert.equal(ui.calls.length, 0);
    let prevented = false;
    original.listeners.keydown({ key: "Enter", isComposing: true, preventDefault() { prevented = true; } });
    assert.equal(prevented, false);
    original.value = "日本"; original.listeners.compositionend(); await tick();
    assert.equal(ui.calls.length, 1); assert.equal(decodeTextInput(ui.calls[0].args.data).text, "日本");
    ui.finish(0, "日本"); await tick();
});

test("failed text delivery prevents an already queued Add from creating a stale task", async () => {
    const ui = page(); ui.edit("Latest"); const clicked = assert.rejects(ui.click(), /latest text/);
    await tick(); ui.calls[0].reject(new Error("Storage unavailable")); await clicked;
    assert.equal(ui.calls.length, 1); assert.equal(ui.field().value, "Latest");
    const retry = ui.click(); await tick();
    assert.equal(ui.calls[1].args.kind, 14); ui.finish(1, "Latest"); await tick();
    assert.equal(ui.calls[2].args.kind, 2); ui.finish(2, ""); await retry;
    assert.equal(ui.field().value, "");
});

test("admission rejects the 33rd input and retains its draft for an explicit retry", async () => {
    const ui = page();
    for (let index = 0; index < 33; index++) ui.edit(String(index));
    await tick(); assert.match(ui.error.textContent, /busy/); assert.equal(ui.field().value, "32");
    for (let index = 0; index < 32; index++) { ui.finish(index, String(index)); await tick(); }
    assert.equal(ui.calls.length, 32); assert.equal(ui.field().value, "32");
    const add = ui.click(); await tick(); assert.equal(ui.calls[32].args.kind, 14);
    ui.finish(32, "32"); await tick(); ui.finish(33, ""); await add;
    assert.equal(ui.error.textContent, "", "Explicit latest-draft delivery clears the resolved failure");
});

test("failed creation retains text; disabled buttons and held Return refuse activation", async () => {
    const ui = page(); ui.button().onclick(); await tick(); assert.equal(ui.calls.length, 0);
    let prevented = false;
    ui.field().listeners.keydown({ key: "Enter", repeat: true, preventDefault() { prevented = true; } });
    assert.ok(prevented); assert.equal(ui.calls.length, 0);
    ui.edit("Draft"); await tick(); ui.finish(0, "Draft"); await tick();
    const submit = assert.rejects(ui.submit(), /creation failed/); await tick();
    ui.calls[1].reject(new Error("creation failed")); await submit; assert.equal(ui.field().value, "Draft");
});

test("text change validates UTF-8, size, target and disabled state", () => {
    const controls = new Map([[1, { kind: "input" }], [2, { kind: "input", disabled: true }]]);
    validateInput(controls, 14, 1, encodeTextInput("日本", []));
    assert.throws(() => validateInput(controls, 14, 1, new Uint8Array([255])));
    assert.throws(() => validateInput(controls, 14, 1, new Uint8Array(MAX_TEXT_EVENT_BYTES + 1)));
    assert.throws(() => validateInput(controls, 14, 2, new Uint8Array()));
});


test("a failure during asynchronous dirty-field flushing invalidates the entire Click", async () => {
    const ui = page(true);
    for (let index = 0; index < 32; index++) ui.edit(String(index));
    ui.editSecond("Second"); // Queue admission fails, so Click must explicitly flush it.
    await tick();
    for (let index = 0; index < 31; index++) { ui.finish(index, String(index)); await tick(); }
    const click = assert.rejects(ui.click(), /latest text/);
    ui.calls[31].reject(new Error("Latest first-field delivery failed")); await tick();
    assert.equal(ui.calls[32].args.target, 4); ui.finish(32, "30"); await click;
    assert.equal(ui.calls.length, 33, "No Click may consume the older first-field draft");
    assert.equal(ui.field().value, "31");
});


test("rejected worker input restores the accepted checkbox without replacing controls or drafts", async () => {
    const ui = page(), check = ui.checkbox(), field = ui.field();
    ui.present("", false, 2); // The web browser clears mixed state before its change event.
    ui.toggle(); await tick();
    assert.equal(check.indeterminate, false);
    field.value = "Uncommitted draft"; field.selectionStart = 2; field.selectionEnd = 8;
    field.listeners.compositionstart(); field.listeners.input({isComposing:true});
    ui.calls[0].reject(new Error("Write denied")); await tick();
    assert.equal(check.checked, false); assert.equal(check.indeterminate, true);
    assert.equal(ui.checkbox(), check); assert.equal(ui.field(), field);
    assert.equal(field.value, "Uncommitted draft"); assert.equal(field.selectionStart, 2); assert.equal(field.selectionEnd, 8);
    assert.match(ui.error.textContent, /Write denied/);
});

test("admission rejection restores native checked state while later successful replies retain the DOM", async () => {
    const ui = page(), check = ui.checkbox();
    for (let index = 0; index < 32; index++) ui.edit(String(index));
    ui.toggle(); await tick();
    assert.equal(check.checked, false); assert.match(ui.error.textContent, /busy/);
    assert.equal(ui.calls.length, 1, "Rejected checkbox did not reach the worker");
    for (let index = 0; index < 32; index++) { ui.finish(index, String(index)); await tick(); }
    ui.toggle(); await tick(); ui.finish(32, "31", false, 1); await tick();
    assert.equal(ui.checkbox(), check); assert.equal(check.checked, true);
    assert.equal(ui.error.textContent, "", "The accepted checkbox retry resolves its earlier failure");
});

test("failed checkbox rollback uses a newer accepted scene instead of its listener's old value", async () => {
    const ui = page(), check = ui.checkbox();
    ui.toggle(); await tick();
    ui.present("", false, 1); // Another accepted response updated this retained control.
    ui.calls[0].reject(new Error("Rejected older input")); await tick();
    assert.equal(ui.checkbox(), check); assert.equal(check.checked, true);
    ui.toggle(); await tick(); ui.finish(1, "", false, 0); await tick();
    assert.equal(ui.checkbox(), check); assert.equal(check.checked, false);
    assert.equal(ui.error.textContent, "", "The accepted retry resolves its earlier failure");
});

test("a successful retry does not clear a newer account or replication error", async () => {
    const ui = page(); ui.toggle(); await tick();
    ui.calls[0].reject(new Error("Write denied")); await tick();
    ui.toggle(); await tick();
    ui.error.textContent = "Account connection unavailable";
    ui.finish(1, "", false, 1); await tick();
    assert.equal(ui.error.textContent, "Account connection unavailable");
});

test("Click skips disabled editors with rejected drafts and marked composition", async () => {
    const ui = page();
    ui.edit("Retained draft"); await tick(); ui.calls[0].reject(new Error("Draft failed")); await tick();
    ui.field().listeners.compositionstart(); ui.field().disabled = true;
    const clicked = ui.click(); await tick();
    assert.equal(ui.calls.length, 2); assert.equal(ui.calls[1].args.kind, 2);
    ui.finish(1, ""); await clicked;
    assert.equal(ui.field().value, "Retained draft");
});


for (const nativeMove of [false, true]) test(`stable checkbox siblings retain focus through reorder, insertion, removal and rejected input (${nativeMove ? "native move" : "portable reorder"})`, async () => {
    const ui = page();
    const checkbox = id => ({ kind: "checkbox", id, label: `Task ${id}`, state: 0, disabled: false });
    const scene = ui.scene(""); scene.children.push(checkbox(5), checkbox(6)); ui.renderScene(scene);
    const root = ui.main.firstElementChild;
    const original = root.children[2], check = original.children[0], last = root.children[3];
    check.focus();
    const reordered = { ...scene, children: [scene.children[0], scene.children[2], scene.children[3], scene.children[1]] };
    ui.renderScene(reordered);
    assert.equal(root.children[1], original); assert.equal(root.children[2], last);
    assert.equal(ui.document.activeElement, check);
    assert.ok(ui.metrics.moves > 0); assert.equal(ui.metrics.replacements, 0);
    const changed = { ...scene, children: [scene.children[0], checkbox(7), scene.children[2], scene.children[1]] };
    ui.renderScene(changed);
    assert.equal(root.children[2], original); assert.equal(ui.document.activeElement, check);
    assert.equal(ui.main.contains(last), false); assert.equal(ui.metrics.insertions, 1);
    check.checked = true; check.listeners.change(); await tick();
    assert.equal(ui.calls[0].args.target, 5); assert.equal(ui.calls[0].args.kind, 2);
    ui.calls[0].reject(new Error("Rejected moved checkbox")); await tick();
    assert.equal(check.checked, false); assert.equal(ui.document.activeElement, check);
    assert.match(ui.error.textContent, /Rejected moved checkbox/);
    check.checked = true; check.listeners.change(); await tick();
    const accepted = { ...changed, children: changed.children.map(node => node.id === 5 ? { ...node, state: 1 } : node) };
    ui.resolveScene(1, accepted); await tick();
    assert.equal(ui.calls[1].args.target, 5); assert.equal(root.children[2], original); assert.equal(check.checked, true);
});

for (const nativeMove of [false, true]) test(`moving a native editor preserves its composition, selection and subsequent committed target (${nativeMove ? "native move" : "portable reorder"})`, async () => {
    const ui = page(true, nativeMove), scene = ui.scene("");
    ui.renderScene(scene);
    const row = ui.main.firstElementChild.firstElementChild, field = row.children[2];
    field.focus(); field.listeners.compositionstart(); field.value = "日本語";
    field.selectionStart = 1; field.selectionEnd = 2; field.selectionDirection = "backward";
    field.listeners.input({ isComposing: true });
    const rowScene = scene.children[0];
    const moved = { ...scene, children: [{ ...rowScene, children: [rowScene.children[2], rowScene.children[1], rowScene.children[0]] }, scene.children[1]] };
    ui.renderScene(moved);
    assert.equal(row.children[0], field); assert.equal(ui.document.activeElement, field);
    assert.equal(field.value, "日本語"); assert.equal(field.selectionStart, 1); assert.equal(field.selectionEnd, 2);
    assert.equal(field.selectionDirection, "backward"); assert.equal(ui.calls.length, 0);
    assert.ok(ui.metrics.moves > 0); assert.equal(ui.metrics.insertions, 0); assert.equal(ui.metrics.replacements, 0);
    field.listeners.compositionend(); await tick();
    assert.equal(ui.calls[0].args.kind, 14); assert.equal(ui.calls[0].args.target, 4);
    assert.equal(decodeTextInput(ui.calls[0].args.data).text, "日本語");
    const accepted = { ...moved, children: [{ ...moved.children[0], children: moved.children[0].children.map(node => node.id === 4 ? { ...node, value: "日本語" } : node) }, scene.children[1]] };
    ui.resolveScene(0, accepted); await tick();
    assert.equal(row.children[0], field); assert.equal(ui.document.activeElement, field);
    assert.equal(field.selectionDirection, "backward");
});

for (const nativeMove of [false, true]) test(`unchanged sibling order allocates no reconciliation map or DOM movement (${nativeMove ? "native move" : "portable reorder"})`, () => {
    const ui = page(false, nativeMove), field = ui.field(), check = ui.checkbox();
    const maps = ui.metrics.maps;
    ui.present("Accepted text", false, 1);
    assert.equal(ui.metrics.maps, maps); assert.equal(ui.metrics.moves, 0);
    assert.equal(ui.metrics.insertions, 0); assert.equal(ui.metrics.replacements, 0);
    assert.equal(ui.field(), field); assert.equal(ui.checkbox(), check);
});

test("web browsers without JSPI open the local app with either DOM reorder implementation", async () => {
    for (const nativeMove of [false, true]) for (const features of [
        { suspending: false, promising: true },
        { suspending: true, promising: false },
        { suspending: false, promising: false },
    ]) {
        const ui = page(false, nativeMove), opening = ui.start(features);
        await tick();
        assert.equal(ui.calls.length, 1); assert.equal(ui.calls[0].method, "open");
        const result = { scene: ui.scene(""), identity: {}, timing: {}, storageBytes: 0 };
        ui.calls[0].resolve(result);
        assert.equal(await opening, result);
        assert.equal(ui.recoveryIdentity(), result.identity);
        assert.equal(ui.status.textContent, "Saved on this web browser · Local execution");
        assert.equal(ui.error.textContent, undefined);
    }
});

function* permutations(values) {
    if (!values.length) { yield []; return; }
    for (let i = 0; i < values.length; i++) {
        for (const rest of permutations(values.filter((_, index) => index !== i))) yield [values[i], ...rest];
    }
}
for (const nativeMove of [false, true]) test(`every sibling permutation preserves the active editor (${nativeMove ? "native move" : "portable reorder"})`, () => {
    const nodes = Array.from({ length: 5 }, (_, index) => ({ kind: "input", id: index + 10, value: "saved", placeholder: "Text", disabled: false }));
    for (const order of permutations(nodes)) for (let focus = -1; focus < nodes.length; focus++) {
        const ui = page(false, nativeMove), scene = { kind: "column", children: nodes };
        ui.renderScene(scene);
        const root = ui.main.firstElementChild, original = [...root.children], field = original[focus];
        if (field) {
            field.focus(); field.listeners.compositionstart(); field.value = "日本語";
            field.selectionStart = 1; field.selectionEnd = 2; field.selectionDirection = "backward";
        }
        ui.renderScene({ ...scene, children: order });
        assert.deepEqual(root.children, order.map(node => original[node.id - 10]));
        if (field) {
            assert.equal(ui.document.activeElement, field); assert.equal(field.value, "日本語");
            assert.equal(field.selectionStart, 1); assert.equal(field.selectionEnd, 2);
            assert.equal(field.selectionDirection, "backward");
        }
        assert.equal(ui.calls.length, 0);
    }
});

test("portable reorder keeps a focused descendant attached while inserting and deleting siblings", () => {
    const ui = page(true, false), base = ui.scene("");
    const fieldNode = base.children[0].children[0];
    const button = id => ({ kind: "button", id, label: `Button ${id}`, state: 0, disabled: false });
    const scene = { kind: "column", children: [button(10), { kind: "row", children: [fieldNode] }, button(11), button(12)] };
    ui.renderScene(scene);
    const root = ui.main.firstElementChild, row = root.children[1], field = row.children[0];
    field.focus(); field.listeners.compositionstart(); field.value = "draft";
    const next = { ...scene, children: [button(12), scene.children[1], button(13), button(10)] };
    ui.renderScene(next);
    assert.equal(root.children[1], row); assert.equal(row.children[0], field);
    assert.equal(ui.document.activeElement, field); assert.equal(field.value, "draft");
    assert.deepEqual(root.children.map(child => child.dataset.control ?? "row"), ["12", "row", "13", "10"]);
    // Removing the actual control still removes it; no stale focused node is pinned.
    ui.renderScene({ ...next, children: [button(12), button(13), button(10)] });
    assert.equal(ui.main.contains(field), false); assert.equal(ui.document.activeElement, null);
});

test("web browsers without moveBefore can open a local app", async () => {
    const ui = page(false, false), opening = ui.start();
    await tick();
    assert.equal(ui.calls.length, 1); assert.equal(ui.calls[0].method, "open");
    const result = { scene: ui.scene(""), identity: {}, timing: {}, storageBytes: 0 };
    ui.calls[0].resolve(result);
    assert.equal(await opening, result);
    assert.equal(ui.recoveryIdentity(), result.identity);
    assert.equal(ui.status.textContent, "Saved on this web browser · Local execution");
    assert.equal(ui.error.textContent, undefined);
});

const canvasScene = (frames, id = 17) => ({ kind: "column", children: [
    { kind: "canvas", id, width: 320, height: 240, frames, items: [] },
] });
test("a regular column receives surface events after a constrained scene releases its window", async () => {
    const ui = page();
    ui.captureSurfaces();
    ui.renderScene(ui.scene(""), { constraints: { minWidth: 500, maxWidth: 500, minHeight: 400, maxHeight: 400, aspectRatio: 0 } });
    ui.resize(500, 400);
    await tick();
    ui.resolveScene(0, ui.scene(""));
    await tick();
    ui.resize(375, 650);
    await tick();
    assert.equal(ui.calls[1].method, "surface");
    assert.deepEqual(ui.calls[1].args, surfaceBytes(375, 650, 1, 3));
    ui.resolveScene(1, ui.scene("compact"));
    await tick();
    assert.equal(ui.field().value, "compact");
    assert.equal(ui.calls.length, 2);
});

test("an open context menu tracks its owner and keeps the action a player saw", async () => {
    const ui = page();
    const scene = label => ({ kind: "context-menu", revision: label === "Join room A" ? 1 : 2, items: [{ kind: "submenu", label: "Room", items: [
        { kind: "menu-action", id: 42, label, disabled: false },
    ] }], children: [ui.scene("")] });
    const open = () => ui.main.firstElementChild.oncontextmenu({ clientX: 12, clientY: 10, preventDefault() {}, stopPropagation() {} });
    ui.renderScene(scene("Join room A"));
    open();
    const popup = ui.main.lastElementChild, oldAction = popup.children[0].children[1];
    ui.renderScene(scene("Join room A"));
    assert.equal(ui.main.lastElementChild, popup, "a normal frame keeps the menu open");
    ui.renderScene(scene("Leave room B"));
    assert.equal(ui.main.contains(popup), false);
    oldAction.onclick();
    await tick();
    assert.equal(ui.calls.length, 0, "the previous label cannot dispatch the new action");
    open();
    const updated = ui.main.lastElementChild.children[0].children[1];
    assert.equal(updated.textContent, "Leave room B");
    updated.onclick();
    await tick();
    assert.equal(ui.calls[0].args.target, 42);
    assert.equal(ui.calls[0].args.menuRevision, 2);
    ui.resolveScene(0, scene("Leave room B"));
    await tick();
    open();
    const removed = ui.main.lastElementChild;
    const held = ui.raw(2, 2, new Uint8Array());
    await tick();
    removed.children[0].children[1].onclick();
    ui.resolveScene(1, scene("Join room A")); await held; await tick();
    assert.match(ui.error.textContent, /menu changed/);
    assert.equal(ui.calls.length, 2, "the stale menu click never crossed into the worker");
    open();
    const lastPopup = ui.main.lastElementChild;
    ui.renderScene(ui.scene(""));
    assert.equal(ui.main.contains(lastPopup), false, "removing the owner also closes its menu");
});

test("ignored canvas geometry is sent again on resize and deduplicated after acceptance", async () => {
    const ui = page();
    ui.captureSurfaces();
    const scene = { ...canvasScene(0).children[0], extent: "window" };
    ui.renderScene(scene);
    await tick();
    assert.equal(ui.calls.length, 1);
    assert.equal(ui.calls[0].method, "surface");
    ui.calls[0].resolve(undefined);
    await tick();
    assert.equal(ui.calls.length, 1, "a deferred resize is not polled");

    ui.resize(320, 240);
    await tick();
    assert.equal(ui.calls.length, 2);
    assert.deepEqual(ui.calls[1].args, ui.calls[0].args);
    ui.resolveScene(1, scene);
    await tick();
    ui.resize(320, 240);
    await tick();
    assert.equal(ui.calls.length, 2, "accepted geometry is deduplicated");
});

test("overlapping resize measurements coalesce to the latest geometry", async () => {
    const ui = page();
    ui.captureSurfaces();
    const scene = { ...canvasScene(0).children[0], extent: "window" };
    ui.renderScene(scene);
    await tick();
    ui.resize(640, 480);
    ui.resize(960, 720);
    assert.equal(ui.calls.length, 1);
    ui.calls[0].resolve(undefined);
    await tick();
    assert.equal(ui.calls.length, 2);
    assert.deepEqual(ui.calls[1].args, surfaceBytes(960, 720, 1, 3));
    ui.resolveScene(1, scene);
    await tick();
    assert.equal(ui.calls.length, 2);
});

for (const delayed of ["success", "error"]) test(`recovered worker receives identical dimensions and ignores a late surface ${delayed}`, async () => {
    const ui = page(), scene = { ...canvasScene(0).children[0], extent: "window" };
    ui.captureSurfaces();
    ui.renderScene(scene);
    await tick(); ui.resolveScene(0, scene); await tick();
    ui.resize(640, 480);
    await tick();
    const old = ui.calls[1];
    ui.replaceRuntime("Opening another recovered app");
    assert.equal(ui.terminated(), 1);
    ui.resize(320, 240);
    await tick();
    assert.equal(ui.calls.length, 2, "no stale scene is sized while awaiting Open");
    const recoveredScene = { ...scene, id: 18 };
    ui.renderScene(recoveredScene); await tick();
    assert.equal(ui.calls[2].method, "surface");
    assert.notEqual(ui.calls[0].worker, ui.calls[2].worker);
    assert.deepEqual(ui.calls[0].args, ui.calls[2].args);
    ui.resolveScene(2, recoveredScene); await tick();

    if (delayed === "success") old.resolve({ scene });
    else old.reject(new Error("Retired worker rejected a resize"));
    await tick();
    ui.resize(320, 240); await tick();
    assert.equal(ui.main.firstElementChild.dataset.control, "18");
    assert.equal(ui.error.textContent, undefined);
    assert.equal(ui.calls.length, 3, "retired responses cannot replace the new worker's acknowledgment");
});

test("static Canvas quiesces; continuous Canvas keeps one callback and one input", async () => {
    const ui = page();
    ui.renderScene(canvasScene(0));
    assert.equal(ui.frames.size, 0);
    ui.renderScene(canvasScene(1)); ui.renderScene(canvasScene(1));
    assert.equal(ui.frames.size, 1);
    const pending = ui.display(100);
    await tick();
    assert.equal(ui.calls.length, 1); assert.equal(ui.calls[0].args.kind, 13);
    ui.renderScene(canvasScene(1));
    assert.equal(ui.frames.size, 0, "no callback while input is pending");
    ui.resolveScene(0, canvasScene(0)); await pending;
    assert.equal(ui.frames.size, 0, "static reply must stop animation");
});
test("Canvas demand rechecks hidden/error/closed state and switches current targets", async () => {
    const ui = page(); ui.renderScene(canvasScene(1));
    ui.document.hidden = true; ui.setFrameState({});
    assert.equal(ui.frames.size, 0);
    ui.document.hidden = false; ui.setFrameState({});
    assert.equal(ui.frames.size, 1);
    ui.setFrameState({guestFailure: "stopped"}); assert.equal(ui.frames.size, 0);
    ui.setFrameState({guestFailure: undefined}); assert.equal(ui.frames.size, 1);
    const pending = ui.display(1000); await tick();
    ui.resolveScene(0, canvasScene(1, 18)); await pending;
    const next = ui.display(2000); await tick();
    assert.equal(ui.calls[1].args.target, 18);
    assert.equal(new DataView(ui.calls[1].args.data.buffer).getUint32(0, true), 16667);
    ui.setFrameState({pageClosing: true});
    ui.resolveScene(1, canvasScene(1, 18)); await next;
    assert.equal(ui.frames.size, 0, "pending completion must not restart a closed page");
    ui.setFrameState({pageClosing: false, engineFailure: "dead"});
    assert.equal(ui.frames.size, 0);
});

test("click ahead of a display callback can stop or replace Canvas demand", async () => {
    for (const next of [canvasScene(0), canvasScene(1, 18)]) {
        const ui = page();
        ui.renderScene({ ...canvasScene(1), children: [...canvasScene(1).children,
            { kind: "button", id: 2, label: "Change canvas", disabled: false }] });
        const click = ui.click(); await tick();
        assert.equal(ui.calls[0].args.kind, 2);
        await ui.display(100); await tick();
        assert.equal(ui.calls.length, 1, "no frame queues behind the held click");
        assert.equal(ui.frames.size, 0, "held input does not spin display callbacks");
        ui.resolveScene(0, next); await click;
        assert.equal(ui.error.textContent, undefined);
        if (next.children[0].frames === 0) assert.equal(ui.frames.size, 0);
        else {
            const frame = ui.display(200); await tick();
            assert.equal(ui.calls[1].args.kind, 13);
            assert.equal(ui.calls[1].args.target, 18, "completion rechecks the adopted target");
            ui.resolveScene(1, canvasScene(0, 18)); await frame;
        }
    }
});

test("Canvas labels clip before painting, use the requested font, and restore for later rectangles", () => {
    const operations = [];
    const context = { clearRect: (...args) => operations.push(["clear", ...args]), setTransform() {},
        fillRect: (...args) => operations.push(["fill", ...args]),
        save: () => operations.push(["save"]), beginPath: () => operations.push(["path"]),
        rect: (...args) => operations.push(["clip-bounds", ...args]), clip: () => operations.push(["clip"]),
        fillText: (...args) => operations.push(["text", context.font, context.textBaseline, ...args]),
        restore: () => operations.push(["restore"]),
    };
    const scope = vm.createContext({ encodeId, decodeId, validId, surface: { getContext: () => context, closest: () => null, toggleAttribute() {}, style: {} }, node: {
        width: 320, height: 240, items: [
            { kind: "text", x: 2, y: 3, width: 100, height: 24, color: 0xffffffff, fontSize: 16, bold: true, text: "日本語" },
            { kind: "rect", x: 1, y: 1, width: 3, height: 4, color: 0 },
        ],
    }});
    drawCanvas(scope.surface, scope.node);
    assert.deepEqual(operations, [
        ["clear", 0, 0, 320, 240], ["save"], ["path"], ["clip-bounds", 2, 3, 100, 24], ["clip"],
        ["text", "700 16px Inter, sans-serif", "top", "日本語", 2, 3], ["restore"], ["fill", 1, 1, 3, 4],
    ]);
});

test("read-only fields remain selectable and suppress edits and submission across updates", async () => {
    const ui = page(), original = ui.field(), scene = ui.scene("Copy me");
    const node = scene.children[0].children[0];
    node.readOnly = true;
    ui.renderScene(scene);
    assert.equal(ui.field(), original);
    assert.equal(original.readOnly, true);
    assert.equal(original.disabled, false);
    original.focus(); original.selectionStart = 0; original.selectionEnd = 4;
    original.listeners.input({ isComposing: false });
    original.listeners.compositionend();
    original.listeners.keydown({ key: "Enter", preventDefault() {} });
    await tick();
    assert.equal(ui.calls.length, 0);
    assert.equal(ui.document.activeElement, original);
    assert.equal(original.selectionEnd, 4);
    node.readOnly = false;
    ui.renderScene(scene);
    assert.equal(original.readOnly, false);
    ui.edit("Editable"); await tick();
    assert.equal(ui.calls.length, 1);
    ui.finish(0, "Editable"); await tick();
});

test("oversized native drafts report the limit and retain text for recovery", async () => {
    const ui = page();
    ui.edit("x".repeat(MAX_TEXT_BYTES + 1)); await tick();
    assert.equal(ui.calls.length, 0);
    assert.equal(ui.field().value.length, MAX_TEXT_BYTES + 1);
    assert.match(ui.error.textContent, /limit/);
    ui.edit("Recovered"); await tick();
    assert.equal(ui.calls.length, 1);
    ui.finish(0, "Recovered"); await tick();
    assert.equal(ui.error.textContent, "");
});

test("large native paste delivers, oversized replacement retains it, and deletion clears the error", async () => {
    const ui = page(), field = ui.field(), large = "x".repeat(66 * 1024);
    const before = () => field.listeners.beforeinput({ inputType: "insertFromPaste", cancelable: true,
        preventDefault() { assert.fail("draft recovery cancelled"); } });
    field.selectionStart = field.selectionEnd = 0; before();
    ui.edit(large); await tick();
    assert.equal(decodeTextInput(ui.calls[0].args.data).text, large);
    ui.finish(0, large); await tick();
    field.selectionStart = 0; field.selectionEnd = large.length; before();
    ui.edit("x".repeat(MAX_TEXT_BYTES + 1)); await tick();
    assert.equal(ui.calls.length, 1);
    assert.equal(field.value, large);
    assert.match(ui.error.textContent, /limit/);
    field.selectionStart = 0; field.selectionEnd = large.length; before();
    ui.edit(""); await tick();
    assert.equal(decodeTextInput(ui.calls[1].args.data).text, "");
    ui.finish(1, ""); await tick();
    assert.equal(ui.error.textContent, "");
});
