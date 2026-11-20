import { decodeId, validId } from './identifier.ts';
// Client-only ciphertext values in the account's ordinary kvdb replica.
// Keep byte domains/layout identical to archipelago-store/src/account.rs.
import type { Invitation, InvitationVolume, VolumeCredentials, RetainedSessionKeys } from "./invitation.ts";
import { unhex, retainedSessionKeys } from "./invitation.ts";
import type { AppScope } from "./recovery.ts";
import { unsupportedAccount } from './account-format.ts';

export const MAX_ACCOUNT_RECORD_BYTES = 48 * 1024;
const OVERHEAD = 29;
const domain = new TextEncoder().encode("app.playbit.account.record.v1\0");
const encode = (value: string): Uint8Array<ArrayBuffer> => new TextEncoder().encode(value);
const bytes = (value: unknown): value is Uint8Array => value instanceof Uint8Array;

// One personal-volume grant inside a `personal_volumes` record or a local selection.
export interface PersonalGrant { volume: string; write: boolean }
// The decrypted account record kinds, as playbit/src/store/account/record.rs serializes them.
export interface PersonalSelection extends PersonalGrant { incarnation: string }
export interface PersonalVolumesRecord { kind: "personal_volumes"; session: string; grants: PersonalSelection[] }
export interface DisplayNameRecord { kind: "display_name"; name: string }
export interface VolumeRecord { kind: "volume"; volume: InvitationVolume; credentials: VolumeCredentials }
export interface VolumeDeletedRecord { kind: "volume_deleted"; volume: string }
export interface InvitationRecord { kind: "invitation"; invitation: Invitation; app_name: string | null }
export interface HostedVolumeRecord { kind: "hosted_volume"; volume: InvitationVolume; credentials: VolumeCredentials; scope: AppScope }
export interface HostedInvitationRecord { kind: "hosted_invitation"; invitation: Invitation; app_name: string | null; scope: AppScope }
export type AccountRecord = HostedVolumeRecord | HostedInvitationRecord | PersonalVolumesRecord | DisplayNameRecord | VolumeRecord | VolumeDeletedRecord | InvitationRecord;
// pb_account_decode's JSON: the record plus the logical key its record key must derive from.
export interface DecodedAccountRecord { logical_key: string; record: AccountRecord | null; unsupported: boolean }
// An authenticated record; retained capabilities acquire URLs only in their channel.
export interface OpenedAccountRecord { record: AccountRecord | null; invitation: RetainedSessionKeys | undefined }
// Runs the runtime Wasm's pb_account_decode on a decrypted record.
export type AccountRecordDecoder = (plain: Uint8Array<ArrayBuffer>) => Uint8Array | Promise<Uint8Array>;
// Per-account record keys and AES-GCM envelopes, as `accountCipher` derives them.
export interface AccountRecordCipher {
    recordKey(logicalKey: Uint8Array<ArrayBuffer>): Promise<Uint8Array<ArrayBuffer>>;
    seal(key: Uint8Array, clock: bigint, plaintext: Uint8Array<ArrayBuffer>): Promise<Uint8Array<ArrayBuffer>>;
    open(key: Uint8Array, clock: bigint, envelope: Uint8Array<ArrayBuffer>): Promise<Uint8Array<ArrayBuffer>>;
}

// decode is the existing runtime Wasm's pb_account_decode export. Do not replace
// it with JSON.parse: duplicate fields and nested capability shapes must follow
// the native schema. This authenticates one record, not its local projection.
export async function openAccountRecord(cipher: AccountRecordCipher, key: Uint8Array, clock: bigint, envelope: Uint8Array<ArrayBuffer>, decode: AccountRecordDecoder): Promise<OpenedAccountRecord> {
    const plain = await cipher.open(key, clock, envelope);
    // The native decoder already enforced the record schema on this JSON.
    const { logical_key, record, unsupported }: DecodedAccountRecord = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(await decode(plain)));
    const expected = await cipher.recordKey(encode(logical_key));
    if (!expected.every((byte, index) => byte === key[index])) throw Error('Account record key does not match its contents');
    if (unsupported) throw unsupportedAccount();
    const invitation = record?.kind === 'invitation' || record?.kind === 'hosted_invitation' ? await retainedSessionKeys(record.invitation) : undefined;
    return { record, invitation };
}

export async function accountCipher(account: string, secret: Uint8Array<ArrayBuffer>): Promise<AccountRecordCipher> {
    if (typeof account !== "string" || !validId(account, 16)
        || !bytes(secret) || secret.length !== 16) throw new Error("Invalid account record credentials");
    const accountBytes = decodeId(account);
    const root = await crypto.subtle.importKey("raw", secret, "HKDF", false, ["deriveKey"]);
    const derive = (info: string, algorithm: AesDerivedKeyParams | HmacImportParams, usages: KeyUsage[]): Promise<CryptoKey> => crypto.subtle.deriveKey({
        name: "HKDF", hash: "SHA-256", salt: accountBytes, info: encode(info),
    }, root, algorithm, false, usages);
    const encryption = await derive("app.playbit.account.encryption.v1", { name: "AES-GCM", length: 256 }, ["encrypt", "decrypt"]);
    const lookup = await derive("app.playbit.account.lookup.v1", { name: "HMAC", hash: "SHA-256", length: 256 }, ["sign"]);
    function aad(key: Uint8Array, clock: bigint): Uint8Array<ArrayBuffer> {
        if (!bytes(key) || key.length !== 32 || typeof clock !== "bigint"
            || clock <= 0n || clock > 0xfffffffffffffffcn) throw new Error("Invalid account record key or clock");
        const value = new Uint8Array(domain.length + 16 + 32 + 8);
        value.set(domain); value.set(accountBytes, domain.length); value.set(key, domain.length + 16);
        new DataView(value.buffer).setBigUint64(domain.length + 48, clock, true);
        return value;
    }
    return {
        async recordKey(logicalKey) {
            if (!bytes(logicalKey) || !logicalKey.length || logicalKey.length > 511) throw new Error("Invalid account record key");
            return new Uint8Array(await crypto.subtle.sign("HMAC", lookup, logicalKey));
        },
        async seal(key, clock, plaintext) {
            const additionalData = aad(key, clock);
            if (!bytes(plaintext) || !plaintext.length || plaintext.length > MAX_ACCOUNT_RECORD_BYTES) throw new Error("Invalid account record size");
            const nonce = crypto.getRandomValues(new Uint8Array(12));
            const ciphertext = new Uint8Array(await crypto.subtle.encrypt({
                name: "AES-GCM", iv: nonce, additionalData, tagLength: 128,
            }, encryption, plaintext));
            const envelope = new Uint8Array(13 + ciphertext.length);
            envelope[0] = 1; envelope.set(nonce, 1); envelope.set(ciphertext, 13);
            return envelope;
        },
        async open(key, clock, envelope) {
            const additionalData = aad(key, clock);
            if (!bytes(envelope) || envelope.length <= OVERHEAD
                || envelope.length > MAX_ACCOUNT_RECORD_BYTES + OVERHEAD || envelope[0] !== 1) throw new Error("Invalid account record envelope");
            try {
                return new Uint8Array(await crypto.subtle.decrypt({
                    name: "AES-GCM", iv: envelope.subarray(1, 13), additionalData, tagLength: 128,
                }, encryption, envelope.subarray(13)));
            } catch { throw new Error("Account record authentication failed"); }
        },
    };
}
