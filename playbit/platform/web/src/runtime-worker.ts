import { MAX_TEXT_EVENT_BYTES } from "./text-input.ts";
import { ChannelHost } from "./channel-host.ts";
import { AbiError } from "./guest-memory.ts";
import { SessionChannels, displayNameBytes } from "./session-channels.ts";
import { encodeId, decodeId, validId } from './identifier.ts';
import { installationIdentity, admitPublisherStatus } from "./installation.ts";
import { openAutomaticStorage } from './automatic-storage.ts';
import { admitWebProfile } from './profile-format.ts';
import { observeStorageFormat, incompatibleStorage, storageFormatAvailable, requireStorageFormat } from './storage-format.ts';
import { corruptStoreOpenMessage, failure, retryable } from "./errors.ts";
import type { Failure } from "./errors.ts";
import { setScopeAddresses, retainedInvitation, sessionProof, replicaCapability } from "./invitation.ts";
import { workerChannel } from './release-channel.ts';
import type { ScopeRequest, ScopeAddresses, InvitationSession, InvitationVolume, VolumeCredentials } from "./invitation.ts";
import { OpfsStore, OpfsResources } from "./opfs-store.ts";
import { createStorageBridge } from "./opfs-bridge.ts";
import type { OpfsStorageBridge } from "./opfs-bridge.ts";
import { decodeScene, validateInput, validateSurface } from "./scene.ts";
import type { Scene } from "./scene.ts";
import { guestOperations } from "./guest-operations.ts";
import { operationSupported } from "./guest-imports.ts";
import { GuestWork } from "./guest-work.ts";
import type { GuestEvent, QueuedEvent, WorkExports } from "./guest-work.ts";
import { IDENTITY_CHANGED, type GuestIdentity, type ObjectHost, type LogConnection } from "./guest-operations.ts";
import { prepareGuest, resolvedSession } from "./updates.ts";
import type { GuestInstance, SessionResolution } from "./updates.ts";
import { personalSelections, validateSelection, MAX_PERSONAL_VOLUMES } from "./personal.ts";
import type { PersonalGrant, PersonalSelection, PersonalSelections } from "./personal.ts";
import { envelopeLength } from "./blob.ts";
import { FileBackend } from "./file-backend.ts";
import { downloadPackage, fetchPackage, writePackage, MAX_PACKAGE_WASM_BYTES } from './package-stream.ts';
import { RuntimeBlobs } from "./runtime-blobs.ts";
import type { FileReference } from "./runtime-blobs.ts";
import type { BlobHash } from "./blob.ts";
import { databaseBytes, databaseIdentity, validDatabase } from "./database-id.ts";
import type { DatabaseId } from "./database-id.ts";
import { replicaReceipt, replicaJSON } from "./replication.ts";
import { VolumePublication, LocalVolumeChanged, PublicationUncertain, copyVolumeBacking } from './volume-publication.ts';
import { retainRejectedCut } from './volume-archive.ts';
import { openRecoveryIndex, openRecoveryDatabase, openRecoveryFile, type RecoveryCut } from './volume-recovery.ts';
import type { OfflineEntry } from './runtime-protocol.ts';
import { bloblogPage } from './bloblog.ts';
import type { PreparedVolume, VolumeWriteEpoch } from './volume-publication.ts';
import { LiveReplication, replicaAddress, replicaSource } from "./live-replication.ts";
import type { ReplicaScope, ReplicaSubscription, ReplicationState } from "./live-replication.ts";
import { observeAccountMetadata, requireAccountAuthority, withAccountAuthority } from './account-outbox.ts';
import { accountOperationStatus } from './guest-authority.ts';
import { unsupportedAccount } from './account-format.ts';
import type { AccountOutboxRecord } from './account-outbox.ts';
import { AccountSignal } from './account-notices.ts';
import { AccountReplica } from './account-replica.ts';
import type { ApDescription, ApOperation, ApResponse, ApStatus } from './ap-protocol.ts';
import { advisoryOperation, type AdvisoryOperation } from './ap-protocol.ts';
import { base64url, type CatalogVolume } from "./recovery.ts";
import type { RuntimeOpenArgs, RuntimeSelectPersonalArgs, RuntimeRequest, RuntimePageMessage } from "./runtime-protocol.ts";

// The dedicated runtime Worker global; the webworker library types `self` for any worker.
declare const self: DedicatedWorkerGlobalScope;
const selectedChannel = workerChannel(self.location.href);

// One entry of the personal volume chooser: a volume this account owns plus its private credentials.
export type PersonalCatalogEntry = CatalogVolume;

// The runtime host (playbit.wasm) exports this worker calls: every function takes
// u32 (or one u64) arguments and returns an i32 status or length.
type HostCall = (...args: number[]) => number;
interface HostExports extends ObjectHost, WorkExports {
    memory: WebAssembly.Memory;
    pb_input: (length: number) => number;
    pb_automatic_needed: () => number;
    pb_automatic_register: (length: number) => number;
    pb_automatic_world_denied: (length: number) => number;
    pb_automatic_user_deleted: (length: number) => number;
    pb_automatic_ready: (length: number) => number;
    pb_automatic_failed: (length: number) => number;
    pb_scope_addresses: (length: number) => number;
    pb_web_validate_surface_input: (kind: number, length: number) => number;
    pb_web_validate_text_input: (length: number) => number;
    pb_output: () => number;
    pb_version: () => number;
    pb_error: () => number;
    pb_blob_cipher_key: () => number;
    pb_blob_hash_begin: () => number;
    pb_blob_hash_update: (id: number, length: number) => number;
    pb_blob_hash_finish: (id: number) => number;
    pb_blob_hash_close: (id: number) => number;
    pb_bloblog_checkpoint: (length: number) => number;
    pb_bloblog_receive: (length: number) => number;
    pb_bloblog_cache_append: (length: number) => number;
    pb_web_bloblog_address: (entry: number) => number;
    pb_blob_pending: (length: number) => number;
    pb_blob_acknowledge: (length: number) => number;
    pb_blob_authorize: (length: number) => number;
    pb_blob_cache: (length: number) => number;
    pb_database_needed: () => number;
    pb_database_preparation: (length: number) => number;
    pb_database_backing: (length: number) => number;
    pb_database_retained_generation: (length: number) => number;
    pb_database_failed: (length: number) => number;
    pb_database_retry: () => number;
    pb_database_make_room: () => number;
    pb_register_database: (length: number) => number;
    pb_seed_database: (length: number) => number;
    pb_seed_blob_reference: (length: number) => number;
    pb_seed_blob: (length: number) => number;
    pb_blob_digest: (length: number) => number;
    pb_blob_publication: (length: number) => number;
    pb_namespace_evidence: (length: number, metadataLength: number) => number;
    pb_sync_begin: (slot: number, direction: number) => number;
    pb_sync_output: () => number;
    pb_sync_step: (length: number) => number;
    pb_sync_done: () => number;
    pb_sync_finish: (commit: number) => number;
    pb_volume_prepare: (length: number) => number;
    pb_volume_rejected_page: (length: number) => number;
    pb_volume_archive_plan: (length: number) => number;
    pb_volume_archive_blobs: (length: number) => number;
    pb_volume_archive_commit: (length: number) => number;
    pb_recovery_cut: (length: number) => number;
    pb_recovery_entries: (length: number) => number;
    pb_recovery_cut_get: (length: number) => number;
    pb_recovery_begin: (length: number) => number;
    pb_recovery_backing: (length: number) => number;
    pb_recovery_copy: () => number;
    pb_recovery_publish: (length: number) => number;
    pb_recovery_abort: () => number;
    pb_volume_log_prepare: () => number;
    pb_volume_admission: (length: number) => number;
    pb_volume_receipt: (length: number) => number;
    pb_volume_qualify: (length: number) => number;
    pb_authority_refresh_required: (length: number) => number;
    pb_replica_catalog_allowed: (length: number) => number;
    pb_session_channel_key: () => number;
    pb_session_authority: (length: number) => number;
    pb_session_admit: (length: number) => number;
    pb_volume_register_backing: (length: number) => number;
    pb_volume_close_backing: () => number;
    pb_volume_local_backing: (length: number) => number;
    pb_volume_local_location: (length: number) => number;
    pb_volume_bind_backing: (length: number) => number;
    pb_volume_catalog_page: (length: number) => number;
    pb_volume_sync_begin: (slot: number) => number;
    pb_volume_validate: () => number;
    pb_volume_publish: () => number;
    pb_volume_abort: () => number;
    pb_volume_log_page: (length: number) => number;
    pb_volume_log_validate: () => number;
    pb_catalog_page: (length: number) => number;
    pb_upload_catalog_page: (length: number) => number;
    pb_world_append_catalog_page: (length: number) => number;
    pb_world_append_next: (length: number) => number;
    pb_world_append_ack: (length: number) => number;
    pb_world_append_published: (slot: number, length: number) => number;
    pb_validate_selection: (length: number) => number;
    pb_select_personal: (length: number) => number;
    pb_register_personal: (length: number) => number;
    pb_begin_adoption: (length: number) => number;
    pb_adopt_session: (length: number) => number;
    pb_finish_adoption: (commit: number) => number;
    pb_package: (slot: number) => number;
    pb_publisher_status: (length: number) => number;
    pb_package_authenticate: (slot: number, length: number) => number;
    pb_package_adopt: (slot: number) => number;
    pb_package_discard: (slot: number) => number;
    pb_initialize: (length: number) => number;
    pb_account_open: (length: number) => number;
    pb_live_rebase: (gapOnly: number) => number;
    pb_live_peek: () => number;
    pb_live_ack: (ticket: bigint) => number;
    pb_live_apply: (length: number) => number;
    pb_live_apply_entry: (length: number) => number;
    pb_live_apply_namespace: (length: number) => number;
    pb_live_terminal: (length: number) => number;
    [name: string]: ((...args: never[]) => number | bigint) | WebAssembly.Memory;
}
interface HostInstance { exports: HostExports }
// The page's view of an app update in progress.
interface UpdateState { kind: "checking" | "downloading" | "opening" | "failed"; build_id?: string; message: string }
// profile.json: the invitation (or a local stand-in) plus everything the runtime retains across opens.
interface ExampleAdmission { issuer: number[]; status: number[]; record: number[] }
interface Profile {
    account: string;
    seed?: number[];
    session: InvitationSession;
    // Physical session replica. The World address is always session.root_volume.
    root: InvitationVolume;
    root_credentials?: VolumeCredentials;
    root_key: number[];
    initializer_admission?: ExampleAdmission;
    replica_ids: number[];
    personal_replicas?: Record<string, number[]>;
    account_replica_id?: number;
    app_name?: string;
    update?: UpdateState | null;
    resolution?: SessionResolution;
}
interface AcceptedInput { root: bigint; event: GuestEvent }
interface PersonalTask { id: number; canceled: boolean; committing: boolean; replied: boolean }
interface ApPending { method: string; advisory?: AdvisoryOperation; resolve: (value: unknown) => void; reject: (reason?: unknown) => void }
// Runs one host operation: directly, or on the local queue while replication owns the turn.
interface Step {
    <T>(callback: () => T, cleanup?: boolean): T | Promise<T>;
    replicaCurrent?: () => boolean;
}
function replicaStep(current: () => boolean): Step {
    const step: Step = (callback, cleanup = false) => localStep(() => {
        if (!cleanup && !current()) throw failure('replica-gap', 'Replica synchronization retired');
        return callback();
    });
    step.replicaCurrent = current;
    return step;
}
// A publication ticket: the host's live-log position, or the account outbox record it came from.
type VolumeTicket = { sequence: bigint; world?: AutomaticMount; account?: undefined };
type LiveTicket = VolumeTicket | { account: AccountOutboxRecord };
// File access failures carry the host's numeric status in `code`.
type FileFailure = Error & { code?: number };
const encoder = new TextEncoder(), decoder = new TextDecoder("utf-8", { fatal: true });
let host: HostInstance, guest: GuestInstance | undefined, profile: Profile, scene: Scene | null, releaseLock: (() => void) | undefined, directory: FileSystemDirectoryHandle, managed: boolean, proof: number[];
let guestFailure: Error | undefined;
let engineFailure: Error | undefined;
let apSequence = 0, updateTask: Promise<void> | null | undefined, retryPending = false, closed = false;
let replicas: LiveReplication<LiveTicket> | undefined, replicationTask: Promise<void> | null | undefined, recoveryTimer: number | undefined;
// First-use mounts outside this run's initial set attach after Shared is live.
let startingReplicas: { volumes: string[]; ready: Promise<void> } | undefined;
let accountReplica: AccountReplica | undefined, accountOpening: Promise<boolean> | undefined, accountTurn = false;
let recoveryList: { account: string; stop: AbortController; task: Promise<void> } | undefined;
interface AccountNamePublication {
    current: () => boolean; dirty: boolean; unsupported: boolean; delay: number; retry?: number; reported?: string;
}
let accountNamePublication: AccountNamePublication | undefined, publishingAccountName = false;
let recoveryPending = false, recoveryDelay = 250, replicationBlocked = false, closing = false;
let selections: PersonalSelections = {}, personalCatalog: PersonalCatalogEntry[] = [], activeGrants: PersonalSelection[] = [];
let personalTask: PersonalTask | undefined, personalRetirement: Promise<void> | undefined;
const personalSlots = new Map<string, number>();
// All secondary mounts share the host's 30 control pairs below named slot 64.
interface ScopeStorage { directory: FileSystemDirectoryHandle; publication: VolumePublication }
interface AutomaticMount { base: number; volume: InvitationVolume; preparing: boolean; credentials?: VolumeCredentials; storage?: ScopeStorage; release?: () => void }
const automaticSlots = new Map<string, AutomaticMount>();
// Remember a requested World across temporary opening failures and authority changes.
let unmountedWorld: string | undefined;
function storageFor(volume: string): ScopeStorage {
    return automaticSlots.get(volume)?.storage ?? { directory, publication: publication! };
}

const openingMounts = new Map<string, {base: number; finished: Promise<void>}>();
function secondaryBase(): number {
    const inUse = new Set([...personalSlots.values(), ...Array.from(automaticSlots.values(), item => item.base),
        ...Array.from(openingMounts.values(), item => item.base)]);
    for (let base = 2; base <= 60; base += 2) if (!inUse.has(base) && !stores.has(base + 1) && !stores.has(base + 2)) return base;
    throw new Error("Volume capacity exceeded");
}
// Personal selection and automatic first use can acquire controls concurrently.
// Reserve the pair before the first await and serialize two opens of the same volume.
async function withSecondaryMount(id: string, open: (base: number) => Promise<void>): Promise<void> {
    while (openingMounts.has(id)) await openingMounts.get(id)!.finished;
    if (closed || closing) throw new Error('Volume preparation retired');
    const base = personalSlots.get(id) ?? automaticSlots.get(id)?.base ?? secondaryBase();
    let release!: () => void;
    const finished = new Promise<void>(resolve => { release = resolve; });
    openingMounts.set(id, {base, finished});
    try { await open(base); }
    finally { openingMounts.delete(id); release(); }
}
function projectedWorld(volume: string | undefined): boolean {
    return volume !== undefined && volume === profile?.session.root_volume && volume !== profile.root.id
        && automaticSlots.has(volume) && !automaticSlots.get(volume)!.credentials;
}
// The recovered capability is withdrawn permanently for this runtime when the
// signed-in account deletes its explicit User volume.
function replicaVolumeCurrent(id: string): boolean {
    return id !== creationScope?.user_volume || boundUser !== undefined;
}
function personalReplica(id: string): boolean {
    return personalSlots.has(id) || (id === creationScope?.user_volume && automaticSlots.has(id));
}
const namedSlots = new Map<string, number>();
// Guest preparation and incoming replication can request the same backing.
// OPFS permits one exclusive handle. The runtime owns the shared opening;
// retiring one replication caller must not cancel another caller's preparation.
const openingDatabases = new Map<string, Promise<number>>();
const preparingDatabases = new Map<string, Promise<void>>();
let databasePreparations = Promise.resolve();
const scopeRecoveries = new Map<string, 'retry' | 'blocked'>();
// Desired subscriptions, not an entry catalog. Each address also consumes one
// of LiveReplication's 32 slots; retire interests with their volume exposure.
const entrySubscriptions = new Set<string>();
let preparingScopes: Promise<boolean> | undefined;
let boundUser: CatalogVolume | undefined;
let creationScope: Extract<import("./recovery.ts").AppScope, {kind: "session"}> | undefined;

let nextNamedSlot = 64;
let blobs: RuntimeBlobs;
let files: FileBackend;
let fileReady: MessageChannel | undefined, fileContinuation: { promise: Promise<void>; resume(): void } | undefined;
const apPending = new Map<number, ApPending>();
const advisoryPending = new Set<AdvisoryOperation>();
// The page's AP client, proxied by method name; the page answers each call with an `apReply`.
function ap(method: "connect"): Promise<ApStatus>;
function ap(method: "describe"): Promise<ApDescription>;
function ap(method: "request", operation: ApOperation, generation?: number): Promise<ApResponse>;
function ap(method: "begin", operation: ApOperation): Promise<number>;
function ap(method: "streamRead" | "streamReplica" | "streamNamespaceEvidence", handle: number): Promise<Uint8Array>;
function ap(method: "streamRead", handle: number, maxBody: number): Promise<Uint8Array>;
function ap(method: "streamResponse", handle: number): Promise<ApResponse>;
function ap(method: "streamWrite", handle: number, bytes: Uint8Array): Promise<undefined>;
function ap(method: "streamFinish" | "streamClose" | "checkpointReplica", handle: number): Promise<undefined>;
function ap(method: "subscribeReplica", operation: ApOperation): Promise<ReplicaSubscription>;
function ap(method: "publishReplica", operation: ApOperation, bytes: Uint8Array, generation?: number): Promise<unknown>;
function ap(method: "accountChanged", account: string): Promise<undefined>;
function ap(method: string, ...args: unknown[]): Promise<unknown>;
function ap(method: string, ...args: unknown[]): Promise<unknown> {
    if (accountAuthority() && method !== 'streamClose' && method !== 'channelConsumed' && method !== 'accountChanged') return Promise.reject(unsupportedAccount());
    if (method === 'begin' || method === 'subscribeReplica' || method === 'publishReplica') {
        const op = args[0] as ApOperation;
        if (projectedWorld(op.volume_id as string | undefined)) {
            if (op.session_id !== profile.session.id || typeof op.build_id !== 'string') throw new Error("World replication requires its admitted session and build");
            const request = op.op === 'sync_database' ? { kind: 'sync', database_id: op.database_id, direction: op.direction, entry_scope: op.entry_scope }
                : op.op === 'subscribe_replica' ? { kind: 'subscribe', entry_scope: op.entry_scope }
                : op.op === 'publish_replica' ? { kind: 'publish', database_id: op.database_id } : undefined;
            if (request) args[0] = { op: 'world_replica', operation_id: op.operation_id, session_id: op.session_id,
                build_id: op.build_id, volume_id: op.volume_id, request };
        }
    }
    const advisory = advisoryOperation(method, args[0] as ApOperation | undefined);
    if (advisory && advisoryPending.has(advisory)) return Promise.reject(failure('ap-capacity', 'An advisory AP request is already pending'));
    if (!advisory && apPending.size - advisoryPending.size >= 64 && method !== 'channelConsumed') return Promise.reject(new Error("Runtime AP request capacity exceeded"));
    if (advisory) advisoryPending.add(advisory);
    const id = ++apSequence;
    return new Promise((resolve, reject) => { apPending.set(id, { method, advisory, resolve, reject }); self.postMessage({ ap: { id, method, args } }); });
}
const accountSignal = new AccountSignal(account => ap('accountChanged', account),
    () => self.postMessage({ accountNoticeFailed: true }));
observeAccountMetadata(account => {
    if (replicationAccount() === account && accountAuthority()) pauseAccountActivity();
    accountSignal.notify(account);
});
observeStorageFormat(() => { if (!closed) pauseAccountActivity(incompatibleStorage().message); });
function replicationAccount(): string | undefined { return recoveryList?.account ?? (managed ? profile?.account : undefined); }
function accountAuthority(): number { return accountOperationStatus(replicationAccount(), 0x7); }
function authorizedStep<T>(callback: () => T): Promise<T> {
    // Account staging holds shared authority before entering this actor. Never
    // hold the actor while waiting behind an exclusive quarantine request.
    const run = () => localStep(() => {
        if (!storageFormatAvailable()) throw new AbiError(-2);
        return callback();
    });
    return (managed ? withAccountAuthority(profile.account, run) : run())
        .catch(error => { if (error.code === 'unsupported-account-semantics') throw new AbiError(-2); throw error; });
}
function pauseAccountActivity(message = unsupportedAccount().message): void {
    replicationBlocked = true; recoveryPending = false;
    clearTimeout(recoveryTimer); recoveryTimer = undefined;
    channelClient?.close();
    // Do not await the actor from inside its own account admission callback.
    void replicas?.stop().catch(() => {});
    self.postMessage({ engineFailure: message });
}
function progress(message: string) { self.postMessage({ progress: message }); }
const stores = new Map<number, OpfsStore>();
// Shared resources for the active storage owner, retired by closeStorage().
let storageResources: OpfsResources | undefined, storageBridge: OpfsStorageBridge | undefined;
const events: QueuedEvent[] = [], logs: string[] = [];
const guestWork = new GuestWork(events, () => host.exports);
let publication: VolumePublication | undefined;
// Capturing a volume uses the same host transaction and scratch files as an
// ordinary database transfer, from admission through publication or abort.
let databaseTransfers = Promise.resolve(), pendingDatabaseTransfers = 0;
let activeDatabaseStream: { current(): boolean; close(): Promise<unknown> } | undefined;
let preparationReady: MessageChannel | undefined;
let preparationContinuation: { resume(): void; promise: Promise<void> } | undefined;
function yieldPreparation(): Promise<void> {
    if (closing || closed) return Promise.reject(new Error('Volume preparation retired'));
    if (preparationContinuation) return preparationContinuation.promise;
    if (!preparationReady) {
        preparationReady = new MessageChannel();
        preparationReady.port1.onmessage = () => {
            const ready = preparationContinuation; preparationContinuation = undefined; ready?.resume();
        };
    }
    let resume!: () => void;
    const promise = new Promise<void>(resolve => { resume = resolve; });
    preparationContinuation = { resume, promise }; preparationReady.port2.postMessage(0);
    return promise;
}
const hex = (bytes: Iterable<number> | ArrayLike<number>) => Array.from(bytes, byte => byte.toString(16).padStart(2, "0")).join("");
const random = (length: number) => crypto.getRandomValues(new Uint8Array(length));
const checked = (memory: WebAssembly.Memory, pointer: number, length: number) => {
    pointer >>>= 0; length >>>= 0;
    if (length > 2 * 1024 * 1024 || pointer + length > memory.buffer.byteLength) throw new Error("Guest memory bounds");
    return new Uint8Array(memory.buffer, pointer, length);
};
const bundleFiles = new Map<number, FileSystemSyncAccessHandle>();
const guestBundles = new WeakMap<GuestInstance, number>();
const bundleStorage = new Map<number, number>();
let nextBundle = 1;
let checkingPackageFeatures = false;
function storageImports() {
    storageBridge = createStorageBridge(stores, { get staging() { return storageResources?.staging ?? []; } }, () => host.exports.memory,
        bytes => crypto.getRandomValues(bytes), () => Math.floor(Date.now() / 1000),
        error => { logs.push(error); if (logs.length > 256) logs.shift(); });
    return { ...storageBridge.imports,
        operation_supported: (opcode: number) => Number(operationSupported(operations, opcode)
            && (opcode !== 0x15 || checkingPackageFeatures || Boolean(guest?.deadline))),
        bundle_length: (slot: number) => bundleFiles.get(slot)?.getSize() ?? -4,
        bundle_read: (slot: number, offset: number, out: number, length: number) => {
            if (length > 65536) return -4;
            try { return bundleFiles.get(slot)?.read(checked(host.exports.memory, out, length), { at: offset }) ?? -4; } catch { return -4; }
        },
        bundle_close: (slot: number) => { bundleFiles.get(slot)?.close(); bundleFiles.delete(slot); bundleStorage.delete(slot); },
        close_database: (slot: number) => {
        storageBridge!.imports.close_database(slot);
        for (const [address, value] of namedSlots) if (value === slot) namedSlots.delete(address);
    }, engine_panic: (pointer: number, length: number) => {
        const report = decoder.decode(checked(host.exports.memory, pointer, Math.min(length, 8192)));
        engineFailure = new Error("Playbit stopped unexpectedly. Restart Playbit to continue. The last action may have completed and will not be repeated automatically.");
        console.error(report, new Error("Engine panic").stack?.slice(0, 4096));
        closed = true;
        self.postMessage({ engineFailure: engineFailure.message });
        // Do not run rollback/cleanup against failed Wasm.
        // The page terminates this worker and the web browser releases its OPFS locks.
        self.close();
        // Returning lets the Rust hook abort; exported functions also reject
        // re-entry while JS catch/finally blocks unwind before termination.
    } };
}

function guestMemory() {
    if (!guest) throw new Error("The app is stopped; reload the page to restart it");
    return guest.exports.memory;
}
// The web browser owns the Wasm instance. Never enter a partially executed instance
// again after a trap, including through autonomous replication refreshes.
async function enterGuest() {
    if (engineFailure) throw engineFailure;
    if (guestFailure) throw guestFailure;
    const current = guest;
    if (!current) throw new Error("The app is stopped; reload the page to restart it");
    operations.queueWake();
    if (current.deadline && !current.deadline.canEnter(events.length !== 0)) return;
    guestWork.beginTurn();
    try {
        if (current.deadline) await current.deadline.enter();
        else current.exports.pb_main();
    }
    catch (cause) {
        if (engineFailure) throw engineFailure;
        operations.cancelWake();
        // Same retained-session distinction as engine/developer.rs: only the
        // creator's exact-version session accepts developer code replacement.
        const developer = profile.session.creator === profile.account && profile.session.selector.kind === "exact";
        const action = developer ? "is halted after an error" : "stopped after an error";
        const message = `${profile.app_name || "This app"} ${action}. Reload the page to restart it; the last action will not be repeated.`;
        const detail = String((cause as Error).message ?? cause);
        guestFailure = new Error(`${message} ${detail}`.slice(0, 512));
        if (!developer) guest = undefined;
        console.error(guestFailure.message, String(cause).slice(0, 2048));
        self.postMessage({ guestFailure: guestFailure.message });
        throw guestFailure;
    } finally {
        if (!engineFailure) { guestWork.endTurn(); operations.data.flushReady(); }
    }
}

// One coalesced credit acknowledgment lane stays available beside stalled reads.
const consumedChannelReceipts=new Set<number>();
let consumedChannelsTask:Promise<void>|undefined;
let retryConsumedChannels=false;
function consumeChannelReceipt(receipt:number):void {
    consumedChannelReceipts.add(receipt);
    flushConsumedChannels();
}
function flushConsumedChannels():void {
    if(!consumedChannelReceipts.size)return;
    if(consumedChannelsTask){retryConsumedChannels=true;return;}
    retryConsumedChannels=false;
    consumedChannelsTask=Promise.resolve().then(async()=>{
        while(consumedChannelReceipts.size){
            retryConsumedChannels=false;
            const receipts=[...consumedChannelReceipts];
            await ap('channelConsumed',receipts);
            for(const receipt of receipts)consumedChannelReceipts.delete(receipt);
        }
    }).catch(()=>{
        // Retain unacknowledged credits. New consumption or another AP reply
        // retries them; a failed acknowledgment never creates a retry loop.
    }).finally(()=>{consumedChannelsTask=undefined;if(retryConsumedChannels)flushConsumedChannels();});
}
let channelClient:SessionChannels|undefined;
let channelDevice=new Uint8Array(16);
let localIdentity: GuestIdentity | undefined;
let accountProfileRevision = -1n, displayName = '';
function currentIdentity(): GuestIdentity {
    if (channelClient) return channelClient.identity;
    if (!localIdentity) {
        const account = decodeId(profile.account);
        // As on native, a local guest uses its profile account for the device ID.
        localIdentity = { account, device: managed ? channelDevice : account,
            instance: random(16), session: decodeId(profile.session.id, 32), displayName };
    }
    return localIdentity;
}
function channelFailure(status: number): void {
    if (closed) return;
    const data = new Uint8Array(new Int32Array([status]).buffer);
    guestWork.queue({ kind: 19, target: 0, data });
    operationsReady();
    void recoverChannels();
}
let dataRevision = 0, guestUpdating = false, dataTurnOwner: GuestInstance | undefined;
const operations = guestOperations({
    waitForEvent: deadline => {
        const owner = guest;
        if (!owner?.deadline) throw new Error("Guest deadline owner unavailable");
        return owner.deadline.suspend(deadline, () => {
            if (guest === owner && !closed && !engineFailure) operationsReady();
        });
    },
    admitOperation: opcode => accountOperationStatus(managed ? profile?.account : undefined, opcode),
    identity: () => guest && currentIdentity(),
    sessionKeySecret: () => profile.seed,
    channels:()=>{
        if(!managed||!profile.seed)throw new AbiError(-1);
        if(!channelClient){
            hostInput(Uint8Array.from(profile.seed));const key=hostOutput(host.exports.pb_session_channel_key()).slice();
            const identity = { ...currentIdentity(), build: decodeId(profile.session.resolved_build_id, 32),
                proof: Uint8Array.from(proof), app: profile.session.selector.kind === 'app' ? profile.session.selector.app : undefined };
            channelClient = new SessionChannels(identity, key, new ChannelHost(() => host.exports), {
                connect: async () => {
                    const connection = await ap('connect');
                    channelDevice = Uint8Array.from(connection.instanceId ?? []);
                    return { device: channelDevice, generation: connection.connectionCount,
                        displayNames: connection.sessionDisplayNames };
                },
                discoverNames: async () => (await ap('describe')).features.includes('session.display-names.v1'),
                open: (frame, device, generation) => ap('beginChannel', frame, device, generation) as Promise<number>,
                read: handle => ap('channelRead', handle) as Promise<import('./ap-codec.ts').Frame & { receipt?: number }>,
                displayName: (handle, name) => ap('channelDisplayName', handle, name),
                consumed: consumeChannelReceipt,
                close: handle => ap('streamClose', handle),
            }, (name, source, message, data) => operations.channelEvent(name, source, message, data),
            () => operations.environmentChanged(5), channelFailure, () => { void checkUpdate(); });
        }
        return channelClient;
    },
    logConnection,
    appendLog,
    readLog,
    memory: guestMemory,
    host: () => host.exports,
    events,
    work: guestWork,
    log: text => { logs.push(text); if (logs.length > 256) logs.shift(); },
    submit: bytes => { scene = decodeScene(bytes, scene); },
    files: () => files,
    fileFailure: error => { logs.push(String(error).slice(0, 512)); if (logs.length > 256) logs.shift(); self.postMessage({ guestFailure: 'Private file cleanup failed. Reload to retry cleanup.' }); },
    execute: async callback => {
        for (let attempt = 0; attempt < 3; attempt++) {
            const result = await authorizedStep(callback);
            if ((result as { status?: number }).status !== -6) return result;
            if (!await requestDatabases()) throw new Error('Database reported pending work without a preparation request');
        }
        throw new Error('Database remained unavailable after preparation');
    },
    wouldBlock: () => { void requestDatabases().catch(error => { if (!closed) self.postMessage({ engineFailure: String(error.message ?? error) }); }); },
    revision: () => `${dataRevision}:${profile.session.resolved_build_id}:${JSON.stringify(activeGrants)}`,
    ready: operationsReady,
});
async function logConnection(): Promise<LogConnection> {
    const expires = Date.now() + 30000;
    const status = await ap('status') as ApStatus;
    if (!status.connected || !replicas?.run?.active || replicas.run.stopped) throw new AbiError(-5);
    return { generation: status.connectionCount, expires };
}
async function appendLog(prepared: Uint8Array<ArrayBuffer>, appendId: Uint8Array<ArrayBuffer>, connection: LogConnection): Promise<void> {
    if (prepared.length < 36 || prepared.length > 65672) throw new AbiError(-3);
    await authorizedStep(() => {
        if (Date.now() >= connection.expires) throw new AbiError(-5);
        if (host.exports.pb_live_peek() !== 0) throw new AbiError(-5);
    });
    const volume = encodeId(prepared.subarray(0, 16)), log = encodeId(prepared.subarray(16, 31));
    const personal = personalReplica(volume);
    const operation = { op: 'publish_bloblog', operation_id: encodeId(random(16)), append_id: encodeId(appendId),
        session_id: personal ? null : profile.session.id, build_id: personal ? null : profile.session.resolved_build_id,
        volume_id: volume, log_id: log, required_features: ['bloblog-append-v1'] };
    try {
        await ap('publishReplica', operation, prepared.slice(32 + prepared[31]!), connection.generation);
    } catch (error) {
        const failure = error as { serviceCode?: string; code?: string };
        throw new AbiError(failure.code === 'bloblog-denied' ? -2 : failure.serviceCode === 'operation_conflict' ? -10
            : failure.code === 'bloblog-offline' ? -5
            : failure.serviceCode && failure.serviceCode !== 'operation_uncertain' ? -5 : -15);
    }
    try {
        await authorizedStep(() => { hostInput(prepared); hostOutput(host.exports.pb_bloblog_cache_append(prepared.length)); });
    } catch { /* The remote commit remains successful when its local cache fails. */ }
}
function logPageHasEntries(bytes: Uint8Array<ArrayBuffer>): boolean {
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const cursor = view.getUint32(0, true);
    if (cursor > 104 || bytes.length < cursor + 9) throw new AbiError(-12);
    return view.getUint32(5 + cursor, true) !== 0;
}
async function readLog(entry: Uint8Array<ArrayBuffer>, read: () => Promise<Uint8Array<ArrayBuffer>>): Promise<Uint8Array<ArrayBuffer>> {
    let page = await read();
    if (logPageHasEntries(page)) return page;
    const expires = Date.now() + 30000;
    const status = await ap('status') as ApStatus;
    if (!status.connected) throw new AbiError(-5);
    const address = await authorizedStep(() => {
        const input = new Uint8Array(40); input.set(entry, 8); hostInput(input);
        return hostOutput(host.exports.pb_web_bloblog_address(8)).slice();
    });
    const volume = encodeId(address.subarray(0, 16)), database = encodeId(address.subarray(16, 31));
    let cursor = await authorizedStep(() => {
        const input = encoder.encode(JSON.stringify({ volume, database })); hostInput(input);
        return JSON.parse(decoder.decode(hostOutput(host.exports.pb_bloblog_checkpoint(input.length)))) as string | null;
    });
    for (;;) {
        if (Date.now() >= expires) throw new AbiError(-5);
        const personal = personalReplica(volume);
        const response = await ap('request', { op: 'get_bloblog', operation_id: encodeId(random(16)),
            session_id: personal ? null : profile.session.id, build_id: personal ? null : profile.session.resolved_build_id,
            volume_id: volume, log_id: database, cursor, required_features: ['bloblog-append-v1'] }, status.connectionCount).catch(() => { throw new AbiError(-5); });
        const received = bloblogPage(response.data, volume, database);
        if (received.page.more && (!received.page.entries.length || received.page.cursor === cursor)) throw new AbiError(-12);
        const input = encoder.encode(`{"address":${JSON.stringify({ volume, database })},"receipt":${decoder.decode(received.bytes)}}`);
        await authorizedStep(() => { hostInput(input); hostOutput(host.exports.pb_bloblog_receive(input.length)); });
        page = await read();
        if (logPageHasEntries(page) || !received.page.more) return page;
        cursor = received.page.cursor;
    }
}
function operationsReady() {
    const owner = guest;
    if (!owner || dataTurnOwner === owner || closed || guestFailure || engineFailure) return;
    dataTurnOwner = owner;
    void localStep(async () => {
        if (dataTurnOwner !== owner) return;
        dataTurnOwner = undefined;
        if (guest !== owner || guestFailure || engineFailure) return;
        // An earlier input can consume the wait before this notification runs.
        if (owner.deadline && !events.length && !operations.wakeDue) {
            if (guestWork.draining || !owner.deadline.parked || !owner.deadline.canEnter(false)) return;
        }
        await enterGuest();
        if (scene) self.postMessage({ replicaScene: presentation() });
        files?.flushPublication(); replicas?.notify();
    }).catch(error => { if (!closed) self.postMessage({ guestFailure: String(error.message ?? error) }); });
}
// Wait outside the local queue so worker completions can enter it. Accepted
// input precedes this fence; readiness continuations can finish finite work.
async function drainGuest(accepted: Promise<unknown>) {
    await accepted;
    if (managed) await requireAccountAuthority(profile.account);
    await localStep(() => {
        operations.cancelWake();
        guestWork.deactivate();
        guest?.deadline?.deactivate();
    });
    while (guestWork.pending || operations.data.waiting) {
        if (accountAuthority()) throw unsupportedAccount();
        if (guestFailure) throw guestFailure;
        if (guestWork.failed) throw new Error("Some accepted work failed. Review the app before closing it.");
        if (operations.data.waiting && !events.length) await operations.data.wait();
        await localStep(async () => {
            operations.data.flushReady();
            if (guest && events.length) await enterGuest();
        });
        if (guestWork.pending && !operations.data.waiting && !events.length) throw new Error("The app did not finish its accepted work. It remains open for review.");
    }
    if (operations.data.failed) throw new Error("Some accepted work failed. Review the app before closing it.");
}

function hostOutput(length: number) {
    if (length < 0) {
        const n = host.exports.pb_error();
        const message = decoder.decode(checked(host.exports.memory, host.exports.pb_output(), n));
        throw Object.assign(new Error(message || `Web database operation failed (${length})`), { hostStatus: length });
    }
    return checked(host.exports.memory, host.exports.pb_output(), length).slice();
}
function hostInput(bytes: Uint8Array) {
    const pointer = host.exports.pb_input(bytes.length);
    if (!pointer) throw new Error("App exceeds web memory limits");
    // Package bytes stay in OPFS; executable and guest syscall buffers have separate caps.
    new Uint8Array(host.exports.memory.buffer, pointer, bytes.length).set(bytes);
}
function blobKey(volume: string) {
    const keys = volume === profile.root.id ? profile.root_credentials
        : automaticSlots.get(volume)?.credentials ?? personalCatalog.find(item => item.volume.id === volume)?.credentials;
    if (!keys?.blob_key || keys.blob_key.length !== 32) throw new Error('Recover this volume’s private credentials to open files');
    return Uint8Array.from(keys.blob_key);
}
function unhex(value: string): Uint8Array<ArrayBuffer> { return Uint8Array.from(value.match(/../g)!, byte => parseInt(byte, 16)); }
// BlobServices errors retain ABI status across asynchronous preparation and sealing.
function blobOutput(length: number) {
    try { return hostOutput(length); } catch (error) {
        if (length < 0) throw Object.assign(error as Error, { code: length });
        throw error;
    }
}
function blobHash(): BlobHash {
    const id = host.exports.pb_blob_hash_begin(); blobOutput(id < 0 ? id : 0);
    let active = true;
    return {
        update(bytes) { if (!active) throw new Error('File hash is closed'); hostInput(bytes); blobOutput(host.exports.pb_blob_hash_update(id, bytes.length)); },
        finish() { if (!active) throw new Error('File hash is closed'); active = false; return hex(blobOutput(host.exports.pb_blob_hash_finish(id))); },
        close() { if (active) { active = false; blobOutput(host.exports.pb_blob_hash_close(id)); } },
    };
}
function initializeBlobs(): RuntimeBlobs {
    const readiness = new MessageChannel(), waiting: (() => void)[] = [];
    readiness.port1.onmessage = () => waiting.shift()?.();
    const json = (operation: (length: number) => number, value: unknown) => {
        const bytes = encoder.encode(JSON.stringify(value)); hostInput(bytes); return blobOutput(operation(bytes.length));
    };
    return new RuntimeBlobs({ directory: () => directory, privateDirectory: volume => automaticSlots.get(volume)?.storage?.directory, managed: () => managed, ap, hash: blobHash,
        cache: request => {
            const bytes = json(host.exports.pb_blob_cache, request);
            return bytes.length ? JSON.parse(decoder.decode(bytes)) : undefined;
        },
        failure: error => console.error('Could not clean up the file cache', error),
        key(reference) {
            const material = new Uint8Array(80); material.set(blobKey(reference.volume)); material.set(decodeId(reference.volume), 32); material.set(unhex(reference.digest), 48);
            hostInput(material); return blobOutput(host.exports.pb_blob_cipher_key());
        },
        operation(reference, upload) {
            const personal = personalReplica(reference.volume);
            return { op: upload ? 'upload_blob' : 'download_blob', operation_id: encodeId(random(16)), session_id: personal ? null : profile.session.id,
                build_id: personal ? null : profile.session.resolved_build_id,
                version: { address: { volume: reference.volume, entry: reference.entry }, revision: reference.revision,
                    len_bytes: reference.len_bytes, digest: Array.from(unhex(reference.digest)), encrypted_key: Array.from(unhex(reference.encrypted_key)) },
                ...(upload ? { content_length: envelopeLength(reference) } : {}) };
        },
        authorize: (reference, write, service) => {
            // The scoped World protocol grants database access without delivering the blob key.
            if (projectedWorld(reference.volume)) throw Object.assign(new Error('World file credentials are unavailable'), { code: -2 });
            json(host.exports.pb_blob_authorize, { reference, write, service });
        },
        pending: (volume, after) => JSON.parse(decoder.decode(json(host.exports.pb_blob_pending, { volume, after }))),
        acknowledge: reference => { json(host.exports.pb_blob_acknowledge, reference); },
        yield: () => new Promise<void>(resolve => {
            if (waiting.length === 3) throw new Error('File readiness capacity exceeded');
            waiting.push(resolve); readiness.port2.postMessage(0);
        }),
        stop() { for (const resolve of waiting.splice(0)) resolve(); readiness.port1.close(); readiness.port2.close(); },
        changed: () => localStep(async () => { if (guest && !closing) await refreshScene(); }),
    });
}
function fileYield(): Promise<void> {
    if (closed) return Promise.reject(new Error('File worker retired'));
    if (fileContinuation) return fileContinuation.promise;
    if (!fileReady) {
        fileReady = new MessageChannel();
        fileReady.port1.onmessage = () => { const next = fileContinuation; fileContinuation = undefined; next?.resume(); };
    }
    let resume!: () => void;
    const promise = new Promise<void>(resolve => { resume = resolve; });
    fileContinuation = { promise, resume }; fileReady.port2.postMessage(0); return promise;
}
async function uploadBlobs() {
    if (!managed) return;
    const volumes = new Set([profile.root.id, ...activeGrants.filter(grant => grant.write).map(grant => grant.volume)]);
    const user = creationScope?.user_volume;
    const mount = user && automaticSlots.get(user);
    if (mount && !mount.preparing) volumes.add(mount.volume.id);
    await blobs.upload([...volumes], replicaVolumeCurrent);
}
async function openVolumeControls(volume: string, base: number, storage = storageFor(volume)): Promise<PreparedVolume | undefined> {
    const { directory, publication } = storage;
    const selected = publication.current(volume), initial = selected ? undefined : publication.prepare(volume);
    const generation = selected ?? initial!.generation;
    try {
        if (initial) await publication.create(directory, initial);
        for (const [offset, database] of ['index', 'revocations'].entries()) {
            const file = await (await publication.file(directory, volume, generation, database, !selected)).createSyncAccessHandle();
            if (!selected && file.getSize()) { file.close(); throw new Error('Fresh volume generation already exists'); }
            stores.set(base + offset + 1, await openStore(file, publication.epoch(volume)));
        }
        return initial;
    } catch (error) {
        for (const slot of [base + 1, base + 2]) { stores.get(slot)?.close(); stores.delete(slot); }
        throw error;
    }
}
function finishInitialVolume(initial: PreparedVolume | undefined): void {
    if (!initial) return;
    const { publication } = storageFor(initial.volume);
    // New controls are initialized before guest admission. No received source
    // facts exist here; capture their completed initialization, then select it.
    publication.publish({ ...initial, revision: initial.epoch.capture() }, () => {}, () => {});
}
async function reclaimVolume(volume: string): Promise<void> {
    const { directory, publication } = storageFor(volume);
    await publication.reclaim(directory, volume, (database, generation) => {
        if (closed || closing) throw new Error('Volume cleanup retired');
        const input = encoder.encode(JSON.stringify({ volume, database })); hostInput(input);
        const retained: unknown = JSON.parse(decoder.decode(hostOutput(host.exports.pb_database_retained_generation(input.length))));
        if (retained !== null && (typeof retained !== 'string' || !validId(retained, 16))) throw new Error('Invalid retained backing generation');
        return retained === generation;
    });
}
async function openNamed(volume: InvitationVolume, database: DatabaseId): Promise<number> {
    if (database === 'index' || database === 'revocations' || !validDatabase(database)) throw new Error('Expected a named database identity');
    const identity = `${volume.id}/${database}`, existing = namedSlots.get(identity);
    if (existing !== undefined && stores.has(existing)) return existing;
    const pending = openingDatabases.get(identity);
    if (pending) return pending;
    if (openingDatabases.size === 27) throw new Error('Database opening capacity exceeded');
    const opening = openNamedBacking(volume, database, identity).finally(() => openingDatabases.delete(identity));
    openingDatabases.set(identity, opening);
    return opening;
}
async function openNamedBacking(volume: InvitationVolume, database: DatabaseId, identity: string): Promise<number> {
    const { directory, publication } = storageFor(volume.id);
    // Initial open already owns the local queue. Later openings use that queue
    // independently of any replication run, with storage waits outside it.
    const step: Step = initializing ? callback => callback() : localStep;
    await step(() => hostOutput(host.exports.pb_database_make_room()));
    if (++nextNamedSlot > 0x7fffffff) throw new Error('Database handle identity exhausted');
    const slot = nextNamedSlot;
    const controlsGeneration = publication.current(volume.id);
    if (!controlsGeneration) throw new Error('Volume backing generation is unavailable');
    const backing: { ready: boolean; generation: string | null } = await step(() => {
        const bytes = encoder.encode(JSON.stringify({ volume: volume.id, database })); hostInput(bytes);
        return JSON.parse(decoder.decode(hostOutput(host.exports.pb_database_backing(bytes.length))));
    });
    if (typeof backing.ready !== 'boolean' || backing.generation !== null && !validId(backing.generation, 16)) throw new Error('Invalid backing location');
    const generation = backing.generation ?? controlsGeneration;
    const file = await (await publication.file(directory, volume.id, generation, database, !backing.ready)).createSyncAccessHandle();
    if (closing || closed) { file.close(); throw new Error('Database preparation retired'); }
    if (publication.current(volume.id) !== controlsGeneration) { file.close(); throw new LocalVolumeChanged(); }
    stores.set(slot, await openStore(file, publication.epoch(volume.id)));
    try {
        await step(() => {
            if (publication.current(volume.id) !== controlsGeneration) throw new LocalVolumeChanged();
            const bytes = encoder.encode(JSON.stringify({ address: { volume: volume.id, database }, slot, replica_id: replicaId(), generation }));
            hostInput(bytes); hostOutput(host.exports.pb_register_database(bytes.length)); namedSlots.set(identity, slot);
        });
        return slot;
    } catch (error) { stores.get(slot)?.close(); stores.delete(slot); throw error; }
}
function replicaId(): number { let id = 0; while (!id) id = new DataView(random(4).buffer).getUint32(0, true); return id; }
type AutomaticRole = "root" | "session" | "user" | "user-session" | "local" | "local-session";
interface AutomaticAddress { role: AutomaticRole; volume: string }
interface ScopeInitializer { databases: { id: string }[]; blobs: { id: string; bytes: number[] }[] }
function automaticCall(operation: "pb_automatic_register" | "pb_automatic_ready" | "pb_automatic_failed" | "pb_automatic_world_denied" | "pb_automatic_user_deleted", input: unknown) {
    const bytes = encoder.encode(JSON.stringify(input)); hostInput(bytes);
    return hostOutput(host.exports[operation](bytes.length));
}
async function publisherWorld(id: string): Promise<InvitationVolume | null> {
    if (!managed || id !== profile.session.root_volume || id === profile.root.id) throw new AbiError(-2);
    const result = await ap("request", { op: "world_replica", operation_id: encodeId(random(16)),
        session_id: profile.session.id, build_id: profile.session.resolved_build_id, volume_id: id, request: { kind: "open" } });
    const record = (result.data as { volume?: InvitationVolume | null })?.volume;
    if (record === null) return null;
    if (!record || record.id !== id || record.owner !== profile.session.app.publisher || record.personal !== false
        || record.share_key_with_service !== true) throw new Error('Invalid publisher World descriptor');
    return record;
}
function prepareAutomatic(address: AutomaticAddress, reopening = false): Promise<void> {
    return withSecondaryMount(address.volume, base => openAutomatic(address, base, reopening));
}
async function openAutomatic(address: AutomaticAddress, base: number, reopening: boolean): Promise<void> {
    const id = address.volume, role = address.role;
    if (automaticSlots.has(id)) return; // Another opener completed while this caller waited.
    let creationBuild: string | undefined, retained: Awaited<ReturnType<typeof openAutomaticStorage>> | undefined;
    let credentials: VolumeCredentials | undefined, volume: InvitationVolume;
    let ids: number[];
    if (role === "root") {
        // A distinct World has no credentials in the Shared invitation. AP performs
        // the publisher, build and account ACL checks before returning its descriptor.
        let record: InvitationVolume | null;
        try { record = await publisherWorld(id); }
        catch (error) {
            if ((error as Failure).code !== 'world-participant-revoked') throw error;
            unmountedWorld = id; throw new AbiError(-2);
        }
        if (!record) { unmountedWorld = id; throw new AbiError(-2); }
        volume = record;
        ids = [replicaId(), replicaId()];
    } else if (role === "user" && creationScope?.user_volume === id) {
        if (boundUser?.volume.id !== id) throw new AbiError(-2);
        volume = boundUser.volume; credentials = boundUser.credentials;
        profile.personal_replicas ??= {};
        ids = profile.personal_replicas[id] ??= [replicaId(), replicaId()];
    } else {
        const names = {session: "Shared", user: "User", "user-session": "User", local: "Local", "local-session": "Local session"};
        const build = role === "session" || role === "user-session" || role === "local-session"
            ? creationScope?.creation_build ?? profile.session.resolved_build_id : profile.session.resolved_build_id;
        retained = await openAutomaticStorage(id, profile.account, build, storageResources!, yieldPreparation);
        volume = {id, owner: profile.account, name: names[role], personal: false, share_key_with_service: false};
        credentials = retained.credentials; ids = retained.replica_ids;
        creationBuild = retained.creation_build;
    }
    let creationSlot: number | undefined, registered = false, standalone = false;
    const mount: AutomaticMount = { base, volume, credentials, storage: retained, release: retained?.close, preparing: true };
    try {
        if (closed || closing) throw new AbiError(-5);
        const storage = retained ?? storageFor(id);
        const { publication } = storage;
        const initial = personalSlots.has(id) ? undefined : await openVolumeControls(id, base, storage);
        const journal = creationBuild ? publication.initialization(id) : undefined;
        if (journal && (journal.build !== creationBuild || initial && journal.complete)) throw new Error("Saved scope initializer differs from its original build");
        if (creationBuild && !journal?.complete && creationBuild !== profile.session.resolved_build_id) {
            if (!managed) throw new Error("Original scope initializer requires its signed app package");
            const original = await downloadVersion({...profile.session, resolved_build_id: creationBuild});
            creationSlot = guestBundles.get(original.instance);
            if (creationSlot === undefined) throw new Error("Original scope package is unavailable");
        }
        if (creationBuild && !journal) publication.beginInitialization(id, creationBuild, creationSlot === undefined ? 0 : bundleStorage.get(creationSlot)!);
        const result = await localStep(() => {
            const result = automaticCall("pb_automatic_register", {
                role, volume, base, replica_ids: ids, key: credentials?.database_key,
                creation_build: journal?.complete ? undefined : creationBuild, creation_slot: creationSlot,
            });
            registered = true;
            automaticSlots.set(id, mount);
            finishInitialVolume(initial);
            return result;
        });
        await reclaimVolume(id);
        if (creationBuild && !journal?.complete) {
            const initializer = JSON.parse(decoder.decode(result)) as ScopeInitializer | null;
            for (const seed of initializer?.databases ?? []) {
                await openNamed(volume, seed.id);
                await localStep(() => {
                    const bytes = encoder.encode(JSON.stringify({ volume: id, seed })); hostInput(bytes); hostOutput(host.exports.pb_seed_database(bytes.length));
                });
            }
            for (const seed of initializer?.blobs ?? []) {
                const reference: FileReference | null = await localStep(() => {
                    const bytes = encoder.encode(JSON.stringify({ volume: id, entry: seed.id })); hostInput(bytes);
                    return JSON.parse(decoder.decode(hostOutput(host.exports.pb_seed_blob_reference(bytes.length))));
                });
                if (reference) hostOutput(await blobs.initialize(reference, Uint8Array.from(seed.bytes), prepared => {
                    const bytes = encoder.encode(JSON.stringify(prepared)); hostInput(bytes); return host.exports.pb_seed_blob(bytes.length);
                }));
            }
        } else if (!creationBuild) {
            standalone = !await captureAutomaticCatalog(volume, false, Boolean(initial));
        }
        await localStep(() => {
            automaticCall("pb_automatic_ready", address);
            mount.preparing = false;
        });
        if (role === 'root') unmountedWorld = undefined;
        if (creationBuild && !journal?.complete) publication.finishInitialization(id, creationBuild);
    } catch (error) {
        if (registered && reopening && automaticSlots.has(id) && !closed && !closing) {
            await localStep(() => withdrawWorld(id, (error as Failure).code === 'world-participant-revoked'));
        }
        // Registered stores belong to Rust; their last reference closes the JS handle.
        if (!registered && !personalSlots.has(id)) {
            for (const [address, slot] of namedSlots) {
                if (!address.startsWith(id + "/")) continue;
                stores.get(slot)?.close(); stores.delete(slot); namedSlots.delete(address);
            }
            for (const slot of [base + 1, base + 2]) { stores.get(slot)?.close(); stores.delete(slot); }
        }
        automaticSlots.delete(id); scopeRecoveries.delete(id); await retained?.close();
        throw error;
    } finally { if (creationSlot !== undefined) host.exports.pb_package_discard(creationSlot); }
    // A connection can start during a detached first capture. Subscribe and
    // capture the intervening changes if its initial plan missed this mount.
    if (standalone) {
        const starting = startingReplicas;
        await starting?.ready;
        if (!starting?.volumes.includes(id) && automaticSlots.get(id) === mount && replicas?.run?.active && !replicas.run.stopped) {
            try { await captureAutomaticCatalog(volume); } catch (error) { catalogRecoveryFailed(id, error); }
        }
    }
}
// Subscribe before capture whenever a live connection exists. An initial local
// opening can precede that connection; attaching then captures the intervening edits.
async function captureAutomaticCatalog(volume: InvitationVolume, renew = true, newControls = false): Promise<boolean> {
    const world = projectedWorld(volume.id);
    if (world && renew) await renewWorld(volume.id);
    const synchronizeCatalog = async (step: Step, current = () => !closed && !closing) => {
        if (!world) {
            await synchronizeOrdinaryVolume(volume, volume.name, !newControls, step, () => current() && replicaVolumeCurrent(volume.id));
            if (!newControls) await blobs.upload([volume.id], replicaVolumeCurrent);
            return;
        }
        await pullVolumeSnapshot(volume, volume.name, step, undefined, undefined, current);
        if (!projectedWorld(volume.id)) return;
        // The pull first adopts current ACLs and quarantines rejected edits.
        // Only the admitted named KV backings can be submitted on this route.
        for await (const { database, blind } of databaseCatalog(volume, step)) {
            await syncDatabase(volume, database, 'push', undefined, step, current, blind ? database : undefined);
        }
        for await (const { database } of databaseCatalog(volume, step, undefined, host.exports.pb_world_append_catalog_page)) {
            await publishWorldAppends(volume, database, step);
        }
    };
    if (!replicas?.run?.active && startingReplicas) await startingReplicas.ready;
    if (!replicas?.run || replicas.run.stopped) {
        await synchronizeCatalog(localStep);
        return false;
    }
    const personal = personalReplica(volume.id);
    try { await replicas.add({ kind: 'volume', volume_id: volume.id,
        session_id: personal ? null : profile.session.id, build_id: personal ? null : profile.session.resolved_build_id },
        current => synchronizeCatalog(replicaStep(current), current)); }
    catch (error) {
        // Permissions may change between Open and subscription admission.
        if (world && renew) {
            if ((error as Failure).code === 'world-participant-revoked') await localStep(() => withdrawWorld(volume.id, true));
            else if ((error as Failure).code === 'ap-rejected') await renewWorld(volume.id);
        }
        throw error;
    }
    return true;
}
async function renewWorld(id: string): Promise<void> {
    try {
        if (await publisherWorld(id)) return;
    } catch (error) {
        if ((error as Failure).code === 'world-participant-revoked') await localStep(() => withdrawWorld(id, true));
        throw error;
    }
    await localStep(() => withdrawWorld(id, false));
    throw new AbiError(-2);
}
// Run in the same local turn as an authenticated terminal or Open response.
function withdrawWorld(id: string, revoked: boolean): void {
    if (!projectedWorld(id)) return;
    automaticCall('pb_automatic_world_denied', { volume: id, revoked });
    removeAutomaticReplica(id);
    unmountedWorld = id;
}
function removeAutomaticReplica(id: string): void {
    replicas?.removeVolume(id);
    for (const key of namedSlots.keys()) if (key.startsWith(id + '/')) namedSlots.delete(key);
    for (const source of entrySubscriptions) if (source.startsWith(id + '/')) {
        entrySubscriptions.delete(source); scopeRecoveries.delete(source);
    }
    automaticSlots.delete(id); scopeRecoveries.delete(id);
}
async function requestAutomaticScopes(): Promise<boolean> {
    if (preparingScopes) return preparingScopes;
    const requests: AutomaticAddress[] = JSON.parse(decoder.decode(hostOutput(host.exports.pb_automatic_needed())));
    if (!requests.length) return false;
    preparingScopes = (async () => {
        for (const address of requests) {
            try { await prepareAutomatic(address); }
            catch (error) {
                if (!closing && !closed) {
                    await localStep(() => automaticCall("pb_automatic_failed", {
                        ...address, status: error instanceof AbiError ? error.status : (error as {hostStatus?: number}).hostStatus ?? -5,
                    }));
                    if (address.role === 'root' && retryable(error)) {
                        unmountedWorld = address.volume;
                        catalogRecoveryFailed(address.volume, error);
                    }
                }
            }
        }
        return true;
    })().finally(() => { preparingScopes = undefined; });
    return preparingScopes;
}
async function requestDatabases(): Promise<boolean> {
    if (!host || closing || closed) return false;
    const scopesPrepared = await requestAutomaticScopes();
    const needed: { volume: string; database: DatabaseId }[] = JSON.parse(decoder.decode(hostOutput(host.exports.pb_database_needed())));
    const waiting: Promise<void>[] = [];
    for (const address of needed) {
        const identity = `${address.volume}/${address.database}`;
        const existing = preparingDatabases.get(identity);
        if (existing) { waiting.push(existing); continue; }
        if (preparingDatabases.size === 27) throw new Error('Database preparation capacity exceeded');
        const volume = replicaRecord(address.volume);
        const task = databasePreparations.catch(() => {}).then(async () => {
            const preparation = await localStep(() => {
                const bytes = encoder.encode(JSON.stringify(address)); hostInput(bytes);
                return JSON.parse(decoder.decode(hostOutput(host.exports.pb_database_preparation(bytes.length))));
            });
            if (preparation === 'local') await openNamed(volume, address.database);
            else if (!managed) throw Object.assign(new Error('Entry is not available locally'), { hostStatus: -4 });
            else if (preparation === 'volume') await pullVolumeSnapshot(volume, volume.name || 'shared data', localStep);
            else if (preparation === 'entry') await connectEntry(volume, address.database);
            else throw new Error('Invalid database preparation mode');
            await localStep(async () => { if (guest) await refreshScene(); });
        }).catch(async error => {
            if (closing || closed) return;
            if (error instanceof LocalVolumeChanged) return;
            const status = (error as { hostStatus?: number }).hostStatus ?? -5;
            await localStep(async () => {
                const bytes = encoder.encode(JSON.stringify({ address, status }));
                hostInput(bytes); hostOutput(host.exports.pb_database_failed(bytes.length));
                logs.push(String(error).slice(0, 512)); if (logs.length > 256) logs.shift();
                if (guest) await refreshScene();
            });
        }).finally(() => preparingDatabases.delete(identity));
        databasePreparations = task; preparingDatabases.set(identity, task); waiting.push(task);
    }
    await Promise.all(waiting);
    return scopesPrepared || waiting.length !== 0;
}

function entryScope(volume: InvitationVolume, entry: DatabaseId): Extract<ReplicaScope, { kind: 'entry' }> {
    if (entry === 'index' || entry === 'revocations') throw new Error('Expected an entry identity');
    const personal = personalReplica(volume.id);
    return { kind: 'entry', volume_id: volume.id, entry_scope: entry,
        session_id: personal ? null : profile.session.id, build_id: personal ? null : profile.session.resolved_build_id };
}
async function connectEntry(volume: InvitationVolume, entry: DatabaseId): Promise<void> {
    const scope = entryScope(volume, entry);
    if (!replicas?.run || replicas.run.stopped) throw Object.assign(new Error('Entry capture requires a connected replica'), { code: 'replica-gap' });
    const source = replicaSource(scope);
    const retained = entrySubscriptions.has(source);
    if (!retained && entrySubscriptions.size === 32) throw new Error('Selected replica interest capacity exceeded');
    entrySubscriptions.add(source);
    try {
        await replicas.add(scope, async current => {
            const step = replicaStep(current);
            await pullVolumeSnapshot(volume, volume.name || 'shared entry', step, undefined, scope.entry_scope, current);
        });
    } finally {
        if (!retained && !replicas.run?.streams.some(stream => stream.source === source)) {
            entrySubscriptions.delete(source); scopeRecoveries.delete(source);
        }
    }
}
async function restoreEntrySubscriptions(): Promise<void> {
    const volumes = new Set(replicatedVolumeIds());
    for (const source of entrySubscriptions) {
        const address = replicaAddress(source);
        if (!volumes.has(address.volume)) {
            entrySubscriptions.delete(source); scopeRecoveries.delete(source); continue;
        }
        if (scopeRecoveries.get(source) === 'blocked') continue;
        if (!replicas?.run || replicas.run.stopped || closed || closing) return;
        try {
            await connectEntry(replicaRecord(address.volume), address.entry!);
        } catch (error) {
            if (entrySubscriptions.has(source)) scopeRecoveries.set(source, retryable(error) ? 'retry' : 'blocked');
        }
    }
    recoveryPending = [...scopeRecoveries.values()].includes('retry');
}
async function saveProfile() {
    requireStorageFormat();
    const file = await (await directory.getFileHandle("profile.json", { create: true })).createSyncAccessHandle();
    try {
        requireStorageFormat();
        const bytes = encoder.encode(JSON.stringify(profile));
        if (file.write(bytes) !== bytes.length) throw new Error("Could not retain web session");
        file.truncate(bytes.length); file.flush();
    } finally { file.close(); }
}
async function syncDatabase(volume: InvitationVolume | null, database: DatabaseId, direction: "pull" | "push", version = volume ? profile.session.resolved_build_id : undefined, step: Step = callback => callback(), current = () => !closed && !closing, entry_scope?: string) {
    return withDatabaseTransfer(() => transferDatabase(volume, database, direction, version, step, current, undefined, entry_scope), current);
}
async function withDatabaseTransfer<T>(operation: () => Promise<T>, current = () => true): Promise<T> {
    if (pendingDatabaseTransfers >= 32) throw failure('ap-capacity', 'Database synchronization is busy');
    pendingDatabaseTransfers++;
    const transfer = databaseTransfers.then(() => {
        requireCurrentTransfer(current);
        return operation();
    });
    databaseTransfers = transfer.then(() => {}, () => {});
    try { return await transfer; } finally { pendingDatabaseTransfers--; }
}
function requireCurrentTransfer(current: () => boolean): void {
    if (closed || closing || !current()) throw failure('replica-gap', 'Database synchronization retired');
}
async function transferDatabase(volume: InvitationVolume | null, database: DatabaseId, direction: "pull" | "push",
    version: string | undefined, step: Step, current: () => boolean, preparedSlot?: number, entry_scope?: string) {
    requireCurrentTransfer(current);
    const personal = volume && personalReplica(volume.id);
    const operation = volume ? { op: "sync_database", operation_id: encodeId(random(16)), session_id: personal ? null : profile.session.id,
        build_id: personal ? null : version, volume_id: volume.id, database_id: database, direction, entry_scope }
        : { op: "sync_account", operation_id: encodeId(random(16)), direction };
    const handle = await ap("begin", operation);
    // The transfer queue owns one AP stream. Retirement can close that stream
    // to wake its pending read. Its transfer still owns Wasm abort and cleanup.
    let closeRequest: Promise<unknown> | undefined;
    const close = () => closeRequest ??= ap("streamClose", handle).catch(() => {});
    if (step.replicaCurrent) activeDatabaseStream = { current: step.replicaCurrent, close };
    let started = false;
    try {
        requireCurrentTransfer(current);
        await ap("streamResponse", handle);
        const base = !volume || volume.id === profile.root.id ? 0 : personalSlots.get(volume.id) ?? automaticSlots.get(volume.id)?.base;
        if (base === undefined) throw new Error('Unknown volume');
        const slot = preparedSlot ?? (!volume ? 0 : database === 'index' ? base + 1 : database === 'revocations' ? base + 2 : await openNamed(volume, database));
        if (preparedSlot !== undefined && (!volume || direction !== 'pull')) throw new Error('Invalid prepared transfer');
        let output = await step(() => hostOutput(preparedSlot === undefined
            ? host.exports.pb_sync_begin(slot, direction === 'pull' ? 0 : entry_scope ? 2 : 1) : host.exports.pb_volume_sync_begin(slot)));
        started = true;
        const writeOutput = async (initial: Uint8Array) => {
            let wrote = false, bytes = initial;
            for (;;) {
                if (bytes.length) { await ap("streamWrite", handle, bytes); wrote = true; }
                if (direction !== "push") return wrote;
                bytes = await step(() => hostOutput(host.exports.pb_sync_output()));
                if (!bytes.length) return wrote;
            }
        };
        await writeOutput(output);
        for (;;) {
            const input = new Uint8Array(await ap("streamRead", handle));
            if (!input.length) throw new Error("Connection closed before database synchronization completed");
            let wrote = false;
            for (let offset = 0; offset < input.length; offset += 256 * 1024) {
                const fragment = input.subarray(offset, offset + 256 * 1024);
                output = await step(() => { hostInput(fragment); return hostOutput(host.exports.pb_sync_step(fragment.length)); });
                wrote = await writeOutput(output) || wrote;
            }
            if (await step(() => host.exports.pb_sync_done())) {
                if (direction === "push" && wrote) {
                    // Equal/empty roots finish when the response is emitted,
                    // but the peer's 8-byte header + 1-byte Bye still follows.
                    // AP stream chunks need not align with that frame. kvdb
                    // validates its contents after this bounded collection.
                    const terminal = new Uint8Array(9);
                    for (let offset = 0; offset < terminal.length;) {
                        const chunk = new Uint8Array(await ap("streamRead", handle));
                        if (!chunk.length || chunk.length > terminal.length - offset) throw new Error("Invalid database synchronization terminal length");
                        terminal.set(chunk, offset); offset += chunk.length;
                    }
                    await step(() => { hostInput(terminal); hostOutput(host.exports.pb_sync_step(terminal.length)); });
                }
                // Account synchronization is only requested once the account replica is open.
                if (!volume && direction === 'pull') await accountReplica!.inspectSync(current);
                await step(() => hostOutput(host.exports.pb_sync_finish(1))); started = false;
                break;
            }
        }
        const evidence = volume && direction === "pull" && database === "index"
            ? new Uint8Array(await ap("streamNamespaceEvidence", handle)) : undefined;
        if (evidence && (evidence.length < 8 || evidence.length > 8 + 4096 * 16)) throw new Error("Invalid namespace evidence size");
        await ap("streamFinish", handle);
        const response = await ap("streamResponse", handle);
        const receipt = volume ? replicaReceipt(response.data) : undefined;
        if (evidence) {
            const metadata = encoder.encode(replicaJSON({ slot, prepared: preparedSlot !== undefined, entry: entry_scope ?? null, receipt }));
            const input = new Uint8Array(metadata.length + evidence.length);
            input.set(metadata); input.set(evidence, metadata.length);
            await step(() => { hostInput(input); hostOutput(host.exports.pb_namespace_evidence(input.length, metadata.length)); });
        }
        return receipt;
    } finally {
        try {
            if (started) await step(() => host.exports.pb_sync_finish(0), true);
        } finally { activeDatabaseStream = undefined; await close(); }
    }
}
async function openSession(session: InvitationSession, step: Step = callback => callback()): Promise<void> {
    const admission = await step(() => {
        const bytes = encoder.encode(JSON.stringify(session)); hostInput(bytes);
        return JSON.parse(decoder.decode(hostOutput(host.exports.pb_session_authority(bytes.length))));
    });
    const response = await ap("request", { op: "open_session", operation_id: encodeId(random(16)), session_id: session.id,
        build_id: session.resolved_build_id, proof });
    await step(() => {
        const registered = (response.data as { session?: unknown } | undefined)?.session;
        const bytes = encoder.encode(JSON.stringify({ admission, registered })); hostInput(bytes);
        hostOutput(host.exports.pb_session_admit(bytes.length));
    });
}
async function catalogReplicaAllowed(volume: string, step: Step = callback => callback()): Promise<boolean> {
    return step(() => {
        if (!replicaVolumeCurrent(volume)) return false;
        const input = encoder.encode(JSON.stringify(volume)); hostInput(input);
        const allowed = JSON.parse(decoder.decode(hostOutput(host.exports.pb_replica_catalog_allowed(input.length))));
        if (typeof allowed !== 'boolean') throw new Error('Invalid catalog replication admission');
        return allowed;
    });
}
async function synchronize(bootstrap = false, step: Step = callback => callback(), plannedVolumes?: string[], current = () => !closed && !closing) {
    if (!managed) return;
    progress("Connecting to Playbit…");
    await ap("connect");
    progress("Checking session access…");
    await openSession(profile.session, step);
    const volumes = (plannedVolumes ?? replicatedVolumeIds()).filter(id => !projectedWorld(id));
    for (const [index, id] of volumes.entries()) {
        if (!await catalogReplicaAllowed(id, step) || !replicaVolumeCurrent(id)) continue;
        const volume = replicaRecord(id);
        const admitted = () => current() && replicaVolumeCurrent(id);
        try {
            await synchronizeOrdinaryVolume(volume, `${volume.name || "shared data"} (${index + 1}/${volumes.length})`, !bootstrap, step, admitted);
        } catch (error) {
            if (replicaVolumeCurrent(id)) throw error;
        }
    }
    progress("Saving pending app files…");
    await uploadBlobs();
    progress("Saving session data…");
    await saveProfile();
}
async function synchronizeOrdinaryVolume(volume: InvitationVolume, label: string, push: boolean, step: Step, current = () => !closed && !closing): Promise<void> {
    const sync = (database: DatabaseId, direction: "pull" | "push") => syncDatabase(volume, database, direction, undefined, step, current);
    if (!volume.share_key_with_service) throw new Error(`The key for ${volume.name || 'this volume'} is not shared with AP. Catch-up requires an authorized online peer; web peer catch-up is not available yet`);
    const fenced = await step(() => {
        const bytes = encoder.encode(JSON.stringify(volume.id)); hostInput(bytes);
        return JSON.parse(decoder.decode(hostOutput(host.exports.pb_authority_refresh_required(bytes.length))));
    });
    if (fenced && push) await pullVolumeSnapshot(volume, volume.name || "shared data", step, undefined, undefined, current);
    if (push) {
        // Publish local inputs before capturing the remote cut. A push
        // between snapshot pulls would invalidate its shared write epoch.
        await sync("index", "pull");
        await sync("revocations", "pull");
        await sync("index", "push");
        await sync("index", "pull");
        for await (const { database } of databaseCatalog(volume, step)) await sync(database, "push");
    }
    await pullVolumeSnapshot(volume, label, step, undefined, undefined, current);
}
async function pullVolumeSnapshot(volume: InvitationVolume, label: string, step: Step,
    selection?: { session: InvitationSession; grants: PersonalGrant[] }, entry_scope?: string, current = () => !closed && !closing): Promise<void> {
    await withDatabaseTransfer(() => captureVolumeSnapshot(volume, label, step, selection, entry_scope, current), current);
}
async function captureVolumeSnapshot(volume: InvitationVolume, label: string, step: Step,
    selection: { session: InvitationSession; grants: PersonalGrant[] } | undefined, entry_scope: string | undefined, current: () => boolean): Promise<void> {
    const { directory, publication } = storageFor(volume.id);
    await reclaimVolume(volume.id);
    for (let attempt = 0; attempt < 3; attempt++) {
        let prepared: PreparedVolume | undefined, slots: number[] = [];
        try {
            prepared = publication.prepare(volume.id);
            await publication.create(directory, prepared);
            const base = allocateBackingSlots(3); slots = [base, base + 1, base + 2];
            await prepareVolumeInputs(volume, prepared, slots, step, selection, entry_scope);
            const pull = async (database: DatabaseId, slot: number) => {
                const receipt = await transferDatabase(volume, database, 'pull', profile.session.resolved_build_id, step, current, slot, entry_scope);
                await step(() => {
                    const bytes = encoder.encode(replicaJSON({ slot, receipt })); hostInput(bytes);
                    hostOutput(host.exports.pb_volume_receipt(bytes.length));
                });
                return receipt;
            };
            progress(`Loading ${label}: catalog…`);
            await pull('index', slots[0]);
            progress(`Loading ${label}: permissions…`);
            await pull('revocations', slots[1]);
            let offset = 0, localFallback = false;
            for await (const [database, protocol] of preparedCatalog(false, step)) {
                progress(`Loading ${label}: app database ${++offset}…`);
                const slot = allocateBackingSlots(1); slots.push(slot);
                await openPreparedBacking(prepared, database, slot);
                await step(() => {
                    const bytes = encoder.encode(JSON.stringify({ entry: database, slot, replica_id: replicaId() }));
                    hostInput(bytes); hostOutput(host.exports.pb_volume_register_backing(bytes.length));
                });
                try {
                    if (protocol === 'kvdb') await pull(database, slot);
                    else await pullPreparedLog(volume, database, step);
                } catch (error) {
                    const message = (error as Error).message;
                    if ((error as { code?: string }).code !== 'ap-rejected'
                        || !/(?:^|: )entry not found$/.test(message) && message !== 'waiting for initial database synchronization') throw error;
                    localFallback = true;
                    publication.check(prepared);
                    await step(() => {
                        const bytes = encoder.encode(JSON.stringify(database)); hostInput(bytes);
                        hostOutput(host.exports.pb_volume_local_backing(bytes.length));
                    });
                    if (protocol === 'blob_log') await validatePreparedLog(step);
                }
                await step(() => hostOutput(host.exports.pb_volume_close_backing()));
                slots.pop();
            }
            if (!localFallback) {
                const receipt = await pull('index', slots[0]);
                await step(() => {
                    const bytes = encoder.encode(replicaJSON(receipt)); hostInput(bytes);
                    hostOutput(host.exports.pb_volume_qualify(bytes.length));
                });
                const operations = {
                    pb_volume_rejected_page: host.exports.pb_volume_rejected_page,
                    pb_volume_archive_plan: host.exports.pb_volume_archive_plan,
                    pb_volume_archive_blobs: host.exports.pb_volume_archive_blobs,
                    pb_volume_archive_commit: host.exports.pb_volume_archive_commit,
                    pb_volume_local_location: host.exports.pb_volume_local_location,
                };
                await retainRejectedCut(directory, publication!, prepared, (name, value) => step(() => {
                    publication.check(prepared!);
                    if (!Object.hasOwn(operations, name)) throw new Error('Unknown recovery operation');
                    const bytes = encoder.encode(JSON.stringify(value)); hostInput(bytes);
                    const output = hostOutput(operations[name as keyof typeof operations](bytes.length));
                    return output.length ? JSON.parse(decoder.decode(output)) : undefined;
                }), blobHash, yieldPreparation);
            }
            await step(() => publication.publish(prepared!,
                () => { hostOutput(host.exports.pb_volume_validate()); }, () => {
                    for (const slot of slots) stores.get(slot)!.attachFence(prepared!.epoch);
                    hostOutput(host.exports.pb_volume_publish());
                }));
            slots = [];
            await reclaimVolume(volume.id);
            return;
        } catch (error) {
            if (error instanceof PublicationUncertain) {
                engineFailure = error; closed = true; self.postMessage({ engineFailure: error.message }); throw error;
            }
            if (!(error instanceof LocalVolumeChanged) && (error as { hostStatus?: number }).hostStatus !== -10) throw error;
            if (attempt === 2) throw new Error("Volume source kept changing during bootstrap; retry when its writers are quiet");
        } finally {
            try { if (!closed) await step(() => host.exports.pb_volume_abort(), true); }
            finally {
                for (const slot of slots) { stores.get(slot)?.close(); stores.delete(slot); }
                if (prepared && !closed) await publication.discard(directory, prepared.volume, prepared.generation);
            }
        }
    }
}
async function prepareVolumeInputs(volume: InvitationVolume, prepared: PreparedVolume, slots: readonly number[], step: Step,
    selection?: { session: InvitationSession; grants: PersonalGrant[] }, entry_scope?: string): Promise<void> {
    const admission = await step(() => {
        const bytes = encoder.encode(JSON.stringify({ volume: volume.id, entry_scope })); hostInput(bytes);
        return JSON.parse(decoder.decode(hostOutput(host.exports.pb_volume_admission(bytes.length))));
    });
    for (const [offset, database] of ['index', 'revocations'].entries()) await copyPreparedBacking(prepared, database, slots[offset], true, step);
    await copyPreparedBacking(prepared, 'index', slots[2], true, step, true);
    await step(() => {
        const bytes = encoder.encode(JSON.stringify({ volume: volume.id, base: slots[0], replica_ids: [replicaId(), replicaId()], accepted_slot: slots[2], accepted_replica_id: replicaId(), generation: prepared.generation, selection, entry_scope, ...admission }));
        hostInput(bytes); hostOutput(host.exports.pb_volume_prepare(bytes.length));
    });
    for await (const [database] of preparedCatalog(true, step)) await copyPreparedBacking(prepared, database, undefined, true, step);
}
async function pullPreparedLog(volume: InvitationVolume, log: DatabaseId, step: Step): Promise<void> {
    for (;;) {
        const complete = await step(() => JSON.parse(decoder.decode(hostOutput(host.exports.pb_volume_log_prepare()))));
        if (complete === true) break;
        if (complete !== false) throw new Error('Invalid append-log preparation result');
        await yieldPreparation();
    }
    let cursor: string | null = null;
    for (;;) {
        const personal = personalReplica(volume.id);
        const response = await ap('request', { op: 'get_bloblog', operation_id: encodeId(random(16)),
            session_id: personal ? null : profile.session.id, build_id: personal ? null : profile.session.resolved_build_id,
            volume_id: volume.id, log_id: log, cursor });
        const received = bloblogPage(response.data, volume.id, log);
        if (received.page.more && (!received.page.entries.length || received.page.cursor === cursor)) throw new Error('Append-log page made no progress');
        await step(() => {
            hostInput(received.bytes); const applied = JSON.parse(decoder.decode(hostOutput(host.exports.pb_volume_log_page(received.bytes.length))));
            if (applied !== received.page.cursor) throw new Error('Append-log cursor admission differs');
        });
        cursor = received.page.cursor;
        if (!received.page.more) break;
    }
    await validatePreparedLog(step);
}
async function validatePreparedLog(step: Step): Promise<void> {
    for (;;) {
        const complete = await step(() => JSON.parse(decoder.decode(hostOutput(host.exports.pb_volume_log_validate()))));
        if (complete === true) return;
        if (complete !== false) throw new Error('Invalid append-log validation result');
        await yieldPreparation();
    }
}
function replicatedVolumeIds(): string[] {
    const volumes = new Set([profile.root.id, ...activeGrants.map(grant => grant.volume)]);
    for (const [id, mount] of automaticSlots) if (!mount.storage && !mount.preparing) volumes.add(id);
    return [...volumes];
}
function replicaRecord(volume: string): InvitationVolume {
    const record = automaticSlots.get(volume)?.volume ?? [profile.root, ...personalCatalog.map(item => item.volume)].find(item => item.id === volume);
    if (!record) throw new Error('Unknown replica volume'); return record;
}
async function catchUpLog(volume: string, database: DatabaseId, current: () => boolean): Promise<void> {
    volume = replicaAddress(volume).volume;
    const record = replicaRecord(volume);
    const step: Step = callback => localStep(() => { if (!current()) throw new Error('Log recovery retired'); return callback(); });
    await openNamed(record, database);
    let cursor: string | null;
    try {
        cursor = await step(() => {
            const bytes = encoder.encode(JSON.stringify({ volume, database })); hostInput(bytes);
            return JSON.parse(decoder.decode(hostOutput(host.exports.pb_bloblog_checkpoint(bytes.length))));
        });
    } catch (error) {
        if ((error as { hostStatus?: number }).hostStatus === -6) throw Object.assign(new Error('Log requires initial volume synchronization'), { code: 'replica-gap' });
        throw error;
    }
    for (;;) {
        const personal = personalReplica(volume);
        const response = await ap('request', { op: 'get_bloblog', operation_id: encodeId(random(16)),
            session_id: personal ? null : profile.session.id, build_id: personal ? null : profile.session.resolved_build_id,
            volume_id: volume, log_id: database, cursor });
        // Live append receipt admits each immutable page independently. A
        // moving source does not invalidate earlier authorized append identities.
        const received = bloblogPage(response.data, volume, database);
        if (received.page.more && (!received.page.entries.length || received.page.cursor === cursor)) throw new Error('Append-log recovery made no progress');
        const address = JSON.stringify({ volume, database });
        const bytes = encoder.encode(`{"address":${address},"receipt":${decoder.decode(received.bytes)}}`);
        await step(() => { hostInput(bytes); hostOutput(host.exports.pb_bloblog_receive(bytes.length)); });
        cursor = received.page.cursor; if (!received.page.more) break;
    }
}
function allocateBackingSlots(count: number): number {
    if (nextNamedSlot + count > 0x7fffffff) throw new Error('Database handle identity exhausted');
    const first = nextNamedSlot + 1; nextNamedSlot += count; return first;
}
async function copyPreparedBacking(prepared: PreparedVolume, database: DatabaseId, slot: number | undefined, required: boolean, step: Step, acceptedIndex = false): Promise<void> {
    const { directory, publication } = storageFor(prepared.volume);
    publication.check(prepared);
    const targetFile = acceptedIndex ? await publication.acceptedIndex(directory, prepared)
        : await publication.file(directory, prepared.volume, prepared.generation, database, true);
    const target = await targetFile.createSyncAccessHandle();
    try {
        if (target.getSize()) throw new Error('Fresh backing generation already exists');
        if (prepared.previous && !acceptedIndex) {
            const sourceGeneration = database === 'index' || database === 'revocations' ? prepared.previous : await step(() => {
                const bytes = encoder.encode(JSON.stringify(database)); hostInput(bytes);
                return JSON.parse(decoder.decode(hostOutput(host.exports.pb_volume_local_location(bytes.length)))) ?? prepared.previous;
            });
            let source: File | undefined;
            try { source = await (await publication.file(directory, prepared.volume, sourceGeneration, database, false)).getFile(); }
            catch (error) { if (required || (error as DOMException).name !== 'NotFoundError') throw error; }
            if (source) {
                await copyVolumeBacking(prepared, source, target, yieldPreparation);
                if (database !== 'index' && database !== 'revocations') await step(() => {
                    const bytes = encoder.encode(JSON.stringify(database)); hostInput(bytes);
                    hostOutput(host.exports.pb_volume_bind_backing(bytes.length));
                });
            }
        }
        if (slot !== undefined) { stores.set(slot, await openStore(target)); return; }
    } catch (error) { target.close(); throw error; }
    target.close();
}
async function openPreparedBacking(prepared: PreparedVolume, database: DatabaseId, slot: number): Promise<void> {
    const { directory, publication } = storageFor(prepared.volume);
    publication.check(prepared);
    const file = await (await publication.file(directory, prepared.volume, prepared.generation, database, true)).createSyncAccessHandle();
    if (closing || closed) { file.close(); throw new Error('Volume preparation retired'); }
    stores.set(slot, await openStore(file));
}
async function openStore(file: FileSystemSyncAccessHandle, fence?: VolumeWriteEpoch): Promise<OpfsStore> {
    const store = await OpfsStore.open(file, storageResources!, yieldPreparation, fence);
    if (closing || closed) { store.close(); throw new Error('Database preparation retired'); }
    return store;
}
async function* preparedCatalog(copy_local: boolean, step: Step): AsyncGenerator<[DatabaseId, 'kvdb' | 'blob_log']> {
    let after: string | null = null, root: number[] | null = null;
    do {
        const page: { backings: [DatabaseId, 'kvdb' | 'blob_log'][]; next: string | null; root: number[] | null } = await step(() => {
            const bytes = encoder.encode(JSON.stringify({ after, expected_root: root, copy_local }));
            hostInput(bytes); return JSON.parse(decoder.decode(hostOutput(host.exports.pb_volume_catalog_page(bytes.length))));
        });
        root = page.root; after = page.next;
        for (const backing of page.backings) yield backing;
    } while (after);
    await step(() => {
        const bytes = encoder.encode(JSON.stringify({ after: null, expected_root: root, copy_local }));
        hostInput(bytes); hostOutput(host.exports.pb_volume_catalog_page(bytes.length));
    });
}
function selectPersonal(session: string, grants: PersonalSelection[]) {
    const bytes = encoder.encode(JSON.stringify({ session, grants }));
    hostInput(bytes); hostOutput(host.exports.pb_select_personal(bytes.length));
    if (session === profile.session.id) activeGrants = structuredClone(grants);
}
async function* databaseCatalog(volume: InvitationVolume, step: Step,
    selection?: { session: InvitationSession; grants: PersonalGrant[] },
    page = host.exports.pb_upload_catalog_page): AsyncGenerator<{ database: DatabaseId; blind: boolean }> {
    let after: string | null = null, root: number[] | null = null;
    do {
        const result: { databases: DatabaseId[]; blind?: string[]; next: string | null; root: number[] } = await step(() => {
            const bytes = encoder.encode(JSON.stringify({ volume: volume.id, after, expected_root: root, selection }));
            hostInput(bytes); return JSON.parse(decoder.decode(hostOutput(page(bytes.length))));
        });
        root = result.root; after = result.next;
        for (const database of result.databases) if (database !== 'index' && database !== 'revocations') {
            yield { database, blind: result.blind?.includes(database) ?? false };
        }
    } while (after);
    // Recheck the source root after the final suspended backing transfer too.
    await step(() => {
        const bytes = encoder.encode(JSON.stringify({ volume: volume.id, after: null, expected_root: root, selection }));
        hostInput(bytes); hostOutput(page(bytes.length));
    });
}
async function publishWorldAppends(volume: InvitationVolume, database: DatabaseId, step: Step): Promise<void> {
    let after: number[] | undefined, through: number[] | undefined;
    await openNamed(volume, database);
    for (;;) {
        const batch = await step(() => {
            const input = encoder.encode(JSON.stringify({ volume: volume.id, database, after, through }));
            hostInput(input);
            return hostOutput(host.exports.pb_world_append_next(input.length));
        });
        if (!batch.length) return;
        if (batch.length < 16 || batch.length > 16 + 65536) throw new Error('Invalid pending World append');
        through ??= Array.from(batch.subarray(0, 8));
        const clock = Array.from(batch.subarray(8, 16));
        if (batch.length > 16) {
            try {
                await ap('publishReplica', { op: 'publish_replica', operation_id: encodeId(random(16)), session_id: profile.session.id,
                    build_id: profile.session.resolved_build_id, volume_id: volume.id, database_id: database }, batch.subarray(16));
            } catch (error) {
                if ((error as Failure).code !== 'world-entry-rejected') throw error;
                // Keep the refused birth for reconciliation and offer later keys.
                after = clock;
                continue;
            }
        }
        // A header-only result identifies a stale local birth. A submitted birth
        // keeps its durable marker until AP acknowledges this exact publication.
        await step(() => {
            const input = encoder.encode(JSON.stringify({ volume: volume.id, database, clock })); hostInput(input);
            hostOutput(host.exports.pb_world_append_ack(input.length));
        });
        after = clock;
    }
}
async function settleWorldReceipt(mount: AutomaticMount, database: DatabaseId, bytes: Uint8Array, current: () => boolean): Promise<void> {
    const volume = mount.volume.id, address = `${volume}/${database}`;
    const active = () => current() && automaticSlots.get(volume) === mount;
    // Snapshot installation can retire all named slots while an AP receipt waits
    // for the local queue. Resolve the backing and apply the receipt in one turn.
    const settle = () => localStep(() => {
        if (!active()) return true;
        const slot = namedSlots.get(address);
        if (slot === undefined || !stores.has(slot)) return false;
        hostInput(bytes);
        const status = host.exports.pb_world_append_published(slot, bytes.length);
        if (status === -1) return false;
        hostOutput(status);
        return true;
    });
    if (await settle()) return;
    try {
        await openNamed(mount.volume, database);
        if (await settle()) return;
    } catch (error) {
        if (!active()) return;
        if (!(error instanceof LocalVolumeChanged)) {
            catalogRecoveryFailed(volume, error);
            return;
        }
    }
    // A second replacement can race the reopen. Keep the birth for this
    // World’s scoped replay and let Shared continue draining its publications.
    if (active() && !scopeRecoveries.has(volume)) replicas?.recoverVolumeCatalog(volume);
}
function validateCandidate(session: InvitationSession, grants: PersonalGrant[]): void {
    const bytes = encoder.encode(JSON.stringify({ session, grants }));
    hostInput(bytes); hostOutput(host.exports.pb_validate_selection(bytes.length));
}
async function preparePersonal(session: InvitationSession, grants: PersonalGrant[], step: Step = callback => callback()) {
    grants = validateSelection(grants, personalCatalog);
    for (const grant of grants) {
        if (personalSlots.has(grant.volume)) continue;
        await withSecondaryMount(grant.volume, base => openPersonalMount(grant.volume, base, step));
    }
    // Record the user's admitted selection before preparing its files. Every
    // preparation captures this consent revision before suspended copying.
    const prepared = grants.map(grant => {
        const saved = selections[session.id]?.find(item => item.volume === grant.volume && item.write === grant.write);
        return { ...grant, incarnation: saved?.incarnation ?? encodeId(random(16)) };
    });
    await step(() => selectPersonal(session.id, prepared));
    await pullPersonal(session, grants, step);
}
async function openPersonalMount(id: string, base: number, step: Step): Promise<void> {
    if (personalSlots.has(id)) return;
    const entry = personalCatalog.find(item => item.volume.id === id)!; // validateSelection matched every grant.
    const automatic = automaticSlots.get(id);
    if (personalSlots.size >= MAX_PERSONAL_VOLUMES) throw new Error("Personal volume capacity exceeded");
    profile.personal_replicas ??= {};
    const replica_ids = profile.personal_replicas[id] ??= Array.from({ length: 2 }, replicaId);
    try {
        const initial = automatic ? undefined : await openVolumeControls(id, base);
        await step(() => {
            const bytes = encoder.encode(JSON.stringify({ volume: entry.volume, key: entry.credentials.database_key, replica_ids, base }));
            hostInput(bytes); hostOutput(host.exports.pb_register_personal(bytes.length));
            personalSlots.set(id, base);
            finishInitialVolume(initial);
        });
        await reclaimVolume(id);
    } catch (error) {
        if (!automatic && !personalSlots.has(id)) for (let database = 1; database <= 2; database++) {
            stores.get(base + database)?.close(); stores.delete(base + database);
        }
        throw error;
    }
}
async function pullPersonal(session: InvitationSession, grants: PersonalGrant[], step: Step) {
    grants = validateSelection(grants, personalCatalog);
    if (grants.some(grant => !personalSlots.has(grant.volume))) {
        throw new Error("Personal volume access changed. Choose its access again to load the new volume");
    }
    // Pulling an app update must never install its captured grants. The host
    // checks current session consent and its revision throughout preparation.
    // Catalog ordering follows the host's retained slots, not chooser order.
    const volumes = [...personalSlots.keys()].filter(id => grants.some(grant => grant.volume === id))
        .map(id => personalCatalog.find(item => item.volume.id === id)!.volume);
    for (const volume of volumes) {
        if (!volume.share_key_with_service) throw new Error(`The key for ${volume.name || 'this personal volume'} is not shared with AP. Open it in Playbit while an authorized peer is online`);
        await pullVolumeSnapshot(volume, volume.name || "personal data", step, { session, grants });
    }
    await saveProfile();
}
async function refreshAccountProfile(): Promise<Set<string>> {
    const snapshot = await accountReplica?.profile();
    if (!snapshot) return new Set();
    const user = boundUser?.volume.id;
    if (user && snapshot.deletedVolumes.has(user) && !closed && !closing) {
        automaticCall("pb_automatic_user_deleted", user);
        boundUser = undefined;
        personalSlots.delete(user);
        personalCatalog = personalCatalog.filter(item => item.volume.id !== user);
        removeAutomaticReplica(user);
    }
    if (snapshot.revision > accountProfileRevision && !closed && !closing) {
        if (displayName !== snapshot.displayName) {
            displayNameBytes(snapshot.displayName);
            displayName = snapshot.displayName;
            if (localIdentity) localIdentity.displayName = displayName;
            channelClient?.setDisplayName(displayName);
            if (guest) operations.environmentChanged(IDENTITY_CHANGED);
        }
        accountProfileRevision = snapshot.revision;
    }
    return snapshot.deletedVolumes;
}
async function refreshPersonal(latest?: PersonalSelections) {
    if (!managed) return;
    latest ??= await personalSelections(profile.account);
    const deleted = await refreshAccountProfile();
    if (deleted.has(profile.root.id)) {
        guestFailure = new Error('A volume used by this app was deleted from your account.');
        self.postMessage({ guestFailure: guestFailure.message });
        throw guestFailure;
    }
    const grants = (latest[profile.session.id] ?? []).filter(grant => !deleted.has(grant.volume));
    if (JSON.stringify(activeGrants) !== JSON.stringify(grants)) {
        // Apply withdrawals locally before any later guest dispatch. Loading a
        // newly selected volume is an explicit host operation, never input I/O.
        const retained = grants.filter(grant => personalSlots.has(grant.volume));
        selectPersonal(profile.session.id, retained);
        if (retained.length !== grants.length) throw new Error("Personal volume access changed. Choose its access again to load the new volume");
    }
    selections = latest;
}
let surface: Uint8Array | undefined, surfaceDeferred = false;

async function startGuest(candidate: GuestInstance) {
    channelClient?.close();channelClient=undefined;localIdentity=undefined;
    operations.reset();
    const bundleSlot = guestBundles.get(candidate);
    if (bundleSlot !== undefined) {
        const result = host.exports.pb_package_adopt(bundleSlot);
        if (result < 0) throw new Error(`Bundle adoption failed: ${result}`);
        guestBundles.delete(candidate);
    }
    guest?.deadline?.retire();
    void files.cleanup().catch(error => { logs.push(String(error).slice(0, 512)); });
    operations.refreshVolumes();
    guest = candidate; guestFailure = undefined; scene = null;
    guestWork.queue({ kind: 1, target: 0, data: new Uint8Array() });
    if (surface) guestWork.queue({ kind: 15, target: 0, objectId: windowObjectId, data: surface });
    if(managed)operations.pb_session?.();
    await enterGuest();
    if (!scene && candidate.deadline?.parked) {
        // A guest may await initialization before submitting its first scene.
        // Keep the normal open receipt and expose no input targets until then.
        scene = { root: { kind: "column", children: [], close: 4 }, controls: new Map() };
    }
    if (!scene) throw new Error(`The app could not open: ${logs.join("; ")}`);
}
function adoptSession(session: InvitationSession, tentative = false) {
    const bytes = encoder.encode(JSON.stringify(session));
    hostInput(bytes); hostOutput(tentative ? host.exports.pb_begin_adoption(bytes.length) : host.exports.pb_adopt_session(bytes.length));
}
async function downloadVersion(session: InvitationSession): Promise<{ instance: GuestInstance; name: string }> {
    const source = downloadPackage({
        begin: operation => ap('begin', operation),
        streamFinish: handle => ap('streamFinish', handle),
        streamResponse: handle => ap('streamResponse', handle),
        streamRead: (handle, maxBody) => ap('streamRead', handle, maxBody),
        streamClose: handle => ap('streamClose', handle),
    }, { op: 'get_blob', operation_id: encodeId(random(16)), hash: session.resolved_build_id });
    return prepareBundle(source, session.id, session.app, session.resolved_build_id);
}
// Only this release-owned same-origin resource can anchor an embedded example.
async function examplePublisher(): Promise<{ issuer: number[]; status: number[] }> {
    const bytes = new Uint8Array(4096);
    let size = 0;
    for await (const chunk of fetchPackage('./example-publisher.json')) {
        if (chunk.length > bytes.length - size) throw new Error("Release publisher information is too large");
        bytes.set(chunk, size); size += chunk.length;
    }
    return JSON.parse(decoder.decode(bytes.subarray(0, size)));
}
async function authenticateBundle(slot: number, metadata: { build_id: string; publisher?: string; signer?: number[] },
    sessionId: string, expectedApp?: InvitationSession["app"], retained?: ExampleAdmission) {
    if (!metadata.publisher || !metadata.signer) throw new Error("The package has no publisher signature");
    let anchor: { enrolled_ca: number[] } | { release_issuer: number[] };
    let status: number[], admission: number[] | undefined;
    if (managed) {
        const connection = await ap("connect");
        if (!connection.caCertificate?.length) throw new Error("The invited package has no verifiable publisher");
        anchor = { enrolled_ca: connection.caCertificate };
        const response = await ap("request", { op: "get_publisher_key_status", operation_id: encodeId(random(16)),
            publisher: metadata.publisher, signer: metadata.signer });
        status = (response.data as { status: number[] }).status;
    } else {
        const publisher = retained ?? await examplePublisher();
        anchor = { release_issuer: publisher.issuer };
        status = publisher.status;
    }
    status = await admitPublisherStatus(metadata.publisher, metadata.signer, status, previous => {
        const update = encoder.encode(JSON.stringify({ anchor, publisher: metadata.publisher, signer: metadata.signer, status, previous }));
        hostInput(update);
        const choice = host.exports.pb_publisher_status(update.length);
        if (choice < 0) hostOutput(choice);
        return choice === 1;
    });
    if (managed) {
        const reply = await ap("request", { op: "get_build_identity", operation_id: encodeId(random(16)), build_id: metadata.build_id });
        admission = (reply.data as { admission: number[] }).admission;
    }
    const auth = encoder.encode(JSON.stringify({ anchor, status, admission, retained_example: retained?.record, session_id: sessionId, expected_app: expectedApp, bound_session: profile?.session }));
    hostInput(auth);
    const verified = JSON.parse(decoder.decode(hostOutput(host.exports.pb_package_authenticate(slot, auth.length))));
    const example: ExampleAdmission | undefined = "release_issuer" in anchor
        ? { issuer: anchor.release_issuer, status, record: verified.local_admission } : undefined;
    return { ...verified, example };
}
async function prepareBundle(source: AsyncIterable<Uint8Array> | undefined, sessionId: string,
    expectedApp?: InvitationSession["app"], expected?: string, retained?: number, admission?: ExampleAdmission) {
    if (retained !== undefined && !admission) throw new Error("Saved initializer publisher admission is missing");
    if (bundleFiles.size >= 4) throw new Error("Bundle file capacity exceeded");
    const slot = nextBundle++;
    if (slot > 0x7fffffff) throw new Error("Bundle identity exhausted");
    const storageSlot = retained ?? [0, 1, 2, 3].find(value => ![...bundleStorage.values()].includes(value));
    if (storageSlot === undefined || !Number.isInteger(storageSlot) || storageSlot < 0 || storageSlot > 3
        || [...bundleStorage.values()].includes(storageSlot) || !source && retained === undefined) throw new Error("Bundle storage capacity exceeded");
    const handle = await (await directory.getFileHandle(`app-bundle-${storageSlot}.pbapp`, { create: retained === undefined })).createSyncAccessHandle();
    bundleStorage.set(slot, storageSlot);
    bundleFiles.set(slot, handle);
    let unsupported = false;
    try {
        if (source) await writePackage(handle, source, fileYield);
        let result: number;
        do {
            // Package admission checks the browser. Guest discovery checks the
            // already-adopted instance, which must explicitly opt in.
            checkingPackageFeatures = true;
            try { result = host.exports.pb_package(slot); }
            finally { checkingPackageFeatures = false; }
            if (result === 0) await fileYield();
        } while (result === 0);
        const metadata = JSON.parse(decoder.decode(hostOutput(result)));
        if (expected !== undefined && metadata.build_id !== expected) throw new Error("Downloaded app does not match the authorized session version");
        if (!Array.isArray(metadata.unsupported_host_features)
            || metadata.unsupported_host_features.length > 64
            || metadata.unsupported_host_features.some((feature: unknown) => typeof feature !== 'string' || feature.length > 128)) {
            throw new Error('Invalid package host requirements');
        }
        if (metadata.unsupported_host_features.length) {
            unsupported = true;
            throw failure("unsupported", `This app needs an unsupported host feature: ${metadata.unsupported_host_features[0]}. Its package is retained`);
        }
        if (!Number.isSafeInteger(metadata.length) || metadata.length < 0 || metadata.length > MAX_PACKAGE_WASM_BYTES) throw new Error('Wasm executable exceeds 64 MiB');
        const identity = await authenticateBundle(slot, metadata, sessionId, expectedApp, admission);
        const wasm = new Uint8Array(metadata.length);
        if (handle.read(wasm, { at: metadata.offset }) !== wasm.length) throw new Error("Incomplete executable read");
        // The feature list belongs to the same package authenticated above.
        const instance = await prepareGuest(wasm, operations, metadata.required_host_features);
        guestBundles.set(instance, slot);
        return { instance, app: identity.app as InvitationSession["app"], world: identity.world as string,
            shared: identity.shared as string | null, name: metadata.name, build: metadata.build_id,
            initializer: identity.initializer, storage: storageSlot, example: identity.example };
    } catch (error) {
        try { if (source && !unsupported) handle.truncate(0); } catch { /* Keep the original package available while initialization is unfinished. */ }
        host.exports.pb_package_discard(slot);
        bundleFiles.get(slot)?.close(); bundleFiles.delete(slot); bundleStorage.delete(slot);
        throw error;
    }
}
function updateState(state: UpdateState | null) {
    if (JSON.stringify(profile.update) === JSON.stringify(state)) return;
    profile.update = state;
    self.postMessage({ update: state });
}
function checkUpdate(retry = false) {
    if (!managed || closed) return;
    const previousUpdate = profile.update;
    // An explicit Retry must survive an in-flight automatic resolution. Multiple
    // clicks coalesce into one further attempt after that check has finished.
    retryPending ||= retry;
    if (retryPending) updateState({ kind: "checking", build_id: previousUpdate?.build_id, message: "Checking for the app update…" });
    if (updateTask) return;
    retry = retryPending; retryPending = false;
    // Network download/compilation does not hold the input queue. Adoption is
    // serialized with database synchronization and guest input below.
    let resumeReplication = false;
    updateTask = (async () => {
        // Assigned first in the try below; only the catch may still see it unassigned, and reads it optionally.
        let candidate!: InvitationSession;
        let preparedInstance: GuestInstance | undefined;
        try {
            // resolvedSession validates the response shape before trusting it.
            const resolution = (await ap("request", { op: "resolve_session", operation_id: encodeId(random(16)), session_id: profile.session.id, proof })).data as SessionResolution;
            candidate = resolvedSession(profile.session, resolution, profile.resolution);
            if (candidate.resolved_build_id === profile.session.resolved_build_id) { updateState(null); return; }
            selections = await personalSelections(profile.account);
            if (!retry && previousUpdate?.kind === "failed" && previousUpdate.build_id === candidate.resolved_build_id) return;
            updateState({ kind: "downloading", build_id: candidate.resolved_build_id, message: "Downloading an app update…" });
            const prepared = await downloadVersion(candidate);
            preparedInstance = prepared.instance;
            updateState({ kind: "opening", build_id: candidate.resolved_build_id, message: "Checking access and opening the app update…" });
            const preparedGrants = structuredClone(selections[candidate.id] ?? []);
            const preparation = controls.catch(() => {}).then(async () => {
                if (closed) return;
                resumeReplication = Boolean(replicas);
                await replicas?.stop();
                await replicationTask;
                if (closed) return;
                self.postMessage({ replication: { kind: "catching_up" } });
                // AP admission and detached database preparation never hold the
                // guest queue. The swap below independently checks current state.
                await openSession(candidate, localStep);
                for (const volume of [profile.root]) {
                    await syncDatabase(volume, "index", "pull", candidate.resolved_build_id, localStep);
                    await syncDatabase(volume, "revocations", "pull", candidate.resolved_build_id, localStep);
                }
                await pullPersonal(candidate, preparedGrants, localStep);
                guestUpdating = true;
                await drainGuest(work);
                await localStep(async () => {
                    const latest = await personalSelections(profile.account);
                    if (JSON.stringify(latest[candidate.id] ?? []) !== JSON.stringify(preparedGrants)
                        || JSON.stringify(activeGrants) !== JSON.stringify(preparedGrants)) {
                        throw new Error("Personal volume access changed while preparing the update. Review its access again");
                    }
                    selections = latest;
                    adoptSession(candidate, true);
                    hostOutput(host.exports.pb_finish_adoption(1));
                    profile.session = candidate;
                    profile.app_name = prepared.name; profile.resolution = resolution;
                    await saveProfile();
                    // The old owner has drained. A candidate trap retains the new
                    // admission and never resumes or replays the old app's input.
                    await startGuest(prepared.instance);
                    updateState(null);
                    self.postMessage({ adopted: { ...presentation(), build_id: candidate.resolved_build_id } });
                });
            });
            controls = preparation;
            await preparation;
        } catch (failure) {
            if (!closed) await localStep(async () => {
                updateState({ kind: "failed", build_id: candidate?.resolved_build_id, message: `Update needs attention: ${(failure as Error).message}` });
                await saveProfile();
            });
        }
        finally {
            const slot = preparedInstance && guestBundles.get(preparedInstance);
            if (slot !== undefined) { host.exports.pb_package_discard(slot); guestBundles.delete(preparedInstance!); }
        }
    })().finally(() => {
        guestUpdating = false;
        if (surfaceDeferred) {
            surfaceDeferred = false;
            if (!closed && !closing) self.postMessage({ surfaceReady: true });
        }
    }).catch(failure => { if (!closed) updateState({ kind: "failed", message: `Update state could not be saved: ${failure.message}` }); })
        .finally(() => {
            updateTask = null;
            if ((resumeReplication || recoveryPending) && !closed) void connectReplication();
            if (retryPending) checkUpdate();
        });
}
let loadingHost: Promise<HostInstance> | undefined;
async function loadHost(): Promise<void> {
    if (!host) {
        loadingHost ??= fetch("./playbit.wasm")
            .then(response => response.arrayBuffer())
            .then(bytes => WebAssembly.instantiate(bytes, { opfs: storageImports() }))
            .then(result => result.instance as HostInstance);
        host = await loadingHost;
    }
}
async function readScopeAddresses(request: ScopeRequest): Promise<ScopeAddresses> {
    await loadHost();
    const bytes = encoder.encode(JSON.stringify(request));
    hostInput(bytes);
    return JSON.parse(decoder.decode(hostOutput(host.exports.pb_scope_addresses(bytes.length))));
}
async function spool(name: string): Promise<FileSystemSyncAccessHandle> {
    return (await directory.getFileHandle(name, { create: true })).createSyncAccessHandle();
}
async function openStorageResources(): Promise<OpfsResources> {
    const writerSpools: FileSystemSyncAccessHandle[] = [], stagingSpools: FileSystemSyncAccessHandle[] = [];
    try {
        writerSpools.push(await spool("transaction-0.tmp")); writerSpools.push(await spool("transaction-1.tmp"));
        stagingSpools.push(await spool("replica-entry.tmp")); stagingSpools.push(await spool("replica-journal.tmp"));
        return new OpfsResources(writerSpools, stagingSpools);
    } catch (error) { for (const file of [...writerSpools, ...stagingSpools]) file.close(); throw error; }
}
// One tab owns the account-only cache. Other recovery lists receive the same
// IndexedDB projection notices and can take over when the owner leaves.
function watchRecoveryList(account: string): void {
    if (!validId(account, 16) || profile || closed || closing) throw Error('Invalid recovery account');
    if (recoveryList) {
        if (recoveryList.account !== account) throw Error('Recovery account changed');
        return;
    }
    const stop = new AbortController();
    const retired = new Promise<void>(resolve => stop.signal.addEventListener('abort', () => resolve(), { once: true }));
    const task = navigator.locks.request(`playbit-recovery-list:${account}`, { signal: stop.signal }, async () => {
        try {
            const origin = await navigator.storage.getDirectory();
            directory = await origin.getDirectoryHandle(`playbit-recovery-list.${account}`, { create: true });
            await admitWebProfile(directory);
            storageResources = await openStorageResources();
            await loadHost();
            setScopeAddresses(readScopeAddresses);
            if (!await openAccountReplica(localStep, account, replicaId())) throw Error('Unlock private credentials before synchronizing');
            if (!stop.signal.aborted) void connectReplication();
            await retired;
        } finally {
            await replicas?.stop(); await replicationTask;
            closeStorage();
        }
    }).catch(error => {
        if (!stop.signal.aborted) replicationState({ kind: 'failed', message: String(error.message ?? error) });
    });
    recoveryList = { account, stop, task };
}
async function open(args: RuntimeOpenArgs) {
    if (windowObjectId === 0xffff_ffff) throw new Error("Guest window IDs exhausted");
    windowObjectId++;
    if (recoveryList) throw Error('The recovery list must close before opening a guest');
    const started = performance.now();
    const installation = await installationIdentity();
    progress("Opening local session storage…");
    if (!/^[a-zA-Z0-9_-]{1,80}$/.test(args.profile)) throw new Error("Invalid web profile name");
    await new Promise<void>((resolve, reject) => {
        navigator.locks.request(`playbit-runtime:${args.profile}`, { ifAvailable: true }, lock => {
            if (!lock) { reject(new Error("This session is already open in another tab. Return to that tab")); return; }
            resolve(); return new Promise<void>(done => { releaseLock = done; });
        }).catch(reject);
    });
    directory = await (await navigator.storage.getDirectory()).getDirectoryHandle(`playbit-${args.profile}`, { create: true });
    const sharedFormat = await admitWebProfile(directory);
    const profileFile = await (await directory.getFileHandle("profile.json", { create: true })).createSyncAccessHandle();
    try {
        if (profileFile.getSize()) {
            const bytes = new Uint8Array(profileFile.getSize()); profileFile.read(bytes);
            try { profile = JSON.parse(decoder.decode(bytes)); }
            catch { throw new Error("Corrupt local profile"); }
        }
    } finally { profileFile.close(); }
    if (profile?.session.selector.kind === "app") throw failure("unsupported", "App-following sessions are not supported in the browser yet. Open this app in Playbit.");
    managed = Boolean(args.invitation);
    if (profile && managed !== Boolean(profile.seed)) throw new Error("This profile belongs to a different session");
    personalCatalog = args.personal ?? [];
    creationScope = args.creationScope;
    boundUser = args.userVolume;
    let local;
    if (managed) {
        // `managed` is the invitation's presence, which names `args.account`; retainedInvitation always sets `parsed.invitation`.
        setScopeAddresses(readScopeAddresses);
        const parsed = await retainedInvitation(args.invitation!, selectedChannel);
        const invitation = parsed.invitation!;
        if (invitation.session.selector.kind === "app") throw failure("unsupported", "App-following sessions are not supported in the browser yet. Open this app in Playbit.");
        if (profile && (profile.account !== args.account || profile.session.id !== invitation.session.id)) throw new Error("Saved session belongs to a different account");
        const replica = replicaCapability(invitation);
        profile ??= { account: args.account!, seed: invitation.seed, session: invitation.session, root: replica.volume,
            root_credentials: replica.credentials, root_key: replica.credentials.database_key, replica_ids: Array.from({ length: 2 }, replicaId) };
        if (profile.root.id !== replica.volume.id) throw new Error("Saved session replica differs from the invitation");
        // Bind guest session information to the invitation validated for this admission.
        profile.seed = invitation.seed;
        profile.account_replica_id ??= new DataView(random(4).buffer).getUint32(0, true);
        proof = await sessionProof(parsed.privateKey, args.account!);
        if (!sharedFormat && profile.session.shared_volume) await admitWebProfile(directory, true);
        await saveProfile();
    }
    storageResources = await openStorageResources();
    publication = await VolumePublication.open(await spool('volume-generations.kvdb'), storageResources, () => encodeId(random(16)), yieldPreparation);
    await publication.discardPending(directory);
    const initialization = !managed && profile ? publication.initialization(profile.root.id) : null;
    progress("Loading the web runtime…");
    // Wasm exports are untyped; HostExports names the ones the runtime host provides.
    await loadHost();
    console.info("Playbit web", decoder.decode(hostOutput(host.exports.pb_version())));
    let prepared;
    if (!managed) {
        const name = args.app === "note" ? "shared-note" : args.app === "kart" ? "micro-kart" : "todo";
        let sessionId = profile?.session.id;
        if (!sessionId) {
            const pair = await crypto.subtle.generateKey("Ed25519", true, ["sign", "verify"]);
            sessionId = encodeId(new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey)));
        }
        prepared = initialization && !initialization.complete
            ? await prepareBundle(undefined, sessionId, profile.session.app, initialization.build, initialization.storage, profile.initializer_admission)
            : await prepareBundle(fetchPackage(`./${name}.pbapp`), sessionId, profile?.session.app);
        if (prepared.app.app_id !== name) throw new Error("The saved profile belongs to another app");
        if (!profile) {
            const account = encodeId(random(16));
            // An example without a Session declaration gets an explicit sandbox
            // Root. Separate profiles never masquerade as one shared World.
            const replica = prepared.shared ?? encodeId(random(16));
            const credentials = { database_key: Array.from(random(32)), blob_key: Array.from(random(32)) };
            profile = { account, root: { id: replica, owner: account, name: prepared.shared ? "Shared" : "Root", share_key_with_service: true, personal: false },
                session: { id: sessionId, app: prepared.app, selector: { kind: "exact", build_id: prepared.build }, resolved_build_id: prepared.build,
                    root_volume: prepared.shared ? prepared.world : replica, ...(prepared.shared ? { shared_volume: prepared.shared } : {}), creator: account, update_authority: "creator" },
                root_key: credentials.database_key, root_credentials: credentials, replica_ids: Array.from({ length: 2 }, replicaId) };
        }
        // This save precedes the journal's first seed writes. A pending initializer
        // can re-admit its exact bytes even after the website rotates its signer.
        if (!initialization) profile.initializer_admission = prepared.example;
        profile.app_name = prepared.name;
        profile.session.selector = { kind: "exact", build_id: prepared.build };
        profile.session.resolved_build_id = prepared.build;
        if (!sharedFormat && profile.session.shared_volume) await admitWebProfile(directory, true);
        await saveProfile();
    }
    const initialRoot = await openVolumeControls(profile.root.id, 0);
    if (!managed) {
        if (!initialRoot && !initialization) throw new Error('Local initialization record is missing');
        if (initialRoot && initialization?.complete) throw new Error('Initialized volume has no published controls');
        if (!initialization?.complete) {
            if (!initialization) publication.beginInitialization(profile.root.id, prepared!.build, prepared!.storage);
            local = prepared!.initializer;
        }
    }
    if (managed) await openAccountReplica();
    if (managed && !profile.app_name) {
        progress("Resolving the invited app version…");
        await ap("connect");
        const resolution = (await ap("request", { op: "resolve_session", operation_id: encodeId(random(16)), session_id: profile.session.id, proof })).data as SessionResolution;
        profile.session = resolvedSession(profile.session, resolution, profile.resolution);
        profile.resolution = resolution;
    }
    const bytes = encoder.encode(JSON.stringify({ ...profile, installation, local, user_volume: creationScope?.user_volume, creation_build: creationScope?.creation_build }));
    hostInput(bytes); hostOutput(host.exports.pb_initialize(bytes.length));
    if(managed){const status=await ap("connect");channelDevice=Uint8Array.from(status.instanceId??[]);}
    finishInitialVolume(initialRoot);
    await reclaimVolume(profile.root.id);
    blobs = initializeBlobs();
    await blobs.open();
    files = new FileBackend({ host: () => host.exports, directory: () => directory, blobs, hash: blobHash,
        step: localStep, admit: authorizedStep, authority: accountAuthority, yield: fileYield, published: () => replicas?.notify() });
    await files.initialize();
    if (local) for (const seed of local.databases) {
        await openNamed(profile.root, seed.id);
        const bytes = encoder.encode(JSON.stringify(seed)); hostInput(bytes); hostOutput(host.exports.pb_seed_database(bytes.length));
    }
    if (local) for (const seed of local.blobs) {
        const bytes = encoder.encode(JSON.stringify(seed.id)); hostInput(bytes);
        const reference: FileReference | null = JSON.parse(decoder.decode(hostOutput(host.exports.pb_seed_blob_reference(bytes.length))));
        if (!reference) continue;
        const status = await blobs.initialize(reference, Uint8Array.from(seed.bytes), prepared => {
            const bytes = encoder.encode(JSON.stringify(prepared)); hostInput(bytes);
            return host.exports.pb_seed_blob(bytes.length);
        });
        hostOutput(status);
    }
    if (local) publication.finishInitialization(profile.root.id, prepared!.build);
    if (managed) {
        progress("Checking session access…");
        await ap("connect");
        await openSession(profile.session);
        progress("Downloading app…");
        prepared = await downloadVersion(profile.session);
        profile.app_name = prepared.name;
        await synchronize(true);
        adoptSession(profile.session);
    }
    if (managed) {
        progress("Restoring selected personal volumes…");
        selections = await personalSelections(profile.account);
        const deleted = await refreshAccountProfile();
        const grants = (selections[profile.session.id] ?? []).filter(grant => !deleted.has(grant.volume));
        await preparePersonal(profile.session, grants);
        selectPersonal(profile.session.id, grants);
    }
    const initialized = performance.now();
    progress(`Opening ${profile.app_name || "app"}…`);
    await startGuest(prepared!.instance);
    if (managed) { await saveProfile(); self.postMessage({ update: profile.update ?? null }); }
    return { ...presentation(), identity: { account: profile.account, root: profile.session.root_volume, shared: profile.session.shared_volume,
        session: profile.session.id, version: profile.session.resolved_build_id },
        timing: { databaseMs: initialized - started, launchMs: performance.now() - initialized },
        storageBytes: Array.from(stores.values()).reduce((sum, store) => sum + store.end, 0) };
}

async function openAccountReplica(step: Step = callback => callback(), account = profile.account, replica = profile.account_replica_id): Promise<boolean> {
    await requireAccountAuthority(account);
    if (accountReplica) return false;
    if (accountOpening) { await accountOpening; return false; }
    accountOpening = (async () => {
        const candidate = await AccountReplica.open(account, (name, bytes, current) => localStep(() => {
            if (closed || closing || !current()) throw Error('Account replica retired');
            if (bytes) hostInput(bytes);
            return hostOutput((host.exports[name] as HostCall)(...(bytes ? [bytes.length] : []))); // pb_account_* exports.
        }));
        if (!candidate) return false; // Invitation-only access keeps private credentials locked.
        const store = await openStore(await (await directory.getFileHandle('account.kvdb', { create: true })).createSyncAccessHandle());
        try {
            await step(() => {
                if (closed || closing) throw Error('Account replica retired');
                stores.set(0, store);
                const input = encoder.encode(JSON.stringify({ account, replica_id: replica }));
                hostInput(input); hostOutput(host.exports.pb_account_open(input.length));
                accountReplica = candidate;
            });
        } catch (error) { candidate.close(); stores.delete(0); store.close(); throw error; }
        return true;
    })();
    try { return await accountOpening; } finally { accountOpening = undefined; }
}
async function accountChanged() {
    if (!managed || closed || closing) return;
    try { await requireAccountAuthority(profile.account); }
    catch (error) { pauseAccountActivity(); throw error; }
    if (await openAccountReplica(localStep)) await replicas?.stop();
    const previousName = displayName;
    await refreshAccountProfile();
    if (previousName !== displayName && accountNamePublication) queueAccountName(accountNamePublication.current);
    replicas?.notify();
    void connectReplication();
}

function localStep<T>(callback: () => T | Promise<T>): Promise<T> {
    const next = work.catch(() => {}).then(() => {
        if (closed) throw new Error("Runtime is closed");
        requireStorageFormat();
        return callback();
    });
    // The queue tracks completion; the caller receives the operation's error.
    // A reported control error must not poison the later guest-close fence.
    work = next.catch(() => {});
    return next;
}
let offlineReading = false;
function offlineError(error: unknown): string {
    switch ((error as { hostStatus?: number }).hostStatus) {
        case -1: return 'The saved entry could not be found. Reload the saved edits.';
        case -2: return 'Your current permissions do not allow this recovery.';
        case -3: return 'Check the new entry name and try again.';
        case -5: return 'The saved data is not available right now. Try again after synchronization finishes.';
        case -9: return 'This recovery exceeds the available storage or memory limit.';
        case -10: return 'That name is already in use, or the volume changed. Choose another name or reload the saved edits.';
        default: return String((error as Error).message ?? error);
    }
}
let recoveryPublication: Promise<void> | undefined;
function publishRecoveredEntry(): void {
    if (!managed || recoveryPublication || closed || closing) return;
    // A new backing needs an initial full transfer. Reuse connection bootstrap;
    // the local reply does not wait for AP or discard other entries' queued edits.
    recoveryPublication = controls.catch(() => {}).then(async () => {
        await replicas?.stop(); await replicationTask;
    }).finally(() => {
        recoveryPublication = undefined;
        if (!closed && !closing) void connectReplication();
    });
    controls = recoveryPublication;
}
async function offlineRestore(request: Extract<RuntimeRequest, { method: 'offline_restore' }>): Promise<void> {
    if (offlineReading) {
        self.postMessage({ id: request.id, error: 'An offline edits operation is already running' }); return;
    }
    offlineReading = true;
    const attached = new Map<number, OpfsStore>();
    let destination: FileSystemDirectoryHandle | undefined, filename: string | undefined;
    let publishing = false, committed = false, started = false;
    let releaseBlob: (() => void) | undefined;
    try {
        if (!publication || !storageResources || closing || closed) throw new Error('Storage is not ready');
        const { volume, cut: generation, entry, name } = request.args;
        const epoch = publication.epoch(volume), current = publication.current(volume);
        let revision = epoch.capture();
        if (!current) throw new Error('Volume is unavailable');
        const check = () => {
            if (closing || closed) throw new Error('Offline edits recovery was canceled');
            epoch.check(revision);
        };
        const call = <T>(operation: (length: number) => number, value: unknown): T => {
            check(); const bytes = encoder.encode(JSON.stringify(value)); hostInput(bytes);
            const output = hostOutput(operation(bytes.length));
            return JSON.parse(output.length ? decoder.decode(output) : 'null');
        };
        const cut = await localStep(() => call<RecoveryCut>(host.exports.pb_recovery_cut_get, { volume, cut: generation }));
        const attach = (store: OpfsStore): number => {
            try { check(); const slot = allocateBackingSlots(1); attached.set(slot, store); stores.set(slot, store); return slot; }
            catch (error) { store.close(); throw error; }
        };
        const slot = attach(await openRecoveryIndex(directory, publication, volume, cut, storageResources, blobHash, yieldPreparation, check));
        const prepared = await localStep(() => {
            const value = call<{ entry: string; blob: FileReference | null; database: boolean }>(host.exports.pb_recovery_begin, { volume, cut: generation, entry, name, slot });
            started = true; return value;
        });
        if (prepared.blob) {
            releaseBlob = blobs.retain(prepared.blob);
            const file = await openRecoveryFile(directory, publication, volume, cut, `${prepared.blob.digest}.blob`, blobHash, yieldPreparation, check);
            try {
                await blobs.restore(file, prepared.blob, check, write => {
                    // Only this synchronous receipt mutation may advance our
                    // fence. Never absorb writes across an asynchronous wait.
                    check(); write(); revision = epoch.capture();
                });
            } finally { file.close(); }
        } else if (prepared.database) {
            const source = attach(await openRecoveryDatabase(directory, publication, volume, cut, `${entry}.kvdb`, storageResources, blobHash, yieldPreparation, check));
            destination = await directory.getDirectoryHandle(publication.directory(volume, current));
            const freshName = `${prepared.entry}.kvdb`;
            // Never truncate a preexisting orphan or collision.
            try { await destination.getFileHandle(freshName); throw new Error('Recovery backing already exists'); }
            catch (error) { if ((error as DOMException).name !== 'NotFoundError') throw error; }
            check(); filename = freshName;
            const target = attach(await openStore(await (await destination.getFileHandle(filename, { create: true })).createSyncAccessHandle()));
            await localStep(() => call(host.exports.pb_recovery_backing, { source, destination: target, replica_id: replicaId() }));
            for (;;) {
                const complete = await localStep(() => { check(); return JSON.parse(decoder.decode(hostOutput(host.exports.pb_recovery_copy()))) as boolean; });
                if (complete) break;
                await yieldPreparation();
            }
        }
        const restored = await localStep(() => {
            check(); publishing = true;
            const value = call<string>(host.exports.pb_recovery_publish, { generation: current, blob: prepared.blob });
            committed = true; started = false; return value;
        });
        self.postMessage({ id: request.id, result: { entry: restored } });
        publishRecoveredEntry();
        void localStep(refreshScene).catch(error => self.postMessage({ guestFailure: String(error.message ?? error) }));
    } catch (error) {
        self.postMessage({ id: request.id, error: offlineError(error) });
    } finally {
        releaseBlob?.();
        if (started && !closed) await localStep(() => host.exports.pb_recovery_abort()).catch(() => {});
        for (const [slot, store] of attached) { store.close(); stores.delete(slot); }
        // A failed commit can have reached durable storage. Retain its body for
        // reopen rather than removing data that the persistent index may name.
        if (!committed && !publishing && destination && filename) {
            await destination.removeEntry(filename).catch(error => { if (error.name !== 'NotFoundError') console.error('Could not remove unpublished recovery backing', error); });
        }
        offlineReading = false;
    }
}
async function offlineEntries(request: Extract<RuntimeRequest, { method: 'offline_entries' }>): Promise<void> {
    if (offlineReading) {
        self.postMessage({ id: request.id, error: 'Offline edits are already being read' });
        return;
    }
    offlineReading = true;
    let slot: number | undefined;
    try {
        if (!publication || !storageResources || closing || closed) throw new Error('Storage is not ready');
        const { volume, after } = request.args;
        const epoch = publication.epoch(volume), revision = epoch.capture();
        const check = () => {
            if (closing || closed) throw new Error('Offline edits read was canceled');
            epoch.check(revision);
        };
        const call = <T>(name: 'pb_recovery_cut' | 'pb_recovery_entries', value: unknown): T => {
            check();
            const bytes = encoder.encode(JSON.stringify(value)); hostInput(bytes);
            return JSON.parse(decoder.decode(hostOutput(host.exports[name](bytes.length))));
        };
        const cut = await localStep(() => call<RecoveryCut | null>('pb_recovery_cut', { volume, after }));
        const entries: OfflineEntry[] = [];
        if (cut) {
            let cursor: number | null = null;
            do {
                const store = await openRecoveryIndex(directory, publication, volume, cut, storageResources,
                    blobHash, yieldPreparation, check);
                try {
                    slot = allocateBackingSlots(1); stores.set(slot, store);
                    const page = await localStep(() => call<{ entries: OfflineEntry[]; next: { cut: string; offset: number } | null }>(
                        'pb_recovery_entries', { volume, cut: cut.generation, slot, after: cursor }));
                    if (entries.length + page.entries.length > 128 || page.next && (page.next.cut !== cut.generation
                        || page.next.offset <= (cursor ?? -1) || page.next.offset >= 128)) throw new Error('Invalid offline edits page');
                    entries.push(...page.entries); cursor = page.next?.offset ?? null;
                } finally { store.close(); if (slot !== undefined) stores.delete(slot); slot = undefined; }
            } while (cursor !== null);
        }
        await localStep(() => {
            call('pb_recovery_cut', { volume, after });
            self.postMessage({ id: request.id, result: { entries, next: cut?.generation ?? null } });
        });
    } catch (error) {
        self.postMessage({ id: request.id, error: offlineError(error) });
    } finally { offlineReading = false; }
}
async function refreshScene() {
    if (engineFailure || guestFailure || closing || guestUpdating || guestWork.draining) return;
    dataRevision++;
    operations.refreshVolumes();
    guestWork.queue({ kind: 4, target: 0, data: new Uint8Array() });
    await enterGuest();
    self.postMessage({ replicaScene: presentation() });
}
function liveOutput(length: number) {
    try { return hostOutput(length); } catch (error) {
        if (["local changes require gap recovery", "database catalog changed during gap recovery"].includes((error as Failure).message)) (error as Failure).code = "replica-gap";
        throw error;
    }
}
// One bounded retry timer per owner, only after a recoverable failure.
// Local input keeps its own queue; steady-state replication never polls.
function scheduleRecovery() {
    if (closed || closing || replicationBlocked || recoveryTimer || !recoveryPending) return;
    const delay = recoveryDelay;
    recoveryDelay = Math.min(recoveryDelay * 2, 30000);
    recoveryTimer = setTimeout(() => {
        recoveryTimer = undefined;
        if (replicas?.run && !replicas.run.stopped) void recoverSubscriptions();
        else void connectReplication();
    }, delay);
}
// Open can fail before a subscription exists to report its own failure.
function catalogRecoveryFailed(id: string, error: unknown): void {
    if (closing || closed) return;
    const message = String(error).slice(0, 512);
    logs.push(message); if (logs.length > 256) logs.shift();
    if (retryable(error) || automaticSlots.has(id)) {
        replicationState({source: id, kind: retryable(error) ? 'reconnecting' : 'failed',
            code: (error as Failure).code, message, stage: 'subscription'});
    }
}
async function recoverSubscriptions(): Promise<void> {
    for (const [source, state] of [...scopeRecoveries]) {
        if (state !== 'retry' || closed || closing || replicationBlocked) continue;
        const { volume, entry } = replicaAddress(source);
        if (openingMounts.has(volume)) continue;
        try {
            if (entry) await connectEntry(replicaRecord(volume), entry);
            else if (automaticSlots.has(volume)) await captureAutomaticCatalog(replicaRecord(volume));
            else if (volume === unmountedWorld) await prepareAutomatic({role: 'root', volume}, true);
            else { scopeRecoveries.delete(source); continue; }
            scopeRecoveries.delete(source);
            await localStep(refreshScene);
        } catch (error) {
            if (!entrySubscriptions.has(source) && !automaticSlots.has(source) && !(source === unmountedWorld && retryable(error))) scopeRecoveries.delete(source);
            else scopeRecoveries.set(source, retryable(error) ? 'retry' : 'blocked');
        }
    }
    recoveryPending = [...scopeRecoveries.values()].includes('retry');
    scheduleRecovery();
}
function replicationState(value: ReplicationState) {
    if (closed || closing) return;
    if (value.source) {
        if (!entrySubscriptions.has(value.source) && !automaticSlots.has(value.source) && value.source !== unmountedWorld) return;
        if (value.kind === 'live') scopeRecoveries.delete(value.source);
        else scopeRecoveries.set(value.source, value.kind === 'reconnecting' ? 'retry' : 'blocked');
        recoveryPending = [...scopeRecoveries.values()].includes('retry');
        if (recoveryPending) scheduleRecovery();
        self.postMessage({ replication: value });
        return;
    }
    if (value.kind === 'live') {
        recoveryDelay = 250; recoveryPending = [...scopeRecoveries.values()].includes('retry');
        if (scopeRecoveries.size) value = { ...value, kind: 'catching_up' };
    }
    if (value.kind === 'reconnecting') { recoveryPending = true; scheduleRecovery(); }
    if (value.kind === 'failed' || value.kind === 'action_required') {
        replicationBlocked = true; recoveryPending = false;
        clearTimeout(recoveryTimer); recoveryTimer = undefined;
    }
    self.postMessage({ replication: value });
}
let channelRecovery:Promise<void>|undefined;
function recoverChannels():Promise<void>|undefined {
    if(!managed||closed||closing||!channelClient)return;
    if(channelRecovery)return channelRecovery;
    channelRecovery=(async()=>{
        const connection=await ap("connect");
        if(!closed&&!closing)await channelClient?.reconnect(Uint8Array.from(connection.instanceId??[]),connection.connectionCount);
    })().catch(()=>{}).finally(()=>{channelRecovery=undefined;});
    return channelRecovery;
}
function queueAccountName(current: () => boolean): void {
    if (!current()) return;
    const run = accountNamePublication ??= { current, dirty: false, unsupported: false, delay: 250 };
    if (run.unsupported) return;
    run.dirty = true;
    void publishAccountName();
}
// Only the active attempt reads the durable winner. Retry and idle state retain no envelope.
async function publishAccountName(): Promise<void> {
    const run = accountNamePublication;
    if (!run || !run.dirty || run.retry !== undefined || publishingAccountName || !run.current()) return;
    publishingAccountName = true; run.dirty = false;
    try {
        const record = await accountReplica!.displayNameRecord(run.current);
        if (!record || !run.current() || run.dirty) return;
        const digest = await crypto.subtle.digest('SHA-256', record.ciphertext);
        const version = record.clock + '/' + base64url(new Uint8Array(digest));
        if (!run.current() || run.dirty || record.name !== displayName || run.reported === version) return;
        await ap('request', { op: 'put_account_display_name', operation_id: encodeId(random(16)),
            name: record.name, record_key: encodeId(record.key), clock: String(record.clock), ciphertext: base64url(record.ciphertext) });
        run.reported = version; run.delay = 250;
    } catch (error) {
        if (!run.current()) return;
        const cause = error as { code?: string; serviceCode?: string };
        if (cause.code === 'ap-rejected' && cause.serviceCode === 'unsupported_operation') {
            run.unsupported = true; run.dirty = false;
        } else {
            run.dirty = true;
            run.retry = setTimeout(() => { run.retry = undefined; void publishAccountName(); }, run.delay);
            run.delay = Math.min(run.delay * 2, 30000);
        }
    } finally {
        publishingAccountName = false;
        if (accountNamePublication?.dirty) void publishAccountName();
    }
}
function connectReplication() {
    if ((!managed && !(recoveryList && accountReplica)) || closed || closing || replicationBlocked || accountAuthority()) return;
    if (replicationTask || updateTask || personalTask) { recoveryPending = true; return replicationTask; }
    if (replicas?.run && !replicas.run.stopped) return;
    clearTimeout(recoveryTimer); recoveryTimer = undefined; recoveryPending = false;
    let ready!: () => void;
    const starting = { volumes: [] as string[], ready: new Promise<void>(resolve => { ready = resolve; }) };
    startingReplicas = starting;
    replicationTask = (async () => {
        const connection=await ap("connect");
        if (closed || closing) return;
        if(channelClient)await channelClient.reconnect(Uint8Array.from(connection.instanceId??[]),connection.connectionCount);
        if (!recoveryList) await openSession(profile.session);
        if (closed || closing) return;
        // The account stream, and so every account ticket, exists only once accountReplica is open.
        replicas ??= new LiveReplication<LiveTicket>({ ap, operationId: () => encodeId(random(16)),
            state: replicationState,
            bootstrap: async current => {
                const step = replicaStep(current);
                if (recoveryList) {
                    await syncDatabase(null, "index", 'pull', undefined, step, current);
                    await accountReplica!.adoptTree(current);
                    return;
                }
                // Every subsequent write remains captured, even when its data
                // also happens to enter one of the outbound snapshots below.
                await localStep(() => {
                    hostOutput(host.exports.pb_database_retry()); blobs.retry();
                });
                const volumes = startingReplicas!.volumes;
                const hasWorld = volumes.some(projectedWorld);
                let complete = true;
                for (const volume of volumes) {
                    if (projectedWorld(volume)) continue;
                    if (!await catalogReplicaAllowed(volume, step) && replicaVolumeCurrent(volume)) complete = false;
                }
                // Keep healthy World tickets. An overflow has already emptied the feed;
                // start tracking new edits before snapshots and the subscribed World
                // catch-up replay the durable pre-gap sources through their own routes.
                if (complete) await step(() => hostOutput(host.exports.pb_live_rebase(Number(hasWorld))));
                await synchronize(false, step, volumes, current);
                if (accountReplica) {
                    await syncDatabase(null, "index", 'pull', undefined, step, current);
                    await accountReplica.adoptTree(current);
                    await step(() => refreshPersonal());
                    await accountReplica.stagePending(current);
                    await syncDatabase(null, "index", 'push', undefined, step, current);
                    queueAccountName(current);
                }
                await step(refreshScene);
            },
            local: {
                catchUpLog,
                volumeCurrent: replicaVolumeCurrent,
                retired: () => {
                    clearTimeout(accountNamePublication?.retry); accountNamePublication = undefined;
                    if (activeDatabaseStream && !activeDatabaseStream.current()) void activeDatabaseStream.close();
                },
                terminal: (source, terminal, current) => localStep(() => {
                    if (!current()) return;
                    const { volume, entry } = replicaAddress(source), offset = entry ? 31 : 16;
                    if (projectedWorld(volume) && terminal.volume === volume && terminal.cause === 2) {
                        withdrawWorld(volume, true);
                        return;
                    }
                    const bytes = new Uint8Array(offset + 17);
                    bytes.set(decodeId(volume));
                    if (entry) bytes.set(decodeId(entry, 15), 16);
                    bytes.set(decodeId(terminal.volume), offset);
                    bytes[offset + 16] = terminal.cause;
                    hostInput(bytes); hostOutput(host.exports.pb_live_terminal(bytes.length));
                }),
                head: async current => {
                    if (accountAuthority()) throw unsupportedAccount();
                    if (recoveryList) return null;
                    accountTurn = !accountTurn;
                    if (accountTurn) {
                        const head = await accountReplica?.head(current);
                        if (head) return head;
                    }
                    const head = await localStep(() => {
                        if (!current()) return null;
                        const length = host.exports.pb_live_peek();
                        if (!length) return null;
                        const bytes = liveOutput(length), view = new DataView(bytes.buffer);
                        const source = encodeId(bytes.subarray(0, 16));
                        const world = projectedWorld(source) ? automaticSlots.get(source) : undefined;
                        if (bytes.length < 41 || bytes[40] !== 0 && !(bytes[40] === 1 && world && bytes.length === 41)) {
                            throw new Error('Invalid local publication frame');
                        }
                        if (bytes[40] === 1 && !scopeRecoveries.has(source)) replicas!.recoverVolumeCatalog(source);
                        const kind = 'volume' as const;
                        return { source, publication: { kind, volume_id: source,
                            session_id: personalReplica(source) ? null : profile.session.id, build_id: personalReplica(source) ? null : profile.session.resolved_build_id },
                            database: databaseIdentity(bytes.subarray(16, 32)),
                            ticket: { sequence: view.getBigUint64(32, true), world }, bytes: bytes.subarray(41) };
                    });
                    return head ?? (accountTurn ? null : accountReplica?.head(current));
                },
                published: async (head, current) => {
                    if (head.publication.kind !== 'volume') return;
                    const volume = head.publication.volume_id;
                    if (head.database !== 'index') {
                        const mount = !head.ticket.account && head.ticket.world;
                        if (!mount || head.database === 'revocations' || !current() || automaticSlots.get(volume) !== mount) return;
                        await settleWorldReceipt(mount, head.database, head.bytes, current);
                        return;
                    }
                    const versions: FileReference[] = await localStep(() => {
                        if (!current()) throw new Error('File publication retired');
                        const bytes = new Uint8Array(16 + head.bytes.length);
                        bytes.set(decodeId(volume));
                        bytes.set(head.bytes, 16); hostInput(bytes);
                        return JSON.parse(decoder.decode(hostOutput(host.exports.pb_blob_publication(bytes.length))));
                    });
                    for (const version of versions) {
                        if (!current()) throw new Error('File publication retired');
                        await blobs.uploadVersion(version);
                    }
                },
                rejected: (head, code, current) => localStep(() => {
                    const ticket = head.ticket;
                    if (ticket.account || !ticket.world) return false;
                    const world = ticket.world.volume.id;
                    if (!current() || automaticSlots.get(world) !== ticket.world) return true;
                    if (code === 'world-participant-revoked') {
                        withdrawWorld(world, true);
                    } else {
                        // Only named KV changes have a durable snapshot or append-birth
                        // replay. Discard this RAM ticket, then capture the current ACL.
                        if (head.database === 'index' || head.database === 'revocations') return false;
                        liveOutput(host.exports.pb_live_ack(ticket.sequence));
                        if (!scopeRecoveries.has(world)) replicas!.recoverVolumeCatalog(world);
                    }
                    return true;
                }),
                ack: (ticket, current) => ticket.account ? accountReplica!.acknowledge(ticket.account, current)
                    : localStep(() => {
                        if (current() && (!ticket.world || automaticSlots.get(ticket.world.volume.id) === ticket.world)) {
                            return liveOutput(host.exports.pb_live_ack(ticket.sequence));
                        }
                    }),
                apply: async (source, database, delta, current, evidence) => {
                    if (source === 'account') {
                        const foundName = await accountReplica!.apply(delta, current);
                        if (recoveryList) return;
                        const latest = await personalSelections(profile.account);
                        await localStep(async () => {
                            if (!current()) return;
                            await refreshPersonal(latest);
                            await refreshScene();
                        });
                        if (foundName) queueAccountName(current);
                        return;
                    }
                    const { volume, entry: selected } = replicaAddress(source);
                    const entry = database !== 'index' && database !== 'revocations';
                    const record = replicaRecord(volume);
                    if (entry) await openNamed(record, database);
                    return localStep(async () => {
                        if (!current()) return;
                        const prefix = selected ? 47 : 32;
                        const extra = evidence === undefined ? 0 : 4 + evidence.length;
                        if (evidence !== undefined && (!selected || database !== 'index')) throw new Error('Invalid namespace evidence scope');
                        const bytes = new Uint8Array(prefix + delta.length + extra);
                        bytes.set(decodeId(volume)); // Volume sources are validated identifiers.
                        if (selected) bytes.set(decodeId(selected, 15), 16);
                        bytes.set(databaseBytes(database), prefix - 16);
                        if (evidence !== undefined) {
                            new DataView(bytes.buffer).setUint32(prefix, delta.length, true);
                            bytes.set(delta, prefix + 4); bytes.set(evidence, prefix + 4 + delta.length);
                        } else bytes.set(delta, prefix);
                        hostInput(bytes);
                        const status = evidence !== undefined ? host.exports.pb_live_apply_namespace(bytes.length)
                            : selected ? host.exports.pb_live_apply_entry(bytes.length) : host.exports.pb_live_apply(bytes.length);
                        if (status === -6) throw Object.assign(new Error('Replica backing requires gap recovery'), { code: 'replica-gap' });
                        hostOutput(status);
                        await refreshScene();
                        if (database === "index") queueMicrotask(() => checkUpdate());
                    });
                },
            },
        });
        const volumes = recoveryList ? [] : replicatedVolumeIds();
        starting.volumes = volumes;
        const catalogs = [];
        for (const volume of volumes) {
            if (projectedWorld(volume)) continue;
            if (await catalogReplicaAllowed(volume, localStep)) catalogs.push(volume);
        }
        await replicas.start([...(accountReplica ? [{ kind: 'account' } satisfies ReplicaScope] : []), ...catalogs.filter(replicaVolumeCurrent).map((volume): ReplicaScope => ({ kind: 'volume', volume_id: volume,
            session_id: personalReplica(volume) ? null : profile.session.id,
            build_id: personalReplica(volume) ? null : profile.session.resolved_build_id }))]);
        ready();
        if (recoveryList) return;
        const reopening = unmountedWorld;
        if (reopening && !openingMounts.has(reopening) && replicas?.run?.active && !replicas.run.stopped) {
            try { await prepareAutomatic({ role: 'root', volume: reopening }, true); }
            catch (error) { catalogRecoveryFailed(reopening, error); }
        }
        for (const id of volumes) {
            const mount = automaticSlots.get(id);
            if (mount && !mount.preparing && id !== reopening && !catalogs.includes(id) && !mount.storage && !personalSlots.has(id) && replicas?.run?.active && !replicas.run.stopped) {
                try { await captureAutomaticCatalog(mount.volume); }
                catch (error) { catalogRecoveryFailed(id, error); }
            }
        }
        await restoreEntrySubscriptions();
        if (accountReplica) {
            void ap("onboarding", 6).then(() => ap("onboarding", 7)).catch(() => {});
        }
    })().catch(error => replicationState({ kind: retryable(error) ? 'reconnecting'
        : ['authentication-required', 'ap-rejected', 'access-revoked'].includes(error.code) ? 'action_required' : 'failed',
        message: String(error.message ?? error).slice(0, 512), code: error.code, stage: 'subscription' }))
        .finally(() => { ready(); startingReplicas = undefined; replicationTask = null; scheduleRecovery(); });
    return replicationTask;
}

function chooserCatalog(items: unknown): PersonalCatalogEntry[] {
    if (items === undefined) return personalCatalog;
    if (!Array.isArray(items) || items.length > 30) throw new Error("Personal volume capacity exceeded");
    let charge = 512;
    const seen = new Set();
    return items.map(item => {
        const volume = item?.volume, credentials = item?.credentials;
        if (!volume || volume.owner !== profile.account || [profile.root.id].includes(volume.id)
            || !validId(volume.id, 16) || seen.has(volume.id) || typeof volume.name !== "string"
            || typeof volume.share_key_with_service !== "boolean" || volume.personal !== true || ![credentials?.database_key, credentials?.blob_key]
                .every(key => Array.isArray(key) && key.length === 32 && key.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255))) {
            throw new Error("Invalid personal volume credentials");
        }
        charge += 1024 + volume.name.length * 2;
        if (charge > 32 * 1024) throw new Error("Personal volume chooser exceeds its memory limit");
        seen.add(volume.id);
        const previous = personalCatalog.find(old => old.volume.id === volume.id);
        if (previous && JSON.stringify(previous.credentials) !== JSON.stringify(credentials)) throw new Error("Personal volume credentials changed. Recover this account again");
        return { volume: { id: volume.id, owner: volume.owner, name: volume.name, share_key_with_service: volume.share_key_with_service, personal: true },
            credentials: { database_key: credentials.database_key.slice(), blob_key: credentials.blob_key.slice() } };
    });
}
// `args` is released once its catalog is validated, so its binding admits undefined.
async function choosePersonal({ id, args }: { id: number; args: RuntimeSelectPersonalArgs | undefined }) {
    if (personalTask) { self.postMessage({ id, error: "A personal volume connection is still finishing. Cancel it or wait before trying again" }); return; }
    const task: PersonalTask = { id, canceled: false, committing: false, replied: false };
    personalTask = task;
    let stoppedReplication = false, retained: PersonalGrant[] | undefined;
    let retainedSelection: PersonalSelection[] | undefined;
    try {
        if (!managed) throw new Error("Personal volumes require a signed-in session");
        const catalog = chooserCatalog(args!.personal), grants = validateSelection(args!.grants, catalog), session = args!.session;
        args = undefined;
        const check = () => {
            if (task.canceled) throw new Error("Personal volume connection canceled");
            if (closed || profile.session.id !== session) throw new Error("The session changed. Review its access again");
        };
        const prepare = await localStep(async () => {
            if (task.canceled) throw new Error("Personal volume connection canceled");
            check();
            personalCatalog = catalog;
            const latest = (await personalSelections(profile.account))[session] ?? [];
            retained = grants.flatMap(grant => {
                const active = activeGrants.find(item => item.volume === grant.volume), saved = latest.find(item => item.volume === grant.volume);
                return active && saved ? [{ volume: grant.volume, write: grant.write && active.write && saved.write }] : [];
            });
            // Persist withdrawals first. Neither cancellation nor failed additions
            // may restore access that this request removed.
            selections = await personalSelections(profile.account, session, retained);
            retainedSelection = selections[session];
            selectPersonal(session, retainedSelection!);
            await refreshScene();
            check();
            return JSON.stringify(retained) !== JSON.stringify(grants);
        });
        if (prepare) {
            progress("Connecting selected personal volumes…");
            const preparation = controls.catch(() => {}).then(async () => {
                check();
                stoppedReplication = true;
                self.postMessage({ replication: { kind: "catching_up" } });
                await replicas?.stop(); await replicationTask;
                check();
                const step: Step = (callback, cleanup = false) => localStep(() => { if (!cleanup) check(); return callback(); });
                await preparePersonal(profile.session, grants, step);
                await step(async () => {
                    validateCandidate(profile.session, grants);
                    task.committing = true;
                    // Compare and save in one IDB transaction. Another window's
                    // changed consent cannot be overwritten by this prepared result.
                    selections = await personalSelections(profile.account, session, grants, retainedSelection);
                    selectPersonal(session, selections[session] ?? []);
                    await refreshScene();
                });
            });
            controls = preparation;
            await preparation;
        }
        if (!task.replied) self.postMessage({ id, result: {} });
    } catch (error) {
        if (!task.replied) self.postMessage({ id, error: (error as Error).message });
    } finally {
        personalTask = undefined;
        if (stoppedReplication && !closed) void controls.catch(() => {}).then(() => connectReplication());
        else if (retained && !closed && !personalRetirement) {
            // Coalesce repeated withdrawals while the prior AP stream retires.
            // Their local acknowledgments do not wait for this network cleanup.
            personalRetirement = controls.catch(() => {}).then(async () => {
                self.postMessage({ replication: { kind: "catching_up" } });
                await replicas?.stop(); await replicationTask;
            })
                .finally(() => { personalRetirement = undefined; if (!closed) void connectReplication(); });
            controls = personalRetirement;
        }
    }
}
async function retireFiles(): Promise<void> {
    await localStep(() => {
        hostOutput(host.exports.pb_web_guest_reset());
        // Reset starts a new Rust work epoch. Fence its old stack before any
        // asynchronous cleanup can admit a queued notification on that epoch.
        if (guest?.deadline) {
            guest.deadline.retire();
            guest = undefined;
            channelClient?.close(); channelClient = undefined;
        }
    });
    await files.cleanup(); await blobs.cleanup();
}
function closeStorage() {
    operations.cancelWake();
    guest?.deadline?.retire();
    channelClient?.close();channelClient=undefined;
    entrySubscriptions.clear(); scopeRecoveries.clear();
    accountSignal.close();
    accountReplica?.close();
    blobs?.close();
    fileContinuation?.resume(); fileContinuation = undefined;
    fileReady?.port1.close(); fileReady?.port2.close(); fileReady = undefined;
    preparationContinuation?.resume(); preparationContinuation = undefined;
    preparationReady?.port1.close(); preparationReady?.port2.close(); preparationReady = undefined;
    clearTimeout(recoveryTimer); recoveryTimer = undefined; recoveryPending = false;
    if (host) { host.exports.pb_recovery_abort(); host.exports.pb_sync_finish(0); }
    storageBridge?.close(); storageBridge = undefined;
    for (const store of stores.values()) store.close(); stores.clear();
    for (const mount of automaticSlots.values()) mount.release?.();
    automaticSlots.clear();
    publication?.close(); publication = undefined;
    storageResources?.close(); storageResources = undefined;
    releaseLock?.(); releaseLock = undefined;
}
// Only called once startGuest has verified that the guest produced a scene.
function presentation() {
    return { scene: scene!.root, ...(scene!.window ? { window: scene!.window } : {}), logs: logs.splice(0) };
}
let work: Promise<unknown> = Promise.resolve(), controls: Promise<unknown> = Promise.resolve();
let initializing = false;
self.onmessage = ({ data }: MessageEvent<RuntimePageMessage>) => {
    if (engineFailure) return;
    if (data.apReply) {
        const pending = apPending.get(data.apReply.id);
        if (!pending) return;
        apPending.delete(data.apReply.id);
        if (pending.advisory) advisoryPending.delete(pending.advisory);
        if (data.apReply.error) pending.reject(Object.assign(new Error(data.apReply.error), { code: data.apReply.code, serviceCode: data.apReply.serviceCode })); else pending.resolve(data.apReply.result);
        if(pending.method!=='channelConsumed')flushConsumedChannels();
        return;
    }
    if (data.method === "reconnect") {
        void connectReplication()?.then(() => checkUpdate()); self.postMessage({ id: data.id, result: {} }); return;
    }
    if (data.method === 'account_changed') {
        void accountChanged().then(() => self.postMessage({ id: data.id, result: {} }), error => self.postMessage({ id: data.id, error: String(error.message ?? error) })); return;
    }
    if (data.method === "select_personal") { void choosePersonal(data); return; }
    if (data.method === 'offline_entries') { void offlineEntries(data); return; }
    if (data.method === 'offline_restore') { void offlineRestore(data); return; }
    if (data.method === "cancel_personal") {
        // `canceled` implies a task: it is computed from `task` itself.
        const task = personalTask, canceled = Boolean(task && !task.committing);
        if (canceled) task!.canceled = true;
        void localStep(() => {
            if (canceled && !task!.replied) { task!.replied = true; self.postMessage({ id: task!.id, result: { canceled: true } }); }
            self.postMessage({ id: data.id, result: { canceled } });
        }).catch(error => self.postMessage({ id: data.id, error: error.message }));
        return;
    }
    if (data.method === "input" && (closing || guestUpdating || guestWork.draining)) {
        self.postMessage({ id: data.id, error: "The app is finishing accepted work. This input was not accepted." });
        return;
    }
    if (data.method === "close") {
        const accepted = work;
        closing = true; clearTimeout(recoveryTimer); recoveryTimer = undefined; recoveryPending = false;
        if (recoveryList) {
            recoveryList.stop.abort();
            void recoveryList.task.then(() => { closed = true; self.postMessage({ id: data.id, result: { closed: true } }); });
            return;
        }
        if (personalTask && !personalTask.committing) personalTask.canceled = true;
        controls = controls.catch(() => {}).then(async () => {
            await replicas?.stop(); await replicationTask;
            try {
                await drainGuest(accepted);
                await retireFiles();
                await localStep(() => handle(data));
            } catch (error) {
                closing = false;
                self.postMessage({ id: data.id, error: String((error as Error).message ?? error) });
            }
            if (!closed) void connectReplication();
        });
        return;
    }
    let accepted: AcceptedInput | undefined;
    if (data.method === "input") {
        try { accepted = acceptInput(data.args); }
        catch (error) { self.postMessage({ id: data.id, error: String((error as Error).message ?? error) }); return; }
    }
    work = work.catch(() => {}).then(() => handle(data, accepted));
};
function validateNativeText(data: Uint8Array): void {
    hostInput(data);
    const status = host.exports.pb_web_validate_text_input(data.length);
    if (status < 0) throw Object.assign(new Error("Invalid native text edit"), { code: status });
}
let windowObjectId = 0;
function validateGuestInput(kind: number, target: number, data: Uint8Array, revision?: number): void {
    const legacyCanvas = scene?.window?.legacyCanvas;
    if (kind >= 22 && kind <= 26) {
        hostInput(data);
        const status = host.exports.pb_web_validate_surface_input(kind, data.length);
        if (status < 0) throw Object.assign(new Error("Invalid surface input"), { code: status });
        if (target && (kind !== 22 || (!status && scene?.controls.get(target)?.kind !== "canvas"))) {
            throw new Error("Invalid surface input target");
        }
        const mask = scene?.window?.input;
        if (mask === undefined || (!status && !(mask & (1 << (kind - 22))))) throw new Error("Input family is disabled");
    } else {
        if ((kind === 11 || kind === 12) && !legacyCanvas) throw new Error("Original canvas input is disabled");
        validateInput(scene!.controls, kind, target, data, validateNativeText, revision);
    }
}
function acceptInput(args: Extract<RuntimeRequest, { method: "input" }>["args"]): AcceptedInput {
    if (engineFailure) throw engineFailure;
    if (guestFailure) throw guestFailure;
    if (!guest || !scene) throw new Error("The app is stopped; reload the page to restart it");
    if (args.data.length > MAX_TEXT_EVENT_BYTES) throw new Error("Guest input exceeds the event limit");
    const data = new Uint8Array(args.data);
    validateGuestInput(args.kind, args.target, data, args.menuRevision);

    return { root: guestWork.input(data.length), event: { kind: args.kind, target: args.target, objectId: windowObjectId, data } };
}
async function handle({ id, method, args }: RuntimeRequest, accepted?: AcceptedInput) {
    let queued = false;
    try {
        if (engineFailure) throw engineFailure;
        let result;
        if (method === "scope_addresses") {
            result = await readScopeAddresses(args);
        } else if (method === "watch_recovery") {
            watchRecoveryList(args.account); result = {};
        } else if (method === "open") {
            initializing = true;
            try { result = await open(args); }
            finally { initializing = false; }
        } else if (method === "surface") {
            if (!guest || !scene || guestFailure || guestUpdating || closing || guestWork.draining) {
                if (guestUpdating && !closing) surfaceDeferred = true;
                self.postMessage({ id }); return;
            }
            validateSurface(args);
            guestWork.queue({ kind: 15, target: 0, objectId: windowObjectId, data: args });
            surface = args;
            await enterGuest(); result = presentation();
        } else if (method === "input") {
            if (guestFailure) throw guestFailure;
            await refreshPersonal();
            operations.refreshVolumes();
            if (!accepted) throw new Error("Input requires an accepted session-instance root");
            validateGuestInput(accepted.event.kind, accepted.event.target, accepted.event.data, args.menuRevision);
            guestWork.queueAccepted(accepted.root, accepted.event); queued = true;
            await enterGuest(); result = presentation();
        } else if (method === "retry_update") {
            checkUpdate(true); result = {};
        } else if (method === "close") {
            operations.data.dispose();
            closed = true;
            closeStorage(); result = { closed: true };
        } else throw new Error("Unknown runtime operation");
        self.postMessage({ id, result });
    } catch (error) {
        if (engineFailure) return;
        if (accepted && !queued) guestWork.reject(accepted.root);
        if (method === "open") { closed = true; try { closeStorage(); } catch (cleanup) { logs.push(String(cleanup).slice(0, 512)); } }
        const message = String((error as Error).message ?? error);
        const cause = (error as { code?: unknown })?.code;
        const code = cause === 'unsupported' || cause === 'incompatible-storage-format' || cause === 'incompatible-installation-format' ? cause
            : method === "open" && corruptStoreOpenMessage(message) ? "corrupt_store" : undefined;
        self.postMessage({ id, error: message, code });
    } finally {
        if (!engineFailure && method === "input") replicas?.notify();
    }
};
