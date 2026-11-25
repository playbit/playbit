import { validId } from './identifier.ts';
import type { ReplicaTerminal } from "./replication.ts";
import type { DatabaseId } from "./database-id.ts";
import { failure, retryable } from "./errors.ts";
import { replicaEvent, ReplicaCursor } from './replication.ts';
import type { ReplicaSubscriptionAck } from './replication.ts';

// A subscription target: the account stream, or one volume stream whose extra
// fields (session/build) become the subscribe operation's fields verbatim.
type VolumeScope = { volume_id: string; session_id?: string | null; build_id?: string | null };
export type ReplicaScope = { kind: 'account' } | (VolumeScope & ({ kind: 'volume' } | { kind: 'entry'; entry_scope: string }));
// The AP operations this driver composes: structurally `ApOperation` from
// ap-client.ts, which the worker program cannot import (it needs the DOM lib).
export interface ReplicaOperation { op: string; operation_id: string; [key: string]: unknown }
// Reply to `subscribeReplica`: structurally `ApReplicaSubscription` from ap-client.ts.
export interface ReplicaSubscription { handle: number; ack: ReplicaSubscriptionAck }
// The runtime's AP request dispatcher, restricted to the methods this driver sends.
export interface LiveReplicationRequest {
    (method: 'subscribeReplica', operation: ReplicaOperation): Promise<ReplicaSubscription>;
    (method: 'streamReplica', handle: number): Promise<Uint8Array>;
    (method: 'checkpointReplica', handle: number): Promise<unknown>;
    (method: 'publishReplica', operation: ReplicaOperation, bytes: Uint8Array): Promise<unknown>;
    (method: 'streamClose', handle: number): Promise<unknown>;
}
// Returns false once the run that issued a callback has been retired.
export type LiveReplicationCurrent = () => boolean;
// The next locally committed change to publish; `ticket` is opaque to the driver
// and handed back to `ack` unchanged.
export interface ReplicaHead<Ticket = unknown> {
    source: string;
    publication: Extract<ReplicaScope, { kind: 'account' | 'volume' }>;
    database: DatabaseId;
    ticket: Ticket;
    bytes: Uint8Array;
}
// The local store: outbound head/ack and inbound apply of a decoded replica delta.
export interface LiveReplicationLocal<Ticket = unknown> {
    retired?(): void;
    volumeCurrent?(id: string): boolean;
    head(current: LiveReplicationCurrent): Promise<ReplicaHead<Ticket> | null | undefined>;
    ack(ticket: Ticket, current: LiveReplicationCurrent): Promise<unknown>;
    published?(head: ReplicaHead<Ticket>, current: LiveReplicationCurrent): Promise<unknown>;
    // A scoped rejection can retire the volatile ticket and recover its durable source.
    rejected?(head: ReplicaHead<Ticket>, code: string, current: LiveReplicationCurrent): Promise<boolean>;
    apply(source: string, database: DatabaseId, delta: Uint8Array, current: LiveReplicationCurrent, evidence?: Uint8Array): Promise<unknown>;
    terminal(source: string, terminal: ReplicaTerminal, current: LiveReplicationCurrent): Promise<unknown>;
    catchUpLog?(source: string, database: DatabaseId, current: LiveReplicationCurrent): Promise<unknown>;
}
// Rebases and synchronizes local state before the streams go live.
export type LiveReplicationBootstrap = (current: LiveReplicationCurrent) => Promise<unknown>;
export type ReplicationStateKind = 'catching_up' | 'live' | 'reconnecting' | 'action_required' | 'failed';
export type ReplicationStage = 'subscription' | 'catch_up' | 'publication' | 'checkpoint' | 'delivery';
export interface ReplicationState { kind: ReplicationStateKind; message?: string; code?: string; source?: string; stage?: ReplicationStage }
// Copy only fixed diagnostic text. Remote errors can contain private names or
// values; the ordinary error presentation remains separate from this record.
export function replicationDiagnostic(state: ReplicationState): string {
    const reasons: Record<string, string> = {
        'ap-transport': 'The connection was interrupted or exceeded its deadline',
        'ap-capacity': 'The service is temporarily at capacity',
        'replica-gap': 'The replica requires catch-up',
        'authentication-required': 'Sign-in is required',
        'ap-rejected': 'The service rejected the operation',
        'access-revoked': 'Access was revoked',
        'replica-authority': 'Replica authority requires catch-up',
    };
    const code = state.code && Object.hasOwn(reasons, state.code) ? state.code : 'unknown';
    return `${state.stage ?? 'connection'}: ${reasons[code] ?? 'Synchronization failed'} (${code})`;
}
export type ReplicationStateListener = (state: ReplicationState) => void;
export interface LiveReplicationOptions<Ticket = unknown> {
    ap: LiveReplicationRequest;
    local: LiveReplicationLocal<Ticket>;
    bootstrap: LiveReplicationBootstrap;
    state: ReplicationStateListener;
    operationId: () => string;
    timeout?: number;
}
export interface LiveReplicationStream {
    additional?: boolean;
    scope: ReplicaScope;
    source: string;
    handle: number;
    cursor?: ReplicaCursor;
    stopped: boolean;
    setup?: Promise<void>;
    checkpoint: { resolve: () => void; reject: (error: unknown) => void } | null;
    reader?: Promise<void>;
}
export interface LiveReplicationRun {
    streams: LiveReplicationStream[];
    stopped: boolean;
    active: boolean;
    dirty: boolean;
    applying: number;
    setup?: Promise<void>;
    publisher?: Promise<void> | null;
}
// Errors arrive as `unknown` from catch clauses and rejections; every producer in
// this client sets `code` to a string when it sets one at all.
type Errorish = { message?: unknown; code?: string };

export function replicaSource(scope: ReplicaScope): string {
    if (scope.kind === 'account' && Object.keys(scope).length === 1) return 'account';
    if ((scope.kind === 'volume' || scope.kind === 'entry') && validId(scope.volume_id, 16)
        && Object.keys(scope).every(key => ['kind', 'volume_id', 'session_id', 'build_id', ...(scope.kind === 'entry' ? ['entry_scope'] : [])].includes(key))) {
        if (scope.kind === 'volume') return scope.volume_id;
        if (validId(scope.entry_scope, 15) && !/^1+$/.test(scope.entry_scope)) return `${scope.volume_id}/${scope.entry_scope}`;
    }
    throw new Error('Invalid replica subscription scope');
}
export function replicaAddress(source: string): { volume: string; entry?: string } {
    const [volume, entry, extra] = source.split('/');
    if (extra !== undefined || !validId(volume) || entry !== undefined && (!validId(entry, 15) || /^1+$/.test(entry))) throw new Error('Invalid replica source');
    return { volume, entry };
}
function operation(scope: ReplicaScope, publish = false): Record<string, unknown> {
    if (scope.kind === 'account') return {};
    const { kind, ...fields } = scope;
    if (publish && 'entry_scope' in fields) delete (fields as Partial<typeof fields>).entry_scope;
    return fields;
}

// One account connection, bounded account/volume streams, and one publication task.
// Callbacks enter the runtime's local queue; AP waits never enter that queue.
export class LiveReplication<Ticket = unknown> {
    ap: LiveReplicationRequest;
    local: LiveReplicationLocal<Ticket>;
    bootstrap: LiveReplicationBootstrap;
    state: ReplicationStateListener;
    operationId: () => string;
    timeout: number;
    run: LiveReplicationRun | null;
    constructor({ ap, local, bootstrap, state, operationId, timeout = 60000 }: LiveReplicationOptions<Ticket>) {
        this.ap = ap; this.local = local; this.bootstrap = bootstrap;
        this.state = state; this.operationId = operationId; this.timeout = timeout;
        this.run = null;
    }
    current(run: LiveReplicationRun): boolean { return this.run === run && !run.stopped; }
    scopeCurrent(scope: ReplicaScope): boolean {
        return scope.kind === 'account' || this.local.volumeCurrent?.(scope.volume_id) !== false;
    }
    streamCurrent(run: LiveReplicationRun, stream: LiveReplicationStream): boolean {
        return this.current(run) && !stream.stopped && run.streams.includes(stream);
    }
    report(run: LiveReplicationRun, kind: ReplicationStateKind, error?: unknown, stage?: ReplicationStage): void {
        if (kind === 'live' && run.streams.some(stream => stream.stopped || stream.setup)) kind = 'catching_up';
        if (this.current(run)) this.state({ kind, ...(error ? { message: String((error as Errorish).message ?? error).slice(0, 512), code: (error as Errorish).code, stage } : {}) });
    }
    fail(run: LiveReplicationRun, error: unknown, stage?: ReplicationStage): void {
        if (!this.current(run)) return;
        this.report(run, retryable(error) ? 'reconnecting' : (['authentication-required', 'ap-rejected', 'access-revoked'] as readonly unknown[]).includes((error as Errorish)?.code) ? 'action_required' : 'failed', error, stage);
        run.stopped = true;
        this.local.retired?.();
        for (const stream of run.streams) {
            stream.checkpoint?.reject(error);
            if (stream.handle) void this.ap('streamClose', stream.handle).catch(() => {});
        }
    }
    failStream(run: LiveReplicationRun, stream: LiveReplicationStream, error: unknown, stage: ReplicationStage = 'delivery'): void {
        if (!this.streamCurrent(run, stream)) return;
        if ((stream.scope.kind !== 'entry' && !stream.additional) || ['ap-transport', 'authentication-required', 'access-revoked', 'replica-authority'].includes((error as Errorish)?.code ?? '')) {
            this.fail(run, error, stage); return;
        }
        stream.stopped = true;
        stream.checkpoint?.reject(error);
        if (stream.handle) void this.ap('streamClose', stream.handle).catch(() => {});
        this.state({ kind: retryable(error) ? 'reconnecting' : (error as Errorish)?.code === 'ap-rejected' ? 'action_required' : 'failed',
            source: stream.source, message: String((error as Errorish)?.message ?? error).slice(0, 512), code: (error as Errorish)?.code, stage });
    }
    async subscribe(run: LiveReplicationRun, stream: LiveReplicationStream): Promise<boolean> {
        if (!this.streamCurrent(run, stream) || !this.scopeCurrent(stream.scope)) return false;
        const { handle, ack } = await this.ap('subscribeReplica', { ...operation(stream.scope),
            op: stream.scope.kind === 'account' ? 'subscribe_account' : 'subscribe_replica', operation_id: this.operationId() });
        if (!this.streamCurrent(run, stream)) { await this.ap('streamClose', handle); return false; }
        stream.handle = handle; stream.cursor = new ReplicaCursor(ack); return true;
    }
    removeVolume(volume: string): void {
        const run = this.run;
        if (!run) return;
        for (const stream of run.streams.slice()) {
            if (stream.scope.kind === 'account' || stream.scope.volume_id !== volume) continue;
            stream.stopped = true;
            run.streams.splice(run.streams.indexOf(stream), 1);
            stream.checkpoint?.reject(new Error('Volume access changed'));
            if (stream.handle) void this.ap('streamClose', stream.handle).catch(() => {});
        }
    }
    recoverVolumeCatalog(volume: string): void {
        const run = this.run;
        if (!run || !this.current(run)) return;
        const source = run.streams.find(stream => stream.source === volume && stream.additional);
        const gap = failure('replica-gap', 'World publication requires fresh catalog authority');
        if (source && !source.stopped) this.failStream(run, source, gap, 'publication');
        else this.state({ source: volume, kind: 'reconnecting', stage: 'publication', code: gap.code, message: gap.message });
    }
    async stop(): Promise<void> {
        const run = this.run;
        if (!run) return;
        this.run = null; run.stopped = true;
        this.local.retired?.();
        for (const stream of run.streams) {
            stream.checkpoint?.reject(new Error('Replica subscription retired'));
            if (stream.handle) void this.ap('streamClose', stream.handle).catch(() => {});
        }
        // Retire the old publisher before a new bootstrap can rebase its feed.
        await Promise.allSettled([run.setup, run.publisher, ...run.streams.flatMap(stream => [stream.setup, stream.reader])]);
    }
    async start(scopes: ReplicaScope[]): Promise<void> {
        if (scopes.length > 512 || new Set(scopes.map(replicaSource)).size !== scopes.length) {
            throw new Error('Invalid replica subscription scopes');
        }
        await this.stop();
        const run: LiveReplicationRun = { streams: [], stopped: false, active: false, dirty: true, applying: 0 };
        this.run = run; this.report(run, 'catching_up');
        let stage: ReplicationStage = 'subscription';
        run.setup = (async () => {
            for (const scope of scopes) {
                if (!this.current(run)) return;
                if (!this.scopeCurrent(scope)) continue;
                const stream: LiveReplicationStream = { scope, source: replicaSource(scope), handle: 0, stopped: false, checkpoint: null };
                run.streams.push(stream);
                try { await this.subscribe(run, stream); }
                catch (error) { if (this.streamCurrent(run, stream)) throw error; }
            }
            if (!this.current(run)) return;
            stage = 'catch_up';
            await this.bootstrap(() => this.current(run));
            if (!this.current(run)) return;
            run.active = true;
            for (const stream of run.streams) stream.reader = this.read(run, stream).catch(error => this.failStream(run, stream, error));
            this.notify();
        })().catch(error => this.fail(run, error, stage));
        await run.setup;
    }
    // Subscribe before detached capture. Reservation and replacement use this
    // run's existing 512 slots; a selected retry never retires healthy siblings.
    async add(scope: Exclude<ReplicaScope, { kind: 'account' }>, bootstrap: LiveReplicationBootstrap): Promise<void> {
        if (scope.kind !== 'entry' && scope.kind !== 'volume') throw new Error('Invalid additional replica scope');
        const identity = replicaSource(scope), run = this.run;
        if (!run || !this.current(run)) throw failure('replica-gap', 'Replica connection requires recovery');
        await run.setup;
        if (!this.current(run) || !run.active) throw failure('replica-gap', 'Replica connection requires recovery');
        if (!this.scopeCurrent(scope)) return;
        const previous = run.streams.find(stream => stream.source === identity);
        if (previous && !previous.stopped) { await previous.setup; return; }
        if (!previous && run.streams.length === 512) throw new Error('Replica subscription capacity exceeded');
        const stream: LiveReplicationStream = { additional: true, scope, source: identity, handle: 0, stopped: false, checkpoint: null };
        if (previous) run.streams[run.streams.indexOf(previous)] = stream;
        else run.streams.push(stream);
        let stage: ReplicationStage = 'subscription';
        stream.setup = (async () => {
            await previous?.reader;
            if (!await this.subscribe(run, stream)) return;
            stage = 'catch_up';
            await bootstrap(() => this.streamCurrent(run, stream));
            if (!this.streamCurrent(run, stream)) return;
            stream.reader = this.read(run, stream).catch(error => this.failStream(run, stream, error));
            stage = 'checkpoint';
            await this.checkpoint(run, stream);
            if (this.streamCurrent(run, stream)) this.state({ kind: 'live', source: identity });
        })().catch(error => { this.failStream(run, stream, error, stage); throw error; });
        try { await stream.setup; }
        finally { stream.setup = undefined; }
    }
    notify(): void {
        const run = this.run;
        if (!run || !this.current(run)) return;
        run.dirty = true; this.report(run, 'catching_up');
        if (!run.active || run.publisher) return;
        run.publisher = this.publish(run).catch(error => this.fail(run, error, 'publication')).finally(() => {
            run.publisher = null;
            if (this.current(run) && run.dirty) this.notify();
        });
    }
    async publish(run: LiveReplicationRun): Promise<void> {
        while (this.current(run)) {
            run.dirty = false;
            for (;;) {
                const head = await this.local.head(() => this.current(run));
                if (!this.current(run)) return;
                if (!head) break;
                const current = () => this.current(run) && this.scopeCurrent(head.publication);
                if (!current()) continue; // The tombstone also discarded this local ticket.
                const request = this.publicationRequest(head);
                if (request) {
                    try { await this.ap('publishReplica', request, head.bytes); }
                    catch (error) {
                        if (!current()) continue;
                        const code = (error as Errorish)?.code;
                        if ((code === 'world-participant-revoked' || code === 'ap-rejected' || code === 'world-entry-rejected')
                            && await this.local.rejected?.(head, code, current)) continue;
                        if (current()) throw error;
                        continue;
                    }
                }
                try {
                    if (request && current()) await this.local.published?.(head, current);
                    if (current()) await this.local.ack(head.ticket, current);
                } catch (error) {
                    // Local processing may have yielded to deletion after AP acknowledged.
                    if (current()) throw error;
                }
                if (!this.current(run)) return;
            }
            // A publication receipt is not proof that the receiver applied it.
            // Consume each stream through an explicit ordered checkpoint.
            for (const stream of run.streams) {
                if (!this.current(run)) return;
                if (stream.stopped || !stream.cursor || stream.setup || stream.checkpoint) continue;
                try { await this.checkpoint(run, stream); }
                catch (error) { this.failStream(run, stream, error, 'checkpoint'); if (!this.current(run)) return; }
            }
            if (!this.current(run)) return;
            if (run.dirty) continue;
            if (!run.applying) this.report(run, 'live');
            return;
        }
    }
    publicationRequest(head: ReplicaHead<Ticket>): ReplicaOperation | null {
        const scope: ReplicaScope = head.publication;
        if (scope.kind !== 'account' && scope.kind !== 'volume' || replicaSource(scope) !== head.source) {
            throw new Error('Invalid publication scope');
        }
        if (scope.kind === 'account' && head.database !== "index") throw new Error('Invalid account replica database');
        if (!head.bytes.length) return null;
        const account = scope.kind === 'account';
        return { ...operation(scope, true), operation_id: this.operationId(),
            op: account ? 'publish_account' : 'publish_replica',
            ...(!account ? { database_id: head.database } : {}) };
    }
    async checkpoint(run: LiveReplicationRun, stream: LiveReplicationStream): Promise<void> {
        if (stream.checkpoint) throw new Error('Replica checkpoint already pending');
        // The executor below runs synchronously, so both are assigned before use.
        let resolve!: () => void, reject!: (error: unknown) => void;
        const promise = new Promise<void>((ok, fail) => { resolve = ok; reject = fail; });
        // Start the deadline before the write. A held write or held receipt is
        // a protocol failure; an otherwise idle subscription has no timer.
        const timer = setTimeout(() => reject(failure(stream.scope.kind === 'entry' ? 'replica-gap' : 'ap-transport', 'Replica checkpoint response deadline exceeded')), this.timeout);
        stream.checkpoint = { resolve, reject };
        try {
            await Promise.all([promise, this.ap('checkpointReplica', stream.handle)]);
            if (!this.streamCurrent(run, stream)) throw new Error('Replica subscription retired');
        } finally { clearTimeout(timer); stream.checkpoint = null; }
    }
    async read(run: LiveReplicationRun, stream: LiveReplicationStream): Promise<void> {
        const cursor = stream.cursor;
        if (!cursor) throw new Error('Replica subscription has no cursor');
        while (this.streamCurrent(run, stream)) {
            const bytes = new Uint8Array(await this.ap('streamReplica', stream.handle));
            if (!this.streamCurrent(run, stream)) return;
            const event = replicaEvent(bytes);
            if (!cursor.accept(event)) continue;
            if (event.kind === 3) {
                const error: Error & { code?: string } = new Error('Replica subscription requires gap recovery'); error.code = 'replica-gap'; throw error;
            }
            if (event.kind === 4) {
                if (stream.scope.kind === 'account' || !event.terminal || event.terminal.cause === 4 && stream.scope.kind !== 'entry'
                    || event.terminal.cause === 4 && event.terminal.volume !== stream.scope.volume_id) throw new Error('Invalid authority terminal scope');
                await this.local.terminal(stream.source, event.terminal, () => this.streamCurrent(run, stream));
                if (!this.streamCurrent(run, stream)) return;
                throw event.terminal.cause === 2
                    ? failure('access-revoked', 'Replica participant authority revoked')
                    : event.terminal.cause === 1 && stream.scope.kind === 'entry'
                        ? failure('replica-authority', 'Volume authority requires authenticated catch-up')
                    : failure('replica-gap', 'Replica authority requires authenticated catch-up');
            }
            if (event.kind === 2) {
                if (!stream.checkpoint) throw new Error('Unrequested replica checkpoint');
                cursor.applied(event); stream.checkpoint.resolve();
                continue;
            }
            if (stream.scope.kind === 'account' && event.database !== "index") throw new Error('Invalid account replica database');
            if (stream.scope.kind === 'entry' && event.database !== 'index' && event.database !== 'revocations' && event.database !== stream.scope.entry_scope) throw new Error('Replica source outside selected entry');
            if (event.namespace && (stream.scope.kind !== 'entry' || event.namespace.entry !== stream.scope.entry_scope)) throw new Error('Namespace evidence outside selected stream');
            run.applying++; this.report(run, 'catching_up');
            try {
                if (event.kind === 6 && !event.delta.length) {
                    if (!this.local.catchUpLog) throw new Error('Append-log catch-up is unavailable');
                    await this.local.catchUpLog(stream.source, event.database, () => this.streamCurrent(run, stream));
                } else {
                    await this.local.apply(stream.source, event.database, event.delta, () => this.streamCurrent(run, stream), event.namespace?.evidence);
                }
                if (!this.streamCurrent(run, stream)) return;
                cursor.applied(event);
            } finally { run.applying--; }
            if (!run.publisher && !run.dirty && !run.applying) this.report(run, 'live');
        }
    }
}
