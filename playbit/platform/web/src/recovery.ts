import { encodeId, decodeId, validId } from './identifier.ts';
import { formatAccountKey, parseAccountKey, backupKey, backupProof } from "./account-key.ts";
// Account keys decrypt private capabilities; email/CSR authentication never does.
import type { Invitation, InvitationSession, VolumeCapability, VolumeCredentials } from "./invitation.ts";
import { replicaCapability, retainedSessionKeys, scopeAddresses, validAppIdentity, validVolumeCapability, validateInvitation, verifyScopeBinding, hex, unhex } from "./invitation.ts";
import type { ApOperation } from "./ap-protocol.ts";
import type { AccountMetadataControl } from "./account-outbox.ts";
import { openAccountMetadata, readAccountMutation, commitAccountMutation, requireAccountAuthority } from "./account-outbox.ts";

// One volume this account holds keys for.
export type CatalogVolume = VolumeCapability;
export type AppScope =
    | { kind: "world"; app: InvitationSession["app"]; build: string; initialization: number[] }
    | { kind: "session"; id: string; creation_build: string; user_volume?: string | null };
// Every private capability of an account: its volumes and retained invitations.
export interface RecoveryCatalog { account: string; volumes: CatalogVolume[]; invitations: Invitation[]; scopes?: AppScope[] }
// An upload of the catalog that AP has not yet acknowledged.
export interface RecoveryPending { operation_id: string; expected_revision: number; ciphertext: string; digest: string }
// The 'recovery' metadata state row. `secret` is present once the account is
// acknowledged; `display_name` is a projected record.
export interface RecoveryState {
    deleted_volumes?: string[];
    catalog: RecoveryCatalog; acknowledged: boolean; revision: number; secret?: number[];
    digest?: string | null; pending?: RecoveryPending; display_name?: string;
}
// Deletions remain independent account records, so stale credentials cannot restore a volume.
export async function availableCatalog(catalog: RecoveryCatalog, deleted: readonly string[] = []): Promise<RecoveryCatalog> {
    if (!deleted.length) return catalog;
    const ids = new Set(deleted), sessions = new Set<string>();
    const invitations = catalog.invitations.filter(item => {
        if (!ids.has(replicaCapability(item).volume.id)) return true;
        sessions.add(item.session.id);
        return false;
    });
    const { scopes, ...base } = catalog;
    const available: RecoveryCatalog = { ...base, volumes: catalog.volumes.filter(item => !ids.has(item.volume.id)), invitations };
    for (const scope of scopes ?? []) {
        if (scope.kind === 'session' ? sessions.has(scope.id) : ids.has((await scopeAddresses({ app: scope.app })).world)) continue;
        (available.scopes ??= []).push(scope);
    }
    return available;
}
// AP's credential vault: revision 0 with null ciphertext until the first backup.
export interface RecoveryVault { revision: number; ciphertext: string | null }
// What the app shows: whether private data is unlocked here, and if a backup is due.
export interface RecoveryStatus { kind: "ready" | "locked" | "setup"; revision: number; pending_save: boolean }
// The state row and control clock as `snapshot()` completes them.
export interface RecoverySnapshot { state: RecoveryState; control: AccountMetadataControl; account: string }
// What `RecoveryStorage.read` returns; `snapshot()` fills `state` and `account` in place.
export interface RecoveryStorageRow { state: RecoveryState | undefined; control: AccountMetadataControl; account?: string }
export interface RecoveryStorage {
    read(account: string): Promise<RecoveryStorageRow>;
    write(account: string, expected: AccountMetadataControl, state: RecoveryState): Promise<void>;
}
// The AP client surface account-key recovery uses (ApClient satisfies it).
export interface RecoveryTransport {
    request(operation: ApOperation): Promise<{ data?: unknown }>;
}
// Serializes an account's recovery work, like `navigator.locks.request`.
export type RecoveryLock = <T>(callback: () => Promise<T>) => Promise<T>;
const encoder = new TextEncoder(), decoder = new TextDecoder("utf-8", { fatal: true });
export const MAX_BYTES = 256 * 1024;
export const MAX_PLAINTEXT_BYTES = 2 * 1024 * 1024;
const id = (value: unknown): value is string => typeof value === "string" && validId(value, 16);
const keyBytes = (value: ArrayLike<number> | null | undefined): boolean => value?.length === 32 && Array.from(value).every(byte => Number.isInteger(byte) && byte >= 0 && byte < 256);
const same = (a: unknown, b: unknown): boolean => JSON.stringify(a) === JSON.stringify(b);
const sameKeys = (a: VolumeCredentials, b: VolumeCredentials): boolean => same(a.database_key, b.database_key) && same(a.blob_key, b.blob_key);
const operationId = (): string => encodeId(crypto.getRandomValues(new Uint8Array(16)));
function aad(account: string, revision: number, version: number): Uint8Array<ArrayBuffer> {
    if (!id(account) || !Number.isSafeInteger(revision) || revision < 1) throw new Error("Invalid credential backup identity or revision");
    const domain = encoder.encode(`app.playbit.credentials.v${version}\0`);
    const bytes = new Uint8Array(domain.length + 24);
    bytes.set(domain); bytes.set(decodeId(account), domain.length);
    new DataView(bytes.buffer).setBigUint64(domain.length + 16, BigInt(revision), true);
    return bytes;
}
function encode(bytes: Uint8Array): string {
    let text = "";
    for (const byte of bytes) text += String.fromCharCode(byte);
    return btoa(text).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}
function decode(text: unknown): Uint8Array<ArrayBuffer> {
    if (typeof text !== "string" || text.length > Math.ceil(MAX_BYTES / 3) * 4 || !/^[A-Za-z0-9_-]+$/.test(text)) throw new Error("Invalid credential backup encoding");
    const bytes = Uint8Array.from(atob(text.replaceAll("-", "+").replaceAll("_", "/")), character => character.charCodeAt(0));
    if (encode(bytes) !== text || bytes.length <= 29 || bytes.length > MAX_BYTES || (bytes[0] !== 1 && bytes[0] !== 2)) throw new Error("Invalid credential backup envelope");
    return bytes;
}
// The output bound is checked while draining, including authenticated compressed input.
async function transform(bytes: Uint8Array<ArrayBuffer>, compress: boolean, limit: number): Promise<Uint8Array<ArrayBuffer>> {
    const stream = new Blob([bytes]).stream().pipeThrough(compress ? new CompressionStream("deflate") : new DecompressionStream("deflate"));
    const reader = stream.getReader(), parts: Uint8Array[] = [];
    let length = 0;
    try {
        for (;;) {
            const {value, done} = await reader.read();
            if (done) break;
            length += value.length;
            if (length > limit) throw new Error("Credential backup exceeds decoded size limit");
            parts.push(value);
        }
    } finally { await reader.cancel(); }
    const result = new Uint8Array(length);
    let offset = 0;
    for (const part of parts) { result.set(part, offset); offset += part.length; }
    return result;
}
export async function seal(account: string, revision: number, secret: Uint8Array<ArrayBuffer>, plaintext: Uint8Array<ArrayBuffer>): Promise<string> {
    if (!keyBytes(secret) || !plaintext.length || plaintext.length > MAX_PLAINTEXT_BYTES) throw new Error("Invalid credential backup size or secret");
    const version = plaintext.length <= MAX_BYTES - 29 ? 1 : 2;
    const body = version === 1 ? plaintext : await transform(plaintext, true, MAX_BYTES - 29);
    const nonce = crypto.getRandomValues(new Uint8Array(12));
    const key = await crypto.subtle.importKey("raw", secret, "AES-GCM", false, ["encrypt"]);
    const ciphertext = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: aad(account, revision, version), tagLength: 128 }, key, body));
    const envelope = new Uint8Array(13 + ciphertext.length);
    envelope[0] = version; envelope.set(nonce, 1); envelope.set(ciphertext, 13);
    return encode(envelope);
}
export async function open(account: string, revision: number, secret: Uint8Array<ArrayBuffer>, ciphertext: unknown): Promise<Uint8Array<ArrayBuffer>> {
    if (!keyBytes(secret)) throw new Error("Invalid account key");
    const bytes = decode(ciphertext);
    const key = await crypto.subtle.importKey("raw", secret, "AES-GCM", false, ["decrypt"]);
    let plaintext: Uint8Array<ArrayBuffer>;
    try { plaintext = new Uint8Array(await crypto.subtle.decrypt({ name: "AES-GCM", iv: bytes.slice(1, 13), additionalData: aad(account, revision, bytes[0]), tagLength: 128 }, key, bytes.slice(13))); }
    catch { throw new Error("This account key does not unlock the account’s private credentials"); }
    return bytes[0] === 1 ? plaintext : transform(plaintext, false, MAX_PLAINTEXT_BYTES);
}
const emptyCatalog = (account: string): RecoveryCatalog => ({ account, volumes: [], invitations: [] });
// Validates a decoded backup or local catalog; the parameter is only a candidate shape.
function scopeKey(scope: AppScope): string {
    return scope.kind === 'world' ? `world/${scope.app.publisher}/${scope.app.app_id}` : `session/${scope.id}`;
}
function sameScope(left: AppScope, right: AppScope): boolean {
    if (left.kind === 'world' && right.kind === 'world') {
        return left.app.publisher === right.app.publisher && left.app.app_id === right.app.app_id
            && left.build === right.build && same(left.initialization, right.initialization);
    }
    return left.kind === 'session' && right.kind === 'session' && left.id === right.id
        && left.creation_build === right.creation_build && (left.user_volume ?? null) === (right.user_volume ?? null);
}
export async function validateCatalog(catalog: Partial<RecoveryCatalog> | null | undefined, account: string, verifySeeds = false): Promise<RecoveryCatalog> {
    if (!catalog || catalog.account !== account || !Array.isArray(catalog.volumes) || !Array.isArray(catalog.invitations)
        || (catalog.scopes !== undefined && !Array.isArray(catalog.scopes))
        || Object.keys(catalog).sort().join() !== (catalog.scopes === undefined ? 'account,invitations,volumes' : 'account,invitations,scopes,volumes')) throw new Error("Invalid account credential catalog");
    const volumes = new Map<string, CatalogVolume>(), sessions = new Map<string, InvitationSession>();
    for (const item of catalog.volumes) {
        if (!item || Object.keys(item).sort().join() !== 'credentials,volume' || !validVolumeCapability(item) || volumes.has(item.volume.id)) throw new Error("Invalid or duplicate recovered volume");
        volumes.set(item.volume.id, item);
    }
    for (const invitation of catalog.invitations) {
        if (verifySeeds) await retainedSessionKeys(invitation);
        else { validateInvitation(invitation); await verifyScopeBinding(invitation); }
        if (sessions.has(invitation.session.id)) throw new Error("Duplicate recovered session");
        sessions.set(invitation.session.id, invitation.session);
        const { volume, credentials } = replicaCapability(invitation);
        const retained = volumes.get(volume.id);
        if (!retained || retained.volume.owner !== volume.owner || retained.volume.personal !== volume.personal
            || retained.volume.share_key_with_service !== volume.share_key_with_service || !sameKeys(retained.credentials, credentials)) throw new Error("Recovered invitation keys conflict");
    }
    const keys = new Set<string>();
    for (const scope of catalog.scopes ?? []) {
        if (!scope || typeof scope !== 'object') throw Error('Invalid recovered app scope');
        const fields = Object.keys(scope).sort().join();
        if (scope.kind === 'world') {
            if (fields !== 'app,build,initialization,kind' || !validAppIdentity(scope.app) || scope.app.publisher !== account
                || !validId(scope.build, 32) || !Array.isArray(scope.initialization) || !keyBytes(scope.initialization)) throw Error('Invalid recovered World scope');
            const volume = volumes.get((await scopeAddresses({ app: scope.app })).world)?.volume;
            if (!volume || volume.owner !== account || volume.personal || !volume.share_key_with_service) throw Error('Recovered World scope has no matching volume capability');
        } else if (scope.kind !== 'session' || (fields !== 'creation_build,id,kind,user_volume' && fields !== 'creation_build,id,kind')
            || !validId(scope.id, 32) || !validId(scope.creation_build, 32) || (scope.user_volume != null && !id(scope.user_volume))
            || sessions.get(scope.id)?.creator !== account) throw Error('Invalid recovered session scope');
        const key = scopeKey(scope);
        if (keys.has(key)) throw Error('Duplicate recovered app scope');
        keys.add(key);
    }
    return catalog as RecoveryCatalog;
}
export async function mergeCatalog(remote: RecoveryCatalog, local: RecoveryCatalog): Promise<RecoveryCatalog> {
    await validateCatalog(remote, local.account); await validateCatalog(local, local.account);
    const volumes = new Map<string, CatalogVolume>(remote.volumes.map(item => [item.volume.id, item]));
    for (const item of local.volumes) {
        const old = volumes.get(item.volume.id);
        if (old && (old.volume.owner !== item.volume.owner || old.volume.personal !== item.volume.personal
            || old.volume.share_key_with_service !== item.volume.share_key_with_service || !sameKeys(old.credentials, item.credentials))) throw new Error("Recovered volume keys conflict with this web browser");
        volumes.set(item.volume.id, item);
    }
    const invitations = new Map<string, Invitation>();
    for (const after of [...remote.invitations, ...local.invitations]) {
        const before = invitations.get(after.session.id);
        if (before) {
            for (const field of ["id", "root_volume", "shared_volume", "creator", "update_authority"] as const) {
                if (before.session[field] !== after.session[field]) throw new Error("Recovered session identity conflicts with this web browser");
            }
            if (before.session.app.publisher !== after.session.app.publisher || before.session.app.app_id !== after.session.app.app_id
                || !same(before.seed, after.seed) || !sameKeys(replicaCapability(before).credentials, replicaCapability(after).credentials)) throw new Error("Recovered session keys conflict with this web browser");
        }
        invitations.set(after.session.id, after);
    }
    const scopes = new Map<string, AppScope>();
    for (const scope of [...remote.scopes ?? [], ...local.scopes ?? []]) {
        const key = scopeKey(scope), previous = scopes.get(key);
        if (previous && !sameScope(previous, scope)) throw Error('Recovered app scope conflicts with this web browser');
        scopes.set(key, scope);
    }
    return {
        account: local.account,
        volumes: [...volumes.values()].sort((a, b) => a.volume.id.localeCompare(b.volume.id)),
        invitations: [...invitations].sort(([a], [b]) => a.localeCompare(b)).map(([, invitation]) => invitation),
        ...(scopes.size ? { scopes: [...scopes].sort(([a], [b]) => a.localeCompare(b)).map(([, scope]) => scope) } : {}),
    };
}
const metadataStorage: RecoveryStorage = {
    async read(account) {
        const metadata = await openAccountMetadata<RecoveryState>('recovery');
        try {
            const { state, control } = await readAccountMutation(metadata, account);
            return { state, control };
        } finally { metadata.database.close(); }
    },
    async write(account, expected, state) {
        const metadata = await openAccountMetadata<RecoveryState>('recovery');
        try { await commitAccountMutation(metadata, account, expected, state); }
        finally { metadata.database.close(); }
    },
};
const digest = async (value: unknown): Promise<string> => hex(new Uint8Array(await crypto.subtle.digest("SHA-256", encoder.encode(JSON.stringify(value)))));
export class RecoveryClient {
    ap: RecoveryTransport;
    account: string;
    storage: RecoveryStorage;
    lock: RecoveryLock;
    recovering?: boolean;
    constructor(ap: RecoveryTransport, account: string, storage: RecoveryStorage = metadataStorage, lock: RecoveryLock = callback => navigator.locks.request(`playbit-recovery:${account}`, callback)) {
        if (!id(account)) throw new Error("Sign in before recovering private credentials");
        this.ap = ap; this.account = account; this.storage = storage; this.lock = lock;
    }
    async snapshot(): Promise<RecoverySnapshot> {
        if (this.storage === metadataStorage) await requireAccountAuthority(this.account);
        const snapshot = await this.storage.read(this.account);
        snapshot.state ??= { catalog: emptyCatalog(this.account), acknowledged: false, revision: 0 };
        snapshot.account = this.account;
        return snapshot as RecoverySnapshot; // `state` and `account` were just filled in.
    }
    async state(): Promise<RecoveryState> { return (await this.snapshot()).state; }
    async store(snapshot: RecoverySnapshot, state: RecoveryState = snapshot.state): Promise<void> {
        if (snapshot.account !== this.account) throw Error('Recovery snapshot belongs to another account');
        state.deleted_volumes ??= snapshot.state.deleted_volumes;
        state.catalog = await availableCatalog(state.catalog, state.deleted_volumes);
        await this.storage.write(this.account, snapshot.control, state);
        snapshot.state = state;
        snapshot.control = { ...snapshot.control, revision: snapshot.control.revision + 1n };
    }
    async vault(): Promise<RecoveryVault> {
        // Service JSON, validated field by field below.
        const value = (await this.ap.request({ op: "get_credential_vault", operation_id: operationId() })).data as RecoveryVault | null | undefined;
        if (!value || !Number.isSafeInteger(value.revision) || value.revision < 0 || (value.revision === 0) !== (value.ciphertext === null)) throw new Error("Invalid credential backup response");
        if (value.ciphertext !== null) decode(value.ciphertext);
        return value;
    }
    async status(): Promise<RecoveryStatus> {
        const state = await this.state();
        if (state.acknowledged) return { kind: "ready", revision: state.revision, pending_save: Boolean(state.pending) || state.digest !== await digest(state.catalog) };
        const vault = await this.vault();
        return { kind: vault.revision ? "locked" : "setup", revision: vault.revision, pending_save: Boolean(state.secret) && !vault.revision };
    }
    async prepare(): Promise<string> {
        return this.lock(async () => {
            if ((await this.status()).kind !== "setup") throw new Error("Use the existing account key");
            const snapshot = await this.snapshot(), state = snapshot.state; state.secret ??= Array.from(crypto.getRandomValues(new Uint8Array(16)));
            await this.store(snapshot); return formatAccountKey(Uint8Array.from(state.secret));
        });
    }
    async save(): Promise<RecoveryStatus> {
        return this.lock(async () => {
            const snapshot = await this.snapshot(), state = snapshot.state;
            if (!state.secret) throw new Error("Save the displayed account key first");
            if (!state.acknowledged && (await this.vault()).revision) throw new Error("An account backup already exists. Recover it instead of replacing it");
            state.acknowledged = true; await this.store(snapshot);
            await this.backupUnlocked(); return this.status();
        });
    }
    async recover(text: unknown): Promise<RecoveryStatus> {
        if (this.recovering) throw new Error("Private credential recovery is already in progress");
        this.recovering = true;
        try {
            const secret = parseAccountKey(text), vault = await this.vault();
            if (!vault.revision) throw new Error("No private credential backup exists for this account");
            const remote = await validateCatalog(JSON.parse(decoder.decode(await open(this.account, vault.revision, await backupKey(this.account, secret), vault.ciphertext))), this.account, true);
            // Network and decryption precede the serialized local commit.
            return await this.lock(async () => {
                const snapshot = await this.snapshot(), state = snapshot.state;
                if (vault.revision < state.revision) throw new Error("The backup is older than this web browser’s acknowledgment");
                const catalog = await mergeCatalog(remote, state.catalog);
                await this.store(snapshot, { catalog, secret: Array.from(secret), acknowledged: true, revision: vault.revision, digest: same(catalog, remote) ? await digest(catalog) : null });
                return this.status();
            });
        } finally { this.recovering = false; }
    }
    async catalog(): Promise<RecoveryCatalog> {
        return this.lock(async () => {
            const snapshot = await this.snapshot(), state = snapshot.state;
            if (!state.acknowledged) return state.catalog;
            // Opening recovered apps or personal-volume selection is an explicit
            // refresh. Unchanged periodic backups keep their local fast path.
            const latest = await this.vault();
            if (latest.revision < state.revision) throw new Error("The backup is older than this web browser’s acknowledgment");
            if (!latest.revision) return state.catalog;
            const remote = await validateCatalog(JSON.parse(decoder.decode(await open(this.account, latest.revision, await backupKey(this.account, Uint8Array.from(state.secret!)), latest.ciphertext))), this.account, true);
            state.catalog = await mergeCatalog(remote, state.catalog);
            state.revision = latest.revision;
            state.digest = same(state.catalog, remote) ? await digest(state.catalog) : null;
            // An uncertain upload retains its operation ID and exact ciphertext
            // until backup replays it or reconciles a competing newer revision.
            await this.store(snapshot);
            return state.catalog;
        });
    }
    async rememberInvitation(invitation: Invitation): Promise<void> {
        return this.lock(async () => {
            await retainedSessionKeys(invitation);
            const snapshot = await this.snapshot(), state = snapshot.state;
            state.catalog = await mergeCatalog(state.catalog, { account: this.account, volumes: [replicaCapability(invitation)], invitations: [invitation] });
            await this.store(snapshot);
            await this.backupUnlocked();
        });
    }
    backup(): Promise<void> { return this.lock(() => this.backupUnlocked()); }
    // An acknowledged state always carries its `secret`, hence the assertions below.
    async backupUnlocked(): Promise<void> {
        const snapshot = await this.snapshot(), state = snapshot.state;
        if (!state.acknowledged) return;
        const currentDigest = await digest(state.catalog);
        if (!state.pending && state.digest === currentDigest) return;
        let latest: RecoveryVault | undefined;
        if (state.pending) {
            const pending = state.pending;
            try {
                await this.sendPending(snapshot);
                if (state.digest === currentDigest) return;
            } catch (failure) {
                state.pending = pending;
                latest = await this.vault();
                if (latest.revision <= pending.expected_revision) throw failure;
                if (latest.revision === pending.expected_revision + 1 && latest.ciphertext === pending.ciphertext) {
                    await this.sendPending(snapshot);
                    if (state.digest === currentDigest) return;
                }
            }
        }
        latest ??= await this.vault();
        if (latest.revision < state.revision) throw new Error("The backup is older than this web browser’s acknowledgment");
        let catalog = state.catalog;
        if (latest.revision) {
            let remote: RecoveryCatalog; // Decoded JSON; mergeCatalog validates it.
            try { remote = JSON.parse(decoder.decode(await open(this.account, latest.revision, await backupKey(this.account, Uint8Array.from(state.secret!)), latest.ciphertext))); }
            catch (failure) {
                if (!state.revision) { state.acknowledged = false; delete state.pending; await this.store(snapshot); }
                throw failure;
            }
            catalog = await availableCatalog(await mergeCatalog(remote, catalog), state.deleted_volumes);
            if (same(catalog, remote)) {
                state.catalog = catalog; state.revision = latest.revision; state.digest = await digest(catalog);
                delete state.pending;
                await this.store(snapshot);
                return;
            }
        }
        state.catalog = catalog;
        state.pending = { operation_id: operationId(), expected_revision: latest.revision,
            ciphertext: await seal(this.account, latest.revision + 1, await backupKey(this.account, Uint8Array.from(state.secret!)), encoder.encode(JSON.stringify(catalog))), digest: await digest(catalog) };
        await this.store(snapshot);
        await this.sendPending(snapshot);
    }
    async sendPending(snapshot: RecoverySnapshot): Promise<void> {
        const state = snapshot.state;
        const upload = state.pending!; // Callers set `pending` before sending it.
        // Service JSON, validated on the next line.
        const value = (await this.ap.request({ op: "put_credential_vault", operation_id: upload.operation_id, expected_revision: upload.expected_revision, ciphertext: upload.ciphertext, ...await backupProof(this.account, upload.operation_id, upload.expected_revision, upload.ciphertext, Uint8Array.from(state.secret!)) })).data as RecoveryVault | null | undefined;
        if (value?.revision !== upload.expected_revision + 1 || value.ciphertext !== upload.ciphertext) throw new Error("Credential backup acknowledgment does not match this web browser’s upload");
        state.revision = Math.max(state.revision, value.revision); state.digest = upload.digest; delete state.pending;
        await this.store(snapshot);
    }
}
