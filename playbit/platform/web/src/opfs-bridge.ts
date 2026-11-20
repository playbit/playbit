// The same concrete imports serve the runtime worker and Wasm-host regressions.
import { IO_CHUNK } from "./opfs-store.ts";
import type { OpfsStore, OpfsResources, OpfsTransaction, OpfsReader, OpfsStagingSpool } from "./opfs-store.ts";
// The `opfs` Wasm import module: every host function takes u32/usize
// arguments and returns an i32 status or length (negative on failure).
export type OpfsHostImports = {
    operation_supported: (opcode: number) => number;
    bundle_length: (slot: number) => number;
    bundle_read: (slot: number, offset: number, out: number, length: number) => number;
    bundle_close: (slot: number) => void;
    random: (pointer: number, length: number) => void;
    now: () => number;
    close_database: (database: number) => void;
    begin: (database: number, write: number) => number;
    get: (identity: number, key: number, length: number, out: number, capacity: number) => number;
    value_length: (identity: number, key: number, length: number) => number;
    read_value: (identity: number, key: number, length: number, offset: number, out: number, capacity: number) => number;
    next_key: (identity: number, key: number, length: number, exclusive: number, reverse: number, out: number, capacity: number) => number;
    next: (identity: number, key: number, length: number, exclusive: number, reverse: number, out: number, capacity: number) => number;
    put: (identity: number, key: number, keyLength: number, value: number, valueLength: number) => number;
    put_start: (identity: number, key: number, keyLength: number, length: number) => number;
    put_part: (identity: number, value: number, length: number) => number;
    put_finish: (identity: number) => number;
    delete: (identity: number, key: number, length: number) => number;
    end: (identity: number, commit: number) => number;
    pin: (identity: number, key: number, length: number) => number;
    reader_length: (identity: number) => number;
    reader_read: (identity: number, offset: number, out: number, capacity: number) => number;
    reader_end: (identity: number) => number;
    stage_begin: (slot: number) => number;
    stage_clear: (slot: number) => number;
    stage_append: (slot: number, pointer: number, length: number) => number;
    stage_read: (slot: number, offset: number, out: number, capacity: number) => number;
    stage_end: (slot: number) => number;
};
// The bridge: the import table plus a close that releases every open
// transaction, reader and staging slot.
export type OpfsStorageBridge = { imports: OpfsHostImports; close(): void };
export function createStorageBridge(stores: Map<number, OpfsStore>, resources: Pick<OpfsResources, 'staging'>, memory: () => WebAssembly.Memory,
    random: (bytes: Uint8Array<ArrayBuffer>) => unknown, now: () => number, onError: (message: string) => void): OpfsStorageBridge {
    const transactions = new Map<number, OpfsTransaction>(), readers = new Map<number, OpfsReader>();
    let nextTransaction = 1, nextReader = 1;
    const bytes = (pointer: number, length: number): Uint8Array<ArrayBuffer> => {
        if (!Number.isInteger(length) || length < 0 || length > 64 * 1024 * 1024) throw new Error("Host memory bounds");
        pointer >>>= 0;
        const buffer = memory().buffer;
        if (pointer + length > buffer.byteLength) throw new Error("Host memory bounds");
        return new Uint8Array(buffer, pointer, length);
    };
    const id = (kind: "tx" | "reader"): number => {
        const value = kind === "tx" ? nextTransaction++ : nextReader++;
        if (value > 0x7fffffff) throw new Error("Database handle identity exhausted");
        return value;
    };
    const guard = <A extends number[]>(fn: (...args: A) => number) => (...args: A): number => { try { return fn(...args); } catch (error) { onError(String(error).slice(0, 512)); return -4; } };
    const tx = (identity: number): OpfsTransaction => { const value = transactions.get(identity); if (!value) throw new Error("Transaction is closed"); return value; };
    const reader = (identity: number): OpfsReader => { const value = readers.get(identity); if (!value) throw new Error("Reader is closed"); return value; };
    const stage = (slot: number): OpfsStagingSpool => { const value = resources.staging[slot]; if (!value?.busy) throw new Error("Sync staging is closed"); return value; };
    const imports: OpfsHostImports = {
        operation_supported: () => 0,
        bundle_length: () => -4, bundle_read: () => -4, bundle_close: () => { throw new Error("Bundle storage is unavailable"); },
        random: (pointer, length) => random(bytes(pointer, length)), now,
        close_database: database => { const store = stores.get(database); if (store) { store.close(); stores.delete(database); } },
        // An unknown database index throws a TypeError, which guard reports as -4.
        begin: guard((database, write) => { const identity = id("tx"); transactions.set(identity, stores.get(database)!.begin(Boolean(write))); return identity; }),
        get: guard((identity, key, length, out, capacity) => {
            const transaction = tx(identity), keyBytes = bytes(key, length), size = transaction.length(keyBytes);
            if (size === null) return -1;
            if (size <= capacity) transaction.read(keyBytes, 0, bytes(out, size));
            return size;
        }),
        value_length: guard((identity, key, length) => tx(identity).length(bytes(key, length)) ?? -1),
        read_value: guard((identity, key, length, offset, out, capacity) => tx(identity).read(bytes(key, length), offset, bytes(out, capacity)) ?? -1),
        next_key: guard((identity, key, length, exclusive, reverse, out, capacity) => {
            const selected = tx(identity).nextKey(bytes(key, length), Boolean(exclusive), Boolean(reverse));
            if (!selected) return -1;
            if (selected.length <= capacity) bytes(out, selected.length).set(selected);
            return selected.length;
        }),
        next: guard((identity, key, length, exclusive, reverse, out, capacity) => {
            // nextKey only yields keys that currently hold a value, so its length is never null.
            const transaction = tx(identity), selected = transaction.nextKey(bytes(key, length), Boolean(exclusive), Boolean(reverse));
            if (!selected) return -1;
            const size = transaction.length(selected)!, total = 4 + selected.length + size;
            if (total <= capacity) {
                const output = bytes(out, total); new DataView(output.buffer, output.byteOffset, 4).setUint32(0, selected.length, true);
                output.set(selected, 4); transaction.read(selected, 0, output.subarray(4 + selected.length));
            }
            return total;
        }),
        put: guard((identity, key, keyLength, value, valueLength) => { tx(identity).put(bytes(key, keyLength), bytes(value, valueLength)); return 0; }),
        put_start: guard((identity, key, keyLength, length) => { tx(identity).startPut(bytes(key, keyLength), length); return 0; }),
        put_part: guard((identity, value, length) => { tx(identity).writePart(bytes(value, length)); return 0; }),
        put_finish: guard(identity => { tx(identity).finishPut(); return 0; }),
        delete: guard((identity, key, length) => { const transaction = tx(identity), keyBytes = bytes(key, length), found = transaction.length(keyBytes) !== null; transaction.put(keyBytes, null); return Number(found); }),
        end: guard((identity, commit) => { const transaction = tx(identity); transactions.delete(identity); transaction.end(Boolean(commit)); return 0; }),
        pin: guard((identity, key, length) => {
            const value = tx(identity).pin(bytes(key, length)); if (!value) return -1;
            try { const identity = id("reader"); readers.set(identity, value); return identity; }
            catch (error) { value.close(); throw error; }
        }),
        reader_length: guard(identity => reader(identity).length),
        reader_read: guard((identity, offset, out, capacity) => reader(identity).read(offset, bytes(out, capacity))),
        reader_end: guard(identity => { const value = reader(identity); readers.delete(identity); value.close(); return 0; }),
        stage_begin: guard(slot => { const value = resources.staging[slot]; if (!value || value.busy) throw new Error("Sync staging capacity exceeded"); value.file.truncate(0); value.length = 0; value.busy = true; return 0; }),
        stage_clear: guard(slot => { const value = stage(slot); value.file.truncate(0); value.length = 0; return 0; }),
        stage_append: guard((slot, pointer, length) => {
            const value = stage(slot); if (length > 256 * 1024 || value.length + length > 64 * 1024 * 1024) throw new Error("Sync staging length exceeds limit");
            const input = bytes(pointer, length);
            for (let offset = 0; offset < input.length; offset += IO_CHUNK) {
                const chunk = input.subarray(offset, offset + IO_CHUNK);
                if (value.file.write(chunk, { at: value.length + offset }) !== chunk.length) throw new Error("Sync staging is full");
            }
            value.length += length; return 0;
        }),
        stage_read: guard((slot, offset, out, capacity) => {
            const value = stage(slot); if (offset > value.length) throw new Error("Invalid sync staging offset");
            const n = Math.min(capacity, value.length - offset), output = bytes(out, n);
            if (value.file.read(output, { at: offset }) !== n) throw new Error("Sync staging read ended early");
            return n;
        }),
        stage_end: guard(slot => { const value = stage(slot); value.busy = false; value.length = 0; value.file.truncate(0); return 0; }),
    };
    return { imports, close() {
        for (const transaction of transactions.values()) transaction.end(false); transactions.clear();
        for (const value of readers.values()) value.close(); readers.clear();
        resources.staging.forEach((value, slot) => { if (value.busy) imports.stage_end(slot); });
    } };
}
