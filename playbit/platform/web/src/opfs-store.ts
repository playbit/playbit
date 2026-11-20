// Worker-owned append-only storage. RAM holds bounded key/location indexes;
// entry bodies and unfinished transactions stay in dedicated OPFS spool files.
import { requireStorageFormat } from './storage-format.ts';
export const IO_CHUNK = 4096;
// kvdb logical frame minus 8-byte frame header, 8-byte node header,
// 6-byte entry lengths and the maximum 511-byte user key. Includes leaf header.
export const MAX_VALUE = 64 * 1024 * 1024 - (8 + 8 + 6 + 511);
const MAX_BATCH = 0xffffffff;
const KEY_MAX = 512;
const ENTRY_CREDIT = 2560;
const RECOVERY_BYTES = 64 * 1024, RECOVERY_ROWS = 64, ORDER_COMPARISONS = 512, RECOVERY_LIMIT = 2;
const BOOKKEEPING_BYTES = 128 * 1024 * 1024;
const HEX_DIGITS = "0123456789abcdef";
// A value body: `length` bytes at `offset` in a database or spool file.
export type OpfsLocation = { file: FileSystemSyncAccessHandle; offset: number; length: number };
// A transaction spool, owned by at most one write transaction at a time.
export type OpfsWriterSpool = { file: FileSystemSyncAccessHandle; busy: boolean };
// A synchronization spool; `length` tracks the bytes staged so far.
export type OpfsStagingSpool = { file: FileSystemSyncAccessHandle; busy: boolean; length: number };
// A pinned snapshot value that outlives its read transaction until closed.
export type OpfsReader = { length: number; read: (offset: number, out: Uint8Array) => number; close: () => void };
// A key/value pair returned by ordered scans.
export type OpfsEntry = { key: Uint8Array; value: Uint8Array | null };
export type OpfsWriteFence = { begin(): void; end(): void };
type OpfsChange = { value: OpfsLocation | null; previous: OpfsLocation | null; final?: OpfsLocation | null };
type OpfsPending = { key: string; length: number; offset: number; written: number; fresh: boolean };
type OpfsVisitor = (key: string, value: OpfsLocation | null) => void;
const keyHex = (bytes: Uint8Array): string => {
    if (!bytes.length || bytes.length > KEY_MAX) throw new Error("Invalid database key");
    let key = "";
    for (const byte of bytes) key += HEX_DIGITS[byte >>> 4] + HEX_DIGITS[byte & 15];
    return key;
};
const fromHex = (value: string): Uint8Array => {
    const bytes = new Uint8Array(value.length / 2);
    for (let i = 0; i < bytes.length; i++) {
        // Only keys produced by keyHex enter the ordered index.
        const high = value.charCodeAt(i * 2), low = value.charCodeAt(i * 2 + 1);
        bytes[i] = ((high <= 57 ? high - 48 : high - 87) << 4)
            | (low <= 57 ? low - 48 : low - 87);
    }
    return bytes;
};
const hashBytes = (hash: number, bytes: Uint8Array): number => {
    for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193) >>> 0;
    return hash;
};
const lowerBound = (keys: string[], key: string): number => {
    let lo = 0, hi = keys.length;
    while (lo < hi) { const mid = (lo + hi) >>> 1; if (keys[mid] < key) lo = mid + 1; else hi = mid; }
    return lo;
};
function readExact(handle: FileSystemSyncAccessHandle, offset: number, out: Uint8Array): void {
    if (handle.read(out, { at: offset }) !== out.length) throw new Error("Database read ended early");
}
function writeExact(handle: FileSystemSyncAccessHandle, offset: number, bytes: Uint8Array): void {
    if (handle.write(bytes, { at: offset }) !== bytes.length) throw new Error("Database is full");
}
function locationRead(location: OpfsLocation, offset: number, out: Uint8Array): number {
    if (!Number.isSafeInteger(offset) || offset < 0 || offset > location.length) throw new Error("Invalid value offset");
    const n = Math.min(out.length, location.length - offset);
    readExact(location.file, location.offset + offset, out.subarray(0, n));
    return n;
}

export class OpfsResources {
    writers: OpfsWriterSpool[];
    staging: OpfsStagingSpool[];
    bytes: number;
    stores: number;
    transactions: number;
    pins: number;
    recoveries = 0;
    constructor(writerFiles: FileSystemSyncAccessHandle[], stagingFiles: FileSystemSyncAccessHandle[]) {
        if (writerFiles.length !== 2) throw new Error("Two transaction spools are required");
        this.writers = writerFiles.map(file => ({ file, busy: false }));
        if (stagingFiles.length !== 2) throw new Error("Two synchronization spools are required");
        this.staging = stagingFiles.map(file => ({ file, busy: false, length: 0 }));
        this.bytes = 0; this.stores = 0; this.transactions = 0; this.pins = 0;
    }
    reserve(bytes: number): void {
        if (!Number.isSafeInteger(bytes) || bytes < 0 || this.bytes + bytes > BOOKKEEPING_BYTES) throw new Error("Database bookkeeping capacity exceeded");
        this.bytes += bytes;
    }
    release(bytes: number): void { this.bytes -= bytes; if (this.bytes < 0) throw new Error("Database credit underflow"); }
    writer(): OpfsWriterSpool {
        requireStorageFormat();
        const spool = this.writers.find(item => !item.busy);
        if (!spool) throw new Error("Database writer capacity exceeded");
        spool.file.truncate(0); spool.busy = true;
        return spool;
    }
    close(): void { for (const spool of [...this.writers, ...this.staging]) spool.file.close(); }
}

export class OpfsStore {
    handle: FileSystemSyncAccessHandle;
    resources: OpfsResources;
    index: Map<string, OpfsLocation>;
    keys: string[];
    end: number;
    closed: boolean;
    // Absent until the first write transaction; tests observe it as undefined.
    declare writer: boolean | undefined;
    fence?: OpfsWriteFence;
    private constructor(handle: FileSystemSyncAccessHandle, resources: OpfsResources, fence?: OpfsWriteFence) {
        if (resources.stores >= 96) { handle.close(); throw new Error("Database handle capacity exceeded"); }
        try { resources.reserve(512); } catch (error) { handle.close(); throw error; }
        resources.stores++;
        this.handle = handle; this.resources = resources; this.fence = fence;
        this.index = new Map(); this.keys = []; this.end = 0; this.closed = false;
    }
    static empty(handle: FileSystemSyncAccessHandle, resources: OpfsResources, fence?: OpfsWriteFence): OpfsStore {
        if (handle.getSize()) { handle.close(); throw new Error('Expected an empty database'); }
        return new OpfsStore(handle, resources, fence);
    }
    static async open(handle: FileSystemSyncAccessHandle, resources: OpfsResources,
        yieldReady: () => Promise<void>, fence?: OpfsWriteFence): Promise<OpfsStore> {
        if (resources.recoveries >= RECOVERY_LIMIT) { handle.close(); throw new Error('Database recovery capacity exceeded'); }
        const store = new OpfsStore(handle, resources, fence);
        resources.recoveries++;
        try {
            for (const _ of store.recover()) {
                await yieldReady();
                if (store.closed) throw new Error('Database recovery retired');
            }
            return store;
        } catch (error) { store.close(); throw error; }
        finally { resources.recoveries--; }
    }
    readAt(offset: number, length: number): Uint8Array {
        const bytes = new Uint8Array(length); readExact(this.handle, offset, bytes); return bytes;
    }
    *visitBatch(offset: number, length: number, visit: OpfsVisitor): Generator<void> {
        const countBytes = this.readAt(offset, 4);
        let count = new DataView(countBytes.buffer).getUint32(0, true), position = 4;
        // Even a deletion has an 8-byte header and a nonempty key.
        if (count > Math.floor((length - 4) / 9)) throw new Error("Corrupt database entry count");
        const header = new Uint8Array(8), key = new Uint8Array(KEY_MAX);
        while (count--) {
            if (position + 8 > length) throw new Error("Corrupt database batch");
            readExact(this.handle, offset + position, header);
            const view = new DataView(header.buffer), keyLength = view.getUint32(0, true), valueLength = view.getUint32(4, true);
            position += 8;
            const deleted = valueLength === 0xffffffff;
            if (!keyLength || keyLength > KEY_MAX || (!deleted && valueLength > MAX_VALUE) || position + keyLength + (deleted ? 0 : valueLength) > length) throw new Error("Corrupt database entry");
            readExact(this.handle, offset + position, key.subarray(0, keyLength)); position += keyLength;
            visit(keyHex(key.subarray(0, keyLength)), deleted ? null : { file: this.handle, offset: offset + position, length: valueLength });
            if (!deleted) position += valueLength;
            if (count % RECOVERY_ROWS === 0) yield;
        }
        if (position !== length) throw new Error("Trailing database batch bytes");
    }
    *recover(): Generator<void> {
        requireStorageFormat();
        const size = this.handle.getSize(), chunk = new Uint8Array(IO_CHUNK);
        while (this.end + 4 <= size) {
            const length = new DataView(this.readAt(this.end, 4).buffer).getUint32(0, true);
            if (length < 4) throw new Error("Corrupt database transaction length");
            if (!Number.isSafeInteger(this.end + length + 12)) throw new Error("Database offset exceeds limit");
            if (this.end + length + 12 > size) break;
            let hash = 0x811c9dc5;
            for (let offset = 0; offset < length;) {
                const n = Math.min(chunk.length, length - offset);
                readExact(this.handle, this.end + 4 + offset, chunk.subarray(0, n));
                hash = hashBytes(hash, chunk.subarray(0, n)); offset += n;
                if (offset % RECOVERY_BYTES === 0) yield;
            }
            yield;
            const tail = new DataView(this.readAt(this.end + 4 + length, 8).buffer);
            if (tail.getUint32(0, true) !== hash || tail.getUint32(4, true) !== length) throw new Error("Corrupt database transaction");
            yield* this.visitBatch(this.end + 4, length, (key, value) => {
                if (value === null) { if (this.index.delete(key)) this.resources.release(ENTRY_CREDIT); }
                else { if (!this.index.has(key)) this.resources.reserve(ENTRY_CREDIT); this.index.set(key, value); }
            });
            yield;
            this.end += length + 12;
        }
        yield* this.orderKeys();
        if (this.end !== size) { requireStorageFormat(); this.handle.truncate(this.end); this.handle.flush(); }
    }
    // The ledger charges both key arrays through ENTRY_CREDIT. No native sort
    // may monopolize the worker when reopening a large persistent index.
    *orderKeys(): Generator<void> {
        let source: string[] = [], count = 0;
        for (const key of this.index.keys()) { source.push(key); if (++count % RECOVERY_ROWS === 0) yield; }
        let target = new Array<string>(source.length);
        for (let width = 1; width < source.length; width *= 2) {
            for (let start = 0; start < source.length; start += width * 2) {
                let left = start, right = Math.min(start + width, source.length);
                const middle = right, end = Math.min(start + width * 2, source.length);
                for (let offset = start; offset < end; offset++) {
                    target[offset] = left < middle && (right === end || source[left] <= source[right]) ? source[left++] : source[right++];
                    if (++count % ORDER_COMPARISONS === 0) yield;
                }
            }
            [source, target] = [target, source];
        }
        this.keys = source;
    }
    get(key: Uint8Array): Uint8Array | null { const value = this.index.get(keyHex(key)); return value ? this.readAt(value.offset, value.length) : null; }
    attachFence(fence: OpfsWriteFence): void {
        if (this.closed || this.writer || this.fence) throw new Error('Database cannot attach a writer fence');
        this.fence = fence;
    }
    begin(write: boolean): OpfsTransaction {
        if (write) requireStorageFormat();
        if (this.closed) throw new Error("Database is closed");
        if (write && this.writer) throw new Error("Database writer is busy");
        const resources = this.resources;
        if (resources.transactions + resources.pins >= 32) throw new Error("Database transaction capacity exceeded");
        const charge = 256 + this.index.size * ENTRY_CREDIT;
        resources.reserve(charge);
        let spool: OpfsWriterSpool | undefined;
        try {
            if (write) { spool = resources.writer(); this.fence?.begin(); }
        } catch (error) { if (spool) spool.busy = false; resources.release(charge); throw error; }
        resources.transactions++;
        if (write) this.writer = true;
        try { return new OpfsTransaction(this, write, spool, charge); }
        catch (error) {
            if (spool) { spool.busy = false; this.fence?.end(); }
            if (write) this.writer = false;
            resources.transactions--; resources.release(charge); throw error;
        }
    }
    close(): void {
        if (this.closed) return;
        this.closed = true; this.handle.close(); this.resources.stores--;
        this.resources.release(512 + this.index.size * ENTRY_CREDIT);
        this.index.clear(); this.keys = [];
    }
}

class OpfsTransaction {
    store: OpfsStore;
    write: boolean;
    spool: OpfsWriterSpool | undefined;
    charge: number;
    snapshot: Map<string, OpfsLocation>;
    keys: string[];
    changes: Map<string, OpfsChange>;
    spoolEnd: number;
    pending: OpfsPending | null;
    ended: boolean;
    constructor(store: OpfsStore, write: boolean, spool: OpfsWriterSpool | undefined, charge: number) {
        this.store = store; this.write = write; this.spool = spool; this.charge = charge;
        this.snapshot = new Map(store.index); this.keys = store.keys.slice();
        this.changes = new Map(); this.spoolEnd = 0; this.pending = null; this.ended = false;
    }
    location(key: Uint8Array): OpfsLocation | null {
        const hex = keyHex(key);
        return this.changes.has(hex) ? this.changes.get(hex)!.value : this.snapshot.get(hex) ?? null;
    }
    length(key: Uint8Array): number | null { return this.location(key)?.length ?? null; }
    read(key: Uint8Array, offset: number, out: Uint8Array): number | null { const value = this.location(key); return value ? locationRead(value, offset, out) : null; }
    get(key: Uint8Array): Uint8Array | null {
        const value = this.location(key); if (!value) return null;
        const bytes = new Uint8Array(value.length); locationRead(value, 0, bytes); return bytes;
    }
    nextKey(key: Uint8Array, exclusive: boolean, reverse: boolean): Uint8Array | null {
        const hex = key.length ? keyHex(key) : "";
        let index = lowerBound(this.keys, hex);
        if (reverse && (exclusive || this.keys[index] !== hex)) index--;
        if (!reverse && exclusive && this.keys[index] === hex) index++;
        while (index >= 0 && index < this.keys.length) {
            const selected = this.keys[index], value = this.changes.has(selected) ? this.changes.get(selected)!.value : this.snapshot.get(selected);
            if (value) return fromHex(selected);
            index += reverse ? -1 : 1;
        }
        return null;
    }
    next(key: Uint8Array, exclusive: boolean, reverse: boolean): OpfsEntry | null { const selected = this.nextKey(key, exclusive, reverse); return selected ? { key: selected, value: this.get(selected) } : null; }
    pin(key: Uint8Array): OpfsReader | null {
        if (this.write) throw new Error("Snapshot reader requires a read transaction");
        const location = this.location(key); if (!location) return null;
        const resources = this.store.resources;
        if (resources.transactions + resources.pins >= 32) throw new Error("Database reader capacity exceeded");
        resources.reserve(ENTRY_CREDIT); resources.pins++;
        let released = false;
        return { length: location.length, read: (offset, out) => { if (released) throw new Error("Reader closed"); return locationRead(location, offset, out); },
            close: () => { if (!released) { released = true; resources.pins--; resources.release(ENTRY_CREDIT); } } };
    }
    startPut(key: Uint8Array, length: number): void {
        if (!this.write || this.ended || this.pending) throw new Error("Invalid database write state");
        if (!Number.isInteger(length) || length < 0 || length > MAX_VALUE || this.spoolEnd + length > MAX_BATCH) throw new Error("Database value exceeds limit");
        const hex = keyHex(key), fresh = !this.changes.has(hex);
        if (fresh) this.store.resources.reserve(ENTRY_CREDIT);
        this.pending = { key: hex, length, offset: this.spoolEnd, written: 0, fresh };
    }
    writePart(bytes: Uint8Array): void {
        const pending = this.pending;
        if (!pending || bytes.length > IO_CHUNK || pending.written + bytes.length > pending.length) throw new Error("Invalid streamed database write");
        // A pending write exists only in write transactions, which always own a spool.
        writeExact(this.spool!.file, pending.offset + pending.written, bytes); pending.written += bytes.length;
    }
    finishPut(): void {
        const pending = this.pending;
        if (!pending || pending.written !== pending.length) throw new Error("Incomplete database value");
        this.spoolEnd += pending.length;
        if (!this.snapshot.has(pending.key) && !this.changes.has(pending.key)) this.keys.splice(lowerBound(this.keys, pending.key), 0, pending.key);
        this.changes.set(pending.key, { value: { file: this.spool!.file, offset: pending.offset, length: pending.length }, previous: this.snapshot.get(pending.key) ?? null });
        this.pending = null;
    }
    put(key: Uint8Array, value: Uint8Array | null): void {
        if (value === null) {
            if (!this.write || this.ended || this.pending) throw new Error("Invalid database write state");
            const hex = keyHex(key);
            if (!this.changes.has(hex)) this.store.resources.reserve(ENTRY_CREDIT);
            this.changes.set(hex, { value: null, previous: this.snapshot.get(hex) ?? null }); return;
        }
        this.startPut(key, value.length);
        for (let offset = 0; offset < value.length; offset += IO_CHUNK) this.writePart(value.subarray(offset, offset + IO_CHUNK));
        this.finishPut();
    }
    commit(): void {
        requireStorageFormat();
        const store = this.store, resources = store.resources;
        let length = 4, additions = 0, removals = 0;
        for (const [key, change] of this.changes) {
            length += 8 + key.length / 2 + (change.value?.length ?? 0);
            if (change.value && !change.previous) additions++;
            if (!change.value && change.previous) removals++;
        }
        if (length > MAX_BATCH || !Number.isSafeInteger(store.end + length + 12)) throw new Error("Database transaction exceeds encoded limit");
        // Reserve all resulting index growth before writing or flushing.
        resources.reserve(additions * ENTRY_CREDIT);
        const chunk = new Uint8Array(IO_CHUNK), header = new Uint8Array(8), prefix = new Uint8Array(4);
        let position = store.end, buffered = 0, hash = 0x811c9dc5, published = false;
        const writeBuffered = () => {
            if (buffered) writeExact(store.handle, position - buffered, chunk.subarray(0, buffered));
            buffered = 0;
        };
        const emit = (bytes: Uint8Array, hashed = true) => {
            if (hashed) hash = hashBytes(hash, bytes);
            for (let offset = 0; offset < bytes.length;) {
                const n = Math.min(chunk.length - buffered, bytes.length - offset);
                chunk.set(bytes.subarray(offset, offset + n), buffered);
                buffered += n; position += n; offset += n;
                if (buffered === chunk.length) writeBuffered();
            }
        };
        try {
            new DataView(prefix.buffer).setUint32(0, length, true); emit(prefix, false);
            new DataView(prefix.buffer).setUint32(0, this.changes.size, true); emit(prefix);
            for (const [key, change] of this.changes) {
                const keyBytes = fromHex(key), view = new DataView(header.buffer);
                view.setUint32(0, keyBytes.length, true); view.setUint32(4, change.value?.length ?? 0xffffffff, true);
                emit(header); emit(keyBytes);
                change.final = change.value ? { file: store.handle, offset: position, length: change.value.length } : null;
                if (change.value) for (let offset = 0; offset < change.value.length;) {
                    const n = locationRead(change.value, offset, chunk.subarray(buffered));
                    hash = hashBytes(hash, chunk.subarray(buffered, buffered + n));
                    buffered += n; position += n; offset += n;
                    if (buffered === chunk.length) writeBuffered();
                }
            }
            new DataView(header.buffer).setUint32(0, hash, true); new DataView(header.buffer).setUint32(4, length, true);
            emit(header, false); writeBuffered();
            // Prepare the bounded in-memory index before flush. Native sync
            // OPFS calls do not yield this worker; snapshots own prior maps.
            published = true;
            for (const [key, change] of this.changes) { if (change.final) store.index.set(key, change.final); else store.index.delete(key); }
            const keys = this.keys.filter(key => store.index.has(key));
            store.handle.flush();
            store.keys = keys; store.end += length + 12;
            resources.release(removals * ENTRY_CREDIT);
        } catch (error) {
            if (published) for (const [key, change] of this.changes) { if (change.previous) store.index.set(key, change.previous); else store.index.delete(key); }
            resources.release(additions * ENTRY_CREDIT);
            store.handle.truncate(store.end); store.handle.flush();
            throw error;
        }
    }
    end(commit: boolean): void {
        if (this.ended) throw new Error("Transaction already ended");
        try {
            if (commit && this.pending) throw new Error("Incomplete streamed database write");
            if (commit && this.write && this.changes.size) this.commit();
        } finally {
            this.ended = true;
            this.store.resources.release(this.charge + this.changes.size * ENTRY_CREDIT + (this.pending?.fresh ? ENTRY_CREDIT : 0));
            this.store.resources.transactions--;
            this.snapshot.clear(); this.changes.clear(); this.keys = []; this.pending = null;
            if (this.spool) { this.spool.busy = false; this.store.writer = false; this.store.fence?.end(); }
        }
    }
}
export type { OpfsTransaction };
