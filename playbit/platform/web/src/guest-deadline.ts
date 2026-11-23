// A parked Wasm stack retains its guest memory, but never the worker's local
// execution queue. Each admitted segment ends on return, trap, or the next wait.
export const INDEFINITE = 0xffff_ffff_ffff_ffffn;
export const clockNs = (): bigint => BigInt(Math.trunc(performance.now() * 1_000_000));
interface Segment { finish(): void; fail(reason: unknown): void }
interface Wait { deadline: bigint; resume(): void; timer?: ReturnType<typeof setTimeout> }
export class GuestDeadline {
    private segment?: Segment;
    private wait?: Wait;
    private retired = false;
    readonly main: () => Promise<void>;
    constructor(main: () => Promise<void>) { this.main = main; }
    get parked(): boolean { return Boolean(this.wait); }

    canEnter(hasEvents: boolean): boolean {
        const wait = this.wait;
        return !this.retired && (!wait || hasEvents || (wait.deadline !== INDEFINITE && clockNs() >= wait.deadline));
    }
    enter(): Promise<void> {
        if (this.retired) throw new Error("Guest deadline owner retired");
        if (this.segment) throw new Error("Guest deadline turn already active");
        const wait = this.wait;
        return new Promise<void>((finish, fail) => {
            this.segment = { finish, fail };
            if (wait) {
                this.wait = undefined;
                clearTimeout(wait.timer);
                wait.resume();
            } else {
                try { this.main().then(() => this.end(), reason => this.end(reason)); }
                catch (reason) { this.end(reason); }
            }
        });
    }
    private end(reason?: unknown): void {
        const segment = this.segment;
        this.segment = undefined;
        if (!segment) throw new Error("Guest deadline turn is missing");
        if (reason === undefined) segment.finish(); else segment.fail(reason);
    }
    suspend(deadline: bigint, ready: () => void): Promise<void> {
        if (!this.segment || this.wait) throw new Error("Invalid guest deadline wait");
        return new Promise<void>(resume => {
            const wait: Wait = { deadline, resume };
            this.wait = wait;
            if (deadline !== INDEFINITE) {
                const arm = () => {
                    if (this.wait !== wait) return;
                    const remaining = deadline - clockNs();
                    if (remaining <= 0n) { ready(); return; }
                    // Browsers represent timeouts as signed 32-bit milliseconds.
                    const delay = Number((remaining + 999_999n) / 1_000_000n);
                    wait.timer = setTimeout(arm, Math.min(0x7fff_ffff, delay));
                };
                arm();
            }
            this.end();
        });
    }
    deactivate(): void {
        // Only already accepted events may resume this owner while draining.
        if (this.wait) {
            clearTimeout(this.wait.timer);
            this.wait.deadline = INDEFINITE;
        }
    }
    retire(): void {
        if (this.segment) throw new Error("Cannot retire an executing guest");
        clearTimeout(this.wait?.timer);
        // Drop the only handle that could resume this stack. Rejecting the
        // import would run old guest exception handlers after its owner retires.
        this.wait = undefined;
        this.retired = true;
    }
}
