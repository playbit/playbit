// AP pages carry full source cuts. Entry identity admission stays in shared Rust.
import { replicaReceipt, replicaJSON } from './replication.ts';
import type { ReplicaReceipt } from './replication.ts';
const encoder = new TextEncoder();
const maxU64 = (1n << 64n) - 1n;
export interface BloblogPage {
    cursor: string;
    more: boolean;
    entries: { clock: bigint; [key: string]: unknown }[];
}
export function bloblogPage(candidate: unknown, volume: string, log: string): {
    receipt: ReplicaReceipt; page: BloblogPage; bytes: Uint8Array;
} {
    const value = candidate as { page?: BloblogPage; database_clock?: bigint; policy_revision?: bigint; source?: unknown } | null;
    const page = value?.page;
    const fields = typeof page?.cursor === 'string' && page.cursor.length <= 104 ? page.cursor.split('/') : [];
    if (!value || Object.keys(value).sort().join() !== 'database_clock,page,policy_revision,source'
        || !page || Object.keys(page).sort().join() !== 'cursor,entries,more'
        || typeof page.more !== 'boolean' || !Array.isArray(page.entries) || page.entries.length > 128
        || !page.entries.every(entry => entry && typeof entry.clock === 'bigint' && entry.clock >= 0n && entry.clock <= maxU64)
        || fields.length !== 6 || fields[0] !== '1' || fields[1] !== volume || fields[2] !== log) {
        throw new Error('Invalid append-log replica page');
    }
    const receipt = replicaReceipt({ namespace: { status: "unavailable" }, root: null, revision: value.database_clock, policy_revision: value.policy_revision, source: value.source });
    const bytes = encoder.encode(replicaJSON(value));
    if (bytes.length > 384 * 1024) throw new Error('Append-log JSON page exceeds its limit');
    return { receipt, page, bytes };
}
