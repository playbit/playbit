import { AbiError } from "./guest-memory.ts";
import { GuestWork } from "./guest-work.ts";

// File admission retains source pins and destination reservations synchronously.
// Release runs once after accepted backend work is terminal, including teardown
// and failed delivery. A successful Take transfers any result-owned capability.
export interface DataLifetime<T> {
    accept(): void;
    release(value: T | undefined, claimed: boolean): void;
    delivery(): number;
    requalify?(): Promise<void>;
    retainedRevision: boolean;
}
interface Entry<T> {
    kind: number;
    revision: string | null;
    mutation: boolean;
    done: boolean;
    abandoned: boolean;
    abandonedStatus?: number;
    notified: boolean;
    qualifying?: boolean;
    qualificationError?: number;
    authorize: () => number;
    status: number;
    value?: T;
    lifetime?: DataLifetime<T>;
}
// One owner retains at most 64 operations and 16 MiB. Begin only copies input
// and schedules work; take cannot run a backend or repeat an uncertain write.
export class GuestData<T> {
    private readonly entries = new Map<bigint, Entry<T>>();
    private disposed = false;
    private mutations = Promise.resolve();
    private logs = new Map<string, Promise<void>>();
    private changed: (() => void)[] = [];
    private readonly ready: () => void;
    private readonly revision: () => string;
    readonly work: GuestWork;
    constructor(work: GuestWork, ready: () => void, revision: () => string) {
        this.work = work; this.ready = ready; this.revision = revision;
    }
    begin(kind: number, credit: number, execute: () => Promise<{ status: number; value?: T }>, authorize: () => number, lifetime?: DataLifetime<T>, independentRevision=false, log?: string): bigint {
        if (this.disposed) throw new AbiError(-5);
        const authority = authorize(); if (authority) throw new AbiError(authority);
        const mutation = this.work.mutation(kind), slot = this.work.begin(kind, credit);
        try { lifetime?.accept(); }
        catch (error) {
            // Admission failed before any accepted I/O. Do not mark a rejected
            // operation as a failed mutation or leave a Work reservation behind.
            this.work.release(slot, 0, false);
            throw error;
        }
        const entry: Entry<T> = { kind, revision: independentRevision?null:this.revision(), mutation, done: false, abandoned: false, notified: false, authorize, status: -6, lifetime };
        this.entries.set(slot, entry);
        const run = () => Promise.resolve().then(() => {
            if (kind === 13 && entry.abandoned) throw new AbiError(-13);
            const authority = authorize(); if (authority) throw new AbiError(authority);
            return execute();
        }).then(result => {
            entry.status = result.status;
            entry.value = result.value; entry.done = true;
            this.work.complete(slot, entry.status);
        }, error => {
            entry.status = error instanceof AbiError ? error.status : -5;
            entry.done = true;
            this.work.complete(slot, entry.status);
        }).then(() => {
            if (this.disposed || entry.abandoned) this.remove(slot, entry.abandonedStatus ?? entry.status, false);
            else { this.flushReady(); this.ready(); }
            for (const resolve of this.changed.splice(0)) resolve();
        });
        if (log !== undefined) {
            const result = (this.logs.get(log) ?? Promise.resolve()).then(run);
            this.logs.set(log, result);
            void result.finally(() => { if (this.logs.get(log) === result) this.logs.delete(log); });
        } else if (mutation) this.mutations = this.mutations.then(run);
        else void Promise.resolve().then(run);
        return slot;
    }
    take(generation: bigint, slot: number, kind: number, output: (value: T) => number | bigint): number | bigint {
        const handle = (generation << 32n) | BigInt(slot);
        const entry = this.entry(handle, kind);
        this.work.take(handle, kind);
        if (entry.qualifying) return -6;
        let status: number | bigint = entry.qualificationError ?? (entry.lifetime ? entry.lifetime.delivery() : entry.mutation ? 0 : entry.authorize());
        if (status === -6) {
            if (entry.done && entry.lifetime?.requalify) this.requalify(handle, entry);
            return -6;
        }
        if (status) { this.remove(handle, status, true); return status; }
        if (!entry.done) return -6;
        status = entry.status;
        // Match native delivery: an obsolete successful read is Unavailable,
        // while the operation handle remains valid until this terminal Take.
        // Preserve backend errors and known mutation acknowledgments.
        const invalidated = status === 0 && !entry.mutation && !entry.lifetime?.retainedRevision
            && entry.revision !== null && entry.revision !== this.revision();
        if (invalidated) status = -5;
        if (!status) {
            if (entry.value === undefined) throw new Error("Missing retained data result");
            status = output(entry.value);
        }
        if (status !== -11 && status !== -6) {
            this.remove(handle, typeof status === "bigint" ? 0 : status, true, invalidated);
        }
        return status;
    }
    admit(generation: bigint, slot: number, kind: number): void { this.entry((generation << 32n) | BigInt(slot), kind); }
    disposeOperation(generation: bigint, slot: number): number {
        const handle = (generation << 32n) | BigInt(slot);
        const entry = this.entry(handle);
        this.work.abandon(handle);
        // Abandon delivery only. Accepted I/O and mutation ordering keep their
        // owner and quota until the terminal result is known.
        if (entry.done && !entry.qualifying) this.remove(handle, entry.status, false);
        else entry.abandoned = true;
        return 0;
    }
    private requalify(slot: bigint, entry: Entry<T>): void {
        entry.qualifying = true;
        entry.qualificationError = undefined;
        void Promise.resolve().then(() => entry.lifetime!.requalify!()).catch(error => {
            entry.qualificationError = error instanceof AbiError ? error.status : -5;
        }).then(() => {
            entry.qualifying = false;
            if (this.disposed || entry.abandoned) this.remove(slot, entry.abandonedStatus ?? entry.status, false);
            else {
                // Reuse an outstanding readiness event, or rearm after the guest
                // consumed it. Retained bytes and operation identity are unchanged.
                entry.notified = this.work.events.some(event => event.operation === slot);
                this.flushReady(); this.ready();
            }
            for (const resolve of this.changed.splice(0)) resolve();
        });
    }
    private entry(handle: bigint, kind = 0): Entry<T> {
        this.work.check(handle, kind);
        const entry = this.entries.get(handle);
        if (!entry) throw new Error("Rust admitted an operation without a retained payload");
        return entry;
    }
    private remove(slot: bigint, status: number, delivered: boolean, invalidated = false): void {
        const entry = this.entries.get(slot);
        if (entry) {
            if (entry.done && !entry.qualifying) {
                // The guest retries invalidated reads; they do not represent failed mutations.
                entry.lifetime?.release(entry.value, delivered && status === 0);
                this.work.release(slot, invalidated || status === -8 && !entry.mutation ? 0 : status, delivered);
                this.entries.delete(slot);
            }
            else {
                if (!entry.abandoned) this.work.abandon(slot);
                entry.abandoned = true; entry.abandonedStatus = status;
            }
        }
    }
    flushReady(): void {
        for (const [slot, entry] of this.entries) {
            if (entry.done && !entry.qualifying && !entry.notified && !entry.abandoned) entry.notified = this.work.ready(slot, entry.kind);
        }
    }
    get failed(): boolean { return this.work.failed; }
    get pending(): number { return [...this.entries.values()].filter(entry => !entry.abandoned).length; }
    get waiting(): boolean { return [...this.entries.values()].some(entry => !entry.done || entry.qualifying); }
    wait(): Promise<void> { return new Promise(resolve => this.changed.push(resolve)); }
    // Replacement requires the previous owner to drain before changing its epoch.
    reset(): void {
        if (this.entries.size) throw new Error("The previous app still has accepted work");
        this.work.reset(); this.disposed = false;
    }
    dispose(): void {
        this.disposed = true;
        for (const [slot, entry] of this.entries) {
            if (!entry.abandoned) this.work.abandon(slot);
            entry.abandoned = true;
            if (entry.done && !entry.qualifying) this.remove(slot, entry.status, false);
        }
    }
}
