import { AbiError } from './guest-memory.ts';
import type { BlobHashFactory } from './blob.ts';
import type { RuntimeBlobs, FileReference } from './runtime-blobs.ts';

export interface FileHost {
    memory: WebAssembly.Memory;
    pb_input(length: number): number;
    pb_output(): number;
    pb_web_file_execute(token: bigint): number;
    pb_web_file_writer_ready(token: bigint): number;
    pb_web_file_write_complete(token: bigint, status: number): number;
    pb_web_file_read_complete(token: bigint, length: number): number;
    pb_web_file_io_failed(token: bigint, status: number): number;
    pb_web_file_seal(token: bigint, length: bigint): number;
    pb_web_file_publish(token: bigint, length: number): number;
    pb_web_file_close_failed(token: bigint, status: number): number;
    pb_web_file_qualify(token: bigint): number;
    pb_web_file_result(token: bigint): number;
    pb_web_file_cleanup(): number;
    pb_web_file_cleaned(engine: bigint): number;
}
export type FileResult = { File: { handle: string } } | { List: { handle: string } }
    | { Status: { kind: number; flags: number; size: number; modification_time: number } }
    | { Close: { error: number; publication: number; recovery: string | null } }
    | { Entry: { kind: number; name: number[] } } | { Read: { bytes: Uint8Array<ArrayBuffer> } } | 'Written' | 'End';
type Task = { kind: 'done'; output: FileResult } | { kind: 'read'; reference: FileReference }
    | { kind: 'create_writer'; engine: string } | { kind: 'write'; engine: string; offset: number }
    | { kind: 'private_read'; engine: string; offset: number; length: number }
    | { kind: 'close'; engine: string; flags: number };
interface Spool { name: string; file?: FileSystemSyncAccessHandle }
interface Services {
    host(): FileHost;
    directory(): FileSystemDirectoryHandle;
    blobs: RuntimeBlobs;
    hash: BlobHashFactory;
    step<T>(callback: () => T): Promise<T>;
    admit?<T>(callback: () => T): Promise<T>;
    authority?(): number;
    yield(): Promise<void>;
    published(): void;
}
const encoder = new TextEncoder(), decoder = new TextDecoder('utf-8', { fatal: true });
const chunk = 65536, extent = 64 * 1024 * 1024;
const status = (error: unknown) => {
    const value = error instanceof AbiError ? error.status : (error as { code?: unknown })?.code;
    return typeof value === 'number' && Number.isInteger(value) && value <= -1 && value >= -14 ? value : -5;
};
function checked(value: number): number { if (value < 0) throw new AbiError(value); return value; }

// Rust retains eight stage credits through the last File/operation owner and
// physical cleanup. This worker owns only the corresponding bounded OPFS spools.
export class FileBackend {
    private readonly services: Services;
    private readonly spools = new Map<string, Spool>();
    private cleanupWork?: Promise<void>;
    private cleanupRequested = false;
    private publicationPending = false;
    constructor(services: Services) { this.services = services; }
    async initialize(): Promise<void> {
        // The runtime holds its exclusive directory lock before this scan.
        // These unpublished spools cannot survive an execution restart.
        let removed = 0;
        const directory = this.services.directory() as FileSystemDirectoryHandle & AsyncIterable<[string, FileSystemHandle]>;
        for await (const [name] of directory) {
            if (/^(?:file-private|blob-stage)-[0-9]+\.tmp$/.test(name)) {
                if (++removed > 14) throw new Error('Private file cleanup exceeds the staging limit');
                await directory.removeEntry(name);
            }
            await this.services.yield();
        }
    }
    private input(bytes: Uint8Array<ArrayBuffer>): void {
        const host = this.services.host(), pointer = host.pb_input(8 + bytes.length);
        if (!pointer) throw new AbiError(-9);
        new Uint8Array(host.memory.buffer, pointer + 8, bytes.length).set(bytes);
    }
    private json<T>(length: number): T {
        checked(length);
        const host = this.services.host();
        return JSON.parse(decoder.decode(new Uint8Array(host.memory.buffer, host.pb_output(), length)));
    }
    private spool(engine: string): FileSystemSyncAccessHandle {
        const file = this.spools.get(engine)?.file;
        if (!file) throw new AbiError(-5);
        return file;
    }
    private admitted<T>(callback: () => T): Promise<T> {
        return this.services.admit ? this.services.admit(callback) : this.services.step(callback);
    }
    async execute(token: bigint, bytes: Uint8Array<ArrayBuffer>): Promise<FileResult> {
        const task = await this.admitted(() => this.json<Task>(this.services.host().pb_web_file_execute(token)));
        let read: Uint8Array<ArrayBuffer> | undefined;
        try {
            switch (task.kind) {
                case 'done': break;
                case 'create_writer': await this.create(task.engine); break;
                case 'write': await this.write(task.engine, task.offset, bytes); break;
                case 'read': {
                    const result = await this.services.blobs.readVersion(task.reference);
                    checked(result.status);
                    if (!result.bytes) throw new AbiError(-5);
                    read = result.bytes; break;
                }
                case 'private_read': read = this.read(task.engine, task.offset, task.length); break;
                case 'close': await this.close(token, task.engine); break;
            }
            await this.admitted(() => {
                const host = this.services.host();
                if (task.kind === 'create_writer') checked(host.pb_web_file_writer_ready(token));
                if (task.kind === 'write') checked(host.pb_web_file_write_complete(token, 0));
                if (read) { this.input(read); checked(host.pb_web_file_read_complete(token, read.length)); }
            });
        } catch (error) {
            await this.retireFailure(token, task, status(error));
            if (task.kind !== 'close') throw error;
        }
        return this.admitted(() => {
            const host = this.services.host();
            checked(host.pb_web_file_qualify(token));
            // Rust retains its validation copy; avoid JSON-encoding read bytes.
            return read ? { Read: { bytes: read } } : this.json<FileResult>(host.pb_web_file_result(token));
        });
    }
    private async retireFailure(token: bigint, task: Task, error: number): Promise<void> {
        await this.services.step(() => {
            const host = this.services.host();
            if (task.kind === 'close') checked(host.pb_web_file_close_failed(token, error));
            else if (task.kind === 'write') checked(host.pb_web_file_write_complete(token, error));
            else if (task.kind !== 'done') checked(host.pb_web_file_io_failed(token, error));
        });
    }
    private async create(engine: string): Promise<void> {
        if (this.spools.size >= 8 || this.spools.has(engine)) throw new AbiError(-9);
        const spool: Spool = { name: `file-private-${engine}.tmp` };
        this.spools.set(engine, spool);
        const handle = await this.services.directory().getFileHandle(spool.name, { create: true });
        spool.file = await handle.createSyncAccessHandle();
        // Never reuse a spool containing bytes from another execution.
        if (spool.file.getSize() !== 0) throw new AbiError(-5);
    }
    private async write(engine: string, offset: number, bytes: Uint8Array<ArrayBuffer>): Promise<void> {
        const file = this.spool(engine);
        if (!Number.isSafeInteger(offset) || offset < 0 || offset + bytes.length > extent) throw new AbiError(-3);
        if (!bytes.length) return;
        const zeros = new Uint8Array(chunk);
        for (let at = file.getSize(); at < offset; at += chunk) {
            await this.services.yield();
            checked(this.services.authority?.() ?? 0);
            const part = zeros.subarray(0, Math.min(chunk, offset - at));
            if (file.write(part, { at }) !== part.length) throw new AbiError(-5);
        }
        for (let at = 0; at < bytes.length; at += chunk) {
            await this.services.yield();
            checked(this.services.authority?.() ?? 0);
            const part = bytes.subarray(at, at + chunk);
            if (file.write(part, { at: offset + at }) !== part.length) throw new AbiError(-5);
        }
    }
    private read(engine: string, offset: number, length: number): Uint8Array<ArrayBuffer> {
        const file = this.spool(engine), count = Math.min(length, Math.max(0, file.getSize() - offset));
        const bytes = new Uint8Array(count);
        if (count && file.read(bytes, { at: offset }) !== count) throw new AbiError(-5);
        return bytes;
    }
    private async close(token: bigint, engine: string): Promise<void> {
        const file = this.spool(engine); file.flush();
        const hash = this.services.hash(), bytes = new Uint8Array(chunk), size = file.getSize();
        let digest: string;
        try {
            for (let offset = 0; offset < size; offset += chunk) {
                await this.services.yield();
                const part = bytes.subarray(0, Math.min(chunk, size - offset));
                if (file.read(part, { at: offset }) !== part.length) throw new AbiError(-5);
                hash.update(part);
            }
            digest = hash.finish();
        } finally { hash.close(); }
        const sealed = await this.admitted(() => {
            this.input(Uint8Array.from(digest.match(/../g)!, byte => parseInt(byte, 16)));
            return this.json<FileReference>(this.services.host().pb_web_file_seal(token, BigInt(size)));
        });
        const release = this.services.blobs.retain(sealed);
        try {
            const prepared = await this.services.blobs.prepareStage(file, sealed);
            await this.admitted(() => {
                const encoded = encoder.encode(JSON.stringify(prepared)); this.input(encoded);
                checked(this.services.host().pb_web_file_publish(token, encoded.length));
                this.publicationPending = true;
            });
        } finally { release(); }
    }
    // The caller drains this notice after owning the Close result. Notification
    // failure retains the notice and cannot relabel or replay publication.
    flushPublication(): void {
        if (!this.publicationPending) return;
        this.services.published();
        this.publicationPending = false;
    }
    cleanup(): Promise<void> {
        this.cleanupRequested = true;
        if (!this.cleanupWork) {
            this.cleanupWork = Promise.resolve().then(() => this.cleanRequested());
        }
        return this.cleanupWork;
    }
    private async cleanRequested(): Promise<void> {
        // Coalesce retirement notifications, never queue one Promise per File.
        try {
            while (this.cleanupRequested) {
                this.cleanupRequested = false;
                await this.cleanupStages();
                await this.services.blobs.cleanup();
            }
        } finally { this.cleanupWork = undefined; }
    }
    private async cleanupStages(): Promise<void> {
        const engines = await this.services.step(() => {
            const host = this.services.host(), length = checked(host.pb_web_file_cleanup());
            if (length > 64 || length % 8) throw new AbiError(-5);
            const view = new DataView(host.memory.buffer, host.pb_output(), length);
            return Array.from({ length: length / 8 }, (_, i) => view.getBigUint64(i * 8, true));
        });
        for (const engine of engines) {
            const spool = this.spools.get(engine.toString());
            if (spool) {
                if (spool.file) { spool.file.close(); spool.file = undefined; }
                await this.services.directory().removeEntry(spool.name).catch(error => {
                    if (error.name !== 'NotFoundError') throw error;
                });
            }
            await this.services.step(() => checked(this.services.host().pb_web_file_cleaned(engine)));
            this.spools.delete(engine.toString());
        }
    }
}
