import { validId } from './identifier.ts';
// Local metadata invalidation only. AP replication still carries actual deltas.
const validAccount = (account: unknown): account is string => typeof account === 'string' && validId(account, 16);

// The frame a watched port receives; it answers with acknowledge(port, ticket).
export interface AccountNotice { event: 'account'; account: string; ticket: number }
// Anything that can receive a notice (a MessagePort in the AP SharedWorker).
export interface AccountNoticePort { postMessage(message: AccountNotice): void }
export interface AccountObserver { account: string; ticket: number; busy: boolean; dirty: boolean }

export class AccountNotices {
    observers: Map<AccountNoticePort, AccountObserver>;
    constructor() { this.observers = new Map(); }
    watch(port: AccountNoticePort, account: string): void {
        if (!validAccount(account)) throw Error('Invalid account notice identity');
        const existing = this.observers.get(port);
        if (existing) {
            if (existing.account !== account) throw Error('Account observer belongs to another account');
            existing.dirty = true; this.deliver(port, existing);
            return;
        }
        if (this.observers.size >= 32) throw Error('Too many open account observers; close another Playbit tab');
        const observer: AccountObserver = { account, ticket: 0, busy: false, dirty: true };
        this.observers.set(port, observer);
        this.deliver(port, observer);
    }
    changed(account: string): void {
        if (!validAccount(account)) throw Error('Invalid account notice identity');
        for (const [port, observer] of this.observers) {
            if (observer.account !== account) continue;
            observer.dirty = true;
            this.deliver(port, observer);
        }
    }
    acknowledge(port: AccountNoticePort, ticket: number): void {
        const observer = this.observers.get(port);
        if (!observer || !observer.busy || ticket !== observer.ticket) throw Error('Invalid account notice acknowledgment');
        observer.busy = false;
        this.deliver(port, observer);
    }
    detach(port: AccountNoticePort): void { this.observers.delete(port); }
    deliver(port: AccountNoticePort, observer: AccountObserver): void {
        if (observer.busy || !observer.dirty) return;
        // Retire rather than wrap a ticket and accept a stale acknowledgment.
        if (observer.ticket === Number.MAX_SAFE_INTEGER) {
            this.detach(port);
            throw Error('Account observer ticket exhausted; reopen this tab');
        }
        observer.busy = true; observer.dirty = false; observer.ticket++;
        try { port.postMessage({ event: 'account', account: observer.account, ticket: observer.ticket }); }
        catch { this.detach(port); }
    }
}

export type AccountSignalSender = (account: string) => unknown;
export type AccountSignalFailure = (error: unknown) => void;

// Notification transport is off the commit path. A failed send stays dirty for
// the next explicit wake; it cannot roll back or misreport a committed decision.
export class AccountSignal {
    send: AccountSignalSender;
    failed: AccountSignalFailure;
    busy: boolean;
    dirty: boolean;
    closed: boolean;
    // Absent until the first accepted notify; never an own property before that.
    declare account: string | undefined;
    constructor(send: AccountSignalSender, failed: AccountSignalFailure) { this.send = send; this.failed = failed; this.busy = false; this.dirty = false; this.closed = false; }
    notify(account: string): void {
        if (this.closed) return;
        if (!validAccount(account) || this.account && this.account !== account) {
            this.failed(Error('Account notice belongs to another account')); return;
        }
        this.account = account; this.dirty = true;
        if (!this.busy) void this.flush();
    }
    async flush(): Promise<void> {
        this.busy = true;
        try {
            while (this.dirty && !this.closed) {
                this.dirty = false;
                // flush only runs from notify, after account has been accepted above.
                await this.send(this.account!);
            }
        } catch (error) { this.dirty = true; if (!this.closed) this.failed(error); }
        finally { this.busy = false; }
    }
    close(): void { this.closed = true; this.dirty = false; }
}
