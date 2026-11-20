// Native selections use UTF-16. Convert their exact ranges before delivering byte edits.
export const MAX_TEXT_BYTES = 256 * 1024, MAX_TEXT_EDITS = 256;
export const MAX_TEXT_EVENT_BYTES = MAX_TEXT_BYTES + 4 + MAX_TEXT_EDITS * 12;
export interface TextEdit { start: number; end: number; inserted_len: number }
const encoder = new TextEncoder(), decoder = new TextDecoder("utf-8", { fatal: true });

export function encodeTextInput(text: string, edits: readonly TextEdit[] | null): Uint8Array {
    const value = encoder.encode(text), count = edits?.length ?? 0;
    if (value.length > MAX_TEXT_BYTES || count > MAX_TEXT_EDITS) throw new Error("Text input exceeds its limit");
    const bytes = new Uint8Array(4 + count * 12 + value.length), view = new DataView(bytes.buffer);
    view.setUint32(0, edits === null ? 0xffffffff : count, true);
    edits?.forEach((edit, index) => {
        [edit.start, edit.end, edit.inserted_len].forEach((word, part) => view.setUint32(4 + index * 12 + part * 4, word, true));
    });
    bytes.set(value, 4 + count * 12);
    return bytes;
}

export function decodeTextInput(bytes: Uint8Array): { text: string; edits: TextEdit[] | null } {
    if (bytes.length < 4 || bytes.length > MAX_TEXT_EVENT_BYTES) throw new Error("Invalid text event");
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength), count = view.getUint32(0, true);
    if (count === 0xffffffff) {
        if (bytes.length - 4 > MAX_TEXT_BYTES) throw new Error("Text exceeds its limit");
        return { text: decoder.decode(bytes.subarray(4)), edits: null };
    }
    if (count > MAX_TEXT_EDITS || 4 + count * 12 > bytes.length || bytes.length - 4 - count * 12 > MAX_TEXT_BYTES) throw new Error("Invalid text edits");
    const edits = Array.from({ length: count }, (_, index) => ({
        start: view.getUint32(4 + index * 12, true), end: view.getUint32(8 + index * 12, true), inserted_len: view.getUint32(12 + index * 12, true),
    }));
    const text = bytes.subarray(4 + count * 12);
    let length = text.length;
    for (let i = edits.length - 1; i >= 0; i--) {
        const edit = edits[i];
        if (edit.start > edit.end || edit.start + edit.inserted_len > length) throw new Error("Invalid edit extent");
        length += edit.end - edit.start - edit.inserted_len;
        if (length > 0xffffffff) throw new Error("Invalid preceding text extent");
    }
    const last = edits.at(-1), boundary = (at: number): boolean => at === text.length || (text[at] & 0xc0) !== 0x80;
    if (last && (!boundary(last.start) || !boundary(last.start + last.inserted_len))) throw new Error("Edit splits a character");
    return { text: decoder.decode(text), edits };
}

interface NativeTextField { value: string; selectionStart: number | null; selectionEnd: number | null }
interface NativeBeforeInput { inputType: string; cancelable: boolean; preventDefault(): void }
interface BeforeEdit { value: string; start: number; end: number; type: string }
export class NativeTextEdits {
    private before?: BeforeEdit;
    private composition?: BeforeEdit;
    private edits: TextEdit[] | null = [];
    beforeInput(field: NativeTextField, event: NativeBeforeInput): void {
        if (field.selectionStart === null || field.selectionEnd === null) { this.before = undefined; return; }
        if ((this.edits?.length ?? 0) === MAX_TEXT_EDITS && event.cancelable) { event.preventDefault(); throw new Error("The app is busy. Retry your retained draft before editing further."); }
        this.before = { value: field.value, start: field.selectionStart, end: field.selectionEnd, type: event.inputType };
    }
    compositionStart(field: NativeTextField): void {
        if (field.selectionStart === null || field.selectionEnd === null) { this.composition = undefined; return; }
        this.composition = { value: field.value, start: field.selectionStart, end: field.selectionEnd, type: "insertCompositionText" };
    }
    compositionEnd(field: NativeTextField): void {
        try { this.capture(field, this.composition); }
        finally { this.composition = undefined; this.before = undefined; }
    }
    input(field: NativeTextField, composing: boolean): void {
        try { if (!composing) this.capture(field, this.before); }
        finally { this.before = undefined; }
    }
    private capture(field: NativeTextField, before?: BeforeEdit): void {
        if (encoder.encode(field.value).length > MAX_TEXT_BYTES) {
            if (before) {
                field.value = before.value; field.selectionStart = before.start; field.selectionEnd = before.end;
            } else { this.edits = null; }
            throw new Error("Text is limited to 256 KiB. Your draft is retained.");
        }
        if (this.edits === null) return;
        // History restoration, replacement/autofill without a native target range,
        // and synthetic input expose only a snapshot. Never guess with a text diff.
        const insertions = ["insertText", "insertFromPaste", "insertLineBreak", "insertParagraph", "insertCompositionText", "insertFromComposition"];
        if (!before || (!insertions.includes(before.type) && !before.type.startsWith("delete"))) { this.edits = null; return; }
        let { start, end } = before;
        if (start === end && before.type.startsWith("delete")) {
            const removed = before.value.length - field.value.length;
            if (removed < 0 || field.selectionStart === null) { this.edits = null; return; }
            start = field.selectionStart; end = start + removed;
        }
        const inserted = field.value.length - before.value.length + end - start;
        if (inserted < 0 || start + inserted > field.value.length || end > before.value.length
            || before.value.slice(0, start) !== field.value.slice(0, start)
            || before.value.slice(end) !== field.value.slice(start + inserted)) { this.edits = null; return; }
        if (this.edits.length === MAX_TEXT_EDITS) { this.edits = null; return; }
        this.edits.push({ start: encoder.encode(before.value.slice(0, start)).length,
            end: encoder.encode(before.value.slice(0, end)).length,
            inserted_len: encoder.encode(field.value.slice(start, start + inserted)).length });
    }
    payload(value: string): Uint8Array { return encodeTextInput(value, this.edits); }
    queued(): void { this.edits = []; }
}
