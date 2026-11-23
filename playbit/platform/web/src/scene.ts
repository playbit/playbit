import { MAX_TEXT_BYTES, MAX_TEXT_EVENT_BYTES } from "./text-input.ts";
const decoder = new TextDecoder("utf-8", { fatal: true });

export interface SceneRect { kind: "rect"; x: number; y: number; width: number; height: number; color: number }
export interface SceneCanvasText { kind: "text"; x: number; y: number; width: number; height: number; color: number; fontSize: number; bold: boolean; text: string }
export type SceneCanvasItem = SceneRect | SceneCanvasText;
// Only scroll containers carry an identity; layout and text nodes stay anonymous.
export interface SceneLayout { kind: "row" | "column"; children: SceneNode[]; close: number; id?: undefined }
export interface SceneScroll { kind: "scroll"; id: number; complexBackground: boolean; children: SceneNode[]; close: 25 }
export type SceneContainer = SceneLayout | SceneScroll;
export interface SceneText { kind: "text"; style: number; text: string; id?: undefined }
export interface SceneInput { kind: "input"; id: number; disabled: boolean; readOnly: boolean; value: string; placeholder: string }
export interface SceneToggle { kind: "button" | "checkbox"; id: number; state: number; disabled: boolean; label: string }
// A canvas is never disabled; the field stays undefined so validateInput reads it uniformly.
export interface SceneCanvas { kind: "canvas"; id: number; width: number; height: number; frames: 0 | 1; clearColor?: number; items: SceneCanvasItem[]; disabled?: undefined }
// Identified nodes share one namespace; scroll viewports accept no guest input.
export type SceneControl = SceneInput | SceneToggle | SceneCanvas | SceneScroll;
export type SceneNode = SceneContainer | SceneText | SceneControl;
export interface Scene { root: SceneContainer; controls: Map<number, SceneControl> }

export function decodeScene(bytes: Uint8Array): Scene {
    if (bytes.length > 1024 * 1024 || decoder.decode(bytes.subarray(0, 4)) !== "PBV1") throw new Error("Invalid guest scene");
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const stack: SceneContainer[] = [];
    const controls: Map<number, SceneControl> = new Map();
    let root: SceneContainer | undefined, count = 0;
    const canvasTotals: CanvasTotals = { items: 0, textBytes: 0 };
    const add = (node: SceneNode): void => {
        if (!stack.length) throw new Error("Guest control requires a container");
        stack.at(-1)!.children.push(node);
        if (node.id) {
            if (controls.size >= 256) throw new Error("Too many guest controls");
            if (controls.has(node.id)) throw new Error("Duplicate guest control identity");
            controls.set(node.id, node);
        }
    };
    for (let offset = 4; offset < bytes.length;) {
        if (++count > 2048 || offset + 9 > bytes.length) throw new Error("Invalid guest scene size");
        const type = bytes[offset], id = view.getUint32(offset + 1, true), length = view.getUint32(offset + 5, true);
        offset += 9;
        if (offset + length > bytes.length) throw new Error("Truncated guest scene");
        const data = bytes.subarray(offset, offset + length), d = new DataView(data.buffer, data.byteOffset, data.length);
        offset += length;
        switch (type) {
            case 1: case 6: case 12: case 24: {
                if (stack.length >= 32) throw new Error("Invalid guest container");
                const node = decodeContainer(type, id, data);
                if (stack.length) add(node); else if (root) throw new Error("Multiple scene roots"); else {
                    root = node;
                    if (node.id) controls.set(node.id, node);
                }
                stack.push(node);
                break;
            }
            case 4: case 7: case 13: case 25: {
                const node = stack.pop();
                if (id || length || node?.close !== type) throw new Error("Invalid guest nesting");
                if (node.kind === "scroll" && node.children.length !== 1) throw new Error("Scroll viewport requires one child");
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
            case 22:
                add(decodeCanvas(id, data, d, canvasTotals));
                break;
            default:
                throw new Error(`Unsupported guest view operation ${type}`);
        }
    }
    if (stack.length || !root) throw new Error("Incomplete guest scene");
    return { root, controls };
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
    if (!width || !height || width > 4096 || height > 4096 || (flags & ~9) !== 0 || (!id && frames !== 0) || n > 4096 - totals.items) throw new Error("Canvas exceeds limits");
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
        } else throw new Error("Unknown Canvas item");
    }
    if (at !== length) throw new Error("Trailing Canvas bytes");
    totals.items += n;
    return { kind: "canvas", id, width, height, frames, clearColor, items };
}

export function validateInput(controls: Map<number, SceneControl>, kind: number, target: number, data: Uint8Array, validateText: (data: Uint8Array) => void): void {
    const node = controls.get(target);
    if (!node || node.kind === "scroll" || node.disabled || data.length > (kind === 14 ? MAX_TEXT_EVENT_BYTES : MAX_TEXT_BYTES)) throw new Error("Input target is unavailable");
    if (kind === 2 && !data.length && ["button", "checkbox"].includes(node.kind)) return;
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
