import { AGREEMENT } from "./ap-codec.ts";
import { validId } from './identifier.ts';
import { advisoryOperation, type AdvisoryOperation } from './ap-protocol.ts';
import { failure } from './errors.ts';
import { workerURL } from './release-channel.ts';
import type { Deployment } from './release-channel.ts';
// The named account worker owns authentication and every AP stream across tabs.

import type { ApOperation, ApResponse, ApDescription, ApStatus, ApResetResult, ApReplicaSubscription, ApPublicationAck, ApInvitationEvent, ApMethods, ApMethod, ApRequestOf, ApAckAccountNoticeRequest, ApDetachRequest, ApChannelEvent, ApWorkerMessage } from "./ap-protocol.ts";

export type ApChannelListener = (event: ApChannelEvent) => void;
export type ApAccountListener = () => unknown;
export type ApFailureListener = (error: Error) => void;

interface PendingRequest {
    // Method syntax: the promise resolver of each `call<M>` is stored here
    // regardless of M, and the worker's `result` arrives as `unknown`.
    resolve(value: unknown): void;
    reject(error: Error): void;
    timer: ReturnType<typeof setTimeout> | null;
    idle: boolean;
    local: boolean;
    credit: boolean;
    advisory?: AdvisoryOperation;
}

export class ApClient {
    account: string | null;
    worker: SharedWorker;
    pending: Map<number, PendingRequest>;
    nextId: number;
    listeners: Map<string, ApChannelListener>;
    closed: boolean;
    observedAccount?: string;
    private describing?: Promise<ApDescription>;
    accountListener?: ApAccountListener;
    accountFailure?: ApFailureListener;

    constructor(selected: Deployment, account: string | null = null) {
        if (account !== null && !validId(account, 16)) throw new Error("Invalid AP account identity");
        if (!globalThis.SharedWorker) throw new Error("This web browser does not support shared AP connections");
        this.account = account;
        this.worker = new SharedWorker(workerURL('./ap-worker.js', import.meta.url, selected), { type: "module", name: `playbit-ap-${selected}` });
        this.pending = new Map();
        this.nextId = 1;
        this.listeners = new Map();
        this.closed = false;
        this.worker.port.onmessage = ({ data }: MessageEvent<ApWorkerMessage>) => {
            if (data.event === "channel") { this.listeners.get(data.channel)?.(data); return; }
            if (data.event === "account-error") {
                const error = Object.assign(new Error(data.error), { code: data.code, serviceCode: data.serviceCode });
                if (data.stack) error.stack = data.stack;
                this.accountFailure?.(error); return;
            }
            if (data.event === "account") {
                if (!this.accountListener || data.account !== this.observedAccount) { this.accountFailure?.(new Error("Unexpected account notice")); return; }
                // The worker retains at most one outstanding notice until the
                // local snapshot read finishes. No AP request slot or timer.
                // `accountListener` is never unset once assigned, so it is still present in the microtask.
                void Promise.resolve().then(() => this.accountListener!()).catch(error => this.accountFailure?.(error)).finally(() => {
                    if (!this.closed) this.worker.port.postMessage({ method: "ackAccountNotice", args: { account: data.account, ticket: data.ticket }, account: this.account } satisfies ApAckAccountNoticeRequest);
                });
                return;
            }
            const pending = this.pending.get(data.id);
            if (!pending) return;
            this.pending.delete(data.id);
            clearTimeout(pending.timer as number | undefined);
            if (data.error) {
                const error = Object.assign(new Error(data.error), { code: data.code, serviceCode: data.serviceCode });
                if (data.stack) error.stack = data.stack;
                pending.reject(error);
            } else pending.resolve(data.result);
        };
        this.worker.onerror = () => this.fail(new Error("AP worker failed"));
        this.worker.port.onmessageerror = () => this.fail(new Error("Invalid AP worker response"));
        this.worker.port.start();
    }

    fail(error: Error): void {
        for (const { reject, timer } of this.pending.values()) { clearTimeout(timer as number | undefined); reject(error); }
        this.pending.clear();
        if (!this.closed) this.accountFailure?.(error);
    }

    call<M extends ApMethod>(method: M, args?: ApMethods[M]["args"], transfer: Transferable[] = []): Promise<ApMethods[M]["result"]> {
        if (this.closed) return Promise.reject(new Error("AP tab closed"));
        const idle = method === "streamReplica" || method === "streamInvitation" || method === "channelRead";
        const local = method === 'watchAccount' || method === 'accountChanged';
        const credit = method === 'channelConsumed';
        const advisory = advisoryOperation(method, args && 'operation' in args ? args.operation : undefined);
        let reads = 0, ordinary = 0, controls = 0, credits = 0;
        for (const pending of this.pending.values()) {
            if (pending.advisory) {
                if (pending.advisory === advisory) return Promise.reject(failure('ap-capacity', 'An advisory AP request is already pending'));
            } else if (pending.credit) credits++; else if (pending.local) controls++; else if (pending.idle) reads++; else ordinary++;
        }
        if (!advisory && (credit ? credits >= 1 : local ? controls >= 3 : idle ? reads >= 33 : ordinary >= 32)) return Promise.reject(new Error("Too many pending AP requests"));
        const id = this.nextId++;
        return new Promise((resolve, reject) => {
            const timer = idle ? null : setTimeout(() => {
                this.pending.delete(id);
                reject(advisory ? failure('ap-transport', 'AP worker response timed out') : new Error("AP worker response timed out"));
            }, 65000);
            this.pending.set(id, { resolve, reject, timer, idle, local, credit, advisory });
            try { this.worker.port.postMessage({ id, method, args, account: this.account } satisfies ApRequestOf<M>, transfer); }
            catch (error) { clearTimeout(timer as number | undefined); this.pending.delete(id); reject(error); }
        });
    }

    onboarding(stage: number): Promise<undefined> { return this.call("onboarding", { stage }); }
    agreement(account: string, accept: boolean): Promise<boolean> {
        return this.call("agreement", { account, ...AGREEMENT, accept });
    }
    resetWebAccount(tab: string, token: string): Promise<ApResetResult> { return this.call('resetWebAccount', { tab, token }); }
    resetState(token: string | null = null): Promise<ApResetResult> { return this.call('resetState', { token }); }
    describe(): Promise<ApDescription> {
        return this.describing ??= this.call('describe').finally(() => { this.describing = undefined; });
    }
    connect(): Promise<ApStatus> { return this.call("connect", { account: this.account }); }
    requestCode({ email, instanceName }: { email: string; instanceName?: string }): Promise<{ email: string; codeSent: true }> { return this.call("requestCode", { email, instanceName }); }
    signInWithKey(email: string, accountKey: string): Promise<ApStatus> { return this.call("signInWithKey", { email, accountKey }); }
    finishCode(code: string): Promise<ApStatus> { return this.call("finishCode", { code, account: this.account }); }
    beginChannel(frame: import("./ap-codec.ts").FrameInput,device:Uint8Array,generation:number): Promise<number> {return this.call("beginChannel",{frame,device,generation});}
    channelRead(handle:number): Promise<import("./ap-codec.ts").Frame & {receipt?:number}> {return this.call("channelRead",{handle});}
    channelDisplayName(handle: number, name: Uint8Array): Promise<undefined> { return this.call("channelDisplayName", { handle, name }); }
    begin(operation: ApOperation): Promise<number> { return this.call("begin", { operation }); }
    subscribeReplica(operation: ApOperation): Promise<ApReplicaSubscription> { return this.call("subscribeReplica", { operation }); }
    streamReplica(handle: number): Promise<Uint8Array> { return this.call("streamReplica", { handle }); }
    checkpointReplica(handle: number): Promise<undefined> { return this.call("checkpointReplica", { handle }); }
    publishReplica(operation: ApOperation, bytes: Uint8Array, generation?: number): Promise<ApPublicationAck> { return this.call("publishReplica", { operation, bytes, generation }); }
    watchInvitation(operation: ApOperation): Promise<number> { return this.call("watchInvitation", { operation }); }
    streamInvitation(handle: number): Promise<ApInvitationEvent> { return this.call("streamInvitation", { handle }); }
    channelConsumed(receipts:number[]):Promise<undefined>{return this.call("channelConsumed",{receipts});}
    streamRead(handle: number, maxBody?: number): Promise<Uint8Array> { return this.call("streamRead", { handle, maxBody }); }
    streamNamespaceEvidence(handle: number): Promise<Uint8Array> { return this.call("streamNamespaceEvidence", { handle }); }
    streamWrite(handle: number, bytes: Uint8Array): Promise<undefined> { return this.call("streamWrite", { handle, bytes }); }
    streamFinish(handle: number): Promise<undefined> { return this.call("streamFinish", { handle }); }
    streamResponse(handle: number): Promise<ApResponse> { return this.call("streamResponse", { handle }); }
    streamClose(handle: number): Promise<undefined> { return this.call("streamClose", { handle }); }
    request(operation: ApOperation, generation?: number): Promise<ApResponse> { return this.call("request", { operation, generation }); }
    upload(operation: ApOperation, readable: ReadableStream<Uint8Array>): Promise<ApResponse> { return this.call("request", { operation, upload: readable }, [readable]); }
    status(): Promise<ApStatus> { return this.call("status"); }
    watchAccount(account: string, listener: ApAccountListener, failed: ApFailureListener): Promise<undefined> {
        if (this.observedAccount && this.observedAccount !== account) return Promise.reject(new Error("Account observer belongs to another account"));
        this.observedAccount = account; this.accountListener = listener; this.accountFailure = failed;
        return this.call("watchAccount", { account });
    }
    accountChanged(account: string): Promise<undefined> { return this.call("accountChanged", { account }); }
    subscribe(channel: string, listener: ApChannelListener): Promise<{ subscribed: string }> { this.listeners.set(channel, listener); return this.call("subscribe", { channel }); }
    publish(channel: string, bytes: Uint8Array): Promise<{ sent: number }> { return this.call("publish", { channel, bytes }); }
    close(): void { if (this.closed) return; this.closed = true; this.worker.port.postMessage({ method: "detach", args: {} } satisfies ApDetachRequest); this.fail(new Error("AP tab closed")); this.worker.port.close(); }
}
