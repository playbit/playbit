import { MAX_TEXT_BYTES, MAX_TEXT_EVENT_BYTES } from "./text-input.ts";
const decoder = new TextDecoder("utf-8", { fatal: true });
export const MAX_VIEW_BYTES = 4 * 1024 * 1024;
export const MAX_WINDOW_EDGE = 2 ** 24;
const MAX_CANVAS_EDGE = 4096;
const MAX_NODES = 2048;

export interface SceneRect { kind: "rect"; x: number; y: number; width: number; height: number; color: number }
export interface SceneCanvasText { kind: "text"; x: number; y: number; width: number; height: number; color: number; fontSize: number; bold: boolean; text: string }
export interface ScenePixels { kind: "pixels"; x: number; y: number; width: number; height: number; sourceWidth: number; sourceHeight: number; rgba: Uint8Array<ArrayBuffer> }
export type SceneCanvasItem = SceneRect | SceneCanvasText | ScenePixels;
// Only scroll containers carry an identity; layout and text nodes stay anonymous.
export interface SceneLayout { kind: "row" | "column"; children: SceneNode[]; close: number; id?: undefined }
export interface SceneScroll { kind: "scroll"; id: number; complexBackground: boolean; children: SceneNode[]; close: 25 }
export type MenuItem = { kind: "separator" } | { kind: "submenu"; label: string; icon: number; disabled: boolean; items: MenuItem[] } | MenuAction;
export interface MenuAction { kind: "menu-action"; revision: number; id: number; label: string; icon: number; disabled: boolean; checked?: boolean; destructive: boolean }
export interface SceneMenu { kind: "context-menu"; revision: number; items: MenuItem[]; children: SceneNode[]; close: 17; id?: undefined }
export type SceneContainer = SceneLayout | SceneScroll | SceneMenu;
export interface SceneText { kind: "text"; style: number; text: string; id?: undefined }
export interface SceneInput { kind: "input"; id: number; disabled: boolean; readOnly: boolean; value: string; placeholder: string }
export interface SceneToggle { kind: "button" | "checkbox"; id: number; state: number; disabled: boolean; label: string }
// A canvas is never disabled; the field stays undefined so validateInput reads it uniformly.
export interface SceneCanvas { kind: "canvas"; id: number; width: number; height: number; frames: 0 | 1; extent?: "window" | "viewport"; clearColor?: number; items: SceneCanvasItem[]; disabled?: undefined }
// Identified nodes share one namespace; scroll viewports accept no guest input.
export type SceneControl = SceneInput | SceneToggle | SceneCanvas | SceneScroll | MenuAction;
export type SceneNode = SceneContainer | SceneText | Exclude<SceneControl, MenuAction>;
export interface WindowConstraints { minWidth: number; minHeight: number; maxWidth: number; maxHeight: number; aspectRatio: number }
export interface SceneWindow { style?: number; constraints?: WindowConstraints; theme?: Map<number, number> }
export interface Scene { root: SceneNode; controls: Map<number, SceneControl>; window?: SceneWindow }

export function surfaceBytes(width: number, height: number, scale: number, inputs: number): Uint8Array {
    const bytes = new Uint8Array(16), data = new DataView(bytes.buffer);
    data.setUint32(0, width, true);
    data.setUint32(4, height, true);
    data.setFloat32(8, scale, true);
    data.setUint32(12, inputs, true);
    return bytes;
}

export function validateSurface(bytes: Uint8Array): void {
    if (bytes.length !== 16) throw new Error("Invalid guest surface");
    const data = new DataView(bytes.buffer, bytes.byteOffset, bytes.length), scale = data.getFloat32(8, true);
    if (data.getUint32(0, true) > MAX_WINDOW_EDGE || data.getUint32(4, true) > MAX_WINDOW_EDGE
        || !Number.isFinite(scale) || scale <= 0 || (data.getUint32(12, true) & ~7) !== 0) throw new Error("Invalid guest surface");
}

let menuSequence = 0;

export function decodeScene(bytes: Uint8Array, previous?: Scene | null): Scene {
    if (bytes.length > MAX_VIEW_BYTES || decoder.decode(bytes.subarray(0, 4)) !== "PBV1") throw new Error("Invalid guest scene");
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const stack: SceneContainer[] = [];
    const controls: Map<number, SceneControl> = new Map();
    let root: SceneNode | undefined, count = 0;
    let window: SceneWindow | undefined;
    const canvasTotals: CanvasTotals = { items: 0, textBytes: 0 };
    const control = (node: SceneControl & { id: number }): void => {
        if (node.id) {
            if (controls.size >= 256) throw new Error("Too many guest controls");
            if (controls.has(node.id)) throw new Error("Duplicate guest control identity");
            controls.set(node.id, node);
        }
    };
    const add = (node: SceneNode): void => {
        if (stack.length) stack.at(-1)!.children.push(node);
        else if (root) throw new Error("Multiple scene roots");
        else if (!("children" in node) && !(node.kind === "canvas" && node.extent)) throw new Error("Guest control requires a container");
        else root = node;
        if (node.id) control(node);
    };
    for (let offset = 4; offset < bytes.length;) {
        if (++count > MAX_NODES || offset + 9 > bytes.length) throw new Error("Invalid guest scene size");
        const type = bytes[offset], id = view.getUint32(offset + 1, true), length = view.getUint32(offset + 5, true);
        offset += 9;
        if (offset + length > bytes.length) throw new Error("Truncated guest scene");
        const data = bytes.subarray(offset, offset + length), d = new DataView(data.buffer, data.byteOffset, data.length);
        offset += length;
        switch (type) {
            case 10: case 27: case 28:
                if (id || stack.length || root) throw new Error("Window metadata must precede the root");
                window ??= {};
                decodeWindowMetadata(window, type, d);
                break;
            case 1: case 6: case 12: case 24: {
                if (stack.length >= 32) throw new Error("Invalid guest container");
                const node = decodeContainer(type, id, data);
                add(node);
                stack.push(node);
                break;
            }
            case 16: {
                if (id || stack.length >= 32) throw new Error("Invalid guest context menu");
                const menu = decodeMenu(data, d, MAX_NODES - count, control);
                count += menu.count;
                const node: SceneMenu = { kind: "context-menu", revision: 0, items: menu.items, children: [], close: 17 };
                add(node); stack.push(node);
                break;
            }
            case 4: case 7: case 13: case 17: case 25: {
                const node = stack.pop();
                if (id || length || node?.close !== type) throw new Error("Invalid guest nesting");
                if (node.kind === "scroll" && node.children.length !== 1) throw new Error("Scroll viewport requires one child");
                if (node.kind === "context-menu") {
                    if (node.children.length !== 1) throw new Error("Context menu requires one child");
                    const before = previousMenu(node, stack, previous?.root);
                    node.revision = before && sameMenu(before.items, node.items) ? before.revision : ++menuSequence;
                    setActionRevisions(node.items, node.revision);
                }
                break;
            }
            case 2:
                if (id || length < 1 || data[0] > 5) throw new Error("Invalid guest text");
                add({ kind: "text", style: data[0], text: decoder.decode(data.subarray(1)) });
                break;
            case 5:
                add(decodeTextInput(id, data, d));
                break;
            case 3: case 8:
                if (!id || length < 2 || data[0] > 2 || data[1] > 1) throw new Error("Invalid guest control");
                add({ kind: type === 3 ? "button" : "checkbox", id, state: data[0], disabled: Boolean(data[1]), label: decoder.decode(data.subarray(2)) });
                break;
            case 22: {
                const canvas = decodeCanvas(id, data, d, canvasTotals);
                if (canvas.extent && stack.some(node => node.kind !== "context-menu")) throw new Error("Extended canvas must be the root");
                add(canvas);
                break;
            }
            default:
                throw new Error(`Unsupported guest view operation ${type}`);
        }
    }
    if (stack.length || !root) throw new Error("Incomplete guest scene");
    return window ? { root, controls, window } : { root, controls };
}

function decodeWindowMetadata(window: SceneWindow, type: number, data: DataView): void {
    if (type === 10) {
        if (window.theme || data.byteLength % 5) throw new Error("Invalid guest theme");
        const theme = new Map<number, number>();
        for (let at = 0; at < data.byteLength; at += 5) {
            const role = data.getUint8(at);
            if (role >= 26 || theme.has(role)) throw new Error("Invalid guest theme role");
            theme.set(role, data.getUint32(at + 1, true));
        }
        window.theme = theme;
    } else if (type === 27) {
        if (window.style !== undefined || data.byteLength !== 4) throw new Error("Invalid window style");
        const bits = data.getUint32(0, true);
        if (bits >= 1024 || (bits & 384) === 384) throw new Error("Invalid window style");
        window.style = bits;
    } else {
        if (window.constraints || data.byteLength !== 20) throw new Error("Invalid window size constraints");
        const [minWidth, minHeight, maxWidth, maxHeight, aspectRatio] = [0, 4, 8, 12, 16].map(at => data.getFloat32(at, true));
        const widthLimit = maxWidth || MAX_WINDOW_EDGE, heightLimit = maxHeight || MAX_WINDOW_EDGE;
        if (![minWidth, minHeight, maxWidth, maxHeight].every(n => Number.isFinite(n) && n >= 0 && n <= MAX_WINDOW_EDGE)
            || minWidth > widthLimit || minHeight > heightLimit || !Number.isFinite(aspectRatio) || aspectRatio < 0
            || (aspectRatio > 0 && (aspectRatio < Math.fround(minWidth / heightLimit)
                || (minHeight !== 0 && aspectRatio > Math.fround(widthLimit / minHeight))))) throw new Error("Invalid window size constraints");
        window.constraints = { minWidth, minHeight, maxWidth, maxHeight, aspectRatio };
    }
}

// Context menus wrap one scene node. Match its ancestry using the page's
// rules for keyed controls and positional anonymous layout nodes.
function previousMenu(menu: SceneMenu, parents: SceneContainer[], before: SceneNode | undefined): SceneMenu | undefined {
    for (const parent of parents) {
        if (!before || !("children" in before) || before.kind !== parent.kind || before.id !== parent.id) return;
        const index = parent.children.length - 1;
        const child = parent.children[index];
        const prior: SceneNode | undefined = before.children[index];
        before = child.id === undefined || prior?.id === child.id ? prior : before.children.find(item => item.id === child.id);
    }
    if (before?.kind !== "context-menu") return;
    let owner = menu.children[0], oldOwner = before.children[0];
    while (owner.kind === "context-menu" && oldOwner.kind === "context-menu") {
        owner = owner.children[0]; oldOwner = oldOwner.children[0];
    }
    if (owner.kind === oldOwner.kind && owner.id === oldOwner.id) return before;
}

function setActionRevisions(items: MenuItem[], revision: number): void {
    for (const item of items) {
        if (item.kind === "menu-action") item.revision = revision;
        else if (item.kind === "submenu") setActionRevisions(item.items, revision);
    }
}

function sameMenu(items: MenuItem[], next: MenuItem[]): boolean {
    return items.length === next.length && items.every((item, index) => {
        const other = next[index];
        if (item.kind === "separator" || other.kind === "separator") return item.kind === other.kind;
        if (item.label !== other.label || item.disabled !== other.disabled || item.icon !== other.icon) return false;
        if (item.kind === "submenu" && other.kind === "submenu") return sameMenu(item.items, other.items);
        return item.kind === "menu-action" && other.kind === "menu-action" && item.id === other.id
            && item.checked === other.checked && item.destructive === other.destructive;
    });
}

function decodeMenu(bytes: Uint8Array, data: DataView, limit: number, control: (action: MenuAction) => void): { items: MenuItem[]; count: number } {
    let at = 0, count = 0;
    const take = (size: number): number => {
        if (size > bytes.length - at) throw new Error("Truncated context menu");
        const start = at; at += size; return start;
    };
    const u32 = (): number => data.getUint32(take(4), true);
    const byte = (): number => bytes[take(1)];
    const entries = (depth: number, disabled: boolean): MenuItem[] => {
        const size = u32();
        if (!size || size > limit - count || depth >= 8) throw new Error("Invalid context menu size");
        const items: MenuItem[] = [];
        for (let i = 0; i < size; i++) {
            if (++count > limit) throw new Error("Too many context menu items");
            const type = byte();
            if (type === 2) { items.push({ kind: "separator" }); continue; }
            const id = type === 1 ? u32() : 0;
            const flags = byte(), icon = byte(), length = u32(), start = take(length);
            if (icon > 6) throw new Error("Invalid context menu icon");
            const label = decoder.decode(bytes.subarray(start, at));
            if (type === 1 && id && flags < 16 && (!(flags & 2) || (flags & 8))) {
                const action: MenuAction = { kind: "menu-action", revision: 0, id, label, icon, disabled: disabled || !!(flags & 1),
                    checked: flags & 8 ? !!(flags & 2) : undefined, destructive: !!(flags & 4) };
                control(action); items.push(action);
            } else if (type === 3 && flags <= 1) {
                const inactive = disabled || !!flags;
                items.push({ kind: "submenu", label, icon, disabled: inactive, items: entries(depth + 1, inactive) });
            } else throw new Error("Invalid context menu item");
        }
        return items;
    };
    const items = entries(0, false);
    if (at !== bytes.length) throw new Error("Trailing context menu bytes");
    return { items, count };
}

// Totals belong to one scene, including all of its decorative and interactive canvases.
interface CanvasTotals { items: number; textBytes: number }

function decodeContainer(type: number, id: number, data: Uint8Array): SceneContainer {
    if (type === 24) {
        if (!id || data.length !== 1 || data[0] > 1) throw new Error("Invalid scroll viewport");
        return { kind: "scroll", id, complexBackground: Boolean(data[0]), children: [], close: 25 };
    }
    if (id || data.length) throw new Error("Invalid guest container");
    return { kind: type === 6 ? "row" : "column", children: [], close: type === 1 ? 4 : type === 6 ? 7 : 13 };
}

function decodeTextInput(id: number, data: Uint8Array, d: DataView): SceneInput {
    const length = data.length;
    if (!id || length < 5 || data[0] > 7) throw new Error("Invalid text input");
    const size = d.getUint32(1, true), placeholder = 5 + size;
    if (placeholder > length) throw new Error("Invalid text input size");
    return { kind: "input", id, disabled: Boolean(data[0] & 1), readOnly: Boolean(data[0] & 4), value: decoder.decode(data.subarray(5, placeholder)),
        placeholder: decoder.decode(data.subarray(placeholder)) };
}

function decodeCanvas(id: number, data: Uint8Array, d: DataView, totals: CanvasTotals): SceneCanvas {
    const length = data.length;
    if (length < 16) throw new Error("Invalid canvas");
    const width = d.getUint32(0, true), height = d.getUint32(4, true), flags = d.getUint32(8, true), n = d.getUint32(12, true);
    const frames = (flags & 1) as 0 | 1;
    const extent = flags & 4 ? "window" : flags & 2 ? "viewport" : undefined;
    const edge = extent === "window" ? MAX_WINDOW_EDGE : MAX_CANVAS_EDGE;
    if (!width || !height || width > edge || height > edge || (flags & ~15) !== 0 || (flags & 6) === 6
        || (!id && frames !== 0) || n > 4096 - totals.items) throw new Error("Canvas exceeds limits");
    const items: SceneCanvasItem[] = [];
    let at = 16;
    let clearColor: number | undefined;
    if (flags & 8) {
        if (length < 20) throw new Error("Truncated Canvas clear color");
        clearColor = d.getUint32(at, true);
        at += 4;
    }
    for (let index = 0; index < n; index++) {
        if (at + 24 > length) throw new Error("Truncated Canvas item");
        const kind = d.getUint32(at, true);
        const bounds = { x: d.getUint32(at + 4, true), y: d.getUint32(at + 8, true), width: d.getUint32(at + 12, true), height: d.getUint32(at + 16, true), color: d.getUint32(at + 20, true) };
        at += 24;
        if (!bounds.width || !bounds.height || bounds.x + bounds.width > width || bounds.y + bounds.height > height) throw new Error("Canvas item outside bounds");
        if (kind === 0) items.push({ kind: "rect", ...bounds });
        else if (kind === 1) {
            if (at + 12 > length) throw new Error("Truncated Canvas text");
            const fontSize = d.getUint32(at, true), bold = d.getUint32(at + 4, true), size = d.getUint32(at + 8, true);
            at += 12;
            if (!fontSize || fontSize > 96 || bold > 1 || size > 4096 || size > 65536 - totals.textBytes || at + size > length) throw new Error("Canvas text exceeds limits");
            const text = decoder.decode(data.subarray(at, at + size));
            if (/[\u0000-\u001f\u007f-\u009f\u2028\u2029]/u.test(text)) throw new Error("Canvas text must be one line");
            items.push({ kind: "text", ...bounds, fontSize, bold: Boolean(bold), text });
            at += size; totals.textBytes += size;
        } else if (kind === 2) {
            if (at + 12 > length) throw new Error("Truncated Canvas pixels");
            const sourceWidth = d.getUint32(at, true), sourceHeight = d.getUint32(at + 4, true), size = d.getUint32(at + 8, true);
            at += 12;
            if (bounds.color || !sourceWidth || !sourceHeight || sourceWidth > MAX_CANVAS_EDGE || sourceHeight > MAX_CANVAS_EDGE
                || size !== sourceWidth * sourceHeight * 4 || size > length - at) throw new Error("Invalid Canvas pixels");
            const { x, y, width, height } = bounds;
            items.push({ kind: "pixels", x, y, width, height, sourceWidth, sourceHeight, rgba: data.slice(at, at + size) });
            at += size;
        } else throw new Error("Unknown Canvas item");
    }
    if (at !== length) throw new Error("Trailing Canvas bytes");
    totals.items += n;
    return { kind: "canvas", id, width, height, frames, clearColor, items, ...(extent ? { extent } : {}) };
}

export function validateInput(controls: Map<number, SceneControl>, kind: number, target: number, data: Uint8Array, validateText: (data: Uint8Array) => void, menuRevision?: number): void {
    const node = controls.get(target);
    if (!node || node.kind === "scroll" || node.disabled || data.length > (kind === 14 ? MAX_TEXT_EVENT_BYTES : MAX_TEXT_BYTES)) throw new Error("Input target is unavailable");
    if ((node.kind === "menu-action" ? node.revision : undefined) !== menuRevision) throw new Error("The menu changed. Open it again to choose an action.");
    if (kind === 2 && !data.length && ["button", "checkbox", "menu-action"].includes(node.kind)) return;
    if (kind === 14 && node.kind === "input" && !node.readOnly) { validateText(data); return; }
    if (kind === 3 && node.kind === "input" && !node.readOnly) { decoder.decode(data); return; }
    const view = new DataView(data.buffer, data.byteOffset, data.length);
    if (node.kind === "canvas") {
        if (kind === 11 && data.length === 16 && view.getUint32(0, true) <= node.width && view.getUint32(4, true) <= node.height
            && view.getUint32(8, true) <= 3 && view.getUint32(12, true) <= 1) return;
        if (kind === 12 && data.length === 8 && view.getUint32(0, true) >= 1 && view.getUint32(0, true) <= 5 && view.getUint32(4, true) <= 1) return;
        if (kind === 13 && node.frames === 1 && data.length === 4 && view.getUint32(0, true) >= 1 && view.getUint32(0, true) <= 100000) return;
    }
    throw new Error("Invalid input for this control");
}
