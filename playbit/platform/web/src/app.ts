import { AGREEMENT } from "./ap-codec.ts";
import { NativeTextEdits, MAX_TEXT_EVENT_BYTES } from "./text-input.ts";
import { parseAccountKey } from "./account-key.ts";
import { ApClient } from "./ap-client.ts";
import { encodeId } from "./identifier.ts";
import { configuration, deploymentChannel, workerURL } from './release-channel.ts';
import type { ApStatus, ApReplicaSubscription } from "./ap-protocol.ts";
import { isSessionPath, replicaCapability, setScopeAddresses, parseInvitation, retainedInvitation, openEnvelope, sessionProof } from "./invitation.ts";
import type { Invitation, ParsedInvitation } from "./invitation.ts";
import { RecoveryClient } from "./recovery.ts";
import type { RecoveryStatus, CatalogVolume } from "./recovery.ts";
import { personalVolumes, personalSelections } from "./personal.ts";
import type { PersonalGrant } from "./personal.ts";
import { pendingAccountRecord, observeAccountMetadata, removedAppsIn } from './account-outbox.ts';
import { AccountSignal } from './account-notices.ts';
import { showOfflineEdits } from './offline-edits.ts';
import { holdWebAccount, reloadWebAccount, SIGN_IN_HANDOFF, SIGN_IN_AFTER_RESET, prepareWebAccount, beginWebReset } from './web-account.ts';
import type { MenuItem, SceneCanvas, SceneMenu, SceneNode, SceneScroll, SceneWindow } from "./scene.ts";
import { drawCanvas } from "./canvas.ts";
import { applyGuestWindow, canvasExtent, layoutGuestWindow } from "./guest-window.ts";
import type { ReplicationState } from "./live-replication.ts";
import { replicationDiagnostic } from "./live-replication.ts";
import type { RuntimeMethod, RuntimeMethods, RuntimeWorkerMessage, RuntimePresentation, RuntimeOpenResult, RuntimeUpdateState } from "./runtime-protocol.ts";
declare global {
    // `Element.moveBefore` is not in lib.dom yet; use it when available to preserve native state during reordering.
    interface Element { moveBefore(node: Node, child: Node | null): void }
    interface Window { playbit: { ready: Promise<RuntimeOpenResult | undefined>; input: typeof input; close: () => Promise<{ closed: true }> } }
}
// `connect`/`finishCode` resolve only once signed in, so the account is present.
type Enrollment = ApStatus & { account: string };
// `children` is readable on every node; scene.ts declares it on containers only.
type SceneTreeNode = SceneNode & { children?: SceneNode[] };
interface Editor { id: number; element: HTMLInputElement; revision: number; sent: number; acknowledged: number; composing: boolean; edits: NativeTextEdits }
interface PendingCall {
    method: RuntimeMethod;
    // Method syntax: `call<M>` stores its typed resolver here, and the worker's `result` arrives as `unknown`.
    resolve(value: unknown): void;
    reject(error: Error): void;
    timer: ReturnType<typeof setTimeout> | undefined;
}
// Mutable state such as `apClient` and `invitation` is assigned during setup and sign-in; the
// functions below run only afterwards, and check for absence where that is possible.
const webTab = crypto.randomUUID();
const selectedChannel = deploymentChannel(document.querySelector<HTMLMetaElement>('meta[name="playbit-channel"]')?.content, location.origin);
const webAccountClient = new ApClient(selectedChannel);
const initialReset = await prepareWebAccount(webAccountClient);
await holdWebAccount(message => reloadWebAccount(message, webTab));
const parameters = new URLSearchParams(location.search);
const app = parameters.get("app") === "note" ? "note" : parameters.get("app") === "kart" ? "kart" : "todo";
const profile = parameters.get("profile") ?? `local-${app}`;
let worker: Worker, recoveryListAccount: string | undefined;
let apClient: ApClient, opened = false, joining = false, paused = false, invitation: ParsedInvitation, enrollment: Enrollment, attempt = 0;
let replicationState: ReplicationState | undefined;
let pageClosing = false;
let guestFailure: string | undefined;
let engineFailure: string | undefined;
let accountPending: boolean | undefined;
let recoveryClient: RecoveryClient, canJoin = false, recoveryVisible = false, runtimeSession: string, personalCatalog: CatalogVolume[] = [], personalProgress: ((text: string) => void) | undefined;
const streams = new Set<number>();
const pending = new Map<number, PendingCall>();
let sequence = 0;
const main = document.querySelector("main") as HTMLElement, error = document.querySelector("#error") as HTMLElement;
const encoder = new TextEncoder();
const accountSignal = new AccountSignal(account => apClient.accountChanged(account), accountNoticeFailed);
observeAccountMetadata(account => accountSignal.notify(account));
const words = (...values: number[]): Uint8Array => {
    const bytes = new Uint8Array(values.length * 4), view = new DataView(bytes.buffer);
    values.forEach((value, index) => view.setUint32(index * 4, value, true)); return bytes;
};
function createWorker() {
    replicationState = undefined; recoveryListAccount = undefined;
    worker = new Worker(workerURL('./runtime-worker.js', import.meta.url, selectedChannel), { type: "module" });
    const current = worker;
    current.onmessage = async ({ data }: MessageEvent<RuntimeWorkerMessage>) => {
        if (current !== worker) return;
        if (data.engineFailure) {
            engineFailure = data.engineFailure;
            updateFrames();
            current.terminate();
            retireCanvasPresses();
            for (const item of pending.values()) { clearTimeout(item.timer); item.reject(new Error(engineFailure)); }
            pending.clear();
            error.textContent = engineFailure;
            status("Playbit needs to restart");
            error.after(action("Restart Playbit", () => location.reload()));
            return;
        }
        if (engineFailure) return;
        if (data.ap) {
            const { id, method, args } = data.ap;
            try {
                // The worker names the AP method; dispatch is dynamic, so no static signature applies.
                const methods = apClient as unknown as Record<string, ((...args: unknown[]) => Promise<unknown>) | undefined>;
                if (!apClient || typeof methods[method] !== "function") throw new Error("Sign in to continue");
                const result = await methods[method]!(...args);
                const handle = method === "begin" || method === "beginChannel" ? result as number
                    : method === "subscribeReplica" ? (result as ApReplicaSubscription).handle : undefined;
                if (current !== worker) {
                    if (handle !== undefined) await apClient.streamClose(handle);
                    return;
                }
                if (handle !== undefined) streams.add(handle);
                if (method === "streamClose") streams.delete(args[0] as number);
                if (method === 'streamRead' && result instanceof Uint8Array && result.buffer instanceof ArrayBuffer) {
                    current.postMessage({ apReply: { id, result } }, [result.buffer]);
                } else current.postMessage({ apReply: { id, result } });
            } catch (error) {
                const cause = error as Error & { code?: string; serviceCode?: string };
                current.postMessage({ apReply: { id, error: cause.message, code: cause.code, serviceCode: cause.serviceCode } });
            }
            return;
        }
        if (data.replication) {
            if (recoveryListAccount) {
                if (data.replication.kind === 'failed' || data.replication.kind === 'action_required')
                    error.textContent = data.replication.message ?? 'Could not synchronize recovered apps';
            } else showReplication(data.replication);
            return;
        }
        if (data.accountNoticeFailed) { accountNoticeFailed(); return; }
        if (Object.hasOwn(data, "guestFailure")) {
            guestFailure = data.guestFailure ?? undefined;
            updateFrames();
            error.textContent = guestFailure ?? "";
            if (guestFailure) status("App needs attention");
            return;
        }
        if (data.replicaScene) { render(data.replicaScene); return; }
        if (data.surfaceReady) { scheduleSurface(); return; }
        if (Object.hasOwn(data, "update")) { showUpdate(data.update as RuntimeUpdateState | null); return; }
        if (data.adopted) {
            guestFailure = undefined; error.textContent = "";
            render(data.adopted);
            const identity = document.querySelector("#identity") as HTMLElement;
            try { identity.textContent = JSON.stringify({ ...JSON.parse(identity.textContent as string), version: data.adopted.build_id }, null, 2); } catch {}
            return;
        }
        if (data.progress) { if (personalProgress) personalProgress(data.progress); else if (!opened) status(data.progress); return; }
        const item = pending.get(data.id as number); if (!item) return;
        pending.delete(data.id as number); clearTimeout(item.timer);
        if (data.error) item.reject(Object.assign(new Error(data.error), { code: data.code })); else item.resolve(data.result);
    };
}
createWorker();
setScopeAddresses(request => call("scope_addresses", request));
function call<M extends RuntimeMethod>(method: M, args?: RuntimeMethods[M]["args"]): Promise<RuntimeMethods[M]["result"]> {
    if (engineFailure) return Promise.reject(new Error(engineFailure));
    const id = ++sequence;
    return new Promise<RuntimeMethods[M]["result"]>((resolve, reject) => {
        const timer = method === "close" || (method === "input" && appClosing) ? undefined : setTimeout(() => {
            engineFailure = "The app stopped responding. Reload to reopen retained data.";
            updateFrames(); pending.delete(id); worker.terminate(); retireCanvasPresses(); reject(new Error(engineFailure));
        }, method === "open" || invitation ? 300000 : 5000);
        pending.set(id, { method, resolve, reject, timer }); worker.postMessage({ id, method, args });
    });
}
const submissions = new Map<number, Promise<RuntimePresentation>>();
const editors = new Map<number, Editor>();
type CanvasPress = { canvas: HTMLCanvasElement; target: number } & ({ kind: 11; pointerId: number } | { kind: 12; key: number });
const heldCanvasPresses = new Set<CanvasPress>();
let reservedCanvasBytes = 0;
let inputTail: Promise<unknown> = Promise.resolve(), inputCount = 0, inputBytes = 0, draftFailures = 0;
let appClosing = false, appClosed = false, closeTask: Promise<{ closed: true }> | undefined;
let inputFailure: { target: number; message: string } | undefined;
type MenuSelection = { owner: HTMLElement; revision: number };
async function input(kind: number, target: number, data: Uint8Array, menu?: MenuSelection): Promise<RuntimePresentation> {
    if (appClosing || appClosed) throw new Error("The app is finishing accepted work. This input was not accepted.");
    if (kind === 3 && submissions.has(target)) return submissions.get(target)!;
    const current = worker;
    const retrying = inputFailure;
    const failures = draftFailures;
    if (kind === 2) {
        for (const field of editors.values()) {
            if (field.element.disabled) continue;
            if (field.composing) {
                const failure = new Error("Finish composing your text before continuing.");
                inputFailure = { target, message: failure.message };
                error.textContent = failure.message; throw failure;
            }
            if (field.revision !== field.sent) await input(14, field.id, field.edits.payload(field.element.value));
        }
    }
    if (current !== worker) throw new Error("The app changed before this input reached it.");
    return queueInput(kind, target, data, retrying, failures, undefined, menu);
}
function queueInput(kind: number, target: number, data: Uint8Array, retrying = inputFailure,
    failures = draftFailures, transition?: { press: CanvasPress; down: boolean }, menu?: MenuSelection): Promise<RuntimePresentation> {
    // Admission happens before chaining: retained closures and payloads are bounded.
    if (appClosing || appClosed) throw new Error("The app is finishing accepted work. This input was not accepted.");
    // A press reserves one slot and its ABI payload size. Its release
    // exchanges that reservation for an entry in the same FIFO.
    const change = transition ? transition.down ? 1 : -1 : 0;
    const reserved = heldCanvasPresses.size + change;
    const reservedBytes = reservedCanvasBytes + change * data.length;
    if (data.length > MAX_TEXT_EVENT_BYTES || inputCount + reserved >= 32
        || inputBytes + data.length + reservedBytes > 2 * 1024 * 1024) {
        const failure = new Error("The app is busy. Your text is retained; try again.");
        inputFailure = { target, message: failure.message };
        error.textContent = failure.message; throw failure;
    }
    if (transition) {
        if (transition.down) heldCanvasPresses.add(transition.press);
        else heldCanvasPresses.delete(transition.press);
        reservedCanvasBytes = reservedBytes;
    }
    const current = worker;
    const editor = (kind === 3 || kind === 14) ? editors.get(target) : null;
    const revision = editor?.revision;
    if (editor) { editor.sent = revision!; editor.edits.queued(); }
    inputCount++; inputBytes += data.length;
    const promise = inputTail.catch(() => {}).then(async () => {
        if (current !== worker) throw new Error("The app changed before this input reached it.");
        if (kind === 2 && failures !== draftFailures) {
            throw new Error("Your latest text has not reached the app. Edit it or press Return to retry.");
        }
        if (menu && (!main.contains(menu.owner) || menus.get(menu.owner)?.revision !== menu.revision)) {
            throw new Error("The menu changed. Open it again to choose an action.");
        }
        const result = await call("input", { kind, target, data, ...(menu ? { menuRevision: menu.revision } : {}) });
        if (current !== worker) throw new Error("The app changed before this input completed.");
        if (editor && (kind === 3 || kind === 14)) {
            editor.acknowledged = Math.max(editor.acknowledged, revision!);
        }
        render(result);
        // Only a new accepted input to the failed control can resolve its error.
        // Earlier queued acknowledgments and unrelated refreshes cannot hide it.
        if (retrying && inputFailure === retrying && retrying.target === target) {
            if (error.textContent === retrying.message) error.textContent = "";
            inputFailure = undefined;
        }
        return result;
    }).catch(failure => {
        if (current === worker && (!transition || main.contains(transition.press.canvas))) {
            if (editor) { draftFailures++; if (editor.sent === revision) editor.sent = editor.acknowledged; }
            inputFailure = { target, message: failure.message }; error.textContent = failure.message;
        }
        throw failure;
    })
        .finally(() => {
            inputCount--; inputBytes -= data.length;
            if (kind === 3 && submissions.get(target) === promise) submissions.delete(target);
            updateFrames();
        });
    inputTail = promise;
    if (kind === 3) submissions.set(target, promise);
    return promise;
}
function sendCanvasPress(press: CanvasPress, data: Uint8Array, down: boolean): boolean {
    try { void queueInput(press.kind, press.target, data, inputFailure, draftFailures, { press, down }).catch(() => {}); }
    catch { return false; }
    return true;
}
function releasePointerCapture(press: CanvasPress): void {
    if (press.kind === 11 && press.canvas.hasPointerCapture(press.pointerId)) press.canvas.releasePointerCapture(press.pointerId);
}
function cancelCanvasPress(press: CanvasPress): void {
    if (sendCanvasPress(press, press.kind === 11 ? words(0, 0, 3, 0) : words(press.key, 0), false)) releasePointerCapture(press);
}
function retireCanvasPresses(): void {
    for (const press of heldCanvasPresses) {
        heldCanvasPresses.delete(press);
        releasePointerCapture(press);
    }
    reservedCanvasBytes = 0;
}
function lockGuestControls(): void {
    for (const editor of editors.values()) editor.element.readOnly = true;
    for (const control of main.querySelectorAll<HTMLButtonElement | HTMLInputElement>("button,input[type=checkbox]")) control.disabled = true;
    for (const canvas of main.querySelectorAll<HTMLCanvasElement>("canvas[data-control]")) {
        canvas.inert = true; canvas.tabIndex = -1;
    }
}
function closeApp(): Promise<{ closed: true }> {
    if (closeTask) return closeTask;
    for (const press of heldCanvasPresses) cancelCanvasPress(press);
    appClosing = true;
    updateFrames();
    for (const item of pending.values()) if (item.method === "input") { clearTimeout(item.timer); item.timer = undefined; }
    lockGuestControls();
    const button = document.querySelector("#close-app") as HTMLButtonElement;
    button.disabled = true; status("Finishing accepted work…");
    // Wait outside the worker queue. Every input already accepted by the page
    // is sent exactly once before the worker deactivates its finite roots.
    closeTask = inputTail.catch(() => {}).then(() => call("close")).then(result => {
        appClosed = true; worker.terminate(); button.hidden = true;
        status("App closed · Saved on this web browser");
        error.after(action("Reopen app", () => location.reload()));
        return result;
    }, failure => {
        error.textContent = failure.message; status("App needs attention");
        error.after(action("Reopen app", () => location.reload()));
        throw failure;
    });
    return closeTask;
}
function element(node: SceneNode): HTMLElement {
    if (node.kind === "context-menu") {
        const container = document.createElement("div"); container.className = "guest-context";
        menus.set(container, node);
        container.append(element(node.children[0]));
        container.oncontextmenu = event => {
            event.preventDefault();
            event.stopPropagation();
            showContextMenu(container, event.clientX, event.clientY);
        };
        return container;
    }
    if (node.kind === "column" || node.kind === "row" || node.kind === "scroll") {
        const container = document.createElement("div"); container.className = node.kind === "scroll" ? "guest-scroll" : node.kind;
        if (node.kind === "scroll") updateScroll(container, node);
        container.append(...node.children.map(element)); return container;
    }
    if (node.kind === "text") {
        const element = document.createElement(node.style === 1 ? "h1" : "p");
        element.textContent = node.text; element.className = node.style === 2 ? "muted" : "";
        if (app === "kart" && node.text.startsWith("Speed")) element.dataset.speed = "";
        return element;
    }
    if (node.kind === "input") {
        const field = document.createElement("input"); field.type = "text"; field.className = "guest-field";
        const editor = { id: node.id, element: field, revision: 0, sent: 0, acknowledged: 0, composing: false, edits: new NativeTextEdits() };
        editors.set(node.id, editor); field.dataset.control = String(node.id);
        const failed = (failure: unknown) => {
            inputFailure = { target: node.id, message: (failure as Error).message };
            error.textContent = inputFailure.message;
        };
        const change = () => {
            if (field.disabled || field.readOnly) return;
            try { input(14, node.id, editor.edits.payload(field.value)).catch(() => {}); }
            catch (failure) { failed(failure); }
        };
        field.addEventListener("beforeinput", event => {
            if (!field.disabled && !field.readOnly) {
                try { editor.edits.beforeInput(field, event as InputEvent); }
                catch (failure) { failed(failure); }
            }
        });
        field.addEventListener("compositionstart", () => { editor.edits.compositionStart(field); editor.composing = true; editor.revision++; });
        field.addEventListener("compositionend", () => {
            editor.composing = false;
            try { editor.edits.compositionEnd(field); change(); }
            catch (failure) { failed(failure); }
        });
        field.addEventListener("input", event => {
            if (field.disabled || field.readOnly) return;
            try { editor.edits.input(field, editor.composing || event.isComposing); }
            catch (failure) { failed(failure); return; }
            editor.revision++;
            if (!editor.composing && !event.isComposing) change();
        });
        field.addEventListener("keydown", event => {
            if (event.key === "Enter" && !editor.composing && !event.isComposing && event.keyCode !== 229) {
                event.preventDefault();
                if (!event.repeat && !field.disabled && !field.readOnly) input(3, node.id, encoder.encode(field.value)).catch(() => {});
            }
        });
        field.value = node.value; updateControl(field, node); return field;
    }
    if (node.kind === "checkbox") {
        const label = document.createElement("label"), check = document.createElement("input"), text = document.createElement("span");
        label.className = "guest-check"; check.type = "checkbox"; check.dataset.control = String(node.id);
        check.addEventListener("change", () => {
            if (!check.disabled) input(2, node.id, new Uint8Array()).catch(() => restoreCheckbox(check, node.id));
        });
        label.append(check, text); updateControl(label, node); return label;
    }
    if (node.kind === "button") {
        const button = document.createElement("button"); button.type = "button"; button.className = "guest-button";
        button.dataset.control = String(node.id);
        button.onclick = () => { if (!button.disabled) input(2, node.id, new Uint8Array()).catch(() => {}); };
        updateControl(button, node); return button;
    }
    if (node.kind === "canvas") {
        const canvas = document.createElement("canvas");
        if (node.id === 0) {
            canvas.setAttribute("aria-hidden", "true"); paintCanvas(canvas, node); return canvas;
        }
        canvas.tabIndex = 0;
        canvas.setAttribute("aria-label", "Game canvas"); canvas.dataset.control = String(node.id);
        const pointer = (event: PointerEvent, phase: number) => {
            if (!main.contains(canvas)) return;
            let active: Extract<CanvasPress, { kind: 11 }> | undefined;
            for (const press of heldCanvasPresses) {
                if (press.kind === 11 && press.canvas === canvas) { active = press; break; }
            }
            if (phase === 1) {
                if (active || event.button !== 0 || !event.isPrimary) return;
            } else if (!active || active.pointerId !== event.pointerId) return;
            if (active && phase === 3) { cancelCanvasPress(active); return; }
            const rect = canvas.getBoundingClientRect();
            const current = canvasScenes.get(canvas)!;
            if (!rect.width || !rect.height) {
                if (active && phase === 2) cancelCanvasPress(active);
                return;
            }
            const width = current.extent ? rect.width : current.width, height = current.extent ? rect.height : current.height;
            const x = Math.round(Math.max(0, Math.min(width, (event.clientX - rect.left) * width / rect.width)));
            const y = Math.round(Math.max(0, Math.min(height, (event.clientY - rect.top) * height / rect.height)));
            const bytes = words(x, y, phase, phase < 2 ? event.buttons & 1 : 0);
            if (phase === 1) {
                const press: CanvasPress = { canvas, target: node.id, kind: 11, pointerId: event.pointerId };
                if (!sendCanvasPress(press, bytes, true)) return;
                try { canvas.setPointerCapture(event.pointerId); }
                catch { cancelCanvasPress(press); return; }
                canvas.focus({ preventScroll: true });
            } else if (phase === 2) {
                if (sendCanvasPress(active!, bytes, false)) releasePointerCapture(active!);
            } else input(11, node.id, bytes).catch(() => {});
        };
        canvas.onpointerdown = event => pointer(event, 1); canvas.onpointermove = event => pointer(event, event.buttons & 1 ? 0 : 2);
        canvas.onpointerup = event => pointer(event, 2); canvas.onpointercancel = event => pointer(event, 3);
        canvas.onlostpointercapture = event => pointer(event, 3);
        const keys: Record<string, number> = { ArrowLeft: 1, ArrowRight: 2, ArrowUp: 3, ArrowDown: 4, Space: 5, " ": 5 };
        const keyboard = (event: KeyboardEvent, down: boolean) => {
            const key = keys[event.code] ?? keys[event.key];
            const shortcut = event.altKey || event.metaKey || event.ctrlKey;
            if (!key || (down && shortcut) || !main.contains(canvas)) return;
            if (!shortcut) event.preventDefault();
            for (const press of heldCanvasPresses) {
                if (press.kind !== 12 || press.canvas !== canvas || press.key !== key) continue;
                if (!down) cancelCanvasPress(press);
                return;
            }
            if (down) sendCanvasPress({ canvas, target: node.id, kind: 12, key }, words(key, 1), true);
        };
        canvas.onkeydown = event => keyboard(event, true); canvas.onkeyup = event => keyboard(event, false);
        canvas.onblur = () => { for (const press of heldCanvasPresses) if (press.canvas === canvas) cancelCanvasPress(press); };
        paintCanvas(canvas, node); return canvas;
    }
    throw new Error(`Unsupported control ${node.kind}`);
}
const canvasScenes = new WeakMap<HTMLCanvasElement, SceneCanvas>();
const menus = new WeakMap<HTMLElement, SceneMenu>();
let contextPopup: (MenuSelection & { element: HTMLElement }) | undefined;
let pixelScale = 1;

function paintCanvas(canvas: HTMLCanvasElement, node: SceneCanvas): void {
    canvasScenes.set(canvas, node);
    drawCanvas(canvas, node, renderedWindow || node.extent ? pixelScale : 1, !!renderedWindow?.constraints);
}

function showContextMenu(container: HTMLElement, x: number, y: number): void {
    contextPopup?.element.remove();
    const popup = document.createElement("div");
    popup.className = "guest-context-popup";
    popup.setAttribute("popover", "auto");
    popup.setAttribute("aria-label", "Game actions");
    const append = (parent: HTMLElement, items: MenuItem[]): void => {
        for (const item of items) {
            if (item.kind === "separator") parent.append(document.createElement("hr"));
            else if (item.kind === "submenu") {
                const group = document.createElement("details"), label = document.createElement("summary");
                label.textContent = item.label; group.append(label);
                if (item.disabled) label.addEventListener("click", event => event.preventDefault());
                else append(group, item.items);
                parent.append(group);
            } else {
                const button = document.createElement("button"); button.type = "button";
                button.className = "guest-button"; button.disabled = item.disabled;
                button.textContent = item.label; button.dataset.style = item.destructive ? "2" : "0";
                if (item.checked !== undefined) button.setAttribute("aria-pressed", String(item.checked));
                button.onclick = () => {
                    if (contextPopup?.element !== popup) return;
                    popup.hidePopover(); input(2, item.id, new Uint8Array(), { owner: container, revision }).catch(() => {});
                };
                parent.append(button);
            }
        }
    };
    const { items, revision } = menus.get(container)!;
    append(popup, items);
    main.append(popup); contextPopup = { element: popup, owner: container, revision };
    popup.addEventListener("toggle", () => {
        if (!popup.matches(":popover-open")) {
            popup.remove();
            if (contextPopup?.element === popup) contextPopup = undefined;
        }
    });
    popup.showPopover();
    const anchor = container.getBoundingClientRect();
    popup.style.left = `${Math.max(0, Math.min(x || anchor.left, window.innerWidth - popup.offsetWidth))}px`;
    popup.style.top = `${Math.max(0, Math.min(y || anchor.top, window.innerHeight - popup.offsetHeight))}px`;
    popup.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
}
function restoreCheckbox(check: HTMLInputElement, id: number): void {
    if (!main.contains(check)) return;
    // The web browser toggles before dispatching change. On rejection, use the
    // latest accepted scene, not the state captured by an older listener.
    const visit = (node: SceneTreeNode): boolean => {
        if (node.kind === "checkbox" && node.id === id) {
            check.checked = node.state === 1; check.indeterminate = node.state === 2;
            return true;
        }
        return node.children?.some(visit) ?? false;
    };
    if (renderedScene) visit(renderedScene);
}
function updateControl(element: Element, node: SceneNode): void {
    if (node.kind === "input") {
        const editor = editors.get(node.id)!;
        (element as HTMLInputElement).placeholder = node.placeholder; (element as HTMLInputElement).disabled = node.disabled;
        (element as HTMLInputElement).readOnly = node.readOnly;
        element.setAttribute("aria-label", node.placeholder || "Text");
        // Never assign .value while composition or a newer native edit exists.
        if (!editor.composing && editor.revision === editor.acknowledged
            && (element as HTMLInputElement).value !== node.value) (element as HTMLInputElement).value = node.value;
    } else if (node.kind === "checkbox") {
        const check = element.children[0] as HTMLInputElement; check.checked = node.state === 1;
        check.indeterminate = node.state === 2; check.disabled = node.disabled;
        element.children[1].textContent = node.label;
    } else if (node.kind === "button") {
        element.textContent = node.label; (element as HTMLButtonElement).disabled = node.disabled; (element as HTMLElement).dataset.style = String(node.state);
    }
}
function updateScroll(element: HTMLElement, node: SceneScroll): void {
    element.dataset.complexBackground = String(node.complexBackground);
}
function patch(dom: Element | undefined, previous: SceneTreeNode, node: SceneTreeNode): Element {
    if (!dom || previous.kind !== node.kind || previous.id !== node.id) {
        if (previous?.kind === "input") editors.delete(previous.id);
        return createReplacement(dom, node);
    }
    if (node.children) {
        if (node.kind === "scroll") updateScroll(dom as HTMLElement, node);
        if (node.kind === "context-menu") menus.set(dom as HTMLElement, node);
        let index = 0;
        // The usual reply changes values, not order. Do not allocate an index.
        for (; index < node.children.length && index < previous.children!.length; index++) {
            const before = previous.children![index], next = node.children[index];
            if (before.kind !== next.kind || before.id !== next.id) break;
            patch(dom.children[index], before, next);
        }
        if (index < node.children.length && index < previous.children!.length) {
            reconcileSiblings(dom, previous.children!, node.children, index);
        } else {
            for (; index < node.children.length; index++) dom.append(element(node.children[index]));
            while (dom.children.length > node.children.length) dom.lastElementChild!.remove();
        }
    } else if (node.kind === "text") dom.textContent = node.text;
    else if (node.kind === "canvas") paintCanvas(dom as HTMLCanvasElement, node);
    else updateControl(dom, node);
    return dom;
}
function reconcileSiblings(parent: Element, previous: SceneNode[], next: SceneNode[], first: number): void {
    // Only this parent's remaining siblings participate. Decoded scenes cap
    // scratch at 4,096 DOM references, 2,048 flags and 256 control-index entries
    // across the traversal; snapshots and indexes do not outlive this patch.
    const children = Array.from(parent.children), used = new Uint8Array(previous.length), controls = new Map<number, number>();
    const ordered: Element[] | undefined = typeof parent.moveBefore === "function" ? undefined : [];
    for (let index = first; index < previous.length; index++) {
        if (previous[index].id !== undefined) controls.set(previous[index].id!, index);
    }
    for (let index = first; index < next.length; index++) {
        const node = next[index];
        // Anonymous layout/text nodes retain positional semantics; stable IDs
        // identify controls only, never a different parent or an old instance.
        // An unknown control has no old index; that lookup misses and the `before` check below inserts it.
        const oldIndex = node.id === undefined ? index : controls.get(node.id) as number;
        const before = previous[oldIndex];
        if (before && !used[oldIndex] && before.kind === node.kind && before.id === node.id) {
            used[oldIndex] = 1;
            const child = patch(children[oldIndex], before, node);
            if (ordered) ordered.push(child);
            else if (child !== parent.children[index]) moveSibling(parent, child, parent.children[index] ?? null);
        } else {
            const child = element(node);
            if (ordered) ordered.push(child);
            else parent.insertBefore(child, parent.children[index] ?? null);
        }
    }
    for (let index = first; index < previous.length; index++) if (!used[index]) children[index].remove();
    if (ordered) {
        // insertBefore detaches existing nodes. Never detach the focused control
        // or an ancestor: restoring focus afterwards cannot restore an IME session.
        // Place the other siblings from right to left around that fixed anchor.
        const focused = document.activeElement;
        let before: Element | null = null;
        for (let index = ordered.length - 1; index >= 0; index--) {
            const child = ordered[index];
            if (!child.contains(focused) && (child.parentElement !== parent || child.nextElementSibling !== before)) {
                moveSibling(parent, child, before);
            }
            before = child;
        }
    }
}
function moveSibling(parent: Element, child: Element, before: Element | null): void {
    // Both DOM move paths can reset nested offsets or scroll a focused descendant into view.
    // Snapshots live only for this move and contain at most the scene's 256 identified nodes.
    const viewports = Array.from(child.querySelectorAll<HTMLElement>(".guest-scroll"));
    if (child.className === "guest-scroll") viewports.push(child as HTMLElement);
    const offsets = viewports.map(viewport => [viewport.scrollLeft, viewport.scrollTop]);
    if (typeof parent.moveBefore === "function") parent.moveBefore(child, before);
    else parent.insertBefore(child, before);
    for (let index = 0; index < viewports.length; index++) {
        viewports[index].scrollLeft = offsets[index][0]; viewports[index].scrollTop = offsets[index][1];
    }
}
function createReplacement(current: Element | undefined, node: SceneNode): HTMLElement {
    const next = element(node); if (current) current.replaceWith(next); return next;
}
let renderedScene: SceneNode | undefined, renderedWindow: SceneWindow | undefined;
function render(result: RuntimePresentation): void {
    (document.querySelector("#close-app") as HTMLButtonElement).hidden = false;
    const measure = !renderedScene || !!(renderedWindow?.constraints || result.window?.constraints
        || canvasExtent(renderedScene) || canvasExtent(result.scene));
    if (renderedWindow || result.window || canvasExtent(result.scene) || (renderedScene && canvasExtent(renderedScene))) {
        renderedWindow = result.window;
        applyGuestWindow(main, result.scene, renderedWindow);
    }
    if (!renderedScene || !main.firstElementChild) main.replaceChildren(element(result.scene));
    else patch(main.firstElementChild, renderedScene, result.scene);
    renderedScene = result.scene;
    if (contextPopup && (!main.contains(contextPopup.owner) || menus.get(contextPopup.owner)?.revision !== contextPopup.revision)) {
        contextPopup.element.remove(); contextPopup = undefined;
    }
    // Removing a focused canvas need not dispatch blur.
    for (const press of heldCanvasPresses) if (!main.contains(press.canvas)) cancelCanvasPress(press);
    if (measure) scheduleSurface();
    updateFrames();
    for (const [id, editor] of editors) if (!main.contains(editor.element)) editors.delete(id);
    for (const log of result.logs ?? []) console.info(`[Playbit] ${log}`);
    if (appClosing) lockGuestControls();
}
let surfaceDelivery: { worker: Worker; queued: boolean; inFlight: boolean; changed: boolean; acknowledged: string } | undefined;
function canDeliverSurface(): boolean {
    return Boolean(renderedScene && !appClosing && !pageClosing && !guestFailure && !engineFailure);
}
function scheduleSurface(): void {
    if (!canDeliverSurface()) return;
    const delivery = surfaceDelivery?.worker === worker ? surfaceDelivery
        : surfaceDelivery = { worker, queued: false, inFlight: false, changed: false, acknowledged: "" };
    if (delivery.inFlight) { delivery.changed = true; return; }
    if (delivery.queued) return;
    delivery.queued = true;
    queueMicrotask(async () => {
        delivery.queued = false;
        if (delivery.worker !== worker || !canDeliverSurface()) return;
        const bytes = layoutGuestWindow(main, renderedWindow), key = bytes.toString();
        if (key === delivery.acknowledged) return;
        delivery.inFlight = true;
        pixelScale = new DataView(bytes.buffer).getFloat32(8, true);
        try {
            const result = await call("surface", bytes);
            if (result && delivery.worker === worker) { delivery.acknowledged = key; render(result); }
        }
        catch (failure) { if (delivery.worker === worker) error.textContent = (failure as Error).message; }
        finally {
            delivery.inFlight = false;
            if (delivery.worker === worker && delivery.changed) { delivery.changed = false; scheduleSurface(); }
        }
    });
}
window.addEventListener("resize", scheduleSurface);
document.addEventListener("visibilitychange", scheduleSurface);
let frameRequest: number | undefined, frameTarget: number | undefined;
let framePending = false, frameTime = 0;
function canvasFrameTarget(node: SceneTreeNode | undefined): number | undefined {
    if (node?.kind === "canvas" && node.frames === 1) return node.id;
    for (const child of node?.children ?? []) {
        const target = canvasFrameTarget(child);
        if (target !== undefined) return target;
    }
}
function updateFrames(): void {
    const target = document.hidden || pageClosing || appClosing || appClosed || guestFailure || engineFailure
        ? undefined : canvasFrameTarget(renderedScene);
    if (target !== frameTarget) frameTime = 0;
    frameTarget = target;
    if (target === undefined) {
        if (frameRequest !== undefined) cancelAnimationFrame(frameRequest);
        frameRequest = undefined;
        frameTime = 0;
    } else if (frameRequest === undefined && !framePending && inputCount === 0) {
        frameRequest = requestAnimationFrame(animate);
    }
}
async function animate(time: number): Promise<void> {
    frameRequest = undefined;
    const target = frameTarget;
    if (target === undefined || inputCount !== 0) return;
    const elapsed = frameTime ? Math.max(1, Math.min(100000, Math.round((time - frameTime) * 1000))) : 16667;
    frameTime = time;
    framePending = true;
    try { await input(13, target, words(elapsed)); }
    catch { /* The input/worker path presents the failure; current demand is rechecked below. */ }
    finally { framePending = false; updateFrames(); }
}
const statusElement = document.querySelector("#status") as HTMLElement, onboarding = document.querySelector("#onboarding") as HTMLElement;
function status(message: string): void { statusElement.textContent = message; }
function textElement<K extends keyof HTMLElementTagNameMap>(tag: K, text: string): HTMLElementTagNameMap[K] { const node = document.createElement(tag); node.textContent = text; return node; }
function action(label: string, callback: (event: MouseEvent) => unknown): HTMLButtonElement { const node = textElement("button", label); node.type = "button"; node.onclick = callback; return node; }
function storageResetAction(label: string, incompatible: boolean): HTMLButtonElement {
    const reset = action(label, async () => {
        if (incompatible && !window.confirm("Clear Playbit’s saved account and app data from this browser? Unsynced edits will be permanently lost.")) return;
        const retainInvitation = incompatible && invitation?.url;
        if (retainInvitation) history.replaceState(null, '', `/s${new URL(invitation.url).hash}`);
        reset.disabled = true;
        error.textContent = "";
        sessionStorage.setItem(SIGN_IN_AFTER_RESET, retainInvitation ? "invitation" : "1");
        try { await beginWebReset(apClient, webTab); }
        catch (failure) {
            if (pageClosing) return;
            sessionStorage.removeItem(SIGN_IN_AFTER_RESET);
            error.textContent = `Couldn’t clear local data: ${(failure as Error).message}`;
            reset.disabled = false;
        }
    });
    return reset;
}
function showStorageProblem(failure: unknown): boolean {
    const code = (failure as { code?: string } | null)?.code;
    if (code !== "incompatible-installation-format" && code !== "incompatible-storage-format" && code !== "corrupt_store") return false;
    document.body.classList.remove("invited");
    if (code === "incompatible-installation-format") {
        status("This browser’s Playbit installation needs a different version");
        error.textContent = "";
        onboarding.replaceChildren(textElement("p", "Open the invitation in a new browser profile to keep this installation and its offline edits. To start over here, remove this site’s data in your browser settings. This erases the saved sign-in, installation identity and unsynced edits."));
        return true;
    }
    if (code === "incompatible-storage-format") {
        status("Your saved browser data needs a different version of Playbit");
        error.textContent = "";
        onboarding.replaceChildren(
            textElement("p", "To keep this data, open the invitation in another browser profile. To sign in again here, clear the saved account and app data. Any edits saved only in this browser will be lost."),
            storageResetAction("Clear local account data and sign in", true),
        );
        return true;
    }
    document.body.classList.add("store-corrupt");
    (document.querySelector("#invitation") as HTMLElement).hidden = true;
    (document.querySelector("#native-actions") as HTMLElement).hidden = true;
    (document.querySelector("#sign-out") as HTMLElement).hidden = true;
    main.replaceChildren();
    status("");
    error.textContent = "";
    onboarding.replaceChildren(
        textElement("h1", "Oh no…"),
        textElement("p", "The local store has become corrupt. Some data may not have been backed up."),
        storageResetAction("Reset & Sign in", false),
    );
    return true;
}
const updateNotice = document.createElement("section");
updateNotice.setAttribute("aria-live", "polite"); updateNotice.hidden = true;
error.after(updateNotice);
function showUpdate(state: RuntimeUpdateState | null): void {
    updateNotice.hidden = !state;
    updateNotice.replaceChildren();
    if (!state) return;
    updateNotice.append(textElement("p", state.message));
    if (state.kind === "failed") updateNotice.append(action("Retry update", () => {
        showUpdate({ kind: "checking", build_id: state.build_id, message: "Checking for the app update…" });
        call("retry_update").catch(failure => { showUpdate(state); error.textContent = failure.message; });
    }));
}
async function showPersonal(session: string, container: HTMLElement = onboarding): Promise<void> {
    error.textContent = '';
    if (container === onboarding) recoveryVisible = true;
    const selections = await personalSelections(enrollment.account);
    const active = invitation?.invitation && replicaCapability(invitation.invitation).volume.id;
    personalCatalog = (await personalVolumes(await recoveryClient.catalog())).filter(item => item.volume.id !== active);
    const selected = selections[session] ?? [];
    const form = document.createElement("form");
    form.append(textElement("p", "Choose personal volumes for this session. Access is retained across app updates."));
    const message = textElement("p", ""); message.setAttribute("aria-live", "polite"); message.hidden = true;
    let pending = false, canceled = false;
    const progress = (text: string) => { message.hidden = false; message.textContent = text; };
    const controls: { volume: string; access: HTMLSelectElement }[] = [];
    for (const entry of personalCatalog) {
        const label = textElement("label", entry.volume.name), access = document.createElement("select");
        access.setAttribute("aria-label", `${entry.volume.name} access`);
        for (const [value, text] of [["none", "Not connected"], ["read", "Read"], ["write", "Read and write"]]) {
            const option = textElement("option", text); option.value = value; access.append(option);
        }
        const grant = selected.find(grant => grant.volume === entry.volume.id);
        access.value = grant ? grant.write ? "write" : "read" : "none";
        label.append(access); form.append(label); controls.push({ volume: entry.volume.id, access });
    }
    if (!controls.length) form.append(textElement("p", "No personal volumes are available on this account. Recover your private credentials to use volumes from another device."));
    const save = textElement("button", "Save access"), without = action("Continue without personal volumes", () => submit([]));
    save.type = "submit";
    const leave = () => recoveryAction();
    const cancel = action("Cancel", async () => {
        if (!pending) { leave(); return; }
        cancel.disabled = true; progress("Canceling the connection…");
        try {
            const result = await call("cancel_personal");
            if (result.canceled) { canceled = true; leave(); status("Connection canceled. Removed access stays removed"); }
            else progress("Finishing saving access…");
        } catch (failure) { error.textContent = (failure as Error).message; cancel.disabled = false; }
    });
    async function submit(grants: PersonalGrant[]) {
        if (pending) return;
        pending = true; canceled = false; error.textContent = "";
        personalProgress = progress; progress("Connecting selected personal volumes…");
        save.disabled = without.disabled = true;
        for (const { access } of controls) access.disabled = true;
        try {
            const result = await call("select_personal", { session, grants, personal: personalCatalog });
            if (result.canceled) { canceled = true; return; }
            if (result.scene) render(result);
            if (container === onboarding) recoveryAction();
        } catch (failure) {
            if (!canceled) { error.textContent = (failure as Error).message; progress("The connection could not finish. Review access and try again"); }
        } finally {
            if (personalProgress === progress) personalProgress = undefined;
            pending = false; save.disabled = without.disabled = cancel.disabled = false;
            for (const { access } of controls) access.disabled = false;
        }
    }
    form.onsubmit = event => { event.preventDefault(); submit(controls.filter(item => item.access.value !== "none")
        .map(item => ({ volume: item.volume, write: item.access.value === "write" }))); };
    form.append(message, save, without, cancel);
    container.replaceChildren(form);
}
function persistInvitation(joined = opened): void {
    localStorage.setItem("playbit-invitation", JSON.stringify({ url: invitation.url, paused, joined, envelope: invitation.envelope }));
}
function replaceWorker(reason: string): void {
    worker.terminate();
    retireCanvasPresses();
    for (const item of pending.values()) { clearTimeout(item.timer); item.reject(new Error(reason)); } pending.clear();
    for (const handle of streams) apClient.streamClose(handle).catch(() => {}); streams.clear();
    surfaceDelivery = undefined;
    if (renderedScene) {
        submissions.clear(); editors.clear(); inputFailure = undefined;
        renderedScene = undefined; renderedWindow = undefined; guestFailure = undefined; pixelScale = 1;
        contextPopup?.element.remove(); contextPopup = undefined;
        main.replaceChildren(); delete main.dataset.scene;
        applyGuestWindow(main, undefined, undefined);
        main.style.removeProperty("width"); main.style.removeProperty("height");
        updateFrames();
    }
    createWorker();
}
function cancelJoin(): void {
    paused = true; joining = false; attempt++; persistInvitation(false);
    replaceWorker("Joining paused"); status("Joining paused. Your invitation is saved");
    onboarding.replaceChildren(action("Resume joining", () => { paused = false; join(); }));
}
function showJoin(): void {
    error.textContent = "";
    status(paused ? "Joining paused. Your invitation is saved" : "Ready to join. Only this app’s shared volumes will be connected");
    onboarding.replaceChildren(action(paused ? "Resume joining" : "Join", () => { paused = false; join(); }));
}
async function afterAuthentication(): Promise<boolean> {
    (document.querySelector('#sign-out') as HTMLButtonElement).hidden = false;
    const account = enrollment.account;
    canJoin = false;
    let accepted;
    try { accepted = await apClient.agreement(account, false); }
    catch (failure) {
        if (showStorageProblem(failure)) return false;
        status("Couldn’t check your agreement");
        error.textContent = (failure as Error).message;
        onboarding.replaceChildren(action("Try again", useWeb));
        return false;
    }
    error.textContent = "";
    if (!accepted) {
        status("Review the Playbit User Agreement");
        const link = textElement("a", `Playbit User Agreement · revision ${AGREEMENT.revision}`);
        link.href = `/help/user-agreement/${AGREEMENT.revision}/`;
        link.target = "_blank";
        link.rel = "noopener";
        const accept = action("Agree and continue", async () => {
            accept.disabled = true;
            error.textContent = "";
            try {
                await apClient.agreement(account, true);
                await afterAgreement();
            } catch (failure) {
                if (showStorageProblem(failure)) return;
                error.textContent = (failure as Error).message;
                accept.disabled = false;
            }
        });
        onboarding.replaceChildren(link, textElement("p", "Choose Agree and continue to accept this agreement."), accept);
        return false;
    }
    return afterAgreement();
}
async function afterAgreement(): Promise<boolean> {
    if (recoveryClient?.account !== enrollment.account) {
        recoveryClient = new RecoveryClient(apClient, enrollment.account);
    }
    try { await apClient.watchAccount(enrollment.account, refreshAccountPending, accountNoticeFailed); }
    catch { accountNoticeFailed(); }
    const recovery = await recoveryClient.status();
    if (recovery.kind === "ready") {
        if (!recovery.pending_save) void apClient.onboarding(5).catch(() => {});
        canJoin = true;
        if (invitation) showJoin(); else await showRecoveredApps();
        return true;
    }
    canJoin = false;
    await showRecovery(recovery);
    return false;
}
async function showRecovery(known?: RecoveryStatus): Promise<void> {
    recoveryVisible = true;
    const recovery = known ?? await recoveryClient.status();
    error.textContent = "";
    if (recovery.kind === "setup") {
        status("Save your account key before joining");
        const secret = await recoveryClient.prepare(), field = document.createElement("textarea");
        field.readOnly = true; field.rows = 3; field.value = secret; field.setAttribute("aria-label", "Account key");
        field.style.cssText = "font:inherit;width:100%;box-sizing:border-box;padding:12px";
        const copy = action("Copy account key", async () => {
            try { await navigator.clipboard.writeText(secret); copy.textContent = "Copied"; }
            catch { error.textContent = "Select and copy the account key above"; }
        });
        const label = document.createElement("label"), acknowledgment = document.createElement("input");
        acknowledgment.type = "checkbox"; acknowledgment.name = "recovery-saved";
        label.append(acknowledgment, textElement("span", "I’ve saved my account key"));
        const save = action("Save and continue", async () => {
            save.disabled = true;
            try { await recoveryClient.save(); void apClient.onboarding(5).catch(() => {}); canJoin = true; if (invitation) showJoin(); else await showRecoveredApps(); }
            catch (failure) { error.textContent = (failure as Error).message; save.disabled = !acknowledgment.checked; }
        });
        save.disabled = true; acknowledgment.onchange = () => { save.disabled = !acknowledgment.checked; };
        onboarding.replaceChildren(textElement("p", "Keep this secret somewhere safe. Playbit never receives your account key. If you lose every copy of this key, signing in alone cannot recover your private data."), field, copy, label, save);
        return;
    }
    if (recovery.kind === "ready") {
        await showRecoveredApps();
        status(recovery.pending_save ? "Private credentials are available. Their backup needs to finish saving" : "Recovery is set up. Keep your account key somewhere safe.");
        if (recovery.pending_save) onboarding.prepend(action("Retry backup", async () => {
            try { await recoveryClient.backup(); await showRecovery(); } catch (failure) { error.textContent = (failure as Error).message; }
        }));
        return;
    }
    status("Recover your private data");
    const form = document.createElement("form"), label = textElement("label", "Account key"), secret = document.createElement("input");
    secret.type = "text"; secret.autocomplete = "off"; secret.spellcheck = false; secret.name = "recovery-secret"; secret.required = true; secret.placeholder = "a…"; label.append(secret);
    const submit = textElement("button", "Recover private data"); submit.type = "submit"; form.append(label, submit);
    form.onsubmit = async event => {
        event.preventDefault(); submit.disabled = true; error.textContent = "";
        try {
            await recoveryClient.recover(secret.value); canJoin = true;
            if (opened) { recoveryAction(); status("Private credentials recovered"); }
            else if (invitation) showJoin(); else await showRecoveredApps();
        }
        catch (failure) { error.textContent = (failure as Error).message; submit.disabled = false; }
    };
    onboarding.replaceChildren(textElement("p", "You’re signed in. Your saved account key is also needed to unlock existing private data."), form);
    if (invitation && !opened) onboarding.append(textElement("p", "This invitation can unlock its shared data. Your other private data will stay locked, and your existing backup will be preserved."), action("Join this invitation", () => { canJoin = true; showJoin(); }));
    if (opened) onboarding.append(action("Return to app", () => { recoveryAction(); }));
}

let recoveryRender = 0;
async function showRecoveredApps(): Promise<void> {
    await recoveryClient.catalog();
    if (!opened) {
        status("Your private credentials are available");
        if (recoveryListAccount !== enrollment.account) {
            if (recoveryListAccount) replaceWorker("Recovery account changed");
            recoveryListAccount = enrollment.account;
            await call("watch_recovery", { account: enrollment.account });
        }
    }
    const message = textElement("p", "");
    message.id = "recovered-app-message";
    const controls = document.createElement("div");
    controls.id = "recovered-app-list";
    onboarding.replaceChildren(message, controls);
    await refreshRecoveredAppVisibility();
    if (!message.isConnected) return;
    if (invitation && !opened) onboarding.append(action("Continue invitation", showJoin));
    if (opened) onboarding.append(action("Return to app", () => { recoveryAction(); }));
}
async function refreshRecoveredAppVisibility(): Promise<void> {
    const controls = onboarding.querySelector("#recovered-app-list"), message = onboarding.querySelector("#recovered-app-message");
    if (!controls || !message) return;
    const generation = ++recoveryRender, account = enrollment.account;
    const { catalog } = await recoveryClient.state();
    const removed = await removedAppsIn(account, catalog.invitations.map(retained => retained.session.app));
    if (!controls.isConnected || generation !== recoveryRender || enrollment.account !== account) return;
    const visible = catalog.invitations.filter(({ session: { app } }) => !removed.has(`${app.publisher}/${app.app_id}`));
    message.textContent = visible.length ? "Open a recovered app. Its shared data will be restored through Playbit."
        : "Your account key is ready. Open an invitation to join an app.";
    const buttons = Array.from(controls.querySelectorAll("button"));
    const label = (retained: Invitation) => replicaCapability(retained).volume.name || "Open shared app";
    if (buttons.length === visible.length && visible.every((entry, i) =>
        buttons[i].dataset.session === entry.session.id && buttons[i].textContent === label(entry))) return;
    controls.replaceChildren(...visible.map(retained => {
        const button = action(label(retained), async () => {
            try {
                const { catalog } = await recoveryClient.state();
                const entry = catalog.invitations.find(item => item.session.id === retained.session.id);
                if (!entry || !button.isConnected || enrollment.account !== account) return;
                const parsed = await retainedInvitation(entry, selectedChannel);
                const removed = await removedAppsIn(account, [entry.session.app]);
                if (removed.size || !button.isConnected || enrollment.account !== account) return;
                if (opened) { await call("close"); replaceWorker("Opening another recovered app"); opened = false; }
                invitation = parsed; paused = false; canJoin = true; persistInvitation(false); showInvitationHeader(); showJoin();
            } catch (failure) { error.textContent = (failure as Error).message; }
        });
        button.dataset.session = retained.session.id;
        return button;
    }));
}
function recoveryAction(): void {
    recoveryVisible = false;
    onboarding.replaceChildren(action("Recovery", () => showRecovery().catch(failure => { error.textContent = failure.message; })),
        action("Personal volumes", () => showPersonal(runtimeSession).catch(failure => { error.textContent = failure.message; })));
    if (replicationState) showReplication(replicationState);
}
function showEmail(): void {
    status(invitation ? "Sign in to join this session" : "Sign in to Playbit");
    const form = document.createElement("form");
    const label = textElement("label", "Email address"), email = document.createElement("input");
    email.type = "email"; email.name = "email"; email.autocomplete = "email"; email.required = true; label.append(email);
    const keyLabel = textElement("label", "Account key"), accountKey = document.createElement("input");
    accountKey.type = "password"; accountKey.autocomplete = "current-password"; accountKey.name = "account-key"; accountKey.spellcheck = false; accountKey.maxLength = 1024; keyLabel.append(accountKey);
    const submit = textElement("button", "Continue"); submit.type = "submit";
    const disclosure = textElement("p", "We use your email for sign-in and account notices. ");
    const privacy = textElement("a", "Privacy policy");
    privacy.href = "https://playbit.app/help/privacy"; privacy.target = "_blank"; privacy.rel = "noopener";
    disclosure.append(privacy);
    form.append(label, keyLabel, textElement("p", "Leave the account key blank to get a code by email."), disclosure, submit);
    form.onsubmit = async event => {
        event.preventDefault(); submit.disabled = true; error.textContent = "";
        status(accountKey.value.trim() ? "Signing in…" : "Sending code…");
        try {
            if (invitation?.url) history.replaceState(null, '', `/s${new URL(invitation.url).hash}`);
            const key = accountKey.value.trim();
            if (key) parseAccountKey(key);
            sessionStorage.setItem(SIGN_IN_HANDOFF, JSON.stringify({ email: email.value.trim().toLowerCase(), accountKey: key }));
            accountKey.value = "";
            await beginWebReset(apClient, webTab);
        }
        // The code is requested after cleanup reloads this page, so anything
        // a failure on a live page is a failure to clear the previous account.
        catch (failure) {
            // Closing the old AP port rejects pending requests during the reset
            // reload. Its successor still needs this one-use sign-in form.
            if (pageClosing) return;
            sessionStorage.removeItem(SIGN_IN_HANDOFF); status("Sign-in could not start. Try again"); error.textContent = (failure as Error).message; submit.disabled = false;
        }
    };
    onboarding.replaceChildren(form); email.focus();
}
function showCode(email: string): void {
    status(`Enter the six-digit code sent to ${email}`);
    const form = document.createElement("form"), label = textElement("label", "Sign-in code"), code = document.createElement("input");
    code.type = "text"; code.autocomplete = "one-time-code"; code.name = "code"; code.required = true; code.inputMode = "numeric"; code.pattern = "[0-9]{6}"; code.placeholder = "6-digit code"; label.append(code);
    const submit = textElement("button", "Sign in"); submit.type = "submit"; form.append(label, submit);
    form.onsubmit = async event => {
        event.preventDefault(); submit.disabled = resend.disabled = true; error.textContent = "";
        status("Signing in…");
        try { enrollment = await apClient.finishCode(code.value) as Enrollment; status("Signed in. Checking private access…"); await afterAuthentication(); }
        catch (failure) { status("Sign-in needs attention. Try again"); error.textContent = (failure as Error).message; submit.disabled = resend.disabled = false; }
    };
    const resend = action("Send another code", async () => {
        submit.disabled = resend.disabled = true; error.textContent = ""; status("Sending code…");
        try { await apClient.requestCode({ email }); status(`A new code was sent to ${email}`); }
        catch (failure) { status("Code could not be sent. Try again"); error.textContent = (failure as Error).message; }
        finally { submit.disabled = resend.disabled = false; }
    });
    onboarding.replaceChildren(form, resend); code.focus();
}
async function useWeb(): Promise<boolean | undefined> {
    try {
        apClient ??= new ApClient(selectedChannel);
        enrollment = await apClient.connect() as Enrollment;
    } catch (failure) {
        if (showStorageProblem(failure)) return false;
        if (/WebTransport|shared AP connections/.test((failure as Error).message)) {
            status("Browser participation is unavailable");
            const next = configuration(selectedChannel).macos_manifest ? "Open this invitation in Playbit" : "Open this invitation in a supported browser";
            error.textContent = `${(failure as Error).message}. ${next}`;
            return;
        }
        showEmail();
        return false;
    }
    if (enrollment) {
        try { return await afterAuthentication(); }
        catch (failure) {
            if (!showStorageProblem(failure)) { error.textContent = (failure as Error).message; onboarding.replaceChildren(action("Try again", useWeb)); }
        }
    }
    return false;
}
const waiting = {
    waiting_registration: "Waiting for this session to be shared…",
    waiting_app: "Waiting for the app to finish uploading…",
    waiting_database: "Waiting for shared data to become available…",
};
function nativeInvitationActions(label = "Open in Playbit"): HTMLAnchorElement[] {
    if (!invitation.uri) return [];
    const link = textElement("a", label); link.href = invitation.uri; return [link];
}
function showJoinProblem(kind: string, message = ""): void {
    error.textContent = message;
    if (kind === "waiting_peer") {
        const needsPeer = invitation.invitation !== null
            && !replicaCapability(invitation.invitation).volume.share_key_with_service;
        status(needsPeer ? "This session needs an online peer" : "Waiting for shared data to become available");
        const explanation = needsPeer
            ? "A volume’s key has not been shared with Playbit’s servers, so an authorized device must supply its data. Web peer catch-up is not available yet."
            : "Ask the sender to bring an authorized device online and finish sharing this session.";
        const handoff = !invitation.uri ? "" : needsPeer
            ? " Open this session in installed Playbit while a participant’s device is online."
            : " If its data is available only from a peer, open it in installed Playbit; Playbit cannot download peer-based data on the web.";
        onboarding.replaceChildren(textElement("p", explanation + handoff + " Your invitation is saved."),
            ...nativeInvitationActions(), action("Check again", join), action("Cancel for now", cancelJoin));
    } else if (kind === "unsupported") {
        status("This app cannot run on the web");
        const handoff = invitation.uri ? " You can also try installed Playbit; support depends on the app." : "";
        onboarding.replaceChildren(textElement("p", "Ask the app’s author for a web-compatible version." + handoff + " Your invitation is saved."),
            ...nativeInvitationActions("Try in Playbit"), action("Check for an updated app", join), action("Cancel for now", cancelJoin));
    } else if (kind === "denied") {
        status("Access to this session is unavailable");
        const copy = action("Copy account ID", async () => {
            try { await navigator.clipboard.writeText(enrollment.account); copy.textContent = "Account ID copied"; }
            catch { error.textContent = `Give the sender this account ID: ${enrollment.account}`; }
        });
        onboarding.replaceChildren(textElement("p", "Ask the sender to check access for your signed-in account and this app version. Share your account ID so they can identify it. Your invitation is saved."),
            copy, action("Check access again", join), action("Cancel for now", cancelJoin));
    } else {
        status("Could not join yet. Your invitation is saved");
        onboarding.replaceChildren(action("Try again", join), action("Cancel", cancelJoin));
    }
}
async function join(): Promise<void> {
    if (joining || paused) return;
    if (!canJoin) { await afterAuthentication(); return; }
    if (recoveryListAccount) replaceWorker("Opening invitation");
    joining = true; persistInvitation(false); error.textContent = "";
    const current = ++attempt;
    status("Checking invitation…");
    onboarding.replaceChildren(action("Cancel", cancelJoin));
    try {
        const currentAccount = await apClient.connect() as Enrollment;
        if (currentAccount.account !== enrollment.account) {
            enrollment = currentAccount;
            joining = false;
            await afterAuthentication();
            return;
        }
        const proof = await sessionProof(invitation.privateKey, enrollment.account);
        // The creator's envelope exists once the session is registered; it is
        // read then, so a link opened early shows its details as they appear.
        const readInvitation = async () => {
            if (invitation.invitation) return;
            const response = await apClient.request({ op: "read_invitation", operation_id: encodeId(crypto.getRandomValues(new Uint8Array(16))),
                session_id: invitation.session_id, proof });
            // Service JSON; `openEnvelope` validates the envelope text.
            invitation.invitation = await openEnvelope(invitation, (response.data as { envelope?: string } | undefined)?.envelope);
            invitation.envelope = (response.data as { envelope?: string }).envelope;
            persistInvitation(false); showInvitationHeader();
        };
        const readiness = await apClient.watchInvitation({ op: "watch_invitation", operation_id: encodeId(crypto.getRandomValues(new Uint8Array(16))),
            session_id: invitation.session_id, proof });
        streams.add(readiness);
        try {
            if (current !== attempt || paused) return;
            for (;;) {
                const state = await apClient.streamInvitation(readiness);
                if (current !== attempt || paused) return;
                if (state.kind !== "waiting_registration") await readInvitation();
                if (current !== attempt || paused) return;
                if (state.kind === "ready") break;
                if (state.kind === "denied" || state.kind === "waiting_peer") {
                    joining = false; showJoinProblem(state.kind, state.message as string | undefined); return;
                }
                if (!(state.kind in waiting)) throw new Error("Invalid invitation readiness state");
                status(waiting[state.kind as keyof typeof waiting]);
            }
        } finally {
            streams.delete(readiness);
            await apClient.streamClose(readiness).catch(() => {});
        }
        status("Opening app…");
        // `readInvitation` above opened the envelope before the readiness stream reported `ready`.
        const replica = replicaCapability(invitation.invitation!).volume.id;
        const catalog = await recoveryClient.catalog();
        personalCatalog = (await personalVolumes(catalog)).filter(item => item.volume.id !== replica);
        const scope = catalog.scopes?.find(scope => scope.kind === "session" && scope.id === invitation.session_id);
        const creationScope = scope?.kind === "session" ? scope : undefined;
        const userVolume = creationScope?.user_volume ? catalog.volumes.find(item => item.volume.id === creationScope.user_volume) : undefined;
        const result = await call("open", { profile: invitation.session_id, account: enrollment.account, invitation: invitation.invitation!,
            personal: personalCatalog, creationScope, userVolume });
        runtimeSession = result.identity.session;
        showOfflineEdits(document.querySelector('#offline-edits') as HTMLElement, result.identity,
            personalCatalog.map(item => item.volume), call);
        if (current !== attempt) return;
        opened = true; joining = false; persistInvitation();
        // An account notice may have arrived between the worker's opening snapshot and this receipt.
        void refreshAccountPending().catch(accountNoticeFailed);
        recoveryAction(); render(result); status("Changes saved on this web browser · Connecting live updates…");
        (document.querySelector("#identity") as HTMLElement).textContent = JSON.stringify(result.identity, null, 2);
        try { await recoveryClient.rememberInvitation(invitation.invitation!); }
        catch (failure) { error.textContent = `The app opened, but its credential backup needs retrying: ${(failure as Error).message}`; }
        void call("reconnect");
    } catch (failure) {
        if (current !== attempt) return;
        console.error("Could not join invitation", failure);
        joining = false;
        replaceWorker("Invitation opening failed");
        if (showStorageProblem(failure)) return;
        const kind = (failure as { code?: string }).code === "unsupported" || /^Unsupported guest view operation \d+$/.test((failure as Error).message) ? "unsupported"
            : /access denied|participant revoked|root version provisioning/i.test((failure as Error).message) ? "denied" : "failed";
        showJoinProblem(kind, (failure as Error).message);
    }
}
async function refreshAccountPending(): Promise<void> {
    if (opened) await call('account_changed');
    await refreshRecoveredAppVisibility();
    try { accountPending = Boolean(await pendingAccountRecord('personal', enrollment.account)); }
    catch { accountPending = undefined; }
    if (!guestFailure && replicationState?.kind === 'live' && (!recoveryVisible || personalProgress)) status(liveReplicationMessage());
}
function accountNoticeFailed(): void {
    accountPending = undefined;
    if (!guestFailure && replicationState?.kind === 'live' && !recoveryVisible) status(liveReplicationMessage());
}
function liveReplicationMessage(): string {
    return accountPending === true ? "App changes up to date · Personal access saved locally; account synchronization pending"
        : accountPending === undefined ? "App changes up to date · Account synchronization status unavailable"
        : "Up to date · Shared changes received and saved";
}
function showReplication(state: ReplicationState): void {
    replicationState = state;
    if (state.message) document.documentElement.dataset.replicationFailure = replicationDiagnostic(state);
    if (engineFailure) { error.textContent = engineFailure; return; }
    if (guestFailure) { error.textContent = guestFailure; status("App needs attention"); return; }
    if (recoveryVisible && !personalProgress) return;
    if (state.kind === "live") {
        status(liveReplicationMessage()); error.textContent = "";
    } else {
        status(state.kind === "catching_up" ? "Changes saved on this web browser · Synchronizing…"
            : state.kind === "reconnecting" ? "Changes saved on this web browser · Reconnecting…"
            : "Changes saved on this web browser · Synchronization needs attention");
        error.textContent = state.kind === "catching_up" || state.kind === "reconnecting" ? "" : state.message ?? "";
    }
}
window.addEventListener("online", () => { if (opened || recoveryListAccount) void call("reconnect"); });
document.addEventListener("visibilitychange", () => { updateFrames(); if ((opened || recoveryListAccount) && document.visibilityState === "visible") void call("reconnect"); });
window.addEventListener("pageshow", () => { if (opened || recoveryListAccount) void call("reconnect"); });
window.addEventListener('pagehide', event => { if (!event.persisted) { pageClosing = true; updateFrames(); if (recoveryListAccount) worker.terminate(); accountSignal.close(); apClient?.close(); } });
function showInvitationHeader(): void {
    (document.querySelector("#invitation-title") as HTMLElement).textContent = "You're invited";
    (document.querySelector("#invitation-detail") as HTMLElement).textContent = `Join ${(invitation.invitation && replicaCapability(invitation.invitation).volume.name) || "a shared app"}. Your personal data stays separate unless you choose to connect it.`;
    const installed = document.querySelector("#open-installed") as HTMLAnchorElement;
    installed.hidden = !invitation.uri;
    if (invitation.uri) installed.href = invitation.uri;
}
(document.querySelector('#sign-out') as HTMLButtonElement).onclick = async event => {
    if (!window.confirm("Sign out and clear Playbit’s saved account and app data from this browser? Unsynced edits will be permanently lost.")) return;
    (event.target as HTMLButtonElement).disabled = true; error.textContent = ''; status('Signing out and clearing web browser data…');
    sessionStorage.removeItem(SIGN_IN_HANDOFF);
    try { await beginWebReset(apClient, webTab); }
    catch (failure) { error.textContent = (failure as Error).message; (event.target as HTMLButtonElement).disabled = false; }
};
async function start(): Promise<RuntimeOpenResult | undefined> {
    apClient = webAccountClient;
    const reset = initialReset;
    const nextEmail = sessionStorage.getItem(SIGN_IN_HANDOFF);
    if (reset.error || nextEmail || sessionStorage.getItem(SIGN_IN_AFTER_RESET)) {
        document.body.classList.remove("invited");
    }
    if (reset.error) {
        sessionStorage.removeItem(SIGN_IN_HANDOFF);
        if (showStorageProblem(reset)) return;
        error.textContent = reset.error; status('Web browser data could not be cleared');
        onboarding.replaceChildren(action('Clear web browser data and try again', () => beginWebReset(apClient, webTab)));
        return;
    }
    const signIn = sessionStorage.getItem(SIGN_IN_AFTER_RESET);
    if ((nextEmail || signIn === "invitation") && isSessionPath(location.pathname) && location.hash) {
        invitation = await parseInvitation(`${configuration(selectedChannel).website}/s${location.hash}`, selectedChannel); showInvitationHeader();
    }
    if (signIn) {
        sessionStorage.removeItem(SIGN_IN_AFTER_RESET);
        showEmail();
        return;
    }
    if (nextEmail) {
        sessionStorage.removeItem(SIGN_IN_HANDOFF);
        status('Sending code…');
        try {
            const { email, accountKey } = JSON.parse(nextEmail) as { email: string; accountKey: string };
            if (accountKey) {
                status("Signing in…");
                enrollment = await apClient.signInWithKey(email, accountKey) as Enrollment;
                await new RecoveryClient(apClient, enrollment.account).recover(accountKey);
                await afterAuthentication();
            } else { await apClient.requestCode({ email }); showCode(email); }
        }
        catch (failure) {
            if (showStorageProblem(failure)) return;
            if (enrollment) await afterAuthentication(); else showEmail();
            error.textContent = (failure as Error).message;
        }
        return;
    }
    // CSS loads a checked glyph only when needed. Decode both states before
    // the app is usable so its first offline interaction has the real asset.
    await Promise.all(["check-on.svg", "check-mixed.svg"].map(async path => {
        const image = new Image(); image.src = new URL(path, import.meta.url).href; await image.decode();
    }));
    if (parameters.has("app")) {
        const result = await call("open", { app, profile });
        render(result); status("Saved on this web browser · Local execution");
        showOfflineEdits(document.querySelector('#offline-edits') as HTMLElement, result.identity, [], call);
        (document.querySelector("#identity") as HTMLElement).textContent = JSON.stringify({ ...result.identity, ...result.timing, storageBytes: result.storageBytes }, null, 2);
        return result;
    }
    // What `persistInvitation` and the invitation form store; `null` when nothing is saved.
    let saved: { url: string; paused?: boolean; joined?: boolean; envelope?: string } | null | undefined;
    try { saved = JSON.parse(localStorage.getItem("playbit-invitation") as string); } catch {}
    const url = isSessionPath(location.pathname) && location.hash ? `${configuration(selectedChannel).website}/s${location.hash}` : saved?.url;
    if (!url) {
        status("Open an invitation to use an app with others");
        const form = document.createElement("form"), field = document.createElement("input"), button = textElement("button", "Open invitation");
        field.type = "url"; field.placeholder = "Paste invitation link"; field.required = true; field.setAttribute("aria-label", "Invitation link");
        button.type = "submit"; form.append(field, button);
        form.onsubmit = async event => { event.preventDefault(); try { const parsed = await parseInvitation(field.value, selectedChannel); localStorage.setItem("playbit-invitation", JSON.stringify({ url: parsed.url })); location.reload(); } catch (failure) { error.textContent = (failure as Error).message; } };
        onboarding.replaceChildren(form, action("Sign in to recover apps", useWeb)); return;
    }
    invitation = await parseInvitation(url, selectedChannel);
    const remembered = saved?.url === invitation.url;
    if (remembered && saved!.envelope) {
        try { invitation.invitation = await openEnvelope(invitation, saved!.envelope); invitation.envelope = saved!.envelope; } catch {}
    }
    paused = Boolean(remembered && saved!.paused); persistInvitation(Boolean(remembered && saved!.joined));
    showInvitationHeader();
    const browser = action("Join in web browser (experimental)", async () => {
        browser.disabled = true;
        document.body.classList.remove("invited");
        status("Connecting…");
        try { await useWeb(); }
        finally { browser.disabled = false; }
    });
    browser.id = "join-browser";
    onboarding.replaceChildren(browser);
    const resume = remembered && saved!.joined && !paused;
    document.body.classList.toggle("invited", !resume);
    if (resume) {
        status("Reopening session…");
        if (await useWeb()) await join();
    } else {
        status("");
    }
}
const ready = start().catch(failure => {
    if (!showStorageProblem(failure)) { error.textContent = failure.message; status("Could not open this invitation"); }
    throw failure;
});
(document.querySelector("#close-app") as HTMLButtonElement).onclick = () => { void closeApp().catch(() => {}); };
window.playbit = { ready, input, close: closeApp };
