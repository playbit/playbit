import { encodeId, decodeId, validId } from './identifier.ts';
import { configuration, deployments } from './release-channel.ts';
import type { Deployment } from './release-channel.ts';
// Session links. Keep derivations and the envelope layout identical to
// archipelago-store/src/invitation.rs: one 16-byte seed derives the session
// signing key (its public key is the session ID) and the AES-GCM key of the
// creator's envelope, which the service stores but cannot open.
const encoder = new TextEncoder();
export const hex = (bytes: Iterable<number> | ArrayLike<number>): string => Array.from(bytes, byte => byte.toString(16).padStart(2, "0")).join("");
export const unhex = (text: unknown): Uint8Array<ArrayBuffer> => {
    if (typeof text !== "string" || !/^(?:[0-9a-f]{2})+$/.test(text)) throw new Error("Invalid invitation identity");
    // The pattern above guarantees at least one pair, so the match is never null.
    return Uint8Array.from(text.match(/../g)!, byte => parseInt(byte, 16));
};
export const SEED_BYTES = 16, CODE_CHARS = 22, MAX_ENVELOPE_BYTES = 4096;
const ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const LOCAL = "session:";
const keyBytes = (value: unknown): value is number[] => Array.isArray(value) && value.length === 32 && value.every(byte => Number.isInteger(byte) && byte >= 0 && byte < 256);
const seedBytes = (value: unknown): value is Uint8Array => value instanceof Uint8Array && value.length === SEED_BYTES;
const damaged = (): Error => new Error("This invitation link is incomplete. Open the full session link");

// The decrypted invitation record, as archipelago-store serializes it, plus the seed.
export type SessionSelector = { kind: "exact"; build_id: string } | { kind: "app"; app: { publisher: string; app_id: string } };
export interface InvitationSession {
    app: { publisher: string; app_id: string };
    id: string; selector: SessionSelector; resolved_build_id: string; root_volume: string; shared_volume?: string;
    creator: string; update_authority: "creator" | "app";
}
export interface InvitationVolume { id: string; name: string; owner: string; share_key_with_service: boolean; personal: boolean }
export interface VolumeCredentials { database_key: number[]; blob_key: number[] }
export interface VolumeCapability { volume: InvitationVolume; credentials: VolumeCredentials }
export type Invitation = { seed: number[]; session: InvitationSession } & (
    | { root: InvitationVolume; root_credentials: VolumeCredentials; shared?: never }
    | { shared: VolumeCapability; root?: never; root_credentials?: never }
);
export function replicaCapability(invitation: Invitation): VolumeCapability {
    if (invitation.shared) return invitation.shared;
    return { volume: invitation.root, credentials: invitation.root_credentials };
}
export type ScopeRequest = { app: InvitationSession["app"]; session?: string };
export type ScopeAddresses = { world: string; shared?: string };
type ScopeResolver = (request: ScopeRequest) => Promise<ScopeAddresses>;
let resolveScopes: ScopeResolver;
export function setScopeAddresses(resolve: ScopeResolver): void { resolveScopes = resolve; }
export function scopeAddresses(request: ScopeRequest): Promise<ScopeAddresses> { return resolveScopes(request); }
// Keys derived from a session seed; `privateKey` signs join proofs, `envelopeKey` opens the creator's envelope.
export interface SessionKeys { seed: Uint8Array<ArrayBuffer>; session_id: string; publicKey: Uint8Array<ArrayBuffer>; privateKey: CryptoKey; envelopeKey: CryptoKey }
// A parsed session link; `invitation` is null until an envelope is opened. Callers may
// remember the envelope text they opened alongside it.
export interface ParsedInvitation extends SessionKeys { url: string; uri: string | null; invitation: Invitation | null; envelope?: string }

export function sessionCode(seed: Uint8Array): string {
    if (!seedBytes(seed)) throw new Error("Invalid session seed");
    let value = BigInt("0x" + hex(seed)), code = "";
    for (let index = 0; index < CODE_CHARS; index++) { code = ALPHABET[Number(value % 58n)] + code; value /= 58n; }
    return code;
}
export function parseSessionCode(code: unknown): Uint8Array<ArrayBuffer> {
    if (typeof code !== "string" || code.length !== CODE_CHARS) throw damaged();
    let value = 0n;
    for (const letter of code) {
        const digit = ALPHABET.indexOf(letter);
        if (digit < 0) throw damaged();
        value = value * 58n + BigInt(digit);
        if (value >= 1n << 128n) throw damaged();
    }
    return unhex(value.toString(16).padStart(SEED_BYTES * 2, "0"));
}
export function isSessionPath(pathname: string): boolean {
    return pathname === '/s' || pathname === '/s/';
}

// One seed, however it was written: the web link, the playbit:session: URI,
// the local session: form or the bare code.
export function parseSessionLink(text: unknown, selected: Deployment): Uint8Array<ArrayBuffer> {
    configuration(selected);
    if (typeof text !== "string" || text.length > 256) throw damaged();
    for (const candidate of deployments) {
        const { channel, website } = configuration(candidate);
        const prefixes = [website + '/s#', website + '/s/#'];
        if (candidate === channel) prefixes.push(`playbit:session:${channel}:`);
        for (const prefix of prefixes) {
            if (!text.startsWith(prefix)) continue;
            if (candidate !== selected) {
                const name = candidate === 'public' ? 'Public' : candidate === 'testing' ? 'Testing' : website;
                throw new Error(`This invitation belongs to ${name}. Open ${website}/s to join it`);
            }
            return parseSessionCode(text.slice(prefix.length));
        }
    }
    if (text.startsWith(LOCAL)) return parseSessionCode(text.slice(LOCAL.length));
    return parseSessionCode(text);
}
export const sessionLink = (seed: Uint8Array, selected: Deployment): string => configuration(selected).website + '/s#' + sessionCode(seed);
export function sessionUri(seed: Uint8Array, selected: Deployment): string | null {
    const { channel } = configuration(selected);
    return selected === channel ? `playbit:session:${channel}:${sessionCode(seed)}` : null;
}

export async function deriveSessionKeys(seed: Uint8Array<ArrayBuffer>): Promise<SessionKeys> {
    if (!seedBytes(seed)) throw new Error("Invalid session seed");
    const root = await crypto.subtle.importKey("raw", seed, "HKDF", false, ["deriveBits", "deriveKey"]);
    const params = (info: string): HkdfParams => ({ name: "HKDF", hash: "SHA-256", salt: encoder.encode("app.playbit.session.v1"), info: encoder.encode(info) });
    const signingSeed = new Uint8Array(await crypto.subtle.deriveBits(params("app.playbit.session.signing.v1"), root, 256));
    const pkcs8 = Uint8Array.from([...unhex("302e020100300506032b657004220420"), ...signingSeed]);
    const exportable = await crypto.subtle.importKey("pkcs8", pkcs8, "Ed25519", true, ["sign"]);
    const jwk = await crypto.subtle.exportKey("jwk", exportable);
    // An exported Ed25519 JWK always carries its public point in `x`.
    const publicKey = Uint8Array.from(atob(jwk.x!.replace(/-/g, "+").replace(/_/g, "/")), char => char.charCodeAt(0));
    const privateKey = await crypto.subtle.importKey("pkcs8", pkcs8, "Ed25519", false, ["sign"]);
    const envelopeKey = await crypto.subtle.deriveKey(params("app.playbit.session.envelope.v1"), root, { name: "AES-GCM", length: 256 }, false, ["decrypt"]);
    return { seed, session_id: encodeId(publicKey), publicKey, privateKey, envelopeKey };
}

function keysAre(value: unknown, sorted: string): boolean {
    return value !== null && typeof value === 'object' && Object.keys(value).sort().join() === sorted;
}
export function validAppIdentity(app: InvitationSession["app"] | undefined): boolean {
    return Boolean(app && keysAre(app, 'app_id,publisher') && validId(app.publisher, 16) && app.publisher !== '1'.repeat(16)
        && typeof app.app_id === 'string' && app.app_id.length <= 64 && /^[a-z0-9]+(?:[.-][a-z0-9]+)*$/.test(app.app_id));
}
export function validVolumeCapability(capability: VolumeCapability): boolean {
    const { volume, credentials } = capability;
    return keysAre(volume, 'id,name,owner,personal,share_key_with_service')
        && validId(volume.id, 16) && validId(volume.owner, 16) && typeof volume.name === 'string'
        && volume.name.trim().length > 0 && encoder.encode(volume.name).length <= 256
        && typeof volume.personal === 'boolean' && typeof volume.share_key_with_service === 'boolean'
        && keysAre(credentials, 'blob_key,database_key') && keyBytes(credentials.database_key) && keyBytes(credentials.blob_key)
        && !credentials.database_key.every((byte, index) => byte === credentials.blob_key[index]);
}
// Both envelope formats confer exactly one capability. A Shared invitation never confers World credentials.
export function validateInvitation(candidate: unknown): Invitation {
    const value = candidate as Invitation;
    const invalid = () => new Error("The invitation identities or permissions are invalid. Ask for a new session link");
    const shared = keysAre(value, 'seed,session,shared');
    if (!(shared ? keysAre(value.shared, 'credentials,volume') : keysAre(value, 'root,root_credentials,seed,session'))) throw invalid();
    const { session, seed } = value;
    const sessionFields = 'app,creator,id,resolved_build_id,root_volume,selector';
    if (!keysAre(session, sessionFields + (shared ? ',shared_volume' : '') + ',update_authority')
        || !Array.isArray(seed) || seed.length !== SEED_BYTES || !seed.every(byte => Number.isInteger(byte) && byte >= 0 && byte < 256)
        || !validId(session.id, 32) || !validId(session.resolved_build_id, 32) || !validId(session.root_volume, 16)
        || !validId(session.creator, 16) || !validAppIdentity(session.app) || !["creator", "app"].includes(session.update_authority)) throw invalid();
    const { selector } = session;
    if (selector?.kind === 'exact') {
        if (!keysAre(selector, 'build_id,kind') || selector.build_id !== session.resolved_build_id || !shared && session.app.publisher !== session.creator) throw invalid();
    } else if (selector?.kind !== 'app' || !keysAre(selector, 'app,kind') || !validAppIdentity(selector.app)
        || selector.app.publisher !== session.app.publisher || selector.app.app_id !== session.app.app_id) throw invalid();
    const capability = replicaCapability(value);
    if (!validVolumeCapability(capability)) throw invalid();
    const volume = capability.volume;
    if (shared ? volume.id !== session.shared_volume || volume.id === session.root_volume || volume.owner !== session.creator
        || volume.personal || !volume.share_key_with_service : volume.id !== session.root_volume) throw invalid();
    return value;
}
export async function verifyScopeBinding(invitation: Invitation): Promise<void> {
    if (!invitation.shared) return;
    const { session } = invitation;
    const addresses = await scopeAddresses({ app: session.app, session: session.id });
    if (session.root_volume !== addresses.world || session.shared_volume !== addresses.shared) throw new Error("The Shared invitation does not match its app and session");
}

// A link's keys with no envelope yet; `invitation` is filled by openEnvelope or retainedInvitation.
export async function parseInvitation(text: unknown, selected: Deployment): Promise<ParsedInvitation> {
    const seed = parseSessionLink(text, selected);
    return { ...await deriveSessionKeys(seed), url: sessionLink(seed, selected), uri: sessionUri(seed, selected), invitation: null };
}
export interface RetainedSessionKeys extends SessionKeys { invitation: Invitation }
// Verify a retained capability without assigning it another environment's URL.
export async function retainedSessionKeys(invitation: Invitation): Promise<RetainedSessionKeys> {
    validateInvitation(invitation);
    await verifyScopeBinding(invitation);
    const keys = await deriveSessionKeys(Uint8Array.from(invitation.seed));
    if (keys.session_id !== invitation.session.id) throw new Error("The invitation key does not match its session");
    return { ...keys, invitation };
}
export async function retainedInvitation(invitation: Invitation, selected: Deployment): Promise<ParsedInvitation> {
    const keys = await retainedSessionKeys(invitation);
    return { ...keys, url: sessionLink(keys.seed, selected), uri: sessionUri(keys.seed, selected) };
}
// `1 ‖ nonce ‖ ciphertext ‖ tag`, bound to the session's public key.
export async function openEnvelope(keys: SessionKeys, text: unknown): Promise<Invitation> {
    if (typeof text !== "string" || !text || text.length > Math.ceil(MAX_ENVELOPE_BYTES / 3) * 4 || !/^[A-Za-z0-9_-]+$/.test(text)) throw new Error("This invitation is damaged. Ask for a new session link");
    const bytes = Uint8Array.from(atob(text.replace(/-/g, "+").replace(/_/g, "/")), char => char.charCodeAt(0));
    if (bytes.length < 30 || bytes.length > MAX_ENVELOPE_BYTES || bytes[0] !== 1) throw new Error("This invitation is damaged. Ask for a new session link");
    const additionalData = Uint8Array.from([...encoder.encode("app.playbit.session.envelope.v1\0"), ...keys.publicKey]);
    let plain: ArrayBuffer;
    try { plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv: bytes.slice(1, 13), additionalData, tagLength: 128 }, keys.envelopeKey, bytes.slice(13)); }
    catch { throw new Error("This invitation does not belong to its session link"); }
    let body: unknown;
    try { body = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(plain)); }
    catch { throw new Error("This invitation is damaged. Ask for a new session link"); }
    if (!body || typeof body !== "object" || "seed" in body) throw new Error("This invitation is damaged. Ask for a new session link");
    const invitation = validateInvitation({ seed: Array.from(keys.seed), ...body });
    await verifyScopeBinding(invitation);
    if (invitation.session.id !== keys.session_id) throw new Error("The invitation key does not match its session");
    return invitation;
}
export async function sessionProof(privateKey: CryptoKey, account: string): Promise<number[]> {
    const bytes = Uint8Array.from([...encoder.encode("app.playbit.session.join.v1\0"), ...decodeId(account)]);
    return Array.from(new Uint8Array(await crypto.subtle.sign("Ed25519", privateKey, bytes)));
}
