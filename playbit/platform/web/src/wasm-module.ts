// The web browser engine does not accept an external memory limiter for a module's
// own memory. Bound that declaration before compilation, preserving all code.
export function boundedGuest(bytes: Uint8Array): Uint8Array<ArrayBuffer> {
    const expected = [0, 97, 115, 109, 1, 0, 0, 0];
    if (bytes.length < 8 || expected.some((byte, i) => bytes[i] !== byte)) throw new Error("Invalid Wasm module header");
    let offset = 8, memories = 0;
    const parts: Uint8Array[] = [bytes.subarray(0, 8)];
    const u32 = (): number => {
        let value = 0;
        for (let i = 0; i < 5; i++) {
            if (offset >= bytes.length) throw new Error("Truncated Wasm integer");
            const byte = bytes[offset++];
            if (i === 4 && byte > 15) throw new Error("Invalid Wasm integer");
            value |= (byte & 127) << (i * 7);
            if (!(byte & 128)) return value >>> 0;
        }
        throw new Error("Invalid Wasm integer");
    };
    const encode = (value: number): number[] => {
        const result: number[] = [];
        do { const byte = value & 127; value >>>= 7; result.push(byte | (value ? 128 : 0)); } while (value);
        return result;
    };
    while (offset < bytes.length) {
        const start = offset, section = bytes[offset++], length = u32(), end = offset + length;
        if (end > bytes.length) throw new Error("Truncated Wasm section");
        if (section === 8) throw new Error("Guest must initialize through pb_main, not a start function");
        if (section === 5) {
            if (memories++ || u32() !== 1) throw new Error("Guest must declare one memory");
            const flags = u32();
            if (flags > 1) throw new Error("Shared and 64-bit guest memory are unsupported");
            const minimum = u32(), maximum = flags ? u32() : 256;
            if (offset !== end || minimum > 256 || maximum < minimum) throw new Error("Guest memory exceeds 16 MiB");
            const memory = [1, 1, ...encode(minimum), ...encode(Math.min(maximum, 256))];
            parts.push(Uint8Array.from([5, ...encode(memory.length), ...memory]));
        } else parts.push(bytes.subarray(start, end));
        offset = end;
    }
    if (memories !== 1) throw new Error("Guest must declare one memory");
    const output = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
    offset = 0; for (const part of parts) { output.set(part, offset); offset += part.length; }
    return output;
}
