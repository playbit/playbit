import { encodeId } from '../src/identifier.ts';

export function numberedId(value, width = 16) {
    const bytes = new Uint8Array(width);
    new DataView(bytes.buffer).setUint32(width - 4, value, false);
    return encodeId(bytes);
}
