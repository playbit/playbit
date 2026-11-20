// Canonical opaque IDs. Match playbit/src/identifier.rs and preserve zero bytes.
const alphabet = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';
export function encodeId(bytes: ArrayLike<number>): string {
    if (!Number.isInteger(bytes.length) || bytes.length < 1 || bytes.length > 32 || !Array.from(bytes).every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255)) throw Error('Invalid identifier width');
    let value = 0n, zeros = 0, text = '';
    for (const byte of Array.from(bytes)) value = value * 256n + BigInt(byte);
    while (zeros < bytes.length && bytes[zeros] === 0) zeros++;
    while (value) { text = alphabet[Number(value % 58n)] + text; value /= 58n; }
    return '1'.repeat(zeros) + text;
}
export function decodeId(text: string, width = 16): Uint8Array<ArrayBuffer> {
    if (typeof text !== 'string' || !text.length || text.length > 44 || ![15, 16, 32].includes(width)) throw Error('Invalid base58 identifier');
    let value = 0n;
    for (const letter of text) {
        const digit = alphabet.indexOf(letter);
        if (digit < 0) throw Error('Invalid base58 identifier');
        value = value * 58n + BigInt(digit);
    }
    const bytes = new Uint8Array(width);
    for (let index = width - 1; index >= 0; index--) { bytes[index] = Number(value & 255n); value >>= 8n; }
    if (value || encodeId(bytes) !== text) throw Error('Invalid base58 identifier width');
    return bytes;
}
export function validId(value: unknown, width = 16): value is string {
    if (typeof value !== 'string') return false;
    try { decodeId(value, width); return true; } catch { return false; }
}

// Native object catalogs order by bytes; base58 text order is different.
export function compareId(left: string, right: string, width = 16): number {
    const a = decodeId(left, width), b = decodeId(right, width);
    for (let index = 0; index < width; index++) if (a[index] !== b[index]) return a[index] - b[index];
    return 0;
}
