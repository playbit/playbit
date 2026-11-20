import { encodeId, validId } from './identifier.ts';
import { replicaCapability, validateInvitation } from "./invitation.ts";
import type { AccountRecordCipher, PersonalGrant, PersonalSelection } from './account-record.ts';
import { accountCipher } from './account-record.ts';
import { encodePersonalRecord } from './account-format.ts';
import type { AccountMetadata, AccountOutboxRecord } from './account-outbox.ts';
import { openAccountMetadata, readAccountMutation, commitAccountMutation, nextAccountClock, requireAccountAuthority } from './account-outbox.ts';
import type { RecoveryCatalog, CatalogVolume, RecoveryState } from './recovery.ts';

export type { PersonalGrant, PersonalSelection };
// The 'personal' metadata state row: the grants chosen for each session.
export type PersonalSelections = Record<string, PersonalSelection[]>;
// Anything that names a volume by id, as `validateSelection` needs it.
export interface PersonalVolumeEntry { volume: { id: string } }
const encoder = new TextEncoder();
export const MAX_PERSONAL_VOLUMES = 30;
let cachedCipher: { account: string; cipher: AccountRecordCipher } | undefined, mutationPending = false;
async function unlockedCipher(account: string): Promise<AccountRecordCipher | null> {
    if (cachedCipher?.account === account) return cachedCipher.cipher;
    const metadata = await openAccountMetadata<RecoveryState>('recovery');
    let state: RecoveryState | undefined;
    try { ({ state } = await readAccountMutation(metadata, account)); }
    finally { metadata.database.close(); }
    if (!state?.acknowledged || !Number.isSafeInteger(state.revision) || state.revision < 1) return null;
    const cipher = await accountCipher(account, Uint8Array.from(state.secret ?? []));
    cachedCipher = { account, cipher }; return cipher;
}

export async function personalVolumes(catalog: RecoveryCatalog): Promise<CatalogVolume[]> {
    const shared = new Set<string>();
    for (const invitation of catalog.invitations) {
        validateInvitation(invitation);
        shared.add(replicaCapability(invitation).volume.id);
    }
    return catalog.volumes.filter(item => item.volume.owner === catalog.account && item.volume.personal && !shared.has(item.volume.id));
}
// `grants` is a candidate from a message; every field is checked here.
export function validateSelection(grants: unknown, volumes: readonly PersonalVolumeEntry[]): PersonalGrant[] {
    if (!Array.isArray(grants)) throw new Error("Select personal volumes explicitly");
    if (grants.length > MAX_PERSONAL_VOLUMES) throw new Error("Personal volume capacity exceeded");
    const eligible = new Set(volumes.map(item => item.volume.id)), selected = new Set<string>();
    for (const grant of grants) {
        if (!grant || typeof grant.write !== "boolean" || !eligible.has(grant.volume) || selected.has(grant.volume)) throw new Error("Invalid personal volume selection");
        selected.add(grant.volume);
    }
    return grants.map(({ volume, write }) => ({ volume, write }));
}
// Reads the selections when `grants` is omitted; otherwise records the decision
// for `session` (checking it still equals `expected`, when given). `grants` is
// re-checked here even though callers pass `validateSelection`'s result.
export async function personalSelections(account: string, session?: string, grants?: PersonalGrant[], expected?: PersonalSelection[]): Promise<PersonalSelections> {
    if (typeof account !== 'string' || !validId(account, 16)) throw Error('Invalid personal volume account');
    await requireAccountAuthority(account);
    if (grants !== undefined && mutationPending) throw Error('A personal volume decision is already being saved');
    if (grants !== undefined) mutationPending = true;
    let metadata: AccountMetadata<PersonalSelections> | undefined;
    try {
        metadata = await openAccountMetadata<PersonalSelections>('personal');
        if (grants === undefined) {
            const selections = (await readAccountMutation(metadata, account)).state ?? {};
            for (const selection of Object.values(selections)) {
                for (const grant of selection) {
                    if (!validId(grant.incarnation, 16) || /^1+$/.test(grant.incarnation)) throw Error('Invalid stored personal volume incarnation');
                }
            }
            return selections;
        }
        if (typeof session !== 'string' || !validId(session, 32) || !Array.isArray(grants) || grants.length > MAX_PERSONAL_VOLUMES) throw Error('Invalid personal volume decision');
        const selected = new Set<string>();
        for (const grant of grants) {
            if (!grant || Object.keys(grant).sort().join() !== 'volume,write' || typeof grant.volume !== 'string' || !validId(grant.volume, 16)
                || typeof grant.write !== 'boolean' || selected.has(grant.volume)) throw Error('Invalid personal volume decision');
            selected.add(grant.volume);
        }
        grants = grants.map(({ volume, write }) => ({ volume, write }));
        // WebCrypto runs before write admission. The final revision comparison
        // rejects concurrent decisions rather than overwriting their consent.
        const cipher = await unlockedCipher(account);
        const key = cipher && await cipher.recordKey(encoder.encode(`session-mounts/${session}`));
        const opaque = key && Array.from(key, byte => byte.toString(16).padStart(2, '0')).join('');
        const snapshot = await readAccountMutation(metadata, account, opaque ?? undefined), value: PersonalSelections = snapshot.state ?? {};
        if (expected !== undefined && JSON.stringify(value[session] ?? []) !== JSON.stringify(expected)) {
            throw Error('Personal volume access changed while connecting. Review its access again');
        }
        const selections: PersonalSelection[] = grants.map(grant => {
            const previous = (value[session] ?? []).find(item => item.volume === grant.volume && item.write === grant.write);
            if (previous && (!validId(previous.incarnation, 16) || /^1+$/.test(previous.incarnation))) throw Error('Invalid stored personal volume incarnation');
            const incarnation = previous ? previous.incarnation : encodeId(crypto.getRandomValues(new Uint8Array(16)));
            return { ...grant, incarnation };
        });
        let pending: AccountOutboxRecord | undefined;
        if (cipher) { // `key` and `opaque` exist exactly when `cipher` does.
            const previous = snapshot.record && await cipher.open(key!, snapshot.record.clock, snapshot.record.ciphertext);
            const plain = encodePersonalRecord(session, selections, previous);
            if (!previous || previous.length !== plain.length || !previous.every((byte, index) => byte === plain[index])) {
                const clock = nextAccountClock(snapshot.control.clock);
                pending = { key: opaque!, clock, ciphertext: await cipher.seal(key!, clock, plain) };
            }
        }
        if (!pending && JSON.stringify(value[session]) === JSON.stringify(selections)) return value;
        value[session] = selections;
        return await commitAccountMutation(metadata, account, snapshot.control, value, pending);
    } finally { metadata?.database.close(); if (grants !== undefined) mutationPending = false; }
}
