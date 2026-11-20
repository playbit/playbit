import { MAX_TEXT_EVENT_BYTES } from "./text-input.ts";
import { ABI as L } from './guest-layout.ts';
import { ChannelHost } from "./channel-host.ts";
import type { ChannelExports } from "./channel-host.ts";
import type { GuestOperations } from "./guest-imports.ts";
import { AbiError, Input, Output, admit, bytes, captureResult, disjoint, view, record, recordArray, publishRecord } from "./guest-memory.ts";
import type { BufferLayout, ResultSnapshot } from "./guest-memory.ts";
import { fileOperations } from "./file-operations.ts";
import type { FileCompletion, OwnedFileHost } from "./file-operations.ts";
import type { FileBackend } from "./file-backend.ts";
import { GuestData } from "./guest-data.ts";
import type { GuestWork, QueuedEvent } from "./guest-work.ts";
export interface ObjectHost extends OwnedFileHost, ChannelExports {
    memory: WebAssembly.Memory;
    pb_input(length: number): number;
    pb_output(): number;
    pb_web_result(): number;
    pb_web_volumes(): number;
    pb_web_volumes_acquire(revision: bigint): number;
    pb_web_file_dispose(generation: number, slot: number): number;
    pb_web_capabilities(): number;
    pb_web_host_limit(limit: number): number;
    pb_web_entry_resolve(generation: bigint, slot: number, name: number, length: number): number;
    pb_web_volume_authorize(generation: bigint, slot: number, rights: number): number;
    pb_web_volume_check(generation: bigint, slot: number, rights: number): number;
    pb_web_entry_authorize(entry: number): number;
    pb_web_kv_keys(entry: number, query: number, after: number, length: number): number;
    pb_web_kv_read(entry: number, key: number, length: number, query: number): number;
    pb_web_kv_put(entry: number, key: number, length: number, value: number, valueLength: number): number;
    pb_web_kv_delete(entry: number, key: number, length: number): number;
    pb_web_bloblog_append(entry: number, blobs: number, count: number, stride: number, appendId: number): number;
    pb_web_bloblog_read(entry: number, cursor: number, length: number): number;
    pb_web_schema_read(entry: number): number;
    pb_web_schema_establish(entry: number, types: number, count: number, stride: number): number;
    pb_web_object_read(entry: number, object: number, fields: number, count: number): number;
    pb_web_object_query(entry: number, query: number): number;
    pb_web_object_create(entry: number, type: number, fields: number, count: number, stride: number): number;
    pb_web_object_edit(entry: number, object: number, fields: number, count: number, stride: number): number;
}
export interface LogConnection { generation: number; expires: number }
interface Services {
    logConnection?(): Promise<LogConnection>;
    appendLog?(prepared: Uint8Array<ArrayBuffer>, appendId: Uint8Array<ArrayBuffer>, connection: LogConnection): Promise<void>;
    readLog?(entry: Uint8Array<ArrayBuffer>, read: () => Promise<Uint8Array<ArrayBuffer>>): Promise<Uint8Array<ArrayBuffer>>;
    channels?(): import("./session-channels.ts").SessionChannels;
    memory(): WebAssembly.Memory;
    host(): ObjectHost;
    events: QueuedEvent[];
    work: GuestWork;
    log(text: string): void;
    wouldBlock?(): void;
    submit(scene: Uint8Array<ArrayBuffer>): void;
    ready?(): void;
    revision?(): string;
    execute?<T>(callback: () => T): Promise<T>;
    files?(): FileBackend;
    fileFailure?(error: unknown): void;
    admitOperation?(opcode: number): number;
}
const decoder = new TextDecoder("utf-8", { fatal: true });
interface Retained { bytes?: Uint8Array<ArrayBuffer>; snapshot?: ResultSnapshot; id?: Uint8Array<ArrayBuffer>; entry?: Uint8Array<ArrayBuffer>; owned?: FileCompletion }
export type RuntimeOperations = GuestOperations & { data: GuestData<Retained>; reset(): void; refreshVolumes(): void; readonly quitOnLastWindow: boolean; channelEvent(name:string,source:Uint8Array,message:Uint8Array,data:Uint8Array):boolean };
const objectLayout: readonly BufferLayout[] = [[24, L.field, 256], [40, L.value, 4096], [56, null, 1048576]];
const maximumPageValues = Math.floor((16 * 1024 * 1024 - 64 * 256 * 40 - 1048576 - 64 * 48) / 80);
const pageLayout: readonly BufferLayout[] = [[32, L.objectInfo, 64], [48, L.field, 16384], [64, L.value, maximumPageValues], [80, null, 1048576]];
const schemaLayout: readonly BufferLayout[] = [[8, L.schemaType, 256], [24, L.schemaField, 4096], [40, null, 1048576]];
const volumesLayout: readonly BufferLayout[] = [[16, L.volume, 256], [32, L.range, 16384], [48, null, 1048576]];
export function guestOperations(services: Services): RuntimeOperations {
    let quitOnLastWindow: boolean | undefined;
    const channelHost=new ChannelHost(()=>services.host());
    const session=()=>{const client=services.channels?.();if(!client)throw new AbiError(-7);return client;};
    let volumeWatch: bigint | undefined;
    let volumes: { status: number; snapshot?: ResultSnapshot } | undefined;
    const pending = new GuestData<Retained>(services.work, () => services.ready?.(), () => services.revision?.() ?? "");
    const execute = <T>(callback: () => T): Promise<T> => services.execute ? services.execute(callback) : Promise.resolve().then(callback);
    const authority = () => services.admitOperation?.(0x7) ?? 0;
    function volumeRevision(): bigint | undefined {
        const prefix = volumes?.snapshot?.prefix;
        return prefix && new DataView(prefix.buffer, prefix.byteOffset, prefix.byteLength).getBigUint64(8, true);
    }
    function queueVolumeChange(): void {
        const revision = volumeRevision();
        if (volumeWatch === undefined || revision === undefined || volumeWatch >= revision || services.work.draining) return;
        const data = new Uint8Array(8);
        new DataView(data.buffer).setBigUint64(0, revision, true);
        try { services.work.queue({ kind: 17, target: 0, data }); }
        catch (error) { if (error instanceof AbiError && error.status === -9) return; throw error; }
        volumeWatch = undefined;
        services.ready?.();
    }
    function begin(kind: number, credit: number, run: () => Promise<{ status: number; value?: Retained }>, authorize: () => number, log?: string): bigint {
        return pending.begin(kind, credit + 256, run, () => authority() || authorize(), undefined, kind === 16 || kind === 14, log);
    }
    function mount(generation: bigint, slot: number, rights: number): () => number {
        const check = () => authority() || services.host().pb_web_volume_check(generation, slot, rights);
        const status = check();
        if (status) throw new AbiError(status);
        return check;
    }
    function fileMount(reference: Uint8Array<ArrayBuffer>, rights: number): () => number {
        const record = new DataView(reference.buffer);
        const current = mount(BigInt(record.getUint32(12, true)), record.getUint32(8, true), rights);
        return () => {
            const status = current();
            if (status || rights !== 1) return status;
            const host = services.host(), input = host.pb_input(40);
            if (!input) return -9;
            const bytes = new Uint8Array(host.memory.buffer, input, 40);
            bytes.fill(0); bytes.set(reference, 8);
            return host.pb_web_entry_authorize(8);
        };
    }
    function catalogMount(generation: bigint, slot: number): () => number {
        const current = mount(generation, slot, 1);
        return () => current() || services.host().pb_web_volume_authorize(generation, slot, 1);
    }
    function hostCall(source: Uint8Array<ArrayBuffer>, operation: (host: ObjectHost) => number, result: (host: ObjectHost) => Retained): Promise<{ status: number; value?: Retained }> {
        return execute(() => {
            const host = services.host(), pointer = host.pb_input(source.length);
            if (!pointer) return { status: -9 };
            new Uint8Array(host.memory.buffer, pointer, source.length).set(source);
            const status = operation(host);
            return { status, value: status ? undefined : result(host) };
        });
    }

    function bytesResult(host: ObjectHost, length: number, maximum: number): Retained {
        if (length < 0 || length > maximum) throw new AbiError(-9);
        const region = admit(host.memory, host.pb_output(), length, 1, 1, maximum);
        return { bytes: bytes(host.memory, region).slice() };
    }
    function bytesTake(generation: bigint, slot: number, kind: number, pointer: number, capacity: number, count: number, limit: number): number | bigint {
        pending.admit(generation, slot, kind);
        const memory = services.memory(), header = admit(memory, count, 1, 4, 4, 1);
        const output = admit(memory, pointer, capacity, 1, 1, limit); disjoint([header, output]);
        return pending.take(generation, slot, kind, value => {
            const frame = value.bytes!; view(memory, header).setUint32(0, frame.length, true);
            if (frame.length > output.length) return -11;
            bytes(memory, output).set(frame); return 0;
        });
    }
    function call(operation: () => number | bigint | Promise<number | bigint>): number | bigint | Promise<number | bigint> {
        try {
            const result = operation();
            const done = (status: number | bigint) => { if (status === -6) services.wouldBlock?.(); return status; };
            return typeof result === 'number' || typeof result === 'bigint' ? done(result) : result.then(done);
        }
        catch (error) {
            if (error instanceof AbiError) {
                if (error.status === -6) services.wouldBlock?.();
                return error.status;
            }
            // Runtime/engine traps must reach enterGuest, which halts this instance.
            throw error;
        }
    }
    return {
        admitOperation: services.admitOperation,
        data: pending,
        channelEvent(name,source,message,data){
            const handle=channelHost.find(name);if(!handle.length)return false;
            const payload=new Uint8Array(104+data.length);payload.set(handle,0);payload.set(source,8);payload.set(message,88);payload.set(data,104);services.work.queue({kind:18,target:0,data:payload});services.ready?.();return true;
        },
        reset: () => {
            // Clear owner declarations only after accepted work has drained.
            pending.reset();
            quitOnLastWindow = undefined;
            volumes = undefined;
            volumeWatch = undefined;
        },
        get quitOnLastWindow() { return quitOnLastWindow ?? true; },
        pb_session: () => call(()=>{void session().start().catch(()=>{});return channelHost.epoch();}),
        pb_channel_open: (pointer,length) => call(()=>{
            const memory=services.memory(), source=admit(memory,pointer,length,1,1,223);
            void session();
            return new DataView(channelHost.open(bytes(memory,source)).buffer).getBigUint64(0, true);
        }),
        pb_channel_begin: (generation,slot,action,pointer,length) => call(()=>{
            const name=channelHost.name(generation,slot,action);
            const memory=services.memory(), source=admit(memory,pointer,length,1,1,32768);
            const data=bytes(memory,source).slice();
            channelHost.command(name,action,data.length);
            const result=begin(16,data.length,async()=>{try{await session().command(name,action,data);return {status:0,value:{}};}catch(error){return {status:error instanceof AbiError?error.status:-5};}},()=>0);
            if(action===4)channelHost.close(generation,slot);return result;
        }),
        pb_channel_take: (generation,slot) => call(()=>pending.take(generation,slot,16,()=>0)),
        pb_identity: pointer => call(() => {
            const memory = services.memory(), output = record(memory, pointer, L.identity), identity = session().identity;
            const data = new Uint8Array(L.identity.size), result = new DataView(data.buffer);
            result.setBigUint64(8, services.work.epoch, true); data.set(identity.account, 16); data.set(identity.device, 32);
            data.set(identity.instance, 48); data.set(identity.session, 64); result.setUint32(96, 2, true);
            publishRecord(memory, output, L.identity, data); return 0;
        }),
        pb_presence: (pointer, capacity, stride, count) => call(() => {
            const memory = services.memory(), header = admit(memory, count, 1, 4, 4, 1);
            const output = recordArray(memory, pointer, capacity, stride, L.participant, false, 64); disjoint([header, output]);
            const participants = session().participants; view(memory, header).setUint32(0, participants.length, true);
            if (capacity < participants.length) return -11;
            participants.forEach((participant, i) => {
                const data = new Uint8Array(L.participant.size); data.set(participant, 8);
                publishRecord(memory, record(memory, pointer + i * stride, L.participant), L.participant, data);
            }); return 0;
        }),
        pb_quit_on_last_window: intent => {
            if (intent !== 0 && intent !== 1) return -3;
            const choice = intent === 1;
            if (quitOnLastWindow !== undefined && quitOnLastWindow !== choice) return -10;
            quitOnLastWindow = choice;
            return 0;
        },
        refreshVolumes: () => {
            const host = services.host(); host.pb_input(0);
            const status = host.pb_web_volumes();
            volumes = { status, snapshot: status ? undefined : captureResult(host.memory, host.pb_web_result(), L.volumes, volumesLayout, 16, 16 * 1024 * 1024) };
            queueVolumeChange();
        },
        pb_random_fill: (pointer, length) => call(() => {
            const memory = services.memory(), output = admit(memory, pointer, length, 1, 1, 65536);
            if (!length) return 0;
            const destination = bytes(memory, output);
            try { globalThis.crypto.getRandomValues(destination); }
            catch { destination.fill(0); return -5; }
            return 0;
        }),
        pb_host_platform: () => 3, // PB_PLATFORM_WEB, independent of the OS.
        pb_host_limit: limit => call(() => services.host().pb_web_host_limit(limit)),
        pb_log: (pointer, length) => call(() => {
            const memory = services.memory(), region = admit(memory, pointer, length, 1, 1, 65536);
            let text: string;
            try { text = decoder.decode(bytes(memory, region)); } catch { return -3; }
            services.log(text); return 0;
        }),
        pb_view_submit: (pointer, length) => call(() => {
            const memory = services.memory(), region = admit(memory, pointer, length, 1, 1, 1024 * 1024);
            services.submit(bytes(memory, region)); return 0;
        }),
        pb_event_poll: (pointer, payload, capacity) => call(() => {
            const memory = services.memory(), header = record(memory, pointer, L.event);
            const output = admit(memory, payload, capacity, 1, 1, MAX_TEXT_EVENT_BYTES);
            disjoint([header, output]);
            const event = services.events[0]; if (!event) return -6;
            if (event.data.length > output.length) return event.data.length;
            bytes(memory, output).set(event.data);
            const data = new Uint8Array(L.event.size), result = new DataView(data.buffer);
            result.setBigUint64(8, services.work.epoch, true); result.setUint32(16, event.kind, true); result.setUint32(20, event.target, true);
            publishRecord(memory, header, L.event, data);
            services.work.poll(); if(event.kind===18)session().consumed(event.data.length,event.data.slice(8,88),event.data.slice(88,104)); queueVolumeChange(); return event.data.length;
        }),
        pb_entry_resolve_begin: (generation, slot, name, length) => call(() => {
            const memory = services.memory();
            const input = new Input(memory), nameOffset = input.text(name, length, 4096), source = input.finish();
            return begin(17, 2 * source.length + 32, () => hostCall(source,
                host => host.pb_web_entry_resolve(generation, slot, nameOffset, length),
                host => ({ entry: new Uint8Array(host.memory.buffer, host.pb_output(), 32).slice() })), catalogMount(generation, slot));
        }),
        pb_entry_resolve_take: (generation, slot, pointer) => call(() => {
            pending.admit(generation, slot, 17);
            const memory = services.memory(), output = record(memory, pointer, L.entryHandle);
            return pending.take(generation, slot, 17, value => { publishRecord(memory, output, L.entryHandle, value.entry!); return 0; });
        }),
        pb_kv_keys_begin: (entry, query, after, length) => call(() => {
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entry(entry);
            const queryOffset = input.kvQuery(query, "keys", length !== 0), afterOffset = input.text(after, length, 511), source = input.finish();
            let count = 0;
            return begin(19, 2 * source.length + 33895, () => hostCall(source,
                host => { count = host.pb_web_kv_keys(entryOffset, queryOffset, afterOffset, length); return count < 0 ? count : 0; },
                host => bytesResult(host, count, 33895)), fileMount(source.slice(entryOffset, entryOffset + 32), 1));
        }),
        pb_kv_keys_take: (generation, slot, pointer, capacity, count) => call(() => bytesTake(generation, slot, 19, pointer, capacity, count, 33895)),
        pb_kv_read_begin: (entry, key, length, query) => call(() => {
            if (!length) return -3;
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entry(entry), queryOffset = input.kvQuery(query, "read");
            const keyOffset = input.text(key, length, 511), source = input.finish();
            let count = 0;
            return begin(20, 2 * source.length + 61489, () => hostCall(source,
                host => { count = host.pb_web_kv_read(entryOffset, keyOffset, length, queryOffset); return count < 0 ? count : 0; },
                host => bytesResult(host, count, 61489)), fileMount(source.slice(entryOffset, entryOffset + 32), 1));
        }),
        pb_kv_read_take: (generation, slot, pointer, capacity, count) => call(() => bytesTake(generation, slot, 20, pointer, capacity, count, 61489)),
        pb_kv_put_begin: (entry, key, length, value, valueLength) => call(() => {
            if (!length) return -3;
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entry(entry);
            const keyOffset = input.text(key, length, 511), valueOffset = input.text(value, valueLength, 61440), source = input.finish();
            return begin(21, 2 * source.length,
                () => hostCall(source, host => host.pb_web_kv_put(entryOffset, keyOffset, length, valueOffset, valueLength), () => ({})),
                fileMount(source.slice(entryOffset, entryOffset + 32), 2));
        }),
        pb_kv_put_take: (generation, slot) => call(() => pending.take(generation, slot, 21, () => 0)),
        pb_kv_delete_begin: (entry, key, length) => call(() => {
            if (!length) return -3;
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entry(entry), keyOffset = input.text(key, length, 511), source = input.finish();
            return begin(22, 2 * source.length,
                () => hostCall(source, host => host.pb_web_kv_delete(entryOffset, keyOffset, length), () => ({})),
                fileMount(source.slice(entryOffset, entryOffset + 32), 2));
        }),
        pb_kv_delete_take: (generation, slot) => call(() => pending.take(generation, slot, 22, () => 0)),
        pb_bloblog_append_begin: (generation, slot, log, blobs, count, stride, appendId) => call(() => {
            if (!appendId) throw new AbiError(-3);
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entryFromId(generation, slot, log);
            const idOffset = input.ids(appendId, 1, 1), blobsOffset = input.blobs(blobs, count, stride), source = input.finish();
            const entry = source.slice(entryOffset, entryOffset + 32);
            const connection = services.logConnection?.().catch(error => error instanceof AbiError ? error : new AbiError(-5));
            let length = 0;
            return begin(13, 2 * source.length + 104, async () => {
                if (!services.appendLog || !connection) return { status: -5 };
                const captured = await connection;
                if (captured instanceof AbiError) throw captured;
                const prepared = await hostCall(source,
                    host => { length = host.pb_web_bloblog_append(entryOffset, blobsOffset, count, L.bloblogBlob.size, idOffset); return length < 0 ? length : 0; },
                    host => bytesResult(host, length, 65672));
                if (prepared.status) return prepared;
                const bytes = prepared.value!.bytes!;
                await services.appendLog(bytes, source.slice(idOffset, idOffset + 16), captured);
                return { status: 0, value: { bytes: bytes.slice(32, 32 + bytes[31]!) } };
            }, fileMount(entry, 2), Array.from(entry).join(','));
        }),
        pb_bloblog_append_take: (generation, slot, pointer, capacity, count) => call(() => bytesTake(generation, slot, 13, pointer, capacity, count, 104)),
        pb_bloblog_read_begin: (generation, slot, log, cursor, length) => call(() => {
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entryFromId(generation, slot, log);
            const cursorOffset = input.text(cursor, length, 104), source = input.finish();
            let count = 0;
            const entry = source.slice(entryOffset, entryOffset + 32);
            return begin(14, 2 * source.length + 65645, async () => {
                if (!services.readLog) return { status: -5 };
                const read = async () => {
                    const result = await hostCall(source,
                        host => { count = host.pb_web_bloblog_read(entryOffset, cursorOffset, length); return count < 0 ? count : 0; },
                        host => bytesResult(host, count, 65645));
                    if (result.status) throw new AbiError(result.status);
                    return result.value!.bytes!;
                };
                return { status: 0, value: { bytes: await services.readLog(entry, read) } };
            }, fileMount(entry, 1));
        }),
        pb_bloblog_read_take: (generation, slot, pointer, capacity, count) => call(() => bytesTake(generation, slot, 14, pointer, capacity, count, 65645)),
        pb_schema_read_begin: (entry) => call(() => {
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entry(entry), source = input.finish();
            return begin(1, 2 * source.length + 256 * 48 + 4096 * 56 + 1048576 + 52,
                () => hostCall(source, host => host.pb_web_schema_read(entryOffset),
                    host => ({ snapshot: captureResult(host.memory, host.pb_web_result(), L.schemaSchema, schemaLayout, 8, 256 * 48 + 4096 * 56 + 1048576 + 8) })),
                fileMount(source.slice(entryOffset, entryOffset + 32), 1));
        }),
        pb_schema_read_take: (generation, slot, pointer) => call(() => {
            pending.admit(generation, slot, 1);
            const output = new Output(services.memory(), pointer, L.schemaSchema, schemaLayout);
            return pending.take(generation, slot, 1, value => output.copySnapshot(value.snapshot!));
        }),
        pb_schema_establish_begin: (entry, types, count, stride) => call(() => {
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entry(entry), typesOffset = input.schema(types, count, stride), source = input.finish();
            return begin(2, 2 * source.length,
                () => hostCall(source, host => host.pb_web_schema_establish(entryOffset, typesOffset, count, L.schemaTypeInput.size), () => ({})),
                fileMount(source.slice(entryOffset, entryOffset + 32), 4));
        }),
        pb_schema_establish_take: (generation, slot) => call(() => pending.take(generation, slot, 2, () => 0)),
        pb_object_query_begin: (entry, query) => call(() => {
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entry(entry), queryOffset = input.query(query), source = input.finish();
            const record = new DataView(source.buffer, queryOffset), limit = record.getUint32(44, true), count = record.getUint32(52, true);
            if (!limit) return -3;
            if (limit > 64) return -9;
            const projected = record.getUint32(40, true) & 2;
            // Reserve before execution; reject oversized output before copying it.
            // Input and operation control also count against Rust's 16 MiB owner.
            const worst = limit * 48 + 32 + (projected ? limit * count * 40 + Math.min(limit * 4096, maximumPageValues) * 80 + 1048576 : 0);
            const reserve = Math.min(worst, 16 * 1024 * 1024 - 2 * source.length - 256);
            return begin(6, 2 * source.length + reserve, () => hostCall(source,
                host => host.pb_web_object_query(entryOffset, queryOffset),
                host => ({ snapshot: captureResult(host.memory, host.pb_web_result(), L.page, pageLayout, 32, reserve) })), fileMount(source.slice(entryOffset, entryOffset + 32), 1));
        }),
        pb_object_query_take: (generation, slot, pointer) => call(() => {
            pending.admit(generation, slot, 6);
            const output = new Output(services.memory(), pointer, L.page, pageLayout);
            return pending.take(generation, slot, 6, value => output.copySnapshot(value.snapshot!));
        }),
        pb_object_read_begin: (entry, object, fields, count) => call(() => {
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entry(entry), objectOffset = input.ids(object, 1, 1), fieldsOffset = input.ids(fields, count), source = input.finish();
            return begin(4, 2 * source.length + 256 * 40 + 4096 * 80 + 1048576 + 68, () => hostCall(source,
                host => host.pb_web_object_read(entryOffset, objectOffset, fieldsOffset, count),
                host => ({ snapshot: captureResult(host.memory, host.pb_web_result(), L.object, objectLayout, 24, 256 * 40 + 4096 * 80 + 1048576 + 24) })), fileMount(source.slice(entryOffset, entryOffset + 32), 1));
        }),
        pb_object_read_take: (generation, slot, pointer) => call(() => {
            pending.admit(generation, slot, 4);
            const output = new Output(services.memory(), pointer, L.object, objectLayout);
            return pending.take(generation, slot, 4, value => output.copySnapshot(value.snapshot!));
        }),
        pb_object_create_begin: (entry, type, fields, count, stride) => call(() => {
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entry(entry), typeOffset = input.ids(type, 1, 1), fieldsOffset = input.changes(fields, count, stride), source = input.finish();
            return begin(3, 2 * source.length + 16, () => hostCall(source,
                host => host.pb_web_object_create(entryOffset, typeOffset, fieldsOffset, count, L.fieldInput.size),
                host => ({ id: new Uint8Array(host.memory.buffer, host.pb_output(), 16).slice() })), fileMount(source.slice(entryOffset, entryOffset + 32), 2));
        }),
        pb_object_create_poll: (generation, slot, pointer) => call(() => {
            pending.admit(generation, slot, 3);
            const memory = services.memory(), output = admit(memory, pointer, 1, 16, 1, 1);
            return pending.take(generation, slot, 3, value => { bytes(memory, output).set(value.id!); return 0; });
        }),
        pb_object_edit_begin: (entry, object, fields, count, stride) => call(() => {
            const memory = services.memory();
            const input = new Input(memory), entryOffset = input.entry(entry), objectOffset = input.ids(object, 1, 1), fieldsOffset = input.changes(fields, count, stride), source = input.finish();
            return begin(5, 2 * source.length, () => hostCall(source,
                host => host.pb_web_object_edit(entryOffset, objectOffset, fieldsOffset, count, L.fieldInput.size), () => ({})), fileMount(source.slice(entryOffset, entryOffset + 32), 2));
        }),
        pb_object_edit_take: (generation, slot) => call(() => pending.take(generation, slot, 5, () => 0)),
        pb_capabilities: pointer => call(() => {
            const output = new Output(services.memory(), pointer, L.capabilities, [[8, L.range, 256], [24, null, 32768]]);
            const host = services.host(); host.pb_input(0);
            const status = host.pb_web_capabilities();
            return status || output.copy(host.memory, host.pb_web_result());
        }),
        pb_volumes_observe: revision => call(() => {
            const current = volumeRevision();
            if (current === undefined) return volumes?.status || -5;
            if (revision === 0n || revision > current) return -3;
            if (services.work.draining) return -10;
            services.work.cancelVolumeChanges();
            volumeWatch = revision;
            queueVolumeChange();
            return 0;
        }),
        pb_operation_dispose: (generation, slot) => call(() => pending.disposeOperation(generation, slot)),
        ...fileOperations({ memory: services.memory, host: services.host, data: pending, step: execute, authority,
            backend: () => { if (!services.files) throw new AbiError(-7); return services.files(); },
            failure: error => { if (services.fileFailure) services.fileFailure(error); else throw error; },
        }),
        pb_volumes: pointer => call(() => {
            const memory = services.memory();
            const output = new Output(memory, pointer, L.volumes, volumesLayout);
            if (!volumes) return -5;
            if (volumes.status) return volumes.status;
            const snapshot = volumes.snapshot!;
            if (!output.fits(snapshot)) return output.copySnapshot(snapshot);
            const host = services.host();
            const acquired = host.pb_web_volumes_acquire(new DataView(snapshot.prefix.buffer).getBigUint64(8, true));
            if (acquired) return acquired;
            const count = snapshot.arrays[0].count;
            const handles = new Uint8Array(host.memory.buffer, host.pb_output(), count * 8);
            const owned = { prefix: snapshot.prefix, arrays: snapshot.arrays.map(array => ({ count: array.count, bytes: array.bytes.slice() })) };
            for (let i = 0; i < count; i++) owned.arrays[0].bytes.set(handles.subarray(i * 8, i * 8 + 8), i * L.volume.size + L.volume.offsets.file);
            return output.copySnapshot(owned);
        }),
    };
}
