// Web adapter for wasm32 records. Regions retain offsets, not views that
// memory.grow can detach. Guest callbacks borrow nonshared memory for one turn.
import { ABI } from './guest-layout.ts';
import type { RecordLayout } from './guest-layout.ts';
export class AbiError extends Error {
    readonly status: number;
    constructor(status: number) { super(`Playbit operation failed (${status})`); this.status = status; }
}
export interface Region { pointer: number; length: number }
export function admit(memory: WebAssembly.Memory, pointer: number, count: number, stride: number, alignment: number, limit: number): Region {
    pointer >>>= 0; count >>>= 0;
    if (count > limit) throw new AbiError(-9);
    if (!count) return { pointer: 0, length: 0 };
    const length = count * stride;
    if (!pointer || pointer % alignment || !Number.isSafeInteger(length) || pointer + length > memory.buffer.byteLength) throw new AbiError(-3);
    return { pointer, length };
}
export function bytes(memory: WebAssembly.Memory, region: Region): Uint8Array<ArrayBuffer> {
    return new Uint8Array(memory.buffer, region.pointer, region.length);
}
export function view(memory: WebAssembly.Memory, region: Region): DataView<ArrayBuffer> {
    return new DataView(memory.buffer, region.pointer, region.length);
}
export function disjoint(regions: readonly Region[]): void {
    for (let i = 0; i < regions.length; i++) for (let j = i + 1; j < regions.length; j++) {
        const a = regions[i], b = regions[j];
        if (a.length && b.length && a.pointer < b.pointer + b.length && b.pointer < a.pointer + a.length) throw new AbiError(-3);
    }
}
export function record(memory: WebAssembly.Memory, pointer: number, layout: RecordLayout, input = false): Region {
    const capacity = view(memory, admit(memory, pointer, 1, 4, layout.alignment, 1)).getUint32(0, true);
    if (capacity > 4096) throw new AbiError(-9);
    if (capacity < layout.minimum) throw new AbiError(input ? -3 : -11);
    if (layout.fields.some(([offset, width]) => offset < capacity && offset + width > capacity)) throw new AbiError(-3);
    const region = admit(memory, pointer, capacity, 1, layout.alignment, 4096);
    if (input && capacity > layout.size && bytes(memory, region).subarray(layout.size).some(byte => byte !== 0)) throw new AbiError(-7);
    return region;
}
export function recordArray(memory: WebAssembly.Memory, pointer: number, count: number, stride: number, layout: RecordLayout, input: boolean, limit: number): Region {
    stride >>>= 0;
    if (stride > 4096) throw new AbiError(-9);
    if (stride < layout.minimum || stride % layout.alignment) throw new AbiError(-3);
    const region = admit(memory, pointer, count, stride, layout.alignment, limit);
    if (region.length > 16 * 1024 * 1024) throw new AbiError(-9);
    for (let i = 0; i < count; i++) {
        if (new DataView(memory.buffer).getUint32(pointer + i * stride, true) > stride) throw new AbiError(-3);
        record(memory, pointer + i * stride, layout, input);
    }
    return region;
}
// Publish only complete known fields. Caller capacity, padding and future output
// tails stay untouched; written describes the initialized known prefix.
export function publishRecord(memory: WebAssembly.Memory, destination: Region, layout: RecordLayout, source: Uint8Array<ArrayBuffer>): void {
    const output = bytes(memory, destination);
    let written = 8;
    for (const [offset, width] of layout.fields.slice(2)) {
        if (offset + width <= destination.length && offset + width <= source.length) {
            output.set(source.subarray(offset, offset + width), offset);
            written = Math.max(written, offset + width);
        }
    }
    view(memory, destination).setUint32(4, written, true);
}

// Normalize admitted inputs to the one current staging layout. Guest traversal
// always uses caller strides, and unknown nonzero input tails fail before work.
export class Input {
    private readonly parts: { offset: number; bytes: Uint8Array<ArrayBuffer> }[] = [];
    private length = 8;
    private readonly memory: WebAssembly.Memory;
    private readonly outputs: readonly Region[];
    constructor(memory: WebAssembly.Memory, outputs: readonly Region[] = []) { this.memory = memory; this.outputs = outputs; }
    private append(data: Uint8Array<ArrayBuffer>, alignment: number) {
        const offset = Math.ceil(this.length / alignment) * alignment;
        if (offset + data.length > 1048576 + 256 * (40 + 7) + 4096 * 80 + 128) throw new AbiError(-9);
        const part = { offset: data.length ? offset : 0, bytes: data };
        this.length = offset + data.length; this.parts.push(part); return part;
    }
    private source(region: Region) { for (const output of this.outputs) disjoint([region, output]); }
    private copy(pointer: number, count: number, stride: number, alignment: number, limit: number) {
        const region = admit(this.memory, pointer, count, stride, alignment, limit); this.source(region);
        return this.append(bytes(this.memory, region).slice(), alignment);
    }
    private records(pointer: number, count: number, stride: number, layout: RecordLayout, limit: number) {
        const region = recordArray(this.memory, pointer, count, stride, layout, true, limit); this.source(region);
        const normalized = new Uint8Array(count * layout.size), data = new DataView(normalized.buffer);
        for (let i = 0; i < count; i++) {
            normalized.set(bytes(this.memory, record(this.memory, pointer + i * stride, layout, true)).subarray(0, layout.size), i * layout.size);
            data.setUint32(i * layout.size, layout.size, true); data.setUint32(i * layout.size + 4, 0, true);
        }
        return this.append(normalized, layout.alignment);
    }
    private one(pointer: number, layout: RecordLayout) {
        const region = record(this.memory, pointer, layout, true); this.source(region);
        const data = bytes(this.memory, region).slice(0, layout.size);
        const header = new DataView(data.buffer); header.setUint32(0, layout.size, true); header.setUint32(4, 0, true);
        return this.append(data, layout.alignment);
    }
    entry(pointer: number): number {
        const entry = this.one(pointer, ABI.entryHandle);
        if (entry.bytes[31] || !entry.bytes.subarray(16, 31).some(byte => byte !== 0)) throw new AbiError(-3);
        return entry.offset;
    }
    entryFromId(generation: bigint, slot: number, pointer: number): number {
        const region = admit(this.memory, pointer, 15, 1, 1, 15); this.source(region);
        const id = bytes(this.memory, region);
        if (!id.some(byte => byte !== 0)) throw new AbiError(-3);
        const entry = new Uint8Array(ABI.entryHandle.size), data = new DataView(entry.buffer);
        data.setUint32(0, ABI.entryHandle.size, true);
        data.setBigUint64(8, (generation << 32n) | BigInt(slot >>> 0), true);
        entry.set(id, 16);
        return this.append(entry, ABI.entryHandle.alignment).offset;
    }
    text(pointer: number, length: number, limit: number): number { return this.copy(pointer, length, 1, 1, limit).offset; }
    ids(pointer: number, count: number, limit = 256): number { return this.copy(pointer, count, 16, 1, limit).offset; }
    blobs(pointer: number, count: number, stride: number): number {
        count >>>= 0;
        if (!count) throw new AbiError(-3);
        const records = this.records(pointer, count, stride, ABI.bloblogBlob, 128), data = new DataView(records.bytes.buffer);
        let total = 4;
        for (let index = 0; index < count; index++) {
            const length = data.getUint32(index * 16 + 12, true);
            total += 59 + length;
            if (length > 32768 || total > 65536) throw new AbiError(-9);
        }
        for (let index = 0; index < count; index++) {
            const payload = this.copy(data.getUint32(index * 16 + 8, true), data.getUint32(index * 16 + 12, true), 1, 1, 32768);
            data.setUint32(index * 16 + 8, payload.offset, true);
        }
        return records.offset;
    }
    changes(pointer: number, count: number, stride: number): number {
        count >>>= 0;
        const fields = this.records(pointer, count, stride, ABI.fieldInput, 256), fieldsView = new DataView(fields.bytes.buffer);
        let valuesLeft = 4096, bytesLeft = 1048576;
        for (let i = 0; i < count; i++) {
            const size = fieldsView.getUint32(i * 40 + 28, true);
            if (size > valuesLeft) throw new AbiError(-9); valuesLeft -= size;
        }
        for (let i = 0; i < count; i++) {
            const base = i * 40, size = fieldsView.getUint32(base + 28, true);
            const values = this.records(fieldsView.getUint32(base + 24, true), size, fieldsView.getUint32(base + 32, true), ABI.valueInput, 4096);
            fieldsView.setUint32(base + 24, values.offset, true); fieldsView.setUint32(base + 32, 80, true);
            const valuesView = new DataView(values.bytes.buffer);
            for (let j = 0; j < size; j++) {
                const start = j * 80, kind = valuesView.getUint32(start + 8, true);
                if (kind !== 1 && kind !== 5) continue;
                const length = valuesView.getUint32(start + 12, true);
                if (length > bytesLeft) throw new AbiError(-9); bytesLeft -= length;
                const payload = this.copy(valuesView.getUint32(start + 72, true), length, 1, 1, bytesLeft + length);
                valuesView.setUint32(start + 72, payload.offset, true);
            }
        }
        return fields.offset;
    }
    schema(pointer: number, count: number, stride: number): number {
        const types = this.records(pointer, count, stride, ABI.schemaTypeInput, 256), data = new DataView(types.bytes.buffer);
        let fieldsLeft = 4096, bytesLeft = 1048576;
        for (let i = 0; i < count; i++) {
            const fields = data.getUint32(i * 48 + 36, true);
            if (fields > fieldsLeft) throw new AbiError(-9); fieldsLeft -= fields;
        }
        const text = (data: DataView, base: number) => {
            const length = data.getUint32(base + 28, true);
            if (length > 255 || length > bytesLeft) throw new AbiError(-3); bytesLeft -= length;
            data.setUint32(base + 24, this.copy(data.getUint32(base + 24, true), length, 1, 1, 255).offset, true);
        };
        for (let i = 0; i < count; i++) {
            const base = i * 48; text(data, base);
            const fields = this.records(data.getUint32(base + 32, true), data.getUint32(base + 36, true), data.getUint32(base + 40, true), ABI.schemaFieldInput, 256);
            data.setUint32(base + 32, fields.offset, true); data.setUint32(base + 40, 56, true);
            const records = new DataView(fields.bytes.buffer);
            for (let j = 0; j < fields.bytes.length / 56; j++) text(records, j * 56);
        }
        return types.offset;
    }
    kvQuery(pointer: number, kind: 'keys' | 'read', continued = false): number {
        const read = kind === 'read', record = this.one(pointer, read ? ABI.kvRead : ABI.kvQuery), data = new DataView(record.bytes.buffer), root = read ? 24 : 8;
        const present = data.getUint32(read ? 20 : 44, true), limit = data.getUint32(read ? 16 : 48, true);
        if (data.getUint32(read ? 60 : 52, true) || present > 1 || data.getUint32(root, true) > 255
            || (!present && record.bytes.subarray(root, root + 36).some(byte => byte !== 0))
            || !limit || limit > (read ? 61440 : 64) || (continued && !present)) throw new AbiError(-3);
        if (read) { const offset = data.getBigUint64(8, true); if (offset > 4194304n || (offset !== 0n && !present)) throw new AbiError(-3); }
        return record.offset;
    }
    query(pointer: number): number {
        const query = this.one(pointer, ABI.query), data = new DataView(query.bytes.buffer);
        data.setUint32(48, this.ids(data.getUint32(48, true), data.getUint32(52, true)), true); return query.offset;
    }
    finish(): Uint8Array<ArrayBuffer> {
        const result = new Uint8Array(this.length); for (const part of this.parts) result.set(part.bytes, part.offset); return result;
    }
}

// Descriptor pointer offset, optional element record layout, maximum count.
// Record descriptors include a fourth scalar stride; byte spans have three.
export type BufferLayout = readonly [number, RecordLayout | null, number];
export interface ResultSnapshot { prefix: Uint8Array<ArrayBuffer>; arrays: { count: number; bytes: Uint8Array<ArrayBuffer> }[] }
export class Output {
    readonly regions: readonly Region[];
    private readonly header: Region;
    private readonly strides: readonly number[];
    private readonly memory: WebAssembly.Memory;
    private readonly layout: RecordLayout;
    private readonly layouts: readonly BufferLayout[];
    constructor(memory: WebAssembly.Memory, pointer: number, layout: RecordLayout, layouts: readonly BufferLayout[]) {
        this.memory = memory; this.layout = layout; this.layouts = layouts;
        this.header = record(memory, pointer, layout);
        const descriptor = view(memory, this.header);
        this.strides = layouts.map(([offset, element]) => element ? descriptor.getUint32(offset + 12, true) : 1);
        const arrays = layouts.map(([offset, element, limit], i) => admit(memory,
            descriptor.getUint32(offset, true), descriptor.getUint32(offset + 4, true), this.strides[i], element?.alignment ?? 1, limit));
        this.regions = [this.header, ...arrays]; disjoint(this.regions);
        layouts.forEach(([offset, element, limit], i) => {
            if (element) recordArray(memory, descriptor.getUint32(offset, true), descriptor.getUint32(offset + 4, true), this.strides[i], element, false, limit);
        });
    }
    fits(snapshot: ResultSnapshot): boolean {
        return snapshot.arrays.every((array, i) => array.count <= this.regions[i + 1].length / this.strides[i]);
    }
    copySnapshot(snapshot: ResultSnapshot): number {
        if (snapshot.arrays.length !== this.layouts.length) throw new Error('Invalid host result shape');
        view(this.memory, this.header).setUint32(4, 0, true);
        this.layouts.forEach(([, element], i) => {
            if (element) for (let j = 0; j < this.regions[i + 1].length / this.strides[i]; j++) {
                new DataView(this.memory.buffer).setUint32(this.regions[i + 1].pointer + j * this.strides[i] + 4, 0, true);
            }
        });
        if (!this.fits(snapshot)) {
            const destination = view(this.memory, this.header);
            snapshot.arrays.forEach((array, i) => destination.setUint32(this.layouts[i][0] + 8, array.count, true));
            return -11;
        }
        const destination = view(this.memory, this.header);
        snapshot.arrays.forEach((array, i) => destination.setUint32(this.layouts[i][0] + 8, array.count, true));
        publishRecord(this.memory, this.header, this.layout, snapshot.prefix);
        snapshot.arrays.forEach((array, i) => {
            const element = this.layouts[i][1];
            if (!element) bytes(this.memory, this.regions[i + 1]).set(array.bytes);
            else for (let j = 0; j < array.count; j++) {
                const pointer = this.regions[i + 1].pointer + j * this.strides[i];
                publishRecord(this.memory, record(this.memory, pointer, element), element, array.bytes.subarray(j * element.size, (j + 1) * element.size));
            }
        });
        // The full descriptor is initialized even when its host prefix carries
        // only result scalars; caller pointers/capacities/strides remain intact.
        let written = 8;
        for (const [offset, width] of this.layout.fields.slice(2)) {
            const end = offset + width;
            const initialized = end <= snapshot.prefix.length || this.layouts.some(([start, element]) => offset >= start && end <= start + (element ? 16 : 12));
            if (end > this.header.length || !initialized) break;
            written = end;
        }
        destination.setUint32(4, written, true);
        return 0;
    }
    copy(host: WebAssembly.Memory, pointer: number, prefix = 8): number {
        return this.copySnapshot(captureResult(host, pointer, this.layout, this.layouts, prefix, 16 * 1024 * 1024));
    }
}
export function captureResult(host: WebAssembly.Memory, pointer: number, layout: RecordLayout, layouts: readonly BufferLayout[], prefix: number, maximum: number): ResultSnapshot {
    const source = record(host, pointer, layout), descriptor = view(host, source);
    const regions = layouts.map(([offset, element, limit]) => {
        const count = descriptor.getUint32(offset + 8, true), stride = element ? descriptor.getUint32(offset + 12, true) : 1;
        const region = element ? recordArray(host, descriptor.getUint32(offset, true), count, stride, element, false, limit)
            : admit(host, descriptor.getUint32(offset, true), count, 1, 1, limit);
        return { count, stride, region, element };
    });
    if (prefix + regions.reduce((total, { count, element }) => total + count * (element?.size ?? 1), 0) > maximum) throw new AbiError(-9);
    const arrays = regions.map(({ count, stride, region, element }) => {
        if (!element) return { count, bytes: bytes(host, region).slice() };
        const data = new Uint8Array(count * element.size);
        for (let i = 0; i < count; i++) data.set(bytes(host, record(host, region.pointer + i * stride, element)).subarray(0, element.size), i * element.size);
        return { count, bytes: data };
    });
    return { prefix: bytes(host, source).slice(0, prefix), arrays };
}
