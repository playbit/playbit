import { encodeId, decodeId, validId } from './identifier.ts';
// Detached backing files become visible through one durable routing pointer.
// The volume index remains the catalog; this store contains file generations only.
import { OpfsStore, OpfsResources } from './opfs-store.ts';
import type { DatabaseId } from './database-id.ts';
import { validDatabase } from './database-id.ts';

const COPY_BYTES = 64 * 1024, VOLUME_LIMIT = 32;
const validVolume = (id: string) => validId(id, 16) && !/^1+$/.test(id);
const bytes = (id: string) => decodeId(id);

const intentKey = (volume: string) => Uint8Array.of(255, ...bytes(volume));
const initializationKey = (volume: string) => Uint8Array.of(254, ...bytes(volume));

export interface VolumeInitialization { build: string; storage: number; complete: boolean }

export class LocalVolumeChanged extends Error {
    constructor() { super('Local volume changed during preparation'); }
}
export class PublicationUncertain extends Error {
    constructor(cause: unknown) { super('Volume publication requires reopening storage', { cause }); }
}

// Every attached backend shares this fence, including raw backend transactions.
// Begin and end both advance it: aborts cannot conceal a writer during capture.
export class VolumeWriteEpoch {
    private revision = 0;
    private writers = 0;
    private failed = false;
    begin(): void {
        // Each admitted writer reserves its eventual end advancement as well.
        if (this.failed || this.revision >= Number.MAX_SAFE_INTEGER - 1 - this.writers) throw new Error('Volume write epoch exhausted');
        this.revision++; this.writers++;
    }
    end(): void {
        if (!this.writers) throw new Error('Volume writer fence is unbalanced');
        this.writers--; this.revision++;
    }
    capture(): number {
        if (this.failed || this.writers) throw new LocalVolumeChanged();
        return this.revision;
    }
    check(expected: number): void {
        if (this.capture() !== expected) throw new LocalVolumeChanged();
    }
    stop(): void { this.failed = true; }
}

export interface PreparedVolume {
    readonly volume: string;
    readonly generation: string;
    readonly previous: string | null;
    readonly epoch: VolumeWriteEpoch;
    readonly revision: number;
}

export class VolumePublication {
    private pointers: OpfsStore;
    private epochs = new Map<string, VolumeWriteEpoch>();
    private stopped = false;
    private randomId: () => string;
    private constructor(pointers: OpfsStore, randomId: () => string) {
        this.randomId = randomId;
        this.pointers = pointers;
        try {
            if (this.pointers.keys.length > VOLUME_LIMIT * 3) throw new Error('Volume pointer capacity exceeded');
            let volumes = 0, pending = 0, initializations = 0;
            const identities = new Set<string>();
            for (const key of this.pointers.keys) {
                const intent = key.length === 34 && key.startsWith('ff');
                const initialization = key.length === 34 && key.startsWith('fe');
                const volume = encodeId(Uint8Array.from((intent || initialization ? key.slice(2) : key).match(/../g)!, byte => parseInt(byte, 16)));
                if (!validVolume(volume)) throw new Error('Corrupt volume pointer identity');
                identities.add(volume);
                if (intent) { pending++; this.pending(volume); }
                else if (initialization) { initializations++; this.initialization(volume); }
                else { volumes++; this.current(volume); }
            }
            if (volumes > VOLUME_LIMIT || pending > VOLUME_LIMIT || initializations > VOLUME_LIMIT || identities.size > VOLUME_LIMIT) throw new Error('Volume pointer capacity exceeded');
        } catch (error) { this.pointers.close(); throw error; }
    }
    static async open(file: FileSystemSyncAccessHandle, resources: OpfsResources,
        randomId: () => string, yieldReady: () => Promise<void>): Promise<VolumePublication> {
        return new VolumePublication(await OpfsStore.open(file, resources, yieldReady), randomId);
    }
    current(volume: string): string | null {
        if (this.stopped) throw new Error('Volume publication is stopped');
        if (!validVolume(volume)) throw new Error('Invalid volume identity');
        const value = this.pointers.get(bytes(volume));
        if (value === null) return null;
        if (value.length !== 16 || !validVolume(encodeId(value))) throw new Error('Corrupt volume generation');
        return encodeId(value);
    }
    initialization(volume: string): VolumeInitialization | null {
        this.current(volume);
        const value = this.pointers.get(initializationKey(volume));
        if (value === null) return null;
        if (value.length !== 34 || value[0]! > 1 || value[1]! > 3) throw new Error('Corrupt volume initialization');
        return { complete: value[0] === 1, storage: value[1]!, build: encodeId(value.subarray(2)) };
    }
    beginInitialization(volume: string, build: string, storage: number): void {
        if (this.initialization(volume) || !validId(build, 32) || !Number.isInteger(storage) || storage < 0 || storage > 3) throw new Error('Invalid volume initialization');
        if (!this.current(volume) && !this.pending(volume)) throw new Error('Volume initialization has no admitted controls');
        this.writeInitialization(volume, { build, storage, complete: false });
    }
    finishInitialization(volume: string, build: string): void {
        const initialization = this.initialization(volume);
        if (!initialization || initialization.complete || initialization.build !== build || !this.current(volume)) throw new Error('Volume initialization changed');
        this.writeInitialization(volume, { ...initialization, complete: true });
    }
    private writeInitialization(volume: string, initialization: VolumeInitialization): void {
        let transaction;
        try {
            transaction = this.pointers.begin(true);
            transaction.put(initializationKey(volume), Uint8Array.of(Number(initialization.complete), initialization.storage, ...decodeId(initialization.build, 32)));
            transaction.end(true);
        } catch (cause) {
            if (transaction && !transaction.ended) transaction.end(false);
            this.stopped = true; this.epochs.get(volume)?.stop(); throw new PublicationUncertain(cause);
        }
    }
    epoch(volume: string): VolumeWriteEpoch {
        this.current(volume);
        let epoch = this.epochs.get(volume);
        if (!epoch) {
            if (this.epochs.size === VOLUME_LIMIT) throw new Error('Volume fence capacity exceeded');
            this.epochs.set(volume, epoch = new VolumeWriteEpoch());
        }
        return epoch;
    }
    prepare(volume: string): PreparedVolume {
        const previous = this.current(volume), epoch = this.epoch(volume), revision = epoch.capture();
        if (previous === null && this.pointers.keys.filter(key => key.length === 32).length === VOLUME_LIMIT) throw new Error('Volume pointer capacity exceeded');
        const generation = this.randomId();
        if (!validVolume(generation) || generation === previous) throw new Error('Invalid fresh volume generation');
        return { volume, generation, previous, epoch, revision };
    }
    directory(volume: string, generation: string): string {
        if (!validVolume(volume) || !validVolume(generation)) throw new Error('Invalid backing address');
        return `${volume}-${generation}`;
    }
    async file(root: FileSystemDirectoryHandle, volume: string, generation: string,
        database: DatabaseId, create: boolean): Promise<FileSystemFileHandle> {
        if (!validVolume(volume) || !validVolume(generation) || !validDatabase(database)) throw new Error('Invalid backing address');
        const directory = await root.getDirectoryHandle(this.directory(volume, generation));
        return directory.getFileHandle(`${database}.kvdb`, { create });
    }
    async acceptedIndex(root: FileSystemDirectoryHandle, prepared: PreparedVolume): Promise<FileSystemFileHandle> {
        this.check(prepared);
        const directory = await root.getDirectoryHandle(this.directory(prepared.volume, prepared.generation));
        return directory.getFileHandle('accepted-index.kvdb', { create: true });
    }
    pending(volume: string): string | null {
        this.current(volume);
        const value = this.pointers.get(intentKey(volume));
        if (value === null) return null;
        const generation = encodeId(value);
        if (value.length !== 16 || !validVolume(generation) || generation === this.current(volume)) throw new Error('Corrupt volume publication intent');
        return generation;
    }
    async create(root: FileSystemDirectoryHandle, prepared: PreparedVolume): Promise<void> {
        this.check(prepared);
        if (this.pending(prepared.volume)) throw new Error('Volume preparation is already pending');
        const name = this.directory(prepared.volume, prepared.generation);
        try { await root.getDirectoryHandle(name); throw new Error('Fresh backing generation already exists'); }
        catch (error) { if ((error as DOMException).name !== 'NotFoundError') throw error; }
        this.check(prepared);
        this.writeIntent(prepared);
        await root.getDirectoryHandle(name, { create: true });
    }
    private writeIntent(prepared: PreparedVolume): void {
        let transaction;
        try {
            transaction = this.pointers.begin(true);
            transaction.put(intentKey(prepared.volume), bytes(prepared.generation)); transaction.end(true);
        } catch (cause) {
            if (transaction && !transaction.ended) transaction.end(false);
            this.stopped = true; prepared.epoch.stop(); throw new PublicationUncertain(cause);
        }
    }
    async discard(root: FileSystemDirectoryHandle, volume: string, generation: string): Promise<void> {
        if (this.pending(volume) !== generation) return;
        try { await root.removeEntry(this.directory(volume, generation), { recursive: true }); }
        catch (error) { if ((error as DOMException).name !== 'NotFoundError') throw error; }
        const transaction = this.pointers.begin(true);
        try { transaction.put(intentKey(volume), null); transaction.end(true); }
        catch (cause) { if (!transaction.ended) transaction.end(false); this.stopped = true; throw new PublicationUncertain(cause); }
    }
    async discardPending(root: FileSystemDirectoryHandle): Promise<void> {
        // No attached host exists yet. Only persisted unselected intents are removed.
        for (const key of this.pointers.keys.slice()) if (key.length === 34 && key.startsWith('ff')) {
            const volume = encodeId(Uint8Array.from(key.slice(2).match(/../g)!, byte => parseInt(byte, 16))); await this.discard(root, volume, this.pending(volume)!);
        }
    }
    // Called with volume preparation serialized and the current host attached.
    // Partial captures may still route named backings into an older generation.
    // Query each file against persistent index metadata; never retain a second catalog.
    async reclaim(root: FileSystemDirectoryHandle, volume: string,
        referenced: (database: DatabaseId, generation: string) => boolean): Promise<void> {
        const current = this.current(volume), pending = this.pending(volume);
        if (!current) return;
        const check = () => {
            if (this.current(volume) !== current || this.pending(volume) !== pending) throw new LocalVolumeChanged();
        };
        for await (const handle of root.values()) {
            check();
            if (handle.kind !== 'directory' || !handle.name.startsWith(`${volume}-`)) continue;
            const generation = handle.name.slice(volume.length + 1);
            if (!validVolume(generation) || generation === current || generation === pending) continue;
            const directory = await root.getDirectoryHandle(handle.name);
            let retained = false;
            for await (const file of directory.values()) {
                check();
                if (file.kind === 'file' && file.name === 'accepted-index.kvdb') {
                    await directory.removeEntry(file.name); continue;
                }
                const database = file.name.endsWith('.kvdb') ? file.name.slice(0, -5) : '';
                if (file.kind !== 'file' || !validDatabase(database)) {
                    retained = true; continue;
                }
                // Synchronous lookup immediately precedes deletion. Serialized
                // publication cannot add a new old-generation reference here.
                if (database !== 'index' && database !== 'revocations' && referenced(database, generation)) {
                    retained = true; continue;
                }
                await directory.removeEntry(file.name);
            }
            check();
            if (!retained) await root.removeEntry(handle.name);
        }
    }
    check(prepared: PreparedVolume): void {
        if (this.current(prepared.volume) !== prepared.previous) throw new LocalVolumeChanged();
        prepared.epoch.check(prepared.revision);
    }
    // validate and activate are synchronous host operations. The caller flushes
    // and validates every required file before entering this publication step.
    publish(prepared: PreparedVolume, validate: () => void, activate: () => void): void {
        this.check(prepared);
        if (validate() !== undefined) throw new Error('Volume validation must be synchronous');
        this.check(prepared);
        if (this.pending(prepared.volume) !== prepared.generation) throw new Error('Volume preparation intent differs');
        let transaction;
        try {
            transaction = this.pointers.begin(true);
            transaction.put(bytes(prepared.volume), bytes(prepared.generation));
            transaction.put(intentKey(prepared.volume), null);
            transaction.end(true);
            if (activate() !== undefined) throw new Error('Volume activation must be synchronous');
        } catch (cause) {
            if (transaction && !transaction.ended) transaction.end(false);
            this.stopped = true; prepared.epoch.stop();
            throw new PublicationUncertain(cause);
        }
    }
    close(): void { this.stopped = true; this.pointers.close(); }
}

// File snapshots and journal prefixes are immutable. A local edit invalidates
// the complete attempt; no copied prefix is installed into an attached backend.
export async function copyVolumeBacking(prepared: PreparedVolume, source: Blob,
    target: FileSystemSyncAccessHandle, yieldReady: () => Promise<void>): Promise<void> {
    prepared.epoch.check(prepared.revision);
    if (target.getSize() !== 0 || !Number.isSafeInteger(source.size)) throw new Error('Invalid detached backing');
    for (let offset = 0; offset < source.size; offset += COPY_BYTES) {
        let chunk: Uint8Array;
        try { chunk = new Uint8Array(await source.slice(offset, Math.min(offset + COPY_BYTES, source.size)).arrayBuffer()); }
        catch (error) { prepared.epoch.check(prepared.revision); throw error; }
        prepared.epoch.check(prepared.revision);
        if (target.write(chunk, { at: offset }) !== chunk.length) throw new Error('Detached backing write was incomplete');
        await yieldReady();
    }
    target.flush(); prepared.epoch.check(prepared.revision);
}
