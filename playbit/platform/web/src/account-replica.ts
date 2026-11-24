// Account storage and metadata adoption on the existing runtime worker. Network
// ownership, publication and retry scheduling remain in LiveReplication.
import type { AccountRecord, AccountRecordCipher, PersonalVolumesRecord } from './account-record.ts';
import { replicaCapability } from './invitation.ts';
import { accountCipher, openAccountRecord, MAX_ACCOUNT_RECORD_BYTES } from './account-record.ts';
import type { AccountMetadataChangedError, AccountOutboxRecord } from './account-outbox.ts';
import { openAccountMetadata, readAccountMutation, adoptAccountMutation,
    pendingAccountRecord, acknowledgeAccountRecord, requireAccountAuthority, quarantineAccount, withAccountAuthority } from './account-outbox.ts';
import type { RecoveryCatalog, RecoveryState } from './recovery.ts';
import { mergeCatalog, availableCatalog, MAX_PLAINTEXT_BYTES } from './recovery.ts';
import type { PersonalSelections } from './personal.ts';

// One entry of the account kvdb as pb_account_* exports lay it out: 32-byte key,
// clock, then the sealed envelope.
export interface AccountEntry { key: Uint8Array<ArrayBuffer>; clock: bigint; ciphertext: Uint8Array<ArrayBuffer> }
// Runs one runtime Wasm export on the local queue with `bytes` as its input and
// returns its output; `current` says whether the caller's turn is still live.
export type AccountReplicaCall = (name: string, bytes: Uint8Array | undefined, current: () => boolean) => Promise<Uint8Array<ArrayBuffer>>;
// The next outbox record staged for publication, as LiveReplication expects a head.
export interface AccountHead {
    source: 'account'; publication: { kind: 'account' }; database: "index";
    ticket: { account: AccountOutboxRecord }; bytes: Uint8Array<ArrayBuffer>;
}

const hex = (bytes: Uint8Array): string => Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('');
const unhex = (value: string): Uint8Array<ArrayBuffer> => Uint8Array.from(value.match(/../g)!, byte => parseInt(byte, 16)); // Outbox keys are validated hex.
const maximumClock = 0xfffffffffffffffcn;

function* entries(bytes: Uint8Array<ArrayBuffer>, missing = false): Generator<AccountEntry, void, undefined> {
    if (!(bytes instanceof Uint8Array) || bytes.length > 256 * 1024) throw Error('Invalid account entry buffer');
    for (let offset = 0; offset < bytes.length;) {
        if (bytes.length - offset < 44) throw Error('Truncated account entry');
        const view = new DataView(bytes.buffer, bytes.byteOffset + offset, bytes.length - offset);
        const clock = view.getBigUint64(32, true), length = view.getUint32(40, true);
        if (length > MAX_ACCOUNT_RECORD_BYTES + 29 || offset + 44 + length > bytes.length
            || (length ? length <= 29 || clock === 0n || clock > maximumClock : !missing || clock !== 0n)) {
            throw Error('Invalid account entry');
        }
        const key = bytes.subarray(offset, offset + 32), ciphertext = bytes.subarray(offset + 44, offset + 44 + length);
        if (length) yield { key, clock, ciphertext };
        offset += 44 + length;
    }
}

// A personal_volumes record projects onto the 'personal' row, every other kind onto the 'recovery' row.
function project(account: string, state: PersonalSelections | undefined, record: PersonalVolumesRecord): Promise<PersonalSelections>;
function project(account: string, state: RecoveryState | undefined, record: Exclude<AccountRecord, PersonalVolumesRecord>): Promise<RecoveryState>;
function project(account: string, state: unknown, record: AccountRecord): Promise<PersonalSelections | RecoveryState>;
async function project(account: string, state: unknown, record: AccountRecord): Promise<PersonalSelections | RecoveryState> {
    if (record.kind === 'personal_volumes') return { ...(state ?? {}), [record.session]: record.grants };
    // Every other kind reads and extends the 'recovery' row, checked next.
    const row = state as RecoveryState | undefined;
    if (!row?.acknowledged || row.revision < 1 || row.catalog?.account !== account) {
        throw Error('Unlock this account’s private credentials before synchronizing');
    }
    if (record.kind === 'display_name') return { ...row, display_name: record.name };
    if (record.kind === 'app_visibility') return row;
    if (record.kind === 'volume_deleted') {
        const deleted = [...new Set([...(row.deleted_volumes ?? []), record.volume])];
        if (deleted.length > 4096) throw Error('Account deletion capacity exceeded');
        return { ...row, deleted_volumes: deleted, catalog: await availableCatalog(row.catalog, deleted), digest: null };
    }
    const incoming: RecoveryCatalog = { account, volumes: [], invitations: [] };
    if (record.kind === 'hosted_volume' || record.kind === 'hosted_invitation') incoming.scopes = [record.scope];
    if (record.kind === 'volume' || record.kind === 'hosted_volume') incoming.volumes.push({ volume: record.volume, credentials: record.credentials });
    else if (record.kind === 'invitation' || record.kind === 'hosted_invitation') {
        const value = record.invitation;
        incoming.invitations.push(value);
        incoming.volumes.push(replicaCapability(value));
    } else throw Error('Unsupported account record');
    // This also refuses changed capability keys/owners and session identities.
    const catalog = await availableCatalog(await mergeCatalog(row.catalog, incoming), row.deleted_volumes);
    if (new TextEncoder().encode(JSON.stringify(catalog)).length > MAX_PLAINTEXT_BYTES) {
        throw Error('Account credential catalog exceeds recovery capacity');
    }
    return { ...row, catalog, digest: null };
}

export class AccountReplica {
    account: string;
    cipher: AccountRecordCipher;
    closed: boolean;
    call: (name: string, bytes?: Uint8Array, current?: () => boolean) => Promise<Uint8Array<ArrayBuffer>>;
    static async open(account: string, call: AccountReplicaCall): Promise<AccountReplica | null> {
        await requireAccountAuthority(account);
        const metadata = await openAccountMetadata<RecoveryState>('recovery');
        let state: RecoveryState | undefined;
        try { ({ state } = await readAccountMutation(metadata, account)); }
        finally { metadata.database.close(); }
        if (!state?.acknowledged) return null;
        if (!Number.isSafeInteger(state.revision) || state.revision < 1) {
            throw Error('Unlock this account’s private credentials before synchronizing');
        }
        return new AccountReplica(account, await accountCipher(account, Uint8Array.from(state.secret ?? [])), call);
    }
    constructor(account: string, cipher: AccountRecordCipher, call: AccountReplicaCall) {
        this.account = account; this.cipher = cipher; this.closed = false;
        this.call = (name, bytes, current = () => true) => call(name, bytes, () => !this.closed && current());
    }
    close(): void { this.closed = true; }
    async profile(): Promise<{ revision: bigint; displayName: string; deletedVolumes: Set<string> }> {
        const metadata = await openAccountMetadata<RecoveryState>('recovery');
        try {
            const { state, control } = await readAccountMutation(metadata, this.account);
            return { revision: control.revision, displayName: state?.display_name ?? '', deletedVolumes: new Set(state?.deleted_volumes ?? []) };
        } finally { metadata.database.close(); }
    }
    async decode(entry: AccountEntry): Promise<AccountRecord | null> {
        try {
            const { record } = await openAccountRecord(this.cipher, entry.key, entry.clock, entry.ciphertext,
                plain => this.call('pb_account_decode', plain));
            return record;
        } catch (error) {
            if ((error as { code?: string }).code === 'unsupported-account-semantics') {
                await quarantineAccount(this.account, { key: hex(entry.key), clock: entry.clock, ciphertext: entry.ciphertext });
            }
            throw error;
        }
    }
    async displayNameRecord(current: () => boolean): Promise<(AccountEntry & { name: string }) | undefined> {
        await requireAccountAuthority(this.account);
        const key = await this.cipher.recordKey(new TextEncoder().encode('settings/display-name'));
        const entry = entries(await this.call('pb_account_read', key, current)).next().value;
        if (!entry || this.closed || !current()) return;
        const value = await this.decode(entry);
        if (value?.kind !== 'display_name') throw Error('Invalid account display name record');
        return { ...entry, name: value.name };
    }
    async adopt(entry: AccountEntry, current: () => boolean): Promise<boolean> {
        const record = await this.decode(entry);
        const metadata = await openAccountMetadata(record?.kind === 'personal_volumes' ? 'personal' : 'recovery');
        try {
            // Retry only a changed local revision, with one record retained.
            // IndexedDB admission yields to input; there is no timer or queue.
            for (let attempt = 0; attempt < 16 && !this.closed && current(); attempt++) {
                const snapshot = await readAccountMutation(metadata, this.account, hex(entry.key));
                const state = record ? await project(this.account, snapshot.state, record) : snapshot.state;
                if (this.closed || !current()) return false;
                try {
                    await adoptAccountMutation(metadata, this.account, snapshot.control, state,
                        { key: hex(entry.key), clock: entry.clock, ciphertext: entry.ciphertext }, () => !this.closed && current(),
                        record?.kind === 'app_visibility' ? record : undefined);
                    return record?.kind === 'display_name';
                } catch (error) { if ((error as Partial<AccountMetadataChangedError>).code !== 'account-metadata-changed' || attempt === 15) throw error; }
            }
        } finally { metadata.database.close(); }
        return false;
    }
    async validate(bytes: Uint8Array<ArrayBuffer>, current: () => boolean, missing = false): Promise<void> {
        // Validate the combined capability projection as well as each envelope:
        // two individually valid records may disagree about the same volume key.
        let state: RecoveryState | undefined;
        for (const entry of entries(bytes, missing)) {
            if (this.closed || !current()) return;
            const record = await this.decode(entry);
            if (!record || record.kind === 'personal_volumes') continue;
            if (!state) {
                const metadata = await openAccountMetadata<RecoveryState>('recovery');
                try { ({ state } = await readAccountMutation(metadata, this.account)); }
                finally { metadata.database.close(); }
            }
            state = await project(this.account, state, record);
        }
    }
    async inspectSync(current: () => boolean): Promise<void> {
        await this.call('pb_account_sync_prepare', undefined, current);
        for (;;) {
            const bytes = await this.call('pb_account_sync_next', undefined, current);
            if (!bytes.length) return;
            await this.validate(bytes, current, true);
        }
    }
    async adoptTree(current: () => boolean): Promise<void> {
        let cursor = new Uint8Array();
        while (!this.closed && current()) {
            const bytes = await this.call('pb_account_next', cursor, current);
            if (!bytes.length) return;
            const entry = entries(bytes).next().value;
            if (!entry || bytes.length !== 44 + entry.ciphertext.length) throw Error('Invalid account cursor result');
            cursor = entry.key.slice();
            await this.adopt(entry, current);
        }
    }
    async stage(record: AccountOutboxRecord, current: () => boolean): Promise<Uint8Array<ArrayBuffer>> {
        await requireAccountAuthority(this.account);
        const entry = { key: unhex(record.key), clock: record.clock, ciphertext: record.ciphertext };
        await this.decode(entry);
        const input = new Uint8Array(40 + record.ciphertext.length);
        input.set(entry.key); new DataView(input.buffer).setBigUint64(32, record.clock, true);
        input.set(record.ciphertext, 40);
        return withAccountAuthority(this.account, () => this.call('pb_account_stage', input, current));
    }
    async stagePending(current: () => boolean): Promise<void> {
        let cursor = '';
        while (!this.closed && current()) {
            const record = await pendingAccountRecord('personal', this.account, cursor);
            if (!record) return;
            cursor = record.key;
            await this.stage(record, current);
        }
    }
    async head(current: () => boolean): Promise<AccountHead | null> {
        const record = await pendingAccountRecord('personal', this.account);
        if (!record || this.closed || !current()) return null;
        const bytes = await this.stage(record, current);
        return { source: 'account', publication: { kind: 'account' }, database: 'index', ticket: { account: record }, bytes };
    }
    async acknowledge(record: AccountOutboxRecord, current: () => boolean): Promise<void> {
        await acknowledgeAccountRecord('personal', this.account, record, () => !this.closed && current());
    }
    async apply(delta: Uint8Array, current: () => boolean): Promise<boolean> {
        await requireAccountAuthority(this.account);
        const decoded = await this.call('pb_account_entries', delta, current);
        await this.validate(decoded, current);
        await withAccountAuthority(this.account, () => this.call('pb_account_apply', delta, current));
        let foundName = false;
        for (const entry of entries(decoded)) {
            if (this.closed || !current()) return false;
            const winner = await this.call('pb_account_read', entry.key, current);
            for (const value of entries(winner)) foundName = await this.adopt(value, current) || foundName;
        }
        return foundName;
    }
}
