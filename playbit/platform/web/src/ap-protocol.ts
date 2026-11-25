// The message and result types shared by the page (ap-client.ts) and the AP
// SharedWorker (ap-worker.ts, ap-connection.ts).

import type { ApTiming } from "./ap-timing.ts";
import type { ResetState } from "./web-account.ts";

// An AP service operation as the app composes it. `op` and `operation_id` are
// the only fields every operation carries; the rest depend on `op`.
export interface ApOperation { op: string; operation_id: string; [key: string]: unknown }

export function isWorldRequest(operation: ApOperation, kind: "sync" | "subscribe" | "publish"): boolean {
    return operation.op === "world_replica" && (operation.request as { kind?: unknown } | null)?.kind === kind;
}

// Each advisory operation has one slot beside ordinary requests at every worker boundary.
export type AdvisoryOperation = 'describe' | 'put_account_display_name' | 'channelDisplayName';
export function advisoryOperation(method: string, operation?: ApOperation): AdvisoryOperation | undefined {
    if (method === 'channelDisplayName') return method;
    const op = method === 'request' || method === 'begin' ? operation?.op : method;
    return op === 'describe' || op === 'put_account_display_name' ? op : undefined;
}

// A service JSON response the worker has already checked for `ok: true` and a
// matching `operation_id`.
export interface ApResponse { timing: ApTiming[]; operation_id: string; ok: true; data?: unknown; [key: string]: unknown }

// Authenticated advisory support information, bounded to one result per connection.
export interface ApDescription {
    protocol: number; operations: string[]; features: string[];
    algorithms: { kvdb_pull: string; kvdb_delta: string; replica_events: string };
    limits: { frame_bytes: number; request_bytes: number; required_features: number; feature_name_bytes: number };
}

// `ApConnection.status()` as replied to `connect`, `finishCode` and `status`.
export interface ApStatus { caCertificate?: number[]; connected: boolean; account: string | null; instanceId: number[] | null; connectionCount: number; activeStreams: number; sessionDisplayNames?: boolean }

// Reply to `resetWebAccount`/`resetState`: the last cleanup failure, if any.
/** What resetWebAccount/resetState reply: web-account.ts's ResetState. */
export type ApResetResult = ResetState;

// Reply to `subscribeReplica`: the stream handle and the service's subscription acknowledgment.
export interface ApReplicaSubscription { handle: number; ack: { epoch: string; sequence: bigint; policy_revision: bigint } }

// Reply to `publishReplica`: the durable publication acknowledgment.
export interface ApPublicationAck { database_clock: bigint; policy_revision: bigint }

// Service JSON events from `streamInvitation`. The
// worker only parses them; the consumer validates the fields before trusting them.
export interface ApInvitationEvent { kind: string; [key: string]: unknown }

// Every request method the tab may send, with its `args` payload and the
// `result` the worker replies with. `undefined` args are sent as `args: undefined`.
export interface ApMethods {
    describe: { args: undefined; result: ApDescription };
    beginChannel: {args: {frame: import("./ap-codec.ts").FrameInput;device:Uint8Array;generation:number}; result:number};
    channelRead: {args:{handle:number};result:import("./ap-codec.ts").Frame & {receipt?:number}};
    channelDisplayName: { args: { handle: number; name: Uint8Array }; result: undefined };
    channelConsumed: {args:{receipts:number[]};result:undefined};
    onboarding: { args: { stage: number }; result: undefined };
    agreement: { args: { account: string; revision: number; sha256: string; accept: boolean }; result: boolean };
    resetWebAccount: { args: { tab: string; token: string }; result: ApResetResult };
    resetState: { args: { token: string | null }; result: ApResetResult };
    connect: { args: { account: string | null }; result: ApStatus };
    requestCode: { args: { email: string; instanceName?: string }; result: { email: string; codeSent: true } };
    signInWithKey: { args: { email: string; accountKey: string }; result: ApStatus };
    finishCode: { args: { code: string; account: string | null }; result: ApStatus };
    begin: { args: { operation: ApOperation }; result: number };
    subscribeReplica: { args: { operation: ApOperation }; result: ApReplicaSubscription };
    streamReplica: { args: { handle: number }; result: Uint8Array };
    checkpointReplica: { args: { handle: number }; result: undefined };
    publishReplica: { args: { operation: ApOperation; bytes: Uint8Array; generation?: number }; result: ApPublicationAck };
    watchInvitation: { args: { operation: ApOperation }; result: number };
    streamInvitation: { args: { handle: number }; result: ApInvitationEvent };
    streamRead: { args: { handle: number; maxBody?: number }; result: Uint8Array };
    streamNamespaceEvidence: { args: { handle: number }; result: Uint8Array };
    streamWrite: { args: { handle: number; bytes: Uint8Array }; result: undefined };
    streamFinish: { args: { handle: number }; result: undefined };
    streamResponse: { args: { handle: number }; result: ApResponse };
    streamClose: { args: { handle: number }; result: undefined };
    request: { args: { operation: ApOperation; upload?: ReadableStream<Uint8Array>; generation?: number }; result: ApResponse };
    status: { args: undefined; result: ApStatus };
    watchAccount: { args: { account: string }; result: undefined };
    accountChanged: { args: { account: string }; result: undefined };
    subscribe: { args: { channel: string }; result: { subscribed: string } };
    publish: { args: { channel: string; bytes: Uint8Array }; result: { sent: number } };
}
export type ApMethod = keyof ApMethods;

// A request the tab posts to the worker port. Every one but the account-notice
// acknowledgment and `detach` carries an `id` the worker answers with an `ApReply`.
export interface ApRequestOf<M extends ApMethod> { id: number; method: M; args: ApMethods[M]["args"]; account: string | null }
export interface ApAckAccountNoticeRequest { method: "ackAccountNotice"; args: { account: string; ticket: number }; account: string | null }
export interface ApDetachRequest { method: "detach"; args: Record<string, never> }
export type ApRequest = { [M in ApMethod]: ApRequestOf<M> }[ApMethod] | ApAckAccountNoticeRequest | ApDetachRequest;

// The worker's answer to an identified request: the method's result, or an
// error message with the AP failure classification and bounded worker stack.
export type ApReply =
    | { event?: undefined; id: number; result: unknown; error?: undefined; code?: undefined }
    | { event?: undefined; id: number; result?: undefined; error: string; code?: string; serviceCode?: string; stack?: string };
// A frame published on a subscribed channel, or the channel's terminal failure.
export type ApChannelEvent =
    | { event: "channel"; channel: string; source: Uint8Array; body: Uint8Array; error?: undefined }
    | { event: "channel"; channel: string; error: string; source?: undefined; body?: undefined };
// The account changed elsewhere; acknowledged with `ackAccountNotice` after the local read.
export interface ApAccountNotice { event: "account"; account: string; ticket: number }
// An `ackAccountNotice` the worker refused.
export interface ApAccountError { event: "account-error"; error: string; code?: string; serviceCode?: string; stack?: string }
// Everything the worker posts to a tab's port.
export type ApWorkerMessage = ApReply | ApChannelEvent | ApAccountNotice | ApAccountError;
