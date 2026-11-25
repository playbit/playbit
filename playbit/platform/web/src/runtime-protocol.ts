// Messages exchanged between the page (app.ts) and the runtime worker
// (runtime-worker.ts). Types only; both programs import from here.
import type { SceneNode, SceneWindow } from "./scene.ts";
import type { ReplicationState } from "./live-replication.ts";
import type { Invitation, ScopeAddresses, ScopeRequest } from "./invitation.ts";
import type { AppScope, CatalogVolume } from "./recovery.ts";
import type { PersonalGrant } from "./personal.ts";

// What the worker presents after every step: the scene root and any guest logs.
export interface RuntimePresentation { scene: SceneNode; window?: SceneWindow; logs: string[] }
export interface RuntimeIdentity { account: string; root: string; shared?: string; session: string; version: string }
export interface RuntimeTiming { databaseMs: number; launchMs: number }
export interface OfflineEntry {
    cursor: { cut: string; offset: number };
    entry: { id: string; name: string; kind: string; protocol: string | null; deleted: boolean; name_conflict: boolean };
}
export interface RuntimeOpenResult extends RuntimePresentation { identity: RuntimeIdentity; timing: RuntimeTiming; storageBytes: number }
// `open`: the OPFS profile name, then either a local app's name or the signed-in
// account with its invitation and personal volume catalog (the worker reads
// `account` only when `invitation` is present).
export type RuntimeOpenArgs =
    | { profile: string; app?: string; account?: undefined; invitation?: undefined; personal?: undefined; userVolume?: undefined; creationScope?: undefined }
    | { profile: string; account: string; invitation: Invitation; personal?: CatalogVolume[]; userVolume?: CatalogVolume; creationScope?: Extract<AppScope, {kind: "session"}>; app?: undefined };
// `input`: one guest event for a scene control.
export interface RuntimeInputArgs { kind: number; target: number; data: Uint8Array; menuRevision?: number }
// `select_personal`: the chooser's grants for one session, with the catalog it chose from.
export interface RuntimeSelectPersonalArgs { session: string; grants: PersonalGrant[]; personal: CatalogVolume[] }
// `select_personal` replies `{}` once the grants apply, `{ canceled: true }` when canceled.
export type RuntimePersonalResult = { canceled?: boolean; scene?: undefined } | (RuntimePresentation & { canceled?: false });
// The app update state the worker reports; `null` clears the notice.
export type RuntimeUpdateState =
    | { kind: "downloading" | "opening"; build_id: string; message: string }
    | { kind: "checking" | "failed"; build_id?: string; message: string };
// Every request the page may send, with its `args` payload and the worker's reply.
export interface RuntimeMethods {
    scope_addresses: { args: ScopeRequest; result: ScopeAddresses };
    open: { args: RuntimeOpenArgs; result: RuntimeOpenResult };
    input: { args: RuntimeInputArgs; result: RuntimePresentation };
    surface: { args: Uint8Array; result: RuntimePresentation | undefined };
    retry_update: { args: undefined; result: Record<string, never> };
    close: { args: undefined; result: { closed: true } };
    reconnect: { args: undefined; result: Record<string, never> };
    watch_recovery: { args: { account: string }; result: Record<string, never> };
    account_changed: { args: undefined; result: Record<string, never> };
    select_personal: { args: RuntimeSelectPersonalArgs; result: RuntimePersonalResult };
    cancel_personal: { args: undefined; result: { canceled: boolean } };
    offline_entries: { args: { volume: string; after: string | null }; result: { entries: OfflineEntry[]; next: string | null } };
    offline_restore: { args: { volume: string; cut: string; entry: string; name: string }; result: { entry: string } };
}
export type RuntimeMethod = keyof RuntimeMethods;
// Distributes over the methods, so the worker narrows `args` by `method`.
export type RuntimeRequest<M extends RuntimeMethod = RuntimeMethod> = M extends RuntimeMethod ? { id: number; method: M; args: RuntimeMethods[M]["args"] } : never;
// The worker calls AP methods by name through the page; the page dispatches dynamically.
export interface RuntimeApRequest { id: number; method: string; args: unknown[] }
// The page's answer to one `RuntimeApRequest`: its result, or the failure's message and code.
export type RuntimeApReply = { id: number; result: unknown; error?: undefined; code?: undefined } | { id: number; error: string; code?: string; serviceCode?: string; result?: undefined };
// What the page posts to the worker: the reply to a proxied AP call, or a request.
export type RuntimePageMessage = { apReply: RuntimeApReply; method?: undefined } | (RuntimeRequest & { apReply?: undefined });
// What the worker posts to the page. Exactly one of the named fields is present;
// a message with none of them is the reply to the `RuntimeRequest` with that `id`.
export interface RuntimeWorkerMessage {
    ap?: RuntimeApRequest;
    replication?: ReplicationState;
    accountNoticeFailed?: true;
    replicaScene?: RuntimePresentation;
    surfaceReady?: true;
    guestFailure?: string | null;
    engineFailure?: string;
    update?: RuntimeUpdateState | null;
    adopted?: RuntimePresentation & { build_id: string };
    progress?: string;
    id?: number; result?: unknown; error?: string; code?: "corrupt_store" | "unsupported" | "incompatible-storage-format" | "incompatible-installation-format";
}
