import { decodeId } from './identifier.ts';
// A 16-byte random account seed. The service receives public keys and proofs only.
import { sessionCode, parseSessionCode, hex, unhex } from "./invitation.ts";
const encode = (text: string): Uint8Array<ArrayBuffer> => new TextEncoder().encode(text);
export function formatAccountKey(seed: Uint8Array): string { return "a" + sessionCode(seed); }
export function parseAccountKey(text: unknown): Uint8Array<ArrayBuffer> {
    if (typeof text !== "string" || text.length > 1024 || !/^a[1-9A-HJ-NP-Za-km-z]{22}$/.test(text.trim())) throw Error("Enter the complete account key, starting with a");
    return parseSessionCode(text.trim().slice(1));
}
async function derive(seed: Uint8Array<ArrayBuffer>, salt: Uint8Array<ArrayBuffer>, info: string): Promise<Uint8Array<ArrayBuffer>> {
    if (seed.length !== 16) throw Error("Invalid account key");
    const root = await crypto.subtle.importKey("raw", seed, "HKDF", false, ["deriveBits"]);
    return new Uint8Array(await crypto.subtle.deriveBits({ name: "HKDF", hash: "SHA-256", salt, info: encode(info) }, root, 256));
}
export function backupKey(account: string, seed: Uint8Array<ArrayBuffer>): Promise<Uint8Array<ArrayBuffer>> {
    return derive(seed, decodeId(account), "app.playbit.account.backup.v1");
}
export async function accountSigningKey(seed: Uint8Array<ArrayBuffer>): Promise<{ privateKey: CryptoKey; publicKey: Uint8Array<ArrayBuffer> }> {
    const signingSeed = await derive(seed, encode("app.playbit.account-key.v1"), "app.playbit.account.signing.v1");
    const pkcs8 = Uint8Array.from([...unhex("302e020100300506032b657004220420"), ...signingSeed]);
    const privateKey = await crypto.subtle.importKey("pkcs8", pkcs8, "Ed25519", true, ["sign"]);
    const jwk = await crypto.subtle.exportKey("jwk", privateKey);
    const publicKey = Uint8Array.from(atob(jwk.x!.replace(/-/g, "+").replace(/_/g, "/")), c => c.charCodeAt(0));
    return { privateKey, publicKey };
}
export function loginMessage(nonce: Uint8Array, request: Uint8Array): Uint8Array<ArrayBuffer> {
    if (nonce.length !== 32) throw Error("Invalid account key challenge");
    return Uint8Array.from([...encode("app.playbit.account.login.v1\0"), ...nonce, ...request]);
}
export function backupMessage(account: string, operation: string, revision: number, ciphertext: string): Uint8Array<ArrayBuffer> {
    const clock = new Uint8Array(8);
    new DataView(clock.buffer).setBigUint64(0, BigInt(revision), true);
    return Uint8Array.from([...encode("app.playbit.account.backup.write.v1\0"), ...decodeId(account), ...decodeId(operation), ...clock, ...encode(ciphertext)]);
}
export async function backupProof(account: string, operation: string, revision: number, ciphertext: string, seed: Uint8Array<ArrayBuffer>): Promise<{ public_key: string; signature: string }> {
    const { privateKey, publicKey } = await accountSigningKey(seed);
    return { public_key: hex(publicKey), signature: hex(new Uint8Array(await crypto.subtle.sign("Ed25519", privateKey, backupMessage(account, operation, revision, ciphertext)))) };
}
