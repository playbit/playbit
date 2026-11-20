import test from "node:test";
import assert from "node:assert/strict";
import { NativeTextEdits, decodeTextInput, encodeTextInput, MAX_TEXT_BYTES, MAX_TEXT_EDITS, MAX_TEXT_EVENT_BYTES } from "./text-input.ts";

test("native ranges retain repeated-text positions and count UTF-8 bytes", () => {
    const edits = new NativeTextEdits(), field = { value: "éaaa", selectionStart: 1, selectionEnd: 1 };
    edits.beforeInput(field, { inputType: "insertText", data: "a" });
    field.value = "éaaaa"; field.selectionStart = field.selectionEnd = 2;
    edits.input(field, false);
    assert.deepEqual(decodeTextInput(edits.payload(field.value)), { text: "éaaaa", edits: [{ start: 2, end: 2, inserted_len: 1 }] });
    edits.queued();
    field.value = "é🦀aaa"; field.selectionStart = field.selectionEnd = 3;
    edits.beforeInput(field, { inputType: "deleteContentBackward", data: null });
    field.value = "éaaa"; field.selectionStart = field.selectionEnd = 1;
    edits.input(field, false);
    assert.deepEqual(decodeTextInput(edits.payload(field.value)).edits, [{ start: 2, end: 6, inserted_len: 0 }]);
});

test("composition reports its committed replacement and unknown native operations stay explicit", () => {
    const edits = new NativeTextEdits(), field = { value: "éaaa", selectionStart: 1, selectionEnd: 2 };
    edits.compositionStart(field);
    field.value = "éにaa"; edits.input(field, true);
    field.value = "é日本aa"; edits.compositionEnd(field);
    assert.deepEqual(decodeTextInput(edits.payload(field.value)).edits, [{ start: 2, end: 3, inserted_len: 6 }]);
    edits.queued();
    edits.beforeInput(field, { inputType: "historyUndo", data: null });
    field.value = "éaaa"; edits.input(field, false);
    assert.equal(decodeTextInput(edits.payload(field.value)).edits, null);
});

test("text event limits include metadata and malformed extents fail", () => {
    const edits = Array.from({ length: MAX_TEXT_EDITS }, () => ({ start: 0, end: 1, inserted_len: 1 }));
    const data = encodeTextInput("x".repeat(MAX_TEXT_BYTES), edits);
    assert.equal(data.length, MAX_TEXT_EVENT_BYTES);
    assert.equal(decodeTextInput(data).edits.length, MAX_TEXT_EDITS);
    assert.throws(() => decodeTextInput(new Uint8Array([1, 0, 0, 0])));
    assert.throws(() => decodeTextInput(encodeTextInput("é", [{ start: 1, end: 1, inserted_len: 1 }])));
    assert.throws(() => encodeTextInput("x".repeat(MAX_TEXT_BYTES + 1), []));
});

test("large drafts deliver and rejected over-limit edits preserve the draft and recovery", () => {
    const edits = new NativeTextEdits(), value = "é".repeat(MAX_TEXT_BYTES / 2);
    const field = { value: "", selectionStart: 0, selectionEnd: 0 };
    edits.beforeInput(field, { inputType: "insertFromPaste", cancelable: true, preventDefault() { assert.fail("paste cancelled"); } });
    field.value = value; field.selectionStart = field.selectionEnd = value.length;
    edits.input(field, false);
    assert.equal(decodeTextInput(edits.payload(field.value)).text, value);
    edits.queued();
    for (let i = 0; i <= MAX_TEXT_EDITS; i++) {
        edits.beforeInput(field, { inputType: "insertText", cancelable: true, preventDefault() { assert.fail("rejected edits filled the log"); } });
        field.value += "x";
        assert.throws(() => edits.input(field, false), /limit/);
        assert.equal(field.value, value);
    }
    assert.deepEqual(decodeTextInput(edits.payload(field.value)).edits, []);
    field.selectionStart = 0; field.selectionEnd = field.value.length;
    edits.beforeInput(field, { inputType: "deleteContentBackward", cancelable: true, preventDefault() { assert.fail("recovery cancelled"); } });
    field.value = ""; field.selectionStart = field.selectionEnd = 0;
    edits.input(field, false);
    assert.deepEqual(decodeTextInput(edits.payload("")).edits, [{ start: 0, end: MAX_TEXT_BYTES, inserted_len: 0 }]);
    edits.queued();
    field.value = "draft"; field.selectionStart = 0; field.selectionEnd = 5;
    edits.compositionStart(field);
    field.value = "x".repeat(MAX_TEXT_BYTES + 1); edits.input(field, true);
    assert.throws(() => edits.compositionEnd(field), /limit/);
    assert.equal(field.value, "draft");
    assert.deepEqual(decodeTextInput(edits.payload(field.value)).edits, []);
});
