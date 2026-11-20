// The web browser API shape actually used by AP; both transports carry the same
// authentication and length-prefixed AP frames on independent byte streams.
export interface ApTransport {
    ready: Promise<unknown>;
    closed: Promise<unknown>;
    close(): void;
    createBidirectionalStream(): Promise<{ readable: ReadableStream<Uint8Array>; writable: WritableStream<Uint8Array> }>;
}

const OPEN = 0, DATA = 1, FIN = 2, STOP = 3, CREDIT = 4;
const WINDOW = 32768, CHUNK = 16384, MAX_STREAMS = 1024, CONNECTION_WINDOW = 2 * 1024 * 1024;
export const WS_PROTOCOL = "playbit-ap-2";

function packet(kind: number, id: number, body: Uint8Array = new Uint8Array()): Uint8Array<ArrayBuffer> {
    const bytes = new Uint8Array(5 + body.length);
    bytes[0] = kind;
    new DataView(bytes.buffer).setUint32(1, id, true);
    bytes.set(body, 5);
    return bytes;
}

interface Stream {
    id: number; queue: Uint8Array[]; receiveCredit: number; sendCredit: number;
    receivedFin: boolean; sentFin: boolean; cancelled: boolean; stopped: boolean;
    readWake?: () => void; writeWake?: () => void;
}

export class WebSocketTransport implements ApTransport {
    readonly ready: Promise<void>;
    readonly closed: Promise<void>;
    private socket: WebSocket;
    private streams = new Map<number, Stream>();
    private nextId = 1;
    private receiveCredit = CONNECTION_WINDOW;
    private sendCredit = CONNECTION_WINDOW;
    private ended = false;
    private finish!: () => void;
    private rejectReady!: (error: Error) => void;

    constructor(url: string, socketFactory = (url: string) => new WebSocket(url, WS_PROTOCOL)) {
        this.socket = socketFactory(url);
        this.socket.binaryType = "arraybuffer";
        this.closed = new Promise(resolve => { this.finish = resolve; });
        this.ready = new Promise((resolve, reject) => {
            this.rejectReady = reject;
            this.socket.onopen = () => {
                if (this.socket.protocol !== WS_PROTOCOL) return this.close();
                resolve();
            };
        });
        this.ready.catch(() => {});
        this.socket.onerror = () => this.close();
        this.socket.onclose = () => this.close();
        this.socket.onmessage = event => {
            try { this.receive(event.data); } catch { this.close(); }
        };
    }

    close(): void {
        if (this.ended) return;
        this.ended = true;
        this.rejectReady(new Error("AP WebSocket closed"));
        for (const stream of this.streams.values()) {
            stream.queue = [];
            stream.readWake?.(); stream.writeWake?.();
        }
        this.streams.clear();
        this.socket.close();
        this.finish();
    }

    private send(kind: number, id: number, body?: Uint8Array): void {
        if (this.ended) throw new Error("AP WebSocket closed");
        if (this.socket.bufferedAmount + 5 + (body?.length ?? 0) > 2 * 1024 * 1024) {
            this.close();
            throw new Error("AP WebSocket output limit");
        }
        this.socket.send(packet(kind, id, body));
    }

    private retire(stream: Stream): void {
        if (stream.sentFin && stream.receivedFin && !stream.queue.length) this.streams.delete(stream.id);
    }

    private receive(value: unknown): void {
        if (!(value instanceof ArrayBuffer) || value.byteLength < 5 || value.byteLength > CHUNK + 5) throw new Error("Invalid AP WebSocket packet");
        const bytes = new Uint8Array(value), kind = bytes[0], id = new DataView(value).getUint32(1, true);
        const body = bytes.subarray(5);
        if (id === 0 && kind === CREDIT && body.length === 4) {
            const credit = new DataView(value).getUint32(5, true);
            if (!credit || credit > CONNECTION_WINDOW - this.sendCredit) throw new Error("Invalid connection credit");
            this.sendCredit += credit;
            for (const stream of this.streams.values()) stream.writeWake?.();
            return;
        }
        if (!id || id >= this.nextId || kind < DATA || kind > CREDIT) throw new Error("Invalid AP WebSocket stream");
        if ((kind === DATA && !body.length) || ((kind === FIN || kind === STOP) && body.length)
            || (kind === CREDIT && body.length !== 4)) throw new Error("Invalid AP WebSocket packet");
        const stream = this.streams.get(id);
        // FIN/STOP/credit can cross in flight while both halves retire.
        if (kind === DATA) {
            if (body.length > this.receiveCredit) throw new Error("AP WebSocket exceeded connection credit");
            this.receiveCredit -= body.length;
        }
        if (!stream) { if (kind === DATA) this.returnConnectionCredit(body.length); return; }
        if (kind === DATA) {
            if (stream.receivedFin || body.length > stream.receiveCredit) throw new Error("AP WebSocket exceeded credit");
            stream.receiveCredit -= body.length;
            if (!stream.cancelled) {
                if (stream.queue.length >= 256) throw new Error("AP WebSocket fragment limit");
                stream.queue.push(body);
            }
            if (stream.cancelled) this.returnConnectionCredit(body.length);
            stream.readWake?.();
        } else if (kind === FIN) {
            if (stream.receivedFin) throw new Error("Duplicate AP WebSocket FIN");
            stream.receivedFin = true;
            stream.readWake?.();
        } else if (kind === STOP) {
            stream.stopped = true;
            this.endWrite(stream);
            stream.writeWake?.();
        } else {
            const credit = new DataView(value).getUint32(5, true);
            if (!credit || stream.sendCredit + credit > WINDOW) throw new Error("Invalid AP WebSocket credit");
            stream.sendCredit += credit;
            stream.writeWake?.();
        }
        this.retire(stream);
    }

    private returnConnectionCredit(count: number): void {
        if (!count || this.ended) return;
        this.receiveCredit += count;
        if (this.receiveCredit > CONNECTION_WINDOW) throw new Error("Invalid returned connection credit");
        const credit = new Uint8Array(4);
        new DataView(credit.buffer).setUint32(0, count, true);
        this.send(CREDIT, 0, credit);
    }

    private endWrite(stream: Stream): void {
        if (stream.sentFin || this.ended) return;
        stream.sentFin = true;
        this.send(FIN, stream.id);
        this.retire(stream);
    }

    async createBidirectionalStream(): Promise<{ readable: ReadableStream<Uint8Array>; writable: WritableStream<Uint8Array> }> {
        await this.ready;
        if (this.ended || this.streams.size >= MAX_STREAMS || this.nextId > 0xffffffff) throw new Error("AP WebSocket stream limit or closure");
        const stream: Stream = { id: this.nextId++, queue: [], receiveCredit: WINDOW, sendCredit: WINDOW,
            receivedFin: false, sentFin: false, cancelled: false, stopped: false };
        this.streams.set(stream.id, stream);
        this.send(OPEN, stream.id);
        const readable = new ReadableStream<Uint8Array>({
            pull: async controller => {
                while (!this.ended && !stream.receivedFin && !stream.queue.length && !stream.cancelled) {
                    await new Promise<void>(resolve => { stream.readWake = resolve; });
                    stream.readWake = undefined;
                }
                if (this.ended) throw new Error("AP WebSocket closed");
                if (stream.cancelled) return;
                const bytes = stream.queue.shift();
                if (bytes) {
                    stream.receiveCredit += bytes.length;
                    const credit = new Uint8Array(4);
                    new DataView(credit.buffer).setUint32(0, bytes.length, true);
                    this.send(CREDIT, stream.id, credit);
                    this.returnConnectionCredit(bytes.length);
                    controller.enqueue(bytes);
                } else controller.close();
                this.retire(stream);
            },
            cancel: () => {
                this.returnConnectionCredit(stream.queue.reduce((count, bytes) => count + bytes.length, 0));
                stream.cancelled = true; stream.queue = []; stream.readWake?.();
                if (!this.ended && !stream.receivedFin) this.send(STOP, stream.id);
                this.retire(stream);
            },
        }, { highWaterMark: 0 });
        const writable = new WritableStream<Uint8Array>({
            start: controller => controller.signal.addEventListener("abort", () => {
                stream.stopped = true; stream.writeWake?.(); this.endWrite(stream);
            }, { once: true }),
            write: async bytes => {
                for (let offset = 0; offset < bytes.length;) {
                    while (!this.ended && !stream.stopped && (!stream.sendCredit || !this.sendCredit)) {
                        await new Promise<void>(resolve => { stream.writeWake = resolve; });
                        stream.writeWake = undefined;
                    }
                    if (this.ended || stream.stopped || stream.sentFin) throw new Error("AP WebSocket writer closed");
                    const count = Math.min(CHUNK, stream.sendCredit, this.sendCredit, bytes.length - offset);
                    this.send(DATA, stream.id, bytes.subarray(offset, offset + count));
                    stream.sendCredit -= count; this.sendCredit -= count; offset += count;
                }
            },
            close: () => this.endWrite(stream),
            abort: () => this.endWrite(stream),
        });
        return { readable, writable };
    }
}

// Select once, before AP authentication. A failed authenticated session goes
// through the account's existing reconnect path, never transport negotiation.
export class WebApTransport implements ApTransport {
    readonly ready: Promise<void>;
    readonly closed: Promise<void>;
    private selected?: ApTransport;
    private ended = false;
    private finish!: () => void;

    constructor(url: string, webTransport = () => new WebTransport(url),
        webSocket = () => new WebSocketTransport(url.replace(/^https:/, "wss:")),
        timeout = 3000) {
        this.closed = new Promise(resolve => { this.finish = resolve; });
        this.ready = (async () => {
            try {
                this.selected = webTransport();
                await this.establish(this.selected, timeout);
            } catch {
                this.selected?.close();
                if (this.ended) throw new Error("AP transport closed");
                this.selected = webSocket();
                await this.establish(this.selected, 7000);
            }
            if (this.ended) throw new Error("AP transport closed");
            this.selected.closed.then(() => this.close(), () => this.close());
        })().catch(error => { this.close(); throw error; });
        this.ready.catch(() => {});
    }

    private async establish(transport: ApTransport, milliseconds: number): Promise<void> {
        // Consume the losing attempt's rejection even when its ready rejects first.
        transport.closed.catch(() => {});
        let timer: ReturnType<typeof setTimeout> | undefined;
        try {
            await Promise.race([transport.ready, this.closed.then(() => { throw new Error("AP transport closed"); }),
                new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error("AP connection timed out")), milliseconds); })]);
        } finally { clearTimeout(timer); }
    }

    close(): void {
        if (this.ended) return;
        this.ended = true; this.selected?.close(); this.finish();
    }

    async createBidirectionalStream() {
        await this.ready;
        if (this.ended || !this.selected) throw new Error("AP transport closed");
        return this.selected.createBidirectionalStream();
    }
}
