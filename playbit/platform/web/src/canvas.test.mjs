import test from "node:test";
import assert from "node:assert/strict";
import { decodeScene, validateInput } from "./scene.ts";
import { drawCanvas } from "./canvas.ts";
const words = (...values) => { const bytes = new Uint8Array(values.length * 4), view = new DataView(bytes.buffer); values.forEach((n, i) => view.setUint32(i * 4, n, true)); return bytes; };
const command = (kind, id, payload = []) => [kind, ...words(id, payload.length), ...payload];
const canvas = frames => new Uint8Array([...new TextEncoder().encode("PBV1"), ...command(1, 0), ...command(22, 17, words(320, 240, frames, 0)), ...command(4, 0)]);
test("Canvas mode is required and canonical; static input keeps pointer and key only", () => {
    for (const mode of [0, 1]) {
        const { controls } = decodeScene(canvas(mode));
        assert.equal(controls.get(17).frames, mode);
        validateInput(controls, 11, 17, words(20, 30, 1, 1));
        validateInput(controls, 12, 17, words(1, 1));
        const frame = () => validateInput(controls, 13, 17, words(16667));
        if (mode) frame(); else assert.throws(frame, /Invalid input/);
        assert.throws(() => validateInput(controls, 13, 18, words(16667)), /unavailable/);
        assert.throws(() => validateInput(controls, 13, 17, words(100001)), /Invalid input/);
    }
    assert.throws(() => decodeScene(canvas(8)), /Truncated Canvas clear color/);
    for (const mode of [2, 4, 0xffffffff]) assert.throws(() => decodeScene(canvas(mode)), /Canvas/);
    const missing = new Uint8Array([...new TextEncoder().encode("PBV1"), ...command(1, 0), ...command(22, 17, words(320, 240, 0)), ...command(4, 0)]);
    assert.throws(() => decodeScene(missing), /canvas/);
});

const rect = [...words(0, 2, 3, 100, 24, 0xaabbccff)];
const label = (text = "日本語 é →", font = 16, bold = 1) => {
    const bytes = new TextEncoder().encode(text);
    return [...words(1, 2, 3, 100, 24, 0xaabbccff, font, bold, bytes.length), ...bytes];
};
const scene = (...canvases) => new Uint8Array([...new TextEncoder().encode("PBV1"), ...command(1, 0),
    ...canvases.flatMap((items, index) => command(22, index + 1, [...words(320, 240, 0, items.length), ...items.flat()])), ...command(4, 0)]);

test("Canvas text preserves Unicode and drawing order, rejecting malformed external items", () => {
    const original = scene([rect, label()]);
    assert.deepEqual(decodeScene(original).controls.get(1).items, [
        { kind: "rect", x: 2, y: 3, width: 100, height: 24, color: 0xaabbccff },
        { kind: "text", x: 2, y: 3, width: 100, height: 24, color: 0xaabbccff, fontSize: 16, bold: true, text: "日本語 é →" },
    ]);
    for (const invalid of [label("x", 0), label("x", 97), label("x", 16, 2),
        ...["\0", "\n", "\r", "\t", "\u0085", "\u2028", "\u2029", "é".repeat(2049)].map(value => label(value))]) {
        assert.throws(() => decodeScene(scene([invalid])));
    }
    for (const [offset, value] of [[0, 2], [4, 0xffffffff], [32, 0xffffffff]]) {
        const bytes = new Uint8Array(label()); new DataView(bytes.buffer).setUint32(offset, value, true);
        assert.throws(() => decodeScene(scene([[...bytes]])));
    }
    const invalidUtf8 = label("a"); invalidUtf8[36] = 0xff;
    assert.throws(() => decodeScene(scene([invalidUtf8])));
    assert.throws(() => decodeScene(scene([[...label(), 0]])), /Trailing/);
    for (let n = 0; n < original.length; n++) assert.throws(() => decodeScene(original.subarray(0, n)));
    assert.equal(decodeScene(scene([label("")])).controls.get(1).items[0].text, "");
});

test("packed pixel Canvas items reject explicitly on the browser host", () => {
    const pixels = [...words(2, 0, 0, 16, 16, 0, 1, 1, 4), 255, 0, 0, 255];
    assert.throws(() => decodeScene(scene([rect, pixels])), /Unknown Canvas item/);
});

test("Canvas item and UTF-8 budgets apply across all controls in a scene", () => {
    decodeScene(scene(Array(2048).fill(rect), Array(2048).fill(rect)));
    assert.throws(() => decodeScene(scene(Array(2048).fill(rect), Array(2049).fill(rect))), /limits/);
    const full = label("é".repeat(2048));
    decodeScene(scene(Array(8).fill(full), Array(8).fill(full)));
    assert.throws(() => decodeScene(scene(Array(8).fill(full), [...Array(8).fill(full), label("a")])), /limits/);
    // Rejected scenes must not consume the next scene's canvas allowance.
    decodeScene(scene(Array(8).fill(full), Array(8).fill(full)));
});

// The current SDK gives decorative static canvases ID zero. They render but
// cannot be addressed by pointer, key or frame input, or request animation.
test("decorative SDK canvases are repeatable, noninteractive and static", () => {
    const encoded = new Uint8Array([...new TextEncoder().encode("PBV1"), ...command(1, 0),
        ...command(22, 0, words(16, 16, 0, 0)), ...command(22, 0, words(16, 16, 0, 0)), ...command(4, 0)]);
    const decoded = decodeScene(encoded);
    assert.equal(decoded.root.children.length, 2);
    assert.equal(decoded.controls.size, 0);
    for (const [kind, data] of [[11, words(1, 1, 1, 1)], [12, words(1, 1)], [13, words(16667)]]) {
        assert.throws(() => validateInput(decoded.controls, kind, 0, data), /unavailable/);
    }
    const animated = new Uint8Array([...new TextEncoder().encode("PBV1"), ...command(1, 0),
        ...command(22, 0, words(16, 16, 1, 0)), ...command(4, 0)]);
    assert.throws(() => decodeScene(animated), /Canvas/);
});

// Exercise the production renderer for both item kinds, including alpha;
// a P3 backing store alone does not reinterpret sRGB CSS fill coordinates.
test("rectangle and text fills preserve Display P3 SDK colors", () => {
    const fills = [];
    const context = {
        clearRect() {}, save() {}, beginPath() {}, rect() {}, clip() {}, restore() {},
        fillRect() { fills.push(["rect", this.fillStyle]); },
        fillText() { fills.push(["text", this.fillStyle]); },
    };
    const canvas = { getContext(kind, options) {
        assert.equal(kind, "2d"); assert.deepEqual(options, { colorSpace: "display-p3" });
        return context;
    } };
    const colored = new Uint8Array([...new TextEncoder().encode("PBV1"), ...command(1, 0),
        ...command(22, 0, words(16, 16, 8, 0, 0x0000ff80)), ...command(4, 0)]);
    const surface = decodeScene(colored).root.children[0];
    assert.equal(surface.clearColor, 0x0000ff80);
    drawCanvas(canvas, { ...surface, items: [
        { kind: "rect", x: 0, y: 0, width: 16, height: 16, color: 0xff0000ff },
        { kind: "text", x: 0, y: 0, width: 16, height: 16, color: 0x00ff0080, fontSize: 12, bold: false, text: "A" },
    ] });
    assert.deepEqual([canvas.width, canvas.height], [16, 16]);
    assert.deepEqual(fills, [["rect", `color(display-p3 0 0 1 / ${128 / 255})`], ["rect", "color(display-p3 1 0 0 / 1)"], ["text", `color(display-p3 0 1 0 / ${128 / 255})`]]);
});
