import { encodeTextInput, decodeTextInput } from "./text-input.ts";
import test from "node:test";
import assert from "node:assert/strict";
import { decodeScene, surfaceBytes, validateSurface, validateInput as validateSceneInput } from "./scene.ts";
import { canvasExtent, contentBounds, layoutGuestWindow } from "./guest-window.ts";

const validateInput = (...args) => validateSceneInput(...args, decodeTextInput);
const text = value => [...new TextEncoder().encode(value)];
const u32 = value => [value & 255, (value >>> 8) & 255, (value >>> 16) & 255, value >>> 24];
const record = (type, id, data = []) => [type, ...u32(id), ...u32(data.length), ...data];
const scene = (...records) => Uint8Array.from([...text("PBV1"), ...record(1, 0), ...records.flat(), ...record(4, 0)]);
const field = flags => record(5, 30, [flags, ...u32(text("Copy me").length), ...text("Copy me"), ...text("Label")]);
const scroll = (id = 1, background = 0) => record(24, id, [background]);

test("scroll viewports preserve nested controls without accepting guest input", () => {
    // Little List wraps its task column in a viewport, including when the list is empty.
    for (const background of [0, 1]) {
        const decoded = decodeScene(scene(scroll(1, background), record(1, 0), field(0), record(4, 0), record(25, 0)));
        const viewport = decoded.root.children[0];
        assert.equal(viewport.kind, "scroll");
        assert.equal(viewport.complexBackground, Boolean(background));
        assert.equal(viewport.children[0].children[0], decoded.controls.get(30));
        assert.equal(decoded.controls.get(1), viewport);
        assert.doesNotThrow(() => validateInput(decoded.controls, 14, 30, encodeTextInput("Edit", [])));
        for (const kind of [2, 3, 11, 12, 13, 14]) {
            assert.throws(() => validateInput(decoded.controls, kind, 1, new Uint8Array()), /Input target is unavailable/);
        }
    }
    // A viewport can be the root, and nested viewports reserve identities too.
    const nested = Uint8Array.from([...text("PBV1"), ...scroll(1), ...scroll(2),
        ...record(1, 0), ...record(4, 0), ...record(25, 0), ...record(25, 0)]);
    const decoded = decodeScene(nested);
    assert.equal(decoded.root, decoded.controls.get(1));
    assert.equal(decoded.root.children[0], decoded.controls.get(2));
});

test("scroll admission rejects malformed flags, nesting, children and duplicate identities", () => {
    // Exercise the SDK's viewport contract at the web browser's untrusted scene boundary.
    const empty = [...record(1, 0), ...record(4, 0)], end = record(25, 0);
    for (const start of [scroll(0), scroll(1, 2), record(24, 1), record(24, 1, [0, 0])]) {
        assert.throws(() => decodeScene(scene(start, empty, end)), /Invalid scroll viewport/);
    }
    for (const children of [[], [...empty, ...empty]]) {
        assert.throws(() => decodeScene(scene(scroll(), children, end)), /Scroll viewport requires one child/);
    }
    for (const closing of [record(25, 1), record(25, 0, [0]), record(7, 0)]) {
        assert.throws(() => decodeScene(scene(scroll(), empty, closing)), /Invalid guest nesting/);
    }
    assert.throws(() => decodeScene(scene(scroll(), empty)), /Invalid guest nesting/);
    for (const bytes of [scene(scroll(30), field(0), end), scene(field(0), scroll(30), empty, end),
        scene(scroll(1), empty, end, scroll(1), empty, end),
        Uint8Array.from([...text("PBV1"), ...scroll(30), ...field(0), ...end])]) {
        assert.throws(() => decodeScene(bytes), /Duplicate guest control identity/);
    }
});

test("scroll containers retain aggregate scene depth and identity limits", () => {
    // Viewports count toward the existing 32-container and 256-identity bounds.
    const nested = depth => scene(...Array.from({ length: depth }, (_, i) => scroll(i + 1)),
        record(2, 0, [0]), ...Array.from({ length: depth }, () => record(25, 0)));
    assert.doesNotThrow(() => decodeScene(nested(31)));
    assert.throws(() => decodeScene(nested(32)), /Invalid guest container/);
    const buttons = count => Array.from({ length: count }, (_, i) => record(3, i + 2, [0, 0])).flat();
    const root = count => Uint8Array.from([...text("PBV1"), ...scroll(), ...record(1, 0), ...buttons(count),
        ...record(4, 0), ...record(25, 0)]);
    assert.equal(decodeScene(root(255)).controls.size, 256);
    assert.throws(() => decodeScene(root(256)), /Too many guest controls/);
});

test("text-field flags preserve read-only semantics and reject unknown bits", () => {
    for (let flags = 0; flags <= 7; flags++) {
        const decoded = decodeScene(scene(field(flags))), node = decoded.controls.get(30);
        assert.equal(node.value, "Copy me");
        assert.equal(node.placeholder, "Label");
        assert.equal(node.disabled, Boolean(flags & 1));
        assert.equal(node.readOnly, Boolean(flags & 4));
        for (const kind of [3, 14]) {
            const send = () => validateInput(decoded.controls, kind, 30, kind === 14 ? encodeTextInput("Edit", []) : Uint8Array.from(text("Edit")));
            if (flags & 5) assert.throws(send); else assert.doesNotThrow(send);
        }
    }
    for (const flags of [8, 12, 128, 255]) assert.throws(() => decodeScene(scene(field(flags))), /Invalid text input/);
});

test("large labels and destructive buttons retain their semantic styles", () => {
    const decoded = decodeScene(scene(record(2, 0, [5, ...text("Large")]), record(3, 40, [2, 0, ...text("Delete")])));
    assert.equal(decoded.root.children[0].style, 5);
    assert.equal(decoded.root.children[0].text, "Large");
    assert.equal(decoded.controls.get(40).state, 2);
    assert.doesNotThrow(() => validateInput(decoded.controls, 2, 40, new Uint8Array()));
    assert.throws(() => decodeScene(scene(record(2, 0, [6, ...text("Invalid")]))), /Invalid guest text/);
});

test("nested scene records preserve order and share control identities with the tree", () => {
    // Decode a sliced buffer so every payload read must honor the view's byte offset.
    const bytes = scene(record(6, 0), record(2, 0, [0, ...text("Title")]), field(0),
        record(12, 0), record(8, 40, [2, 1, ...text("Mixed")]), record(13, 0), record(7, 0));
    const sliced = Uint8Array.from([255, ...bytes, 255]).subarray(1, -1);
    const decoded = decodeScene(sliced), row = decoded.root.children[0];
    assert.equal(decoded.root.kind, "column");
    assert.equal(row.kind, "row");
    assert.deepEqual(row.children.map(node => node.kind), ["text", "input", "column"]);
    assert.equal(row.children[0].text, "Title");
    assert.equal(row.children[2].children[0].state, 2);
    assert.equal(row.children[2].children[0].disabled, true);
    assert.deepEqual([...decoded.controls.keys()], [30, 40]);
    assert.equal(decoded.controls.get(30), row.children[1]);
    assert.equal(decoded.controls.get(40), row.children[2].children[0]);
});

test("scene framing, nesting and control identity violations retain explicit errors", () => {
    // These are protocol contracts at the dispatcher boundary, including payload validation
    // before attachment and duplicate-identity validation across nested containers.
    const invalid = [
        [Uint8Array.from(text("PBV1")), "Incomplete guest scene"],
        [scene(record(7, 0)), "Invalid guest nesting"],
        [scene(record(6, 1)), "Invalid guest container"],
        [scene(record(6, 0), record(13, 0)), "Invalid guest nesting"],
        [scene(field(0), record(6, 0), field(0), record(7, 0)), "Duplicate guest control identity"],
        [scene(record(5, 0, [0, 0, 0, 0, 0])), "Invalid text input"],
        [scene(record(5, 1, [0, ...u32(1)])), "Invalid text input size"],
        [scene(record(99, 0)), "Unsupported guest view operation 99"],
        [Uint8Array.from([...text("PBV1"), ...record(5, 0)]), "Invalid text input"],
        [Uint8Array.from([...text("PBV1"), ...field(0)]), "Guest control requires a container"],
        [Uint8Array.from([...scene(), ...record(1, 0), ...record(4, 0)]), "Multiple scene roots"],
    ];
    for (const [bytes, message] of invalid) assert.throws(() => decodeScene(bytes), { name: "Error", message });
});

const words = (...values) => values.flatMap(u32);
const float = value => {
    const data = new DataView(new ArrayBuffer(4)); data.setFloat32(0, value, true);
    return Array.from(new Uint8Array(data.buffer));
};
const document = (...records) => Uint8Array.from([...text("PBV1"), ...records.flat()]);
const canvas = (id, width, height, flags, items = []) => record(22, id, [
    ...words(width, height, flags, Number(items.length !== 0)), ...(flags & 8 ? u32(0x205060ff) : []), ...items,
]);
const pixels = (srcW, srcH, rgba, color = 0) => [...words(2, 3, 4, 8, 8, color, srcW, srcH, rgba.length), ...rgba];

test("standalone window canvas and context actions retain metadata and owned pixel bytes", () => {
    const action = id => [1, ...u32(id), 0, 0, ...u32(4), ...text("Pass")];
    const rgba = [0, 21, 255, 255, 90, 42, 161, 127];
    const size = record(28, 0, [640, 0, 1280, 0, 1.25].flatMap(float));
    const start = record(16, 0, [...u32(1), ...action(2)]), end = record(17, 0);
    const source = document(record(10, 0, [0, ...u32(0x302010ff)]), size, record(27, 0, u32(0x13f)),
        start, canvas(1, 5000, 640, 13, pixels(2, 1, rgba)), end);
    const decoded = decodeScene(source), board = decoded.root.children[0];
    assert.equal(canvasExtent(decoded.root), "window");
    assert.equal(decoded.window.theme.get(0), 0x302010ff);
    assert.equal(decoded.window.style, 0x13f);
    assert.equal(decoded.window.constraints.minWidth, 640);
    assert.equal(board.clearColor, 0x205060ff);
    assert.equal(board.frames, 1);
    assert.equal(board.items[0].sourceWidth, 2);
    source.fill(0);
    assert.deepEqual(Array.from(board.items[0].rgba), rgba);
    assert.doesNotThrow(() => validateSceneInput(decoded.controls, 2, 2, new Uint8Array(), decodeTextInput, decoded.root.revision));
    assert.doesNotThrow(() => validateInput(decoded.controls, 11, 1, Uint8Array.from(words(5000, 640, 1, 1))));
    assert.throws(() => validateInput(decoded.controls, 11, 1, Uint8Array.from(words(5001, 640, 1, 1))));
    assert.throws(() => decodeScene(document(start, canvas(2, 640, 448, 4), end)), /Duplicate guest control/);
});

test("canvas pixels and window constraints reject invalid wire shapes", () => {
    const valid = canvas(1, 640, 448, 4);
    const inline = canvas(1, 16, 16, 0);
    assert.throws(() => decodeScene(document(inline)), /requires a container/);
    assert.equal(decodeScene(scene(inline)).root.children[0].kind, "canvas");
    assert.throws(() => decodeScene(scene(valid)), /Extended canvas must be the root/);
    assert.throws(() => decodeScene(document(canvas(1, 640, 448, 6))), /Canvas exceeds limits/);
    assert.throws(() => decodeScene(document(canvas(1, 640, 448, 4, pixels(2, 2, [1, 2, 3, 4])))), /Invalid Canvas pixels/);
    assert.throws(() => decodeScene(document(canvas(1, 640, 448, 4, pixels(1, 1, [1, 2, 3, 4], 1)))), /Invalid Canvas pixels/);
    assert.throws(() => decodeScene(document(record(27, 0, u32(384)), valid)), /Invalid window style/);
    assert.throws(() => decodeScene(document(valid, record(27, 0, u32(0)))), /must precede/);
    assert.throws(() => decodeScene(document(record(28, 0, [640, 0, 0, 200, 2].flatMap(float)), valid)), /Invalid window size constraints/);
    assert.throws(() => decodeScene(document(record(28, 0, [0, 0, Infinity, 0, 0].flatMap(float)), valid)), /Invalid window size constraints/);
});

test("the padded root canvas retains viewport extent and its smaller edge bound", () => {
    const decoded = decodeScene(document(record(27, 0, u32(0x10f)), canvas(1, 4096, 640, 2)));
    assert.equal(canvasExtent(decoded.root), "viewport");
    assert.equal(decoded.window.style, 0x10f);
    assert.throws(() => decodeScene(document(canvas(1, 4097, 640, 2))), /Canvas exceeds limits/);
    assert.throws(() => decodeScene(scene(canvas(1, 640, 448, 2))), /Extended canvas must be the root/);
});

test("a Night Lobby-sized sRGB source fits the browser host scene bound", () => {
    const rgbaSize = 640 * 448 * 4;
    const payload = words(640, 448, 4, 1, 2, 0, 0, 640, 448, 0, 640, 448, rgbaSize);
    const header = Uint8Array.from([...text("PBV1"), 22, ...words(1, payload.length + rgbaSize), ...payload]);
    const bytes = new Uint8Array(header.length + rgbaSize); bytes.set(header);
    assert.equal(decodeScene(bytes).root.items[0].rgba.length, rgbaSize);
});

test("surface geometry preserves the guest aspect ratio and validates little endian event fields", () => {
    const constraints = { minWidth: 640, minHeight: 0, maxWidth: 1280, maxHeight: 800, aspectRatio: 2 };
    assert.deepEqual(contentBounds(1200, 550, constraints), [1100, 550]);
    assert.deepEqual(contentBounds(400, 350, constraints), [640, 320]);
    assert.deepEqual(contentBounds(5000, 4000, constraints), [1280, 640]);
    const bytes = surfaceBytes(1100, 550, 1.25, 7);
    assert.doesNotThrow(() => validateSurface(bytes));
    assert.deepEqual([...bytes], words(1100, 550, 1067450368, 7));
    assert.throws(() => validateSurface(surfaceBytes(1100, 550, NaN, 7)), /Invalid guest surface/);
    assert.throws(() => validateSurface(surfaceBytes(1100, 550, 1, 8)), /Invalid guest surface/);
});

test("repainting preserves a menu choice and reusing its ID cannot retarget a queued click", () => {
    const menu = (label, body) => document(record(16, 0, [...u32(1), 1, ...u32(42), 0, 0, ...u32(label.length), ...text(label)]),
        record(2, 0, [0, ...text(body)]), record(17, 0));
    const original = decodeScene(menu("Join", "Waiting"));
    const repainted = decodeScene(menu("Join", "Another frame"), original);
    const changed = decodeScene(menu("Leave", "Another room"), repainted);
    const restored = decodeScene(menu("Join", "Next room"), changed);
    const click = (scene, revision) => validateSceneInput(scene.controls, 2, 42, new Uint8Array(), decodeTextInput, revision);
    click(repainted, original.root.revision);
    assert.throws(() => click(changed, original.root.revision), /menu changed/);
    assert.throws(() => click(restored, original.root.revision), /menu changed/);
    click(restored, restored.root.revision);
});

test("a menu click belongs to the canvas and its location in the scene", () => {
    const actions = (id, label) => record(16, 0, [...u32(1), 1, ...u32(id), 0, 0, ...u32(label.length), ...text(label)]);
    const wrapped = (id, width) => [actions(42, "Inspect"), actions(43, "Copy"), canvas(id, width, 16, 0), record(17, 0), record(17, 0)];
    const panel = (id, canvasId, width) => [scroll(id), ...wrapped(canvasId, width), record(25, 0)];
    const title = record(2, 0, [0, ...text("Tools")]);
    const original = decodeScene(scene(...panel(10, 7, 16), title));
    const repainted = decodeScene(scene(title, ...panel(10, 7, 32)), original);
    const replaced = decodeScene(scene(title, ...panel(10, 8, 32)), repainted);
    const moved = decodeScene(scene(title, ...panel(11, 8, 32)), replaced);
    for (const action of [42, 43]) {
        const click = (scene, revision) => validateSceneInput(scene.controls, 2, action, new Uint8Array(), decodeTextInput, revision);
        const originalRevision = original.controls.get(action).revision;
        click(repainted, originalRevision);
        assert.throws(() => click(replaced, originalRevision), /menu changed/);
        const newRevision = replaced.controls.get(action).revision;
        click(replaced, newRevision);
        assert.throws(() => click(moved, newRevision), /menu changed/);
    }
});

test("ordinary surface height follows the available viewport even when its own contents grow or shrink", () => {
    let contentHeight = 30;
    const main = { hasAttribute: () => false, style: { removeProperty() {} },
        getBoundingClientRect: () => ({ top: 100, width: 300, height: contentHeight }) };
    const oldWindow = globalThis.window, oldDocument = globalThis.document, oldStyle = globalThis.getComputedStyle;
    globalThis.window = { scrollY: 0, innerHeight: 600, devicePixelRatio: 1 };
    globalThis.document = { body: {}, hidden: false };
    globalThis.getComputedStyle = () => ({ paddingBottom: "10", paddingTop: "10", paddingLeft: "10", paddingRight: "10" });
    const available = () => Array.from(new Uint32Array(layoutGuestWindow(main).buffer).subarray(0, 2));
    try {
        assert.deepEqual(available(), [280, 470]);
        contentHeight = 90;
        assert.deepEqual(available(), [280, 470]);
        globalThis.window.innerHeight = 500;
        assert.deepEqual(available(), [280, 370]);
    } finally {
        globalThis.window = oldWindow; globalThis.document = oldDocument; globalThis.getComputedStyle = oldStyle;
    }
});
