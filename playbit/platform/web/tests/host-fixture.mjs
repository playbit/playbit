import { encodeId, decodeId } from '../src/identifier.ts';
import { readFile } from "node:fs/promises";
import { OpfsStore, OpfsResources } from "../src/opfs-store.ts";
import { createStorageBridge } from "../src/opfs-bridge.ts";
import { guestOperations } from "../src/guest-operations.ts";
import { operationSupported } from "../src/guest-imports.ts";
import { GuestWork } from "../src/guest-work.ts";
const encoder = new TextEncoder(), decoder = new TextDecoder();
// CLI probes select their artifact by argument. Node test-runner consumers use
// the contributor-kit debug artifact, or an explicitly frozen qualification file.
const wasm = await readFile(process.argv[2] ?? process.env.PLAYBIT_WEB_HOST_WASM ?? new URL("../../../../target/wasm32-unknown-unknown/debug/playbit.wasm", import.meta.url));
// Ordinary Root adapter tests use the current TODO schema explicitly; this does
// not advertise automatic Session identity or browser synchronization support.
export const initializer = JSON.parse(await readFile(new URL("../../../../guests/todo/playbit-volumes.json", import.meta.url))).session;
export const entryId = initializer.databases[0].id;
export const schema = initializer.databases[0].kind.schema;
export class File {
    bytes = new Uint8Array();
    getSize() { return this.bytes.length; }
    read(out, { at = 0 } = {}) { const bytes = this.bytes.subarray(at, at + out.length); out.set(bytes); return bytes.length; }
    write(bytes, { at = 0 } = {}) { const next = new Uint8Array(Math.max(at + bytes.length, this.bytes.length)); next.set(this.bytes); next.set(bytes, at); this.bytes = next; return bytes.length; }
    truncate(length) { this.bytes = this.bytes.slice(0, length); }
    flush() {}
    close() {}
}
export const account = encodeId(new Uint8Array(16).fill(0x01)), key = Array(32).fill(9), version = encodeId(new Uint8Array(32).fill(0x02));
export function bootstrap(rootId, { personal = false } = {}) {
    const local = structuredClone(initializer); local.databases[0].kind.objects[0].id = encodeId(new Uint8Array(16).fill(0x01));
    return { installation: encodeId(new Uint8Array(16).fill(0x2a)), account, root: { id: rootId, owner: account, name: "Root", share_key_with_service: false, personal },

        session: { id: encodeId(new Uint8Array(32).fill(0x03)), app: { publisher: account, app_id: "todo" }, selector: { kind: "exact", build_id: version }, resolved_build_id: version,
            root_volume: rootId,  creator: account, update_authority: "creator" },
        root_key: key,  replica_ids: [1,2], local };
}
export async function createHost(profile, existingStores, now = () => 1000, wrapImports = imports => imports, registerExisting = true) {
    const resources = existingStores?.values().next().value.resources ?? new OpfsResources([new File(), new File()], [new File(), new File()]);
    const stores = existingStores ?? new Map(Array.from({ length: 2 }, (_, i) => [i + 1, OpfsStore.empty(new File(), resources)]));
    let host, operations; const storageErrors = [];
    const bytes = (p, n) => new Uint8Array(host.exports.memory.buffer, p, n);
    const bridge = createStorageBridge(stores, resources, () => host.exports.memory,
        value => crypto.getRandomValues(value), now, error => storageErrors.push(error));
    host = (await WebAssembly.instantiate(wasm, { opfs: wrapImports({ ...bridge.imports,
        operation_supported: opcode => Number(operationSupported(operations, opcode)),
        engine_panic() { throw new Error("Unexpected engine panic"); } }) })).instance;
    const memory = new WebAssembly.Memory({ initial: 1 }), events = [];
    operations = guestOperations({ memory: () => memory, host: () => host.exports, events,
        work: new GuestWork(events, () => host.exports), log() {}, submit() {} });
    const call = (name, value) => {
        const input = encoder.encode(JSON.stringify(value)), pointer = host.exports.pb_input(input.length);
        bytes(pointer,input.length).set(input);
        const result = host.exports[name](input.length);
        if (result < 0) { const n = host.exports.pb_error(); throw new Error(decoder.decode(bytes(host.exports.pb_output(),n)) || `Host error ${result}`); }
    };
    call("pb_initialize",profile);
    const raw = (name, input, ...args) => {
        const pointer = host.exports.pb_input(input.length); bytes(pointer,input.length).set(input);
        const result = host.exports[name](...args);
        return result < 0 ? result : bytes(host.exports.pb_output(),result).slice();
    };
    const register = (entry, volume = profile.root.id, slot = 65, generation) => {
        stores.set(slot, stores.get(slot) ?? OpfsStore.empty(new File(), resources));
        const input = encoder.encode(JSON.stringify({ volume, database: entry }));
        const backing = raw('pb_database_backing', input, input.length);
        if (!(backing instanceof Uint8Array)) throw new Error(`Backing lookup failed: ${backing}`);
        generation ??= JSON.parse(decoder.decode(backing)).generation ?? encodeId(new Uint8Array(16).fill(0xb1));
        call("pb_register_database", { address: { volume, database: entry }, slot, replica_id: profile.replica_ids[0] + slot, generation });
        return slot;
    };
    if (profile.local) for (const [index, seed] of profile.local.databases.entries()) {
        register(seed.id, profile.root.id, 65 + index); call("pb_seed_database", seed);
    }
    else if (registerExisting && existingStores?.has(65) && profile.account === profile.root.owner) register(entryId);
    const handle = (slot = 1, generation = 1, entry = entryId) => {
        host.exports.pb_input(0);
        let handle = (BigInt(generation) << 32n) | BigInt(slot);
        if (host.exports.pb_web_volumes() === 0) {
            const catalog = new DataView(host.exports.memory.buffer), result = host.exports.pb_web_result();
            if (slot > 0 && slot <= catalog.getUint32(result + 24, true)) {
                const pointer = catalog.getUint32(result + 16, true) + (slot - 1) * catalog.getUint32(result + 28, true);
                handle = catalog.getBigUint64(pointer + 8, true);
                if (generation !== 1) handle = (BigInt(generation) << 32n) | (handle & 0xffffffffn);
            }
        }
        const bytes = new Uint8Array(32), view = new DataView(bytes.buffer);
        view.setUint32(0, 32, true); view.setBigUint64(8, handle, true); bytes.set(decodeId(entry, 15), 16); return bytes;
    };
    const stage = input => { const pointer = host.exports.pb_input(input.length); bytes(pointer, input.length).set(input); };
    const id = byte => new Uint8Array(16).fill(byte);
    const booleanInput = (value, object = id(1), slot = 1, generation = 1) => {
        const input = new Uint8Array(176), view = new DataView(input.buffer);
        input.set(handle(slot, Number(generation)), 8); input.set(object, 40); input.fill(3, 64, 80);
        view.setUint32(56, 40, true); view.setUint32(80, 96, true); view.setUint32(84, 1, true);
        view.setUint32(88, 80, true); view.setUint32(92, 1, true);
        view.setUint32(96, 80, true); view.setUint32(104, 2, true); view.setBigUint64(112, BigInt(Number(value)), true); return input;
    };
    const edit = (value, object = id(1), slot = 1, generation = 1) => {
        stage(booleanInput(value, object, slot, generation)); return host.exports.pb_web_object_edit(8, 40, 56, 1, 40);
    };
    const create = value => {
        const input = booleanInput(value); input.fill(1, 40, 56); stage(input);
        const status = host.exports.pb_web_object_create(8, 40, 56, 1, 40);
        return status < 0 ? status : bytes(host.exports.pb_output(), 16).slice();
    };
    const readField = (field = id(3), object = id(1)) => {
        const input = new Uint8Array(72); input.set(handle(), 8); input.set(object, 40); input.set(field, 56); stage(input);
        const status = host.exports.pb_web_object_read(8, 40, 56, 1);
        if (status < 0) return status;
        const data = new DataView(host.exports.memory.buffer), result = host.exports.pb_web_result();
        const fields = data.getUint32(result + 24, true), values = data.getUint32(result + 40, true);
        if (!data.getUint32(fields + 32, true)) return null;
        return { kind: data.getUint32(values + 16, true), scalar: data.getBigUint64(values + 16, true) };
    };
    const read = (object = id(1)) => { const result = readField(id(3), object); return typeof result === "number" ? result : result?.scalar === 1n; };
    const volumes = () => {
        host.exports.pb_input(0); const status = host.exports.pb_web_volumes();
        if (status < 0) return status;
        const data = new DataView(host.exports.memory.buffer), result = host.exports.pb_web_result();
        const pointer = data.getUint32(result + 16, true), count = data.getUint32(result + 24, true);
        return Array.from({ length: count }, (_, i) => encodeId(bytes(pointer + i * data.getUint32(result + 28, true) + 16, 16)));
    };
    const query = () => {
        const input = new Uint8Array(100); input.set(handle(), 8); input.fill(1, 48, 64); const queryView = new DataView(input.buffer); queryView.setUint32(40, 60, true); queryView.setUint32(84, 64, true); stage(input);
        const status = host.exports.pb_web_object_query(8, 40);
        if (status < 0) return status;
        const data = new DataView(host.exports.memory.buffer), result = host.exports.pb_web_result();
        const pointer = data.getUint32(result + 32, true), count = data.getUint32(result + 40, true);
        return Array.from({ length: count }, (_, i) => encodeId(bytes(pointer + i * 48 + 8, 16)));
    };
    return { host, stores, resources, call, raw, edit, create, read, readField, volumes, query, register, handle, storageErrors, operations };

}
