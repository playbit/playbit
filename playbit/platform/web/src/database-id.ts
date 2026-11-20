import { encodeId, decodeId, validId } from './identifier.ts';
// Canonical transport identity. A volume supplies the addressing context.
export type DatabaseId = "index" | "revocations" | string;
const zero = "1".repeat(15);
export function validDatabase(value: unknown): value is DatabaseId {
    return value === "index" || value === "revocations" || typeof value === "string" && validId(value, 15) && value !== zero;
}
export function databaseBytes(id: DatabaseId): Uint8Array<ArrayBuffer> {
    if (!validDatabase(id)) throw new Error("Invalid database identity");
    const bytes = new Uint8Array(16);
    if (id === "revocations") bytes[0] = 1;
    else if (id !== "index") { bytes[0] = 2; bytes.set(decodeId(id, 15), 1); }
    return bytes;
}
export function databaseIdentity(bytes: Uint8Array): DatabaseId {
    if (bytes.length !== 16) throw new Error("Invalid database identity width");
    if (bytes[0] <= 1 && !bytes.subarray(1).some(Boolean)) return bytes[0] ? "revocations" : "index";
    if (bytes[0] === 2 && bytes.subarray(1).some(Boolean)) return encodeId(bytes.subarray(1));
    throw new Error("Invalid database identity");
}
