import { BLOB_CHUNK, openCachedBlob, decryptBlob, encryptBlob, transferBlob } from './blob.ts';
import type { BlobReference, BlobHashFactory, BlobTransport } from './blob.ts';
interface FileReadResult { status: number; total?: bigint; bytes?: Uint8Array<ArrayBuffer> }
export interface FileReference extends BlobReference { offset: number; limit: number }
interface Ready { file: FileSystemSyncAccessHandle; blocks: string[]; pins: number; corrupt?: boolean }
interface Loading { promise: Promise<Ready>; users: number }
interface Lease { reference: FileReference; ready?: Ready; failed?: number; preparing: boolean; pending: boolean; loaded?: Promise<void> }
interface Stage { name: string; file?: FileSystemSyncAccessHandle; closed: boolean; cleanupFailed: boolean }
type OpenStage = Stage & { file: FileSystemSyncAccessHandle };
class CacheCorruption extends Error {}
export interface BlobServices {
    cache(request: Record<string, unknown>): { kind: 'done' | 'more' | 'remove'; digest?: string; bytes?: number } | undefined;
    failure(error: unknown): void;
    directory(): FileSystemDirectoryHandle;
    key(reference: FileReference): Uint8Array<ArrayBuffer>;
    hash: BlobHashFactory;
    ap: BlobTransport;
    operation(reference: FileReference, upload: boolean): Record<string, unknown>;
    authorize(reference: FileReference, write: boolean, service: boolean): void;
    pending(volume: string, after: [string, string] | null): [FileReference[], [string, string] | null];
    acknowledge(reference: FileReference): void;
    changed(): Promise<void>;
    yield(): Promise<void>;
    stop(): void;
    managed(): boolean;
}
const identity = (reference: BlobReference) => `${reference.volume}/${reference.entry}/${reference.revision}/${reference.digest}/${reference.encrypted_key}`;
const code = (error: unknown): number => {
    const value = (error as { code?: unknown })?.code;
    return typeof value === 'number' && Number.isInteger(value) && value < 0 ? value : -5;
};
export class RuntimeBlobs {
    private readonly services: BlobServices;
    private readonly leases = new Map<string, Lease>();
    private readonly loading = new Map<string, Loading>();
    // Two preparations use at most encrypted/plain/save stages each. Failed
    // deletion keeps its slot and blocks new preparation until explicit cleanup.
    private readonly stages = new Map<string, Stage>();
    private active = 0;
    private stopped = false;
    private sequence = 0;
    private cleanupWork?: Promise<void>;
    private cleanupRequested = false;
    constructor(services: BlobServices) { this.services = services; }
    async open(): Promise<void> {
        const directory = this.services.directory() as FileSystemDirectoryHandle & AsyncIterable<[string, FileSystemHandle]>;
        for await (const [name] of directory) {
            if (/^blob-[0-9a-f]{64}$/.test(name)) {
                const file = await (await directory.getFileHandle(name)).createSyncAccessHandle();
                try { this.services.cache({ kind: 'observe', digest: name.slice(5), length: file.getSize() }); }
                finally { file.close(); }
            }
            await this.services.yield();
        }
        await this.cleanup();
    }
    retain(reference: FileReference): () => void {
        this.services.cache({ kind: 'pin', digest: reference.digest });
        return () => {
            this.services.cache({ kind: 'unpin', digest: reference.digest });
            if (!this.stopped) void this.cleanup().catch(error => this.services.failure(error));
        };
    }
    private reserve(): void {
        if ([...this.stages.values()].some(stage => stage.cleanupFailed)) throw Object.assign(new Error('File cleanup must finish before preparation'), { code: -5 });
        if (this.stopped || this.active === 2) throw Object.assign(new Error('File preparation capacity exceeded'), { code: -9 });
        this.active++;
    }
    private reserveContent(reference: FileReference): () => void {
        const release = this.retain(reference);
        try { this.reserve(); } catch (error) { release(); throw error; }
        return () => { this.active--; release(); };
    }
    private room(): void {
        if (this.leases.size < 32) return;
        const candidate = [...this.leases].find(([, lease]) => !lease.preparing && !lease.pending);
        if (!candidate) throw Object.assign(new Error('File handle capacity exceeded'), { code: -9 });
        this.leases.delete(candidate[0]); this.release(candidate[1]);
    }
    private release(lease: Lease): void {
        if (lease.ready) this.releaseReady(lease.ready);
    }
    private releaseReady(ready: Ready): void {
        if (!ready.pins && ![...this.leases.values()].some(lease => lease.ready === ready)) ready.file.close();
    }
    private async temporary(): Promise<OpenStage> {
        if (++this.sequence > 0x7fffffff || this.stopped) throw new Error('File preparation retired');
        if (this.stages.size >= 6) throw Object.assign(new Error('File staging capacity exceeded'), { code: -9 });
        const stage: Stage = { name: `blob-stage-${this.sequence}.tmp`, closed: false, cleanupFailed: false };
        this.stages.set(stage.name, stage);
        try {
            stage.file = await (await this.services.directory().getFileHandle(stage.name, { create: true })).createSyncAccessHandle();
            return stage as OpenStage;
        } catch (error) { await this.discard(stage); throw error; }
    }
    private async discard(stage: Stage): Promise<void> {
        try {
            if (stage.file && !stage.closed) { stage.file.close(); stage.closed = true; }
            await this.services.directory().removeEntry(stage.name).catch(error => { if (error.name !== 'NotFoundError') throw error; });
            this.stages.delete(stage.name);
        } catch (error) { stage.cleanupFailed = true; throw error; }
    }
    cleanup(): Promise<void> {
        this.cleanupRequested = true;
        this.cleanupWork ??= Promise.resolve().then(() => this.cleanRequested());
        return this.cleanupWork;
    }
    private async cleanRequested(): Promise<void> {
        try {
            while (this.cleanupRequested && !this.stopped) {
                this.cleanupRequested = false;
                for (const stage of [...this.stages.values()]) if (stage.cleanupFailed) await this.discard(stage);
                await this.evict();
            }
        } finally { this.cleanupWork = undefined; }
    }
    private async evict(): Promise<void> {
        for (;;) {
            if (this.stopped) return;
            const next = this.services.cache({ kind: 'next' });
            if (!next || next.kind === 'done') return;
            if (next.kind === 'remove') {
                const digest = next.digest!;
                let success = false;
                try {
                    for (const [key, lease] of this.leases) if (lease.reference.digest === digest) {
                        this.leases.delete(key); this.release(lease);
                    }
                    await this.services.directory().removeEntry(`blob-${digest}`).catch(error => { if (error.name !== 'NotFoundError') throw error; });
                    success = true;
                } finally { this.services.cache({ kind: 'deleted', digest, success }); }
            }
            await this.services.yield();
        }
    }
    private async verify(file: FileSystemSyncAccessHandle, reference: FileReference): Promise<string[]> {
        if (file.getSize() !== reference.len_bytes) throw new CacheCorruption('Cached file length changed');
        const whole = this.services.hash(), blocks: string[] = [];
        try {
            for (let offset = 0; offset < reference.len_bytes; offset += BLOB_CHUNK) {
                await this.services.yield(); if (this.stopped) throw new Error("File preparation retired");
                const bytes = new Uint8Array(Math.min(BLOB_CHUNK, reference.len_bytes - offset));
                if (file.read(bytes, { at: offset }) !== bytes.length) throw new CacheCorruption('Cached file ended early');
                whole.update(bytes); const block = this.services.hash();
                try { block.update(bytes); blocks.push(block.finish()); } finally { block.close(); }
            }
            if (whole.finish() !== reference.digest || file.getSize() !== reference.len_bytes) throw new CacheCorruption('Cached file digest changed');
            return blocks;
        } finally { whole.close(); }
    }
    private async save(
        source: FileSystemSyncAccessHandle,
        reference: FileReference,
        receipt: (write: () => void) => void = write => write(),
    ): Promise<void> {
        const saved = () => {
            try {
                receipt(() => this.services.cache({
                    kind: 'saved', digest: reference.digest, length: reference.len_bytes, confirmed: false,
                }));
            } catch (error) {
                // Publication already made the complete file physical. Even a
                // rejected receipt must retain its byte charge as unknown.
                this.services.cache({ kind: 'observe', digest: reference.digest, length: reference.len_bytes });
                throw error;
            }
        };
        const shared = [...this.leases.values()].find(lease => lease.ready && lease.reference.digest === reference.digest && lease.reference.len_bytes === reference.len_bytes)?.ready;
        if (shared) {
            shared.pins++;
            try { await this.verify(shared.file, reference); saved(); return; }
            finally { shared.pins--; this.releaseReady(shared); }
        }
        const existing = await openCachedBlob(this.services.directory(), reference.digest);
        if (existing) {
            try {
                this.services.cache({ kind: 'observe', digest: reference.digest, length: existing.getSize() });
                await this.verify(existing, reference);
                saved(); return;
            }
            catch (error) { if (!(error instanceof CacheCorruption)) throw error; }
            finally { existing.close(); }
        }
        const stage = await this.temporary();
        const target = stage.file;
        try {
            const bytes = new Uint8Array(BLOB_CHUNK);
            for (let offset = 0; offset < reference.len_bytes; offset += bytes.length) {
                await this.services.yield(); if (this.stopped) throw new Error("File preparation retired");
                const part = bytes.subarray(0, Math.min(bytes.length, reference.len_bytes - offset));
                if (source.read(part, { at: offset }) !== part.length || target.write(part, { at: offset }) !== part.length) throw new Error('Could not retain file');
            }
            target.truncate(reference.len_bytes); target.flush(); target.close(); stage.closed = true;
            const handle = await this.services.directory().getFileHandle(stage.name) as FileSystemFileHandle & { move(name: string): Promise<void> };
            if (typeof handle.move !== 'function') throw new Error('Atomic file publication is unavailable on this web browser');
            if (this.stopped) throw new Error('File preparation retired');
            await handle.move(`blob-${reference.digest}`);
            saved();
        } finally { await this.discard(stage); }
    }
    // The recovery coordinator owns the retained handle and current-policy fence.
    // Persist verified content before publishing its fresh entry in the index.
    async restore(
        source: FileSystemSyncAccessHandle,
        reference: FileReference,
        check: () => void,
        receipt: (write: () => void) => void,
    ): Promise<void> {
        const release = this.reserveContent(reference);
        try {
            check();
            const encrypted = await this.temporary();
            try {
                if (await encryptBlob(reference, this.services.key(reference), source, encrypted.file, this.services.hash) !== reference.encrypted_key) {
                    throw new Error('Retained file envelope differs');
                }
            } finally { await this.discard(encrypted); }
            check();
            if (this.stopped) throw new Error('File preparation retired');
            await this.save(source, reference, receipt);
            check();
        } finally { release(); }
    }
    // An owned writer supplies a complete private spool. Rust seals its exact
    // digest and size before this step and publishes only after it returns.
    async prepareStage(source: FileSystemSyncAccessHandle, reference: FileReference): Promise<FileReference> {
        const release = this.reserveContent(reference);
        try {
            const encrypted = await this.temporary();
            try {
                const prepared = { ...reference };
                prepared.encrypted_key = await encryptBlob(prepared, this.services.key(prepared), source, encrypted.file, this.services.hash);
                if (this.stopped) throw new Error('File preparation retired');
                await this.save(source, prepared);
                if (this.stopped) throw new Error('File preparation retired');
                return prepared;
            } finally { await this.discard(encrypted); }
        } finally { release(); }
    }
    private async load(reference: FileReference): Promise<Ready> {
        this.reserve(); let file: FileSystemSyncAccessHandle | null = null;
        try {
            file = await openCachedBlob(this.services.directory(), reference.digest);
            if (file) {
                const cached = file;
                try {
                    this.services.cache({ kind: 'observe', digest: reference.digest, length: cached.getSize() });
                    const blocks = await this.verify(cached, reference);
                    if (this.stopped) throw new Error('File preparation retired');
                    const ready = { file: cached, blocks, pins: 0 }; file = null; return ready;
                } catch (error) {
                    if (!(error instanceof CacheCorruption) || !this.services.managed()) throw error;
                    cached.close(); file = null;
                }
            }
            if (!file) {
                if (!this.services.managed()) throw new Error('File content is unavailable');
                this.services.authorize(reference, false, true);
                const encrypted = await this.temporary();
                try {
                    await transferBlob(this.services.ap, this.services.operation(reference, false), reference, encrypted.file, false);
                    const plain = await this.temporary();
                    try {
                        await decryptBlob(reference, this.services.key(reference), encrypted.file, plain.file, this.services.hash);
                        this.services.authorize(reference, false, false);
                        if (this.stopped) throw new Error('File preparation retired');
                        await this.save(plain.file, reference);
                        this.services.cache({ kind: 'saved', digest: reference.digest, length: reference.len_bytes, confirmed: true });
                    } finally { await this.discard(plain); }
                } finally { await this.discard(encrypted); }
                file = await openCachedBlob(this.services.directory(), reference.digest);
                if (!file) throw new Error('Verified file publication failed');
            }
            const blocks = await this.verify(file, reference);
            if (this.stopped) throw new Error('File preparation retired');
            const ready = { file, blocks, pins: 0 }; file = null; return ready;
        } finally { file?.close(); this.active--; }
    }
    private async hydrate(lease: Lease, loaded: () => void): Promise<void> {
        const key = lease.reference.digest; let ready: Ready | undefined;
        let loading = this.loading.get(key);
        if (!loading) { loading = { promise: this.load(lease.reference), users: 0 }; this.loading.set(key, loading); }
        loading.users++;
        try {
            ready = await loading.promise;
            this.services.authorize(lease.reference, false, false);
            if (this.stopped) throw new Error('File preparation retired');
            lease.ready = ready;
        } catch (error) { lease.failed = code(error); }
        finally {
            try {
                if (--loading.users === 0) { this.loading.delete(key); if (ready) this.releaseReady(ready); }
            } catch (error) { lease.failed = code(error); }
            lease.preparing = false;
            // Complete retained reads before scheduling scene work. A scene
            // turn may itself be waiting for that same guest operation.
            loaded();
            if (!this.stopped) { lease.pending = true; try { await this.services.changed(); } finally { lease.pending = false; } }
        }
    }
    private readReference(reference: FileReference): FileReadResult {
        try {
            this.services.authorize(reference, false, false);
            const required = Math.min(reference.limit, Math.max(0, reference.len_bytes - reference.offset));
            if (!required) return { status: 0, total: BigInt(reference.len_bytes), bytes: new Uint8Array() };
            const key = identity(reference); let lease = this.leases.get(key);
            if (!lease) {
                this.room();
                const shared = [...this.leases.values()].find(other => other.ready && other.reference.digest === reference.digest && other.reference.len_bytes === reference.len_bytes);
                lease = { reference, ready: shared?.ready, preparing: !shared, pending: false };
                this.leases.set(key, lease);
                if (!shared) {
                    let loaded!: () => void;
                    lease.loaded = new Promise(resolve => { loaded = resolve; });
                    void this.hydrate(lease, loaded);
                }
            }
            this.leases.delete(key); this.leases.set(key, lease);
            if (lease.ready?.corrupt) return { status: -5 };
            if (lease.failed) return { status: lease.failed };
            if (!lease.ready) return { status: -6 };
            const bytes = new Uint8Array(required), ready = lease.ready;
            let copied = 0;
            for (let start = Math.floor(reference.offset / BLOB_CHUNK) * BLOB_CHUNK; start < reference.offset + required; start += BLOB_CHUNK) {
                const block = new Uint8Array(Math.min(BLOB_CHUNK, reference.len_bytes - start));
                if (ready.file.read(block, { at: start }) !== block.length) throw new Error('Verified file ended early');
                const hash = this.services.hash();
                try { hash.update(block); if (hash.finish() !== ready.blocks[start / BLOB_CHUNK]) { ready.corrupt = true; throw new Error('Verified file block changed'); } } finally { hash.close(); }
                const from = Math.max(reference.offset - start, 0), count = Math.min(block.length - from, required - copied);
                bytes.set(block.subarray(from, from + count), copied); copied += count;
            }
            this.services.authorize(reference, false, false);
            if (ready.file.getSize() !== reference.len_bytes) return { status: -10 };
            return { status: 0, total: BigInt(reference.len_bytes), bytes };
        } catch (error) { return { status: code(error) }; }
    }
    // Owned Files select their immutable version at Open. Current authority is
    // checked for that identity; replacing its path never selects different bytes.
    async readVersion(reference: FileReference): Promise<FileReadResult> {
        if (!Number.isSafeInteger(reference.offset) || reference.offset < 0
            || !Number.isInteger(reference.limit) || reference.limit < 0 || reference.limit > 65536
            || !Number.isSafeInteger(reference.len_bytes) || reference.len_bytes < 0 || reference.len_bytes > 64 * 1024 * 1024) {
            return { status: -3 };
        }
        let release: (() => void) | undefined;
        try { release = this.retain(reference); return await this.awaitReference({ ...reference }); }
        catch (error) { return { status: code(error) }; }
        finally { release?.(); }
    }
    private async awaitReference(reference: FileReference): Promise<FileReadResult> {
        try {
            const result = this.readReference(reference);
            if (result.status !== -6) return result;
            const loaded = this.leases.get(identity(reference))?.loaded;
            if (!loaded) return { status: -5 };
            await loaded;
            if (this.stopped) return { status: -5 };
            this.services.authorize(reference, false, false);
            return this.readReference(reference);
        } catch (error) { return { status: code(error) }; }
    }
    // Trusted root initialization supplies a host-validated seed and owns its
    // declaration-bound publication. Guest writes use complete private spools.
    async initialize(reference: FileReference, content: Uint8Array<ArrayBuffer>, publish: (reference: FileReference) => number): Promise<number> {
        return this.prepare(content, reference, publish);
    }
    private async prepare(content: Uint8Array<ArrayBuffer>, reference: FileReference, publish: (reference: FileReference) => number): Promise<number> {
        let release: (() => void) | undefined;
        try {
            release = this.reserveContent(reference);
            const plain = await this.temporary();
            try {
                for (let offset = 0; offset < content.length; offset += BLOB_CHUNK) {
                    const bytes = content.subarray(offset, offset + BLOB_CHUNK);
                    if (plain.file.write(bytes, { at: offset }) !== bytes.length) throw new Error('Could not prepare file');
                }
                plain.file.truncate(content.length); plain.file.flush();
                const encrypted = await this.temporary();
                try {
                    reference.encrypted_key = await encryptBlob(reference, this.services.key(reference), plain.file, encrypted.file, this.services.hash);
                    if (this.stopped) throw new Error('File preparation retired');
                    await this.save(plain.file, reference);
                    if (this.stopped) throw new Error("File preparation retired");
                    return publish(reference);
                } finally { await this.discard(encrypted); }
            } finally { await this.discard(plain); }
        } catch (error) { return code(error); }
        finally { release?.(); }
    }
    async upload(volumes: string[]): Promise<void> {
        for (const volume of volumes) {
            let after: [string, string] | null = null;
            do {
                const [references, next] = this.services.pending(volume, after); after = next;
                for (const reference of references) await this.uploadVersion(reference);
            } while (after);
        }
    }
    async uploadVersion(reference: FileReference): Promise<void> {
        this.services.authorize(reference, true, true);
        const release = this.reserveContent(reference);
        let plain: FileSystemSyncAccessHandle | null = null;
        const shared = [...this.leases.values()].find(lease => lease.ready && lease.reference.digest === reference.digest && lease.reference.len_bytes === reference.len_bytes)?.ready;
        if (shared) shared.pins++;
        try {
            plain = shared?.file ?? await openCachedBlob(this.services.directory(), reference.digest);
            if (!plain) throw new Error('Pending file content is unavailable');
            await this.verify(plain, reference);
            const encrypted = await this.temporary();
            try {
                if (await encryptBlob(reference, this.services.key(reference), plain, encrypted.file, this.services.hash) !== reference.encrypted_key) throw new Error('Pending file envelope changed');
                this.services.authorize(reference, true, true);
                await transferBlob(this.services.ap, this.services.operation(reference, true), reference, encrypted.file, true);
                if (this.stopped) throw new Error("File upload retired");
                this.services.authorize(reference, true, true); this.services.acknowledge(reference);
            } finally { await this.discard(encrypted); }
        } finally { if (shared) { shared.pins--; this.releaseReady(shared); } else plain?.close(); release(); }
    }
    retry(): void { for (const [key, lease] of this.leases) if ((lease.failed || lease.ready?.corrupt) && !lease.pending) { this.leases.delete(key); this.release(lease); } }
    close(): void {
        this.stopped = true; this.services.stop();
        const ready = new Set([...this.leases.values()].flatMap(lease => lease.ready ? [lease.ready] : []));
        this.leases.clear(); for (const lease of ready) this.releaseReady(lease);
    }
}
