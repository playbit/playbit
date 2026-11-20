import { ABI as L } from './guest-layout.ts';
import type { RecordLayout } from './guest-layout.ts';
import { AbiError, admit, bytes, disjoint, view, record, publishRecord } from './guest-memory.ts';
import type { GuestOperations } from './guest-imports.ts';
import type { GuestData, DataLifetime } from './guest-data.ts';
import type { FileBackend, FileHost, FileResult } from './file-backend.ts';
export interface OwnedFileHost extends FileHost {
    pb_web_file_begin(generation: number, slot: number, kind: number, offset: bigint, length: number, flags: number): bigint;
    pb_web_file_check(token: bigint, delivery: number): number;
    pb_web_file_finish(token: bigint, engine: bigint, entry: number): number;
    pb_web_file_release(token: bigint): number;
    pb_web_file_dispose(generation: number, slot: number): number;
    pb_web_file_list_close(generation: number, slot: number): number;
}
export interface FileCompletion { token: bigint; result: FileResult }
export interface FileValue { owned?: FileCompletion }
interface Services {
    memory(): WebAssembly.Memory;
    host(): OwnedFileHost;
    backend(): FileBackend;
    data: GuestData<FileValue>;
    step<T>(callback: () => T): Promise<T>;
    failure(error: unknown): void;
    authority(): number;
}
const empty = new Uint8Array(), decoder = new TextDecoder('utf-8', { fatal: true });
function checked(status: number): number { if (status < 0) throw new AbiError(status); return status; }
function fileGeneration(value: bigint): number {
    if (value < 0n || value > 0xffffffffn) throw new AbiError(-8);
    return Number(value);
}
export function fileOperations(services: Services): GuestOperations {
    // Admission, not callback scheduling, establishes each engine File's order.
    const order = new Map<bigint, Promise<void>>();
    const call = (run: () => number | bigint) => { try { return run(); } catch (error) { if (error instanceof AbiError) return error.status; throw error; } };
    const cleanup = () => { void services.backend().cleanup().catch(services.failure); };
    function begin(generation: bigint, slot: number, kind: number, input = empty, flags = 0, offset = 0n, length = input.length): bigint {
        checked(services.authority());
        const admittedGeneration = fileGeneration(generation);
        let token = 0n, engine = 0n, previous = Promise.resolve(), finished!: () => void, tail!: Promise<void>;
        let completion: FileCompletion | undefined, failed = false;
        const lifetime: DataLifetime<FileValue> = {
            retainedRevision: true,
            accept() {
                const host = services.host(), p = host.pb_input(8 + input.length);
                if (!p) throw new AbiError(-9);
                new Uint8Array(host.memory.buffer, p + 8, input.length).set(input);
                token = host.pb_web_file_begin(admittedGeneration, slot, kind === 28 ? 23 : kind, offset, length, flags);
                if (token < 0n) throw new AbiError(Number(token));
                engine = new DataView(host.memory.buffer).getBigUint64(host.pb_output(), true);
                previous = order.get(engine) ?? Promise.resolve();
                tail = new Promise(resolve => { finished = resolve; }); order.set(engine, tail);
            },
            delivery: () => failed ? 0 : services.authority() || services.host().pb_web_file_check(token, 1),
            async requalify() {
                await services.step(() => {
                    const host = services.host(); checked(host.pb_web_file_qualify(token));
                    if (completion && !(typeof completion.result === 'object' && 'Read' in completion.result)) {
                        const size = checked(host.pb_web_file_result(token));
                        completion.result = JSON.parse(decoder.decode(new Uint8Array(host.memory.buffer, host.pb_output(), size)));
                    }
                });
            },
            release() { checked(services.host().pb_web_file_release(token)); cleanup(); },
        };
        const operation = services.data.begin(kind, 512 + input.length * 2 + (kind === 8 ? length * 2 : 0), async () => {
            await previous;
            try {
                checked(services.authority());
                completion = { token, result: await services.backend().execute(token, input) };
                return { status: 0, value: { owned: completion } };
            } catch (error) { failed = true; throw error; }
            finally { finished(); if (order.get(engine) === tail) order.delete(engine); }
        }, () => 0, lifetime);
        return operation;
    }
    function finish(completion: FileCompletion): Uint8Array<ArrayBuffer> {
        const result = completion.result;
        const engine = typeof result !== 'object' ? 0 : 'File' in result ? result.File.handle
            : 'List' in result ? result.List.handle : 'Close' in result ? result.Close.recovery ?? 0 : 0;
        const host = services.host();
        checked(host.pb_web_file_finish(completion.token, BigInt(engine), Number(typeof result === 'object' && 'Entry' in result)));
        return new Uint8Array(host.memory.buffer, host.pb_output(), 8).slice();
    }
    function fixed(generation: bigint, slot: number, kind: number, pointer: number, layout: RecordLayout | null, copy: (value: FileCompletion, output: DataView) => void): number | bigint {
        services.data.admit(generation, slot, kind);
        const memory = services.memory(), output = layout ? record(memory, pointer, layout) : admit(memory, pointer, 1, 8, 4, 1);
        return services.data.take(generation, slot, kind, value => {
            const data = new Uint8Array(layout?.size ?? 8); copy(value.owned!, new DataView(data.buffer));
            if (layout) publishRecord(memory, output, layout, data); else bytes(memory, output).set(data);
            return 0;
        });
    }
    function capability(generation: bigint, slot: number, kind: number): number | bigint {
        return services.data.take(generation, slot, kind, value => new DataView(finish(value.owned!).buffer).getBigUint64(0, true));
    }
    function read(generation: bigint, slot: number, pointer: number, capacity: number, count: number): number | bigint {
        services.data.admit(generation, slot, 8);
        const memory = services.memory(), header = admit(memory, count, 1, 4, 4, 1), output = admit(memory, pointer, capacity, 1, 1, 65536);
        disjoint([header, output]);
        return services.data.take(generation, slot, 8, value => {
            const completion = value.owned!, result = completion.result;
            const data = typeof result === 'object' && 'Read' in result ? result.Read.bytes : empty;
            view(memory, header).setUint32(0, data.length, true);
            if (output.length < data.length) return -11;
            finish(completion); bytes(memory, output).set(data); return 0;
        });
    }
    function list(generation: bigint, slot: number, pointer: number): number | bigint {
        services.data.admit(generation, slot, 27);
        const memory = services.memory(), header = record(memory, pointer, L.fileListEntry), descriptor = view(memory, header);
        const output = admit(memory, descriptor.getUint32(16, true), descriptor.getUint32(20, true), 1, 1, 255); disjoint([header, output]);
        return services.data.take(generation, slot, 27, value => {
            const completion = value.owned!, result = completion.result;
            const data = typeof result === 'object' && 'Entry' in result ? Uint8Array.from(result.Entry.name) : empty;
            if (output.length < data.length) { descriptor.setUint32(24, data.length, true); return -11; }
            finish(completion);
            const resultBytes = bytes(memory, header).slice(0, L.fileListEntry.size), resultView = new DataView(resultBytes.buffer);
            resultView.setUint32(8, result === 'End' ? 0 : 1, true);
            resultView.setUint32(12, typeof result === 'object' && 'Entry' in result ? result.Entry.kind : 0, true);
            resultView.setUint32(24, data.length, true);
            publishRecord(memory, header, L.fileListEntry, resultBytes); bytes(memory, output).set(data); return 0;
        });
    }
    return {
        pb_file_open_begin: (generation, slot, path, length, flags) => call(() => {
            const memory = services.memory(), source = admit(memory, path, length, 1, 1, 4096);
            return begin(generation, slot, flags & 4 ? 28 : 23, bytes(memory, source).slice(), flags);
        }),
        pb_file_open_take: (generation, slot) => call(() => {
            // Write and read opens have separate admission kinds but one ABI Take.
            let kind = 23;
            try { services.data.admit(generation, slot, kind); } catch (error) { if (!(error instanceof AbiError) || error.status !== -8) throw error; kind = 28; }
            return capability(generation, slot, kind);
        }),
        pb_file_read_begin: (generation, slot, offset, length) => call(() => begin(generation, slot, 8, empty, 0, offset, length)),
        pb_file_read_take: (generation, slot, output, capacity, count) => call(() => read(generation, slot, output, capacity, count)),
        pb_file_write_begin: (generation, slot, offset, pointer, length) => call(() => {
            const memory = services.memory(), source = admit(memory, pointer, length, 1, 1, 1048576);
            return begin(generation, slot, 9, bytes(memory, source).slice(), 0, offset);
        }),
        pb_file_write_take: (generation, slot) => call(() => services.data.take(generation, slot, 9, value => { finish(value.owned!); return 0; })),
        pb_file_status_begin: (generation, slot) => call(() => begin(generation, slot, 24)),
        pb_file_status_take: (generation, slot, output) => call(() => fixed(generation, slot, 24, output, L.fileStatus, (value, record) => {
            if (typeof value.result !== 'object' || !('Status' in value.result)) throw new Error('Invalid File status result');
            const status = value.result.Status; finish(value);
            record.setUint32(8, status.kind, true); record.setUint32(12, status.flags, true);
            record.setBigUint64(16, BigInt(status.size), true); record.setBigInt64(24, BigInt(status.modification_time), true);
        })),
        pb_file_close_begin: (generation, slot, flags) => call(() => begin(generation, slot, 25, empty, flags)),
        pb_file_close_take: (generation, slot, output) => call(() => fixed(generation, slot, 25, output, L.fileCloseResult, (value, record) => {
            if (typeof value.result !== 'object' || !('Close' in value.result)) throw new Error('Invalid File close result');
            const result = value.result.Close, handle = finish(value);
            record.setInt32(8, result.error, true); record.setUint32(12, result.publication, true);
            new Uint8Array(record.buffer, record.byteOffset + 16, 8).set(handle);
        })),
        pb_file_list_open_begin: (generation, slot, flags) => call(() => begin(generation, slot, 26, empty, flags)),
        pb_file_list_open_take: (generation, slot) => call(() => capability(generation, slot, 26)),
        pb_file_list_next_begin: (generation, slot) => call(() => begin(generation, slot, 27)),
        pb_file_list_next_take: (generation, slot, output) => call(() => list(generation, slot, output)),
        pb_file_list_close: (generation, slot) => call(() => { checked(services.host().pb_web_file_list_close(fileGeneration(generation), slot)); cleanup(); return 0; }),
        pb_file_dispose: (generation, slot) => call(() => { checked(services.host().pb_web_file_dispose(fileGeneration(generation), slot)); cleanup(); return 0; }),
    };
}
