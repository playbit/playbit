import { encodeTextInput, decodeTextInput } from "./text-input.ts";
import test from "node:test";
import assert from "node:assert/strict";
import { decodeScene, validateInput as validateSceneInput } from "./scene.ts";

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
