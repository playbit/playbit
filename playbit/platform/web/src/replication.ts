import { encodeId, decodeId, validId } from './identifier.ts';
import { databaseIdentity } from "./database-id.ts";
import type { DatabaseId } from "./database-id.ts";
// Public archipelago-ap::replication framing. This grants no storage authority.
export const MAX_REPLICA_EVENT: number = 52 + 64 * 1024;
const maxU64 = (1n << 64n) - 1n;
// The `JSON.parse` reviver context: `source` is the raw text of a primitive value.
export interface ReplicaIntegerContext { source?: string }
export interface ReplicaSnapshotToken { generation: string; epoch: bigint }
export type NamespaceReceipt = { status: "unavailable" } | { status: "accepted"; entry: string | null; count: number; digest: number[] };
export interface ReplicaReceipt { namespace: NamespaceReceipt; root: null | [number, number[]]; revision: bigint; policy_revision: bigint; source: ReplicaSnapshotToken }
export function snapshotInteger(key: string, value: unknown, context?: ReplicaIntegerContext): unknown {
    if (!["revision", "policy_revision", "epoch"].includes(key)) return value;
    if (typeof value !== "number" || !/^(0|[1-9][0-9]*)$/.test(context?.source ?? "")) throw new Error("Invalid snapshot integer");
    const exact = BigInt(context!.source!);
    if (exact > maxU64) throw new Error("Snapshot integer exceeds uint64");
    return exact;
}
export function bloblogInteger(key: string, value: unknown, context?: ReplicaIntegerContext): unknown {
    if (key !== 'clock' && key !== 'database_clock') return snapshotInteger(key, value, context);
    if (typeof value !== 'number' || !/^(0|[1-9][0-9]*)$/.test(context?.source ?? '')) throw new Error('Invalid append-log clock');
    const exact = BigInt(context!.source!);
    if (exact > maxU64) throw new Error('Append-log clock exceeds uint64');
    return exact;
}
export function replicaReceipt(candidate: unknown): ReplicaReceipt {
    const value = candidate as Partial<ReplicaReceipt> | null | undefined;
    const source = value?.source;
    const integer = (number: unknown): number is bigint => typeof number === "bigint" && number >= 0n && number <= maxU64;
    const root = value?.root;
    const namespace = value?.namespace;
    if (!namespace || namespace.status !== "unavailable" && namespace.status !== "accepted") throw new Error("Missing namespace receipt");
    if (!value || Object.keys(value).sort().join() !== "namespace,policy_revision,revision,root,source"
        || !integer(value.revision) || !integer(value.policy_revision) || !value.policy_revision
        || !source || Object.keys(source).sort().join() !== "epoch,generation"
        || typeof source.generation !== "string" || !validId(source.generation, 16)
        || /^1+$/.test(source.generation) || !integer(source.epoch)
        || root !== null && (!Array.isArray(root) || root.length !== 2 || !Number.isInteger(root[0]) || root[0] < 0 || root[0] > 255
            || !Array.isArray(root[1]) || root[1].length !== 32 || !root[1].every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255))) {
        throw new Error("Invalid replica snapshot receipt");
    }
    return value as ReplicaReceipt;
}
// Preserve exact wire integers when returning an authenticated receipt to Rust.
// The shared Rust SnapshotCut owns source-cut equality and admission.
export function replicaJSON(value: unknown): string {
    const exact = JSON as typeof JSON & { rawJSON(text: string): unknown };
    return JSON.stringify(value, (_, item) => typeof item === 'bigint' ? exact.rawJSON(item.toString()) : item);
}
export interface ReplicaSubscriptionAck { epoch: string; sequence: bigint; policy_revision: bigint }
export interface ReplicaTerminal { volume: string; cause: 1 | 2 | 3 | 4 }
export interface ReplicaEvent { namespace?: { entry: string; evidence: Uint8Array }; terminal: ReplicaTerminal | null; kind: number; epoch: Uint8Array; sequence: bigint; policy_revision: bigint; database: DatabaseId; delta: Uint8Array }
export function replicaInteger(key: string, value: unknown, context?: ReplicaIntegerContext): unknown {
    if (!["database_clock", "policy_revision", "sequence"].includes(key)) return value;
    if (typeof value !== "number" || !/^(0|[1-9][0-9]*)$/.test(context?.source ?? "")) throw new Error("Invalid replica integer");
    // The digit pattern above cannot match without a source string.
    const exact = BigInt(context!.source!);
    if (exact > maxU64) throw new Error("Replica integer exceeds uint64");
    return exact;
}
export function subscriptionAck(candidate: unknown): ReplicaSubscriptionAck {
    // Decoded JSON: view it as a candidate record for the structural checks below.
    const value = candidate as Partial<ReplicaSubscriptionAck> | null | undefined;
    if (!value || Object.keys(value).sort().join() !== "epoch,policy_revision,sequence"
        || !validId(value.epoch, 16) || /^1+$/.test(value.epoch) || value.sequence !== 0n
        || typeof value.policy_revision !== "bigint" || value.policy_revision < 0n || value.policy_revision > maxU64) {
        throw new Error("Invalid replica subscription acknowledgment");
    }
    return value as ReplicaSubscriptionAck;
}
export function replicaEvent(bytes: Uint8Array): ReplicaEvent {
    if (!(bytes instanceof Uint8Array) || bytes.length < 52 || bytes.length > MAX_REPLICA_EVENT
        || bytes[0] !== 1 || bytes[2] || bytes[3]) throw new Error("Invalid replica event header");
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const event: ReplicaEvent = { terminal: null, kind: bytes[1], epoch: bytes.subarray(4, 20), sequence: view.getBigUint64(20, true),
        policy_revision: view.getBigUint64(28, true), database: databaseIdentity(bytes.subarray(36, 52)), delta: bytes.subarray(52) };
    if (!event.epoch.some(Boolean) || event.sequence === 0n) throw new Error("Invalid replica event identity");
    if (event.kind === 1) {
        if (!event.delta.length || !event.policy_revision) throw new Error("Invalid replica changeset");
    } else if (event.kind === 7) {
        if (event.database === "index" || event.database === "revocations" || !event.policy_revision || event.delta.length < 4) throw new Error("Invalid namespace event");
        const size = new DataView(event.delta.buffer, event.delta.byteOffset, 4).getUint32(0, true);
        if (!size || size > event.delta.length - 4) throw new Error("Invalid namespace delta length");
        const evidence = event.delta.subarray(4 + size);
        if (evidence.length > 1024 || evidence.length % 16) throw new Error("Invalid namespace evidence length");
        event.namespace = { entry: event.database, evidence };
        event.database = "index";
        event.delta = event.delta.subarray(4, 4 + size);
    } else if (event.kind === 6) {
        if ((event.database === "index" || event.database === "revocations") || !event.policy_revision || event.delta.length > 2048) throw new Error("Invalid bloblog event");
    } else if (event.kind === 4) {
        const body = event.delta;
        if (event.database !== "index" || body.length !== 17 || !body.subarray(0, 16).some(Boolean)
            || body[16] < 1 || body[16] > 4) throw new Error("Invalid replica authority terminal");
        event.terminal = { volume: encodeId(body.subarray(0, 16)),
            cause: body[16] as 1 | 2 | 3 | 4 };
    } else if (![2, 3].includes(event.kind) || event.database !== "index" || event.delta.length || event.kind === 2 && !event.policy_revision) {
        throw new Error("Invalid replica terminal event");
    }
    return event;
}
export class ReplicaCursor {
    epoch: Uint8Array;
    sequence: bigint;
    policy_revision: bigint;
    constructor(ack: ReplicaSubscriptionAck) {
        subscriptionAck(ack);
        this.epoch = decodeId(ack.epoch); this.sequence = 0n; this.policy_revision = ack.policy_revision;
    }
    accept(event: ReplicaEvent): boolean {
        if (!this.epoch.every((byte, index) => event.epoch[index] === byte)) throw new Error("Replica stream epoch changed");
        if (event.sequence <= this.sequence) return false;
        // A terminal may replace queued data on overflow or revocation.
        if (event.kind === 3 || event.kind === 4) return true;
        if (event.sequence !== this.sequence + 1n) {
            const error: Error & { code?: string } = new Error("Replica delivery sequence gap"); error.code = "replica-gap"; throw error;
        }
        if (event.policy_revision < this.policy_revision) throw new Error("Replica policy revision regressed");
        return true;
    }
    applied(event: ReplicaEvent): void { this.sequence = event.sequence; this.policy_revision = event.policy_revision; }
}
