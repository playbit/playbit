import { AbiError } from "./guest-memory.ts";

export interface GuestEvent { kind: number; target: number; objectId?: number; data: Uint8Array }
export interface QueuedEvent extends GuestEvent { key: bigint; operation?: bigint }
export interface WorkExports {
    pb_web_guest_input(bytes: number): bigint;
    pb_web_guest_queue(root: bigint, kind: number, bytes: number): bigint;
    pb_web_guest_reject(root: bigint): number;
    pb_web_guest_begin(kind: number, bytes: bigint): bigint;
    pb_web_guest_ready(slot: bigint): bigint;
    pb_web_guest_poll(key: bigint): number;
    pb_web_guest_take(slot: bigint, kind: number): number;
    pb_web_guest_release(slot: bigint, status: number, delivered: number): number;
    pb_web_guest_check(handle: bigint, kind: number): number;
    pb_web_guest_abandon(handle: bigint): number;
    pb_web_guest_complete(handle: bigint, status: number): number;
    pb_web_guest_epoch(): bigint;
    pb_web_guest_turn(begin: number): number;
    pb_web_guest_deactivate(): number;
    pb_web_guest_cancel_volume_changes(): number;
    pb_web_guest_cancel_wake(): number;
    pb_web_guest_reset(): number;
    pb_web_guest_state(): number;
    pb_web_guest_mutation(kind: number): number;
}
function checked(status: number): number {
    if (status < 0) throw new AbiError(status);
    return status;
}
// Rust owns bounded admission and ancestry. This adapter retains payloads only.
export class GuestWork {
    readonly events: QueuedEvent[];
    private readonly host: () => WorkExports;
    constructor(events: QueuedEvent[], host: () => WorkExports) { this.events = events; this.host = host; }
    input(bytes: number): bigint {
        const root = this.host().pb_web_guest_input(bytes);
        if (root < 0n) throw new AbiError(Number(root));
        return root;
    }
    queueAccepted(root: bigint, event: GuestEvent): void {
        const key = this.host().pb_web_guest_queue(root, event.kind, event.data.length);
        if (key < 0n) throw new AbiError(Number(key));
        this.events.push({ ...event, key });
    }
    queue(event: GuestEvent): void {
        const root = this.input(event.data.length);
        try { this.queueAccepted(root, event); }
        catch (error) { this.reject(root); throw error; }
    }
    reject(root: bigint): void { checked(this.host().pb_web_guest_reject(root)); }
    begin(kind: number, credit: number): bigint {
        if (!Number.isSafeInteger(credit) || credit < 0) throw new AbiError(-3);
        const handle = this.host().pb_web_guest_begin(kind, BigInt(credit));
        if (handle < 0n) throw new AbiError(Number(handle));
        return handle;
    }
    ready(slot: bigint, kind: number): boolean {
        const key = this.host().pb_web_guest_ready(slot);
        if (key === -9n) return false; // Keep the completed result; retry notification after a turn.
        if (key < 0n) throw new AbiError(Number(key));
        const data = new Uint8Array(8), view = new DataView(data.buffer);
        view.setBigUint64(0, slot, true);
        this.events.push({ key, operation: slot, kind: kind === 3 ? 7 : 16, target: 0, data });
        return true;
    }
    poll(): QueuedEvent | undefined {
        const event = this.events[0];
        if (!event) return;
        checked(this.host().pb_web_guest_poll(event.key));
        this.events.shift();
        return event;
    }
    take(slot: bigint, kind: number): void { checked(this.host().pb_web_guest_take(slot, kind)); }
    release(slot: bigint, status: number, delivered: boolean): void {
        checked(this.host().pb_web_guest_release(slot, status, Number(delivered)));
        for (let i = this.events.length - 1; i >= 0; i--) {
            if (this.events[i].operation === slot) this.events.splice(i, 1);
        }
    }
    check(handle: bigint, kind = 0): void { checked(this.host().pb_web_guest_check(handle, kind)); }
    complete(handle: bigint, status: number): void { checked(this.host().pb_web_guest_complete(handle, status)); }
    abandon(handle: bigint): void {
        checked(this.host().pb_web_guest_abandon(handle));
        // Rust invalidates readiness immediately; accepted I/O keeps its owner until release.
        for (let i = this.events.length - 1; i >= 0; i--) {
            if (this.events[i].operation === handle) this.events.splice(i, 1);
        }
    }
    get epoch(): bigint { return this.host().pb_web_guest_epoch(); }
    beginTurn(): void { checked(this.host().pb_web_guest_turn(1)); }
    endTurn(): void { checked(this.host().pb_web_guest_turn(0)); }
    cancelVolumeChanges(): void {
        checked(this.host().pb_web_guest_cancel_volume_changes());
        for (let i = this.events.length - 1; i >= 0; i--) {
            if (this.events[i].kind === 17) this.events.splice(i, 1);
        }
    }
    cancelWake(): void {
        const index = this.events.findIndex(event => event.kind === 10);
        if (index < 0) return;
        checked(this.host().pb_web_guest_cancel_wake());
        this.events.splice(index, 1);
    }
    deactivate(): void {
        checked(this.host().pb_web_guest_deactivate());
        for (let i = this.events.length - 1; i >= 0; i--) {
            if (this.events[i].kind === 10 || this.events[i].kind === 13 || this.events[i].kind === 17) this.events.splice(i, 1);
        }
    }
    reset(): void { checked(this.host().pb_web_guest_reset()); this.events.length = 0; }
    mutation(kind: number): boolean { return checked(this.host().pb_web_guest_mutation(kind)) === 1; }
    get pending(): boolean { return (this.host().pb_web_guest_state() & 1) !== 0; }
    get failed(): boolean { return (this.host().pb_web_guest_state() & 2) !== 0; }
    get draining(): boolean { return (this.host().pb_web_guest_state() & 4) !== 0; }
}
