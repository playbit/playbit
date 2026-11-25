import { readFileSync } from "node:fs";
import assert from "node:assert/strict";
import test from "node:test";
import { combineSurfaceMotion, encodeKey, legacyCanvasEvent, observeSurface } from "./surface-input.ts";

test("logical keys, physical keys, and the C SDK share dense key assignments", () => {
    const header = readFileSync(new URL("../../../../playbit-api/include/playbit/sys/abi.h", import.meta.url), "utf8");
    const values = Object.fromEntries(Array.from(header.matchAll(/PB_KEY_(\w+) = (\d+)/g), ([, name, value]) => [name, Number(value)]));
    const { COUNT, ...codes } = values;
    assert.deepEqual(Object.values(codes).sort((a, b) => a - b), Array.from({ length: COUNT }, (_, i) => i));
    assert.deepEqual(encodeKey("KeyQ", "a"), [values.A, values.Q]);
    assert.deepEqual(encodeKey("ArrowRight", "ArrowRight"), [values.Right, values.Right]);
    assert.deepEqual(encodeKey("F24", "F24"), [values.F24, values.F24]);
    assert.deepEqual(encodeKey("NumpadEnter", "Enter"), [values.Enter, values.NumpadEnter]);
    assert.deepEqual(encodeKey("Unidentified", "あ"), [values.None, values.None]);
    assert.deepEqual(encodeKey("Fn", "Fn"), [values.Fn, values.Fn]);
});

function surface(accept = () => true) {
    const listeners = new Map();
    class Node {
        contains(other) { return this === other || other?.parent === this; }
    }
    class Element extends Node {
        closest() { return this instanceof Canvas ? this : null; }
        getBoundingClientRect() { return { left: 10, top: 20 }; }
    }
    class Canvas extends Element {
        dataset = { control: "7" };
        setPointerCapture(id) { this.capture = id; }
        hasPointerCapture(id) { return this.capture === id; }
        releasePointerCapture() { this.capture = undefined; }
        focus() {}
        getBoundingClientRect() { return { left: 25, top: 35, width: 100, height: 100 }; }
    }
    const document = { addEventListener() {}, hidden: false, hasFocus: () => true, body: new Element() };
    const window = { addEventListener: (type, listener) => listeners.set(type, listener) };
    Object.assign(globalThis, { window, document, Node, Element, HTMLCanvasElement: Canvas });
    const root = new Element(), control = new Element(), canvas = new Canvas();
    control.parent = canvas.parent = root;
    const delivered = [];
    const input = observeSurface(root, (kind, target, bytes, motion, reservation) => {
        const event = { kind, target, view: new DataView(bytes.buffer), motion, reservation };
        if (!accept(event)) return false;
        delivered.push(event);
        return true;
    }, () => 1, () => ({ width: 200, height: 200 }));
    const emit = (type, target, fields) => listeners.get(type)({
        target, pointerId: 1, pointerType: "mouse", isPrimary: true, timeStamp: 50, getModifierState: () => false,
        preventDefault() {}, clientX: 29, clientY: 44, button: 0, buttons: 1, ...fields,
    });
    return { emit, delivered, input, canvas, root, control, Canvas };
}

test("Fn chords preserve logical keys, reported physical codes, and modifier state", () => {
    const { emit, delivered, input, control } = surface();
    input.configure(2);
    for (const [code, physical] of [["Fn", 128], ["", 0], ["Unidentified", 0], ["ControlLeft", 65]]) {
        const fnHeld = key => key === "Fn";
        const arrowCode = code && code !== "Unidentified" ? "ArrowRight" : code;
        const arrow = { key: "ArrowRight", code: arrowCode, getModifierState: fnHeld };
        emit("keydown", control, { key: "Fn", code, getModifierState: fnHeld });
        emit("keydown", control, arrow);
        emit("keyup", control, arrow);
        emit("keyup", control, { key: "Fn", code });
        assert.deepEqual(delivered.splice(0).map(({ view, reservation }) => [
            view.getUint16(20, true), view.getUint16(22, true), view.getUint16(16, true), view.getUint8(24), reservation,
        ]), [
            [128, physical, 32, 1, 1], [...encodeKey(arrowCode, arrow.key), 32, 1, 1],
            [...encodeKey(arrowCode, arrow.key), 32, 0, -1], [128, physical, 0, 0, -1],
        ]);
    }
});

test("a guest window observes control input and reserves terminal edges for held input", () => {
    const { emit, delivered, input, canvas, root, control, Canvas } = surface();
    emit("pointerdown", control);
    assert.equal(delivered.length, 0);
    input.configure(3);
    emit("pointerdown", canvas);
    emit("keydown", control, { key: "q", code: "KeyA", ctrlKey: true });
    emit("keydown", control, { key: "q", code: "KeyA", repeat: true, ctrlKey: true });
    emit("keyup", control, { key: "q", code: "KeyA", ctrlKey: true });
    emit("blur", root);
    assert.deepEqual(delivered.map(({ kind, target, reservation }) => [kind, target, reservation]), [
        [22, 7, 1], [23, 0, 1], [23, 0, 0], [23, 0, -1], [22, 7, -1],
    ]);
    const [down, key, repeat,, cancel] = delivered.map(({ view }) => view);
    assert.equal(down.getBigUint64(0, true), 50_000_000n);
    assert.equal(down.getFloat32(32, true), 19);
    assert.equal(down.getFloat32(48, true), 8);
    assert.equal(down.getFloat32(52, true), 18);
    assert.notEqual(key.getUint16(20, true), key.getUint16(22, true));
    assert.equal(key.getUint16(16, true), 2);
    assert.equal(repeat.getUint8(25), 1);
    assert.equal(cancel.getUint8(24), 3);

    const original = (kind, target, view) => legacyCanvasEvent({kind, target, objectId: 4,
        data: new Uint8Array(view.buffer)}, { width: 6, height: 18 });
    assert.deepEqual(original(22, 7, down), {kind: 11, target: 7, objectId: 4,
        data: Uint8Array.of(6, 0, 0, 0, 18, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0)});
    const up = new DataView(new ArrayBuffer(60));
    up.setUint16(22, encodeKey("ArrowUp", "ArrowUp")[1], true);
    up.setUint8(24, 1);
    assert.deepEqual(original(23, 7, up), {kind: 12, target: 7, objectId: 4,
        data: Uint8Array.of(3, 0, 0, 0, 1, 0, 0, 0)});

    delivered.length = 0;
    emit("pointerdown", control, { button: 2, buttons: 2 });
    emit("pointerup", control, { button: 2, buttons: 0 });
    emit("pointerdown", control, { button: 1, buttons: 4 });
    emit("pointerup", control, { button: 1, buttons: 0 });
    assert.deepEqual(delivered.map(({ view }) => [view.getUint8(26), view.getUint16(30, true)]),
        [[1, 2], [1, 0], [2, 4], [2, 0]]);

    delivered.length = 0;
    emit("keydown", control, { key: "あ", code: "Unidentified" });
    emit("keyup", control, { key: "あ", code: "Unidentified" });
    assert.equal(delivered.length, 2);
    assert.equal(delivered[0].view.getUint32(20, true), 0);
    assert.equal(delivered[0].view.getUint32(28, true), "あ".codePointAt(0));

    delivered.length = 0;
    input.configure(3, true);
    const second = new Canvas();
    second.parent = root;
    second.dataset.control = "9";
    emit("keydown", control, { key: "ArrowUp", code: "ArrowUp" });
    emit("keydown", canvas, { key: "ArrowUp", code: "ArrowUp", altKey: true });
    emit("keydown", second, { key: "ArrowUp", code: "ArrowUp" });
    emit("focusout", second);
    emit("keyup", second, { key: "ArrowUp", code: "ArrowUp" });
    assert.deepEqual(delivered.map(({ kind, target, reservation }) => [kind, target, reservation]),
        [[23, 9, 1], [23, 9, -1]]);
});

test("releasing the primary button in a chord ends the legacy canvas drag", () => {
    const { emit, delivered, input, canvas } = surface();
    input.configure(3, true);
    emit("pointerdown", canvas, { button: 0, buttons: 1 });
    emit("pointermove", canvas, { button: 2, buttons: 3 });
    emit("pointermove", canvas, { button: 0, buttons: 2 });
    assert.equal(canvas.hasPointerCapture(1), false);
    emit("pointerup", canvas, { button: 2, buttons: 0 });
    const packets = delivered.map(({ kind, target, view, reservation }) => {
        const event = legacyCanvasEvent({ kind, target, data: new Uint8Array(view.buffer) }, { width: 200, height: 200 });
        assert.equal(event.kind, 11);
        const words = new DataView(event.data.buffer);
        return [words.getUint32(8, true), words.getUint32(12, true), reservation];
    });
    assert.deepEqual(packets, [[1, 1, 1], [0, 1, 0], [2, 0, -1]]);
});

test("a subscription change retires only the removed input family", () => {
    const { emit, delivered, input, canvas } = surface();
    input.configure(3);
    const key = { key: "ArrowUp", code: "ArrowUp" };
    emit("keydown", canvas, key);
    emit("pointerdown", canvas);
    input.configure(2);
    assert.deepEqual(delivered.map(({ kind, view }) => [kind, view.getUint8(24)]), [[23, 1], [22, 1], [22, 3]]);
    emit("keyup", canvas, key);
    assert.deepEqual(delivered.slice(3).map(({ kind, reservation }) => [kind, reservation]), [[23, -1]]);
});

test("each admitted gesture reserves its end and rejected gestures stay silent", () => {
    // Leave room for pan and pinch to start, then fill every unreserved slot before the fingers lift.
    let free = 4;
    const { emit, delivered, input, canvas } = surface(({ reservation }) => {
        if (free < 1 + reservation) return false;
        free -= 1 + reservation;
        return true;
    });
    input.configure(8);
    const finger = pointerId => ({ pointerType: "touch", pointerId });
    emit("pointerdown", canvas, finger(1));
    emit("pointerdown", canvas, finger(2));
    assert.equal(free, 0);
    free = 3; // Simulate the worker draining the queue before the next move.
    emit("pointermove", canvas, finger(1));
    free = 0;
    emit("pointerup", canvas, finger(2));
    assert.deepEqual(delivered.map(({ view, reservation }) => [view.getUint8(20), view.getUint8(21), reservation]), [
        [0, 1, 1], [1, 1, 1], [0, 0, 0], [1, 0, 0], [0, 2, -1], [1, 2, -1],
    ]);
});

test("coalescing retains relative motion in the newest packet", () => {
    const previous = new Uint8Array(48), next = new Uint8Array(48);
    const a = new DataView(previous.buffer), b = new DataView(next.buffer);
    a.setFloat32(32, 5, true); b.setFloat32(32, 7, true);
    a.setFloat32(40, 1.5, true); b.setFloat32(40, 2, true);
    combineSurfaceMotion(25, previous, next);
    assert.equal(b.getFloat32(32, true), 12);
    assert.equal(b.getFloat32(40, true), 3);
});
