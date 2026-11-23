// Typed host callbacks behind one bounded scalar syscall entry.
import { clockNs } from "./guest-deadline.ts";
// A tiny import/re-export module gives JS callbacks real Wasm signatures. The
// engine then rejects wrong guest signatures at link time, before pb_main runs.
// Data begin/take return immediately; readiness resumes a later guest turn.
const signatures = {
    pb_session:"", pb_channel_open:"ii", pb_channel_begin:"Iiiii", pb_channel_take:"Ii",
    pb_host_platform: "", pb_host_limit: "i", pb_quit_on_last_window: "i", pb_log: "ii", pb_view_submit: "ii", pb_event_poll: "iii",
    pb_trace_record: "iiII", pb_clock_read: "", pb_random_fill: "ii", pb_event_poll_deadline: "iiiI",
    pb_identity: "i", pb_session_info: "i", pb_wake_after: "i", pb_update_defer: "I",
    pb_volumes: "i", pb_volumes_observe: "I", pb_file_dispose: "Ii", pb_operation_dispose: "Ii",
    pb_entry_resolve_begin: "Iiii", pb_entry_resolve_take: "Iii",
    pb_kv_keys_begin: "iiii", pb_kv_keys_take: "Iiiii",
    pb_kv_read_begin: "iiii", pb_kv_read_take: "Iiiii",
    pb_kv_put_begin: "iiiii", pb_kv_put_take: "Ii",
    pb_kv_delete_begin: "iii", pb_kv_delete_take: "Ii",
    pb_bloblog_append_begin: "Iiiiiii", pb_bloblog_append_take: "Iiiii",
    pb_bloblog_read_begin: "Iiiii", pb_bloblog_read_take: "Iiiii",
    pb_schema_read_begin: "i", pb_schema_read_take: "Iii",
    pb_schema_establish_begin: "iiii", pb_schema_establish_take: "Ii",
    pb_capabilities: "i", pb_startup_arguments: "i", pb_presence: "iiii",
    pb_file_read_begin: "IiIi", pb_file_read_take: "Iiiii",
    pb_file_write_begin: "IiIii", pb_file_write_take: "Ii",
    pb_file_open_begin: "Iiiii", pb_file_open_take: "Ii",
    pb_file_status_begin: "Ii", pb_file_status_take: "Iii",
    pb_file_close_begin: "Iii", pb_file_close_take: "Iii",
    pb_file_list_open_begin: "Iii", pb_file_list_open_take: "Ii",
    pb_file_list_next_begin: "Ii", pb_file_list_next_take: "Iii", pb_file_list_close: "Ii",
    pb_object_create_begin: "iiiii", pb_object_create_poll: "Iii",
    pb_file_copy_begin: "ii", pb_file_copy_poll: "Iii",
    pb_file_select_begin: "", pb_file_select_poll: "Iii",
    pb_object_read_begin: "iiii", pb_object_read_take: "Iii",
    pb_object_edit_begin: "iiiii", pb_object_edit_take: "Ii",
    pb_object_query_begin: "ii", pb_object_query_take: "Iii",
    pb_object_field_read_authenticated_begin: "iii",
    pb_object_field_read_authenticated_take: "Iii",
    pb_audio_open_begin: "i", pb_audio_open_take: "Ii", pb_audio_close: "Ii",
    pb_audio_sample_rate: "Ii", pb_audio_volume: "Ii", pb_audio_set_volume: "Iii",
    pb_sound_open_pcm: "Iiiiii", pb_sound_close: "Ii", pb_sound_play: "IiiiIIi",
    pb_sound_tell: "Iiii", pb_sound_seek: "IiiI", pb_sound_set_gain: "Iiii",
    pb_sound_set_balance: "Iiii", pb_sound_stop: "Iii",
} as const;
export type OperationName = keyof typeof signatures;
const promiseIntegration = WebAssembly as typeof WebAssembly & {
    Suspending?: new (callback: (opcode: number, ...args: bigint[]) => bigint | Promise<bigint>) => WebAssembly.ImportValue;
    promising?: (main: () => void) => () => Promise<void>;
};
export const supportsDeadline = () => typeof promiseIntegration.Suspending === "function"
    && typeof promiseIntegration.promising === "function";
export const promisingMain = (main: () => void) => promiseIntegration.promising!(main);
type Arguments<S extends string> = S extends `${infer First}${infer Rest}`
    ? [First extends "I" ? bigint : number, ...Arguments<Rest>] : [];
export type GuestOperations = { [K in Exclude<OperationName, "pb_clock_read">]?: (...args: Arguments<typeof signatures[K]>) => number | bigint | Promise<number | bigint> }
    & { pb_clock_read?(): bigint; admitOperation?(opcode: number): number };

function u32(value: number): number[] {
    const bytes: number[] = [];
    do { const byte = value & 127; value >>>= 7; bytes.push(byte | (value ? 128 : 0)); } while (value);
    return bytes;
}
const text = (value: string) => [...u32(value.length), ...new TextEncoder().encode(value)];
const section = (id: number, bytes: number[]) => [id, ...u32(bytes.length), ...bytes];
const header = [0, 97, 115, 109, 1, 0, 0, 0];
const entries = ["pb_syscall"];
const bindings = new WebAssembly.Module(Uint8Array.from([
    ...header,
    ...section(1, [1, 0x60, 7, 0x7f, ...Array(6).fill(0x7e), 1, 0x7e]),
    ...section(2, [1, ...entries.flatMap(name => [...text("playbit"), ...text(name), 0, 0])]),
    ...section(7, [1, ...entries.flatMap((name, index) => [...text(name), 0, index])]),
]));
const entryPoint = new WebAssembly.Module(Uint8Array.from([
    ...header, ...section(1, [1, 0x60, 0, 0]),
    ...section(2, [1, ...text("playbit"), ...text("pb_main"), 0, 0]),
]));

export function bindOperations(module: WebAssembly.Module, operations: GuestOperations, deadline = false): WebAssembly.Exports {
    const imports = WebAssembly.Module.imports(module);
    if (imports.length > 27 || imports.some(item => item.module !== "playbit"
        || item.kind !== "function" || !entries.includes(item.name))) {
        throw new Error("This app may import only pb_syscall");
    }
    const syscall = (opcode: number, ...args: bigint[]) => dispatch(operations, opcode, args, deadline);
    return new WebAssembly.Instance(bindings, { playbit: {
        pb_syscall: deadline
            ? new promiseIntegration.Suspending!(syscall) : syscall,
    } }).exports;
}

export function validateEntryPoint(main: WebAssembly.ExportValue | undefined): void {
    if (typeof main !== "function") throw new Error("Guest must export pb_main with type () -> ()");
    new WebAssembly.Instance(entryPoint, { playbit: { pb_main: main } });
}

interface Layout { name: OperationName; handle: boolean; count: number; wide: number }
const layouts: Record<number, Layout> = {
    19:{name:"pb_session",handle:false,count:0,wide:0},
    0x160:{name:"pb_channel_open",handle:false,count:2,wide:0},
    0x161:{name:"pb_channel_begin",handle:true,count:4,wide:1},
    0x162:{name:"pb_channel_take",handle:true,count:1,wide:1},
    0x163: { name: "pb_session_info", handle: false, count: 1, wide: 0 },
    0x140: { name: "pb_kv_keys_begin", handle: false, count: 4, wide: 0 },
    0x141: { name: "pb_kv_keys_take", handle: true, count: 4, wide: 1 },
    0x142: { name: "pb_kv_read_begin", handle: false, count: 4, wide: 0 },
    0x143: { name: "pb_kv_read_take", handle: true, count: 4, wide: 1 },
    0x144: { name: "pb_kv_put_begin", handle: false, count: 5, wide: 0 },
    0x145: { name: "pb_kv_put_take", handle: true, count: 1, wide: 1 },
    0x146: { name: "pb_kv_delete_begin", handle: false, count: 3, wide: 0 },
    0x147: { name: "pb_kv_delete_take", handle: true, count: 1, wide: 1 },
    0x170: { name: "pb_bloblog_append_begin", handle: true, count: 6, wide: 1 },
    0x171: { name: "pb_bloblog_append_take", handle: true, count: 4, wide: 1 },
    0x172: { name: "pb_bloblog_read_begin", handle: true, count: 4, wide: 1 },
    0x173: { name: "pb_bloblog_read_take", handle: true, count: 4, wide: 1 },
    0x106: { name: "pb_entry_resolve_begin", handle: true, count: 3, wide: 1 },
    0x107: { name: "pb_entry_resolve_take", handle: true, count: 2, wide: 1 },
    0x102: { name: "pb_schema_read_begin", handle: false, count: 1, wide: 0 },
    0x103: { name: "pb_schema_read_take", handle: true, count: 2, wide: 1 },
    0x104: { name: "pb_schema_establish_begin", handle: false, count: 4, wide: 0 },
    0x105: { name: "pb_schema_establish_take", handle: true, count: 1, wide: 1 },
    0x130: { name: "pb_object_read_begin", handle: false, count: 4, wide: 0 },
    0x131: { name: "pb_object_read_take", handle: true, count: 2, wide: 1 },
    0x132: { name: "pb_object_edit_begin", handle: false, count: 5, wide: 0 },
    0x133: { name: "pb_object_edit_take", handle: true, count: 1, wide: 1 },
    0x134: { name: "pb_object_query_begin", handle: false, count: 2, wide: 0 },
    0x135: { name: "pb_object_query_take", handle: true, count: 2, wide: 1 },
    0x114: { name: "pb_object_create_begin", handle: false, count: 5, wide: 0 },
    0x115: { name: "pb_object_create_poll", handle: true, count: 2, wide: 1 },
    0x116: { name: "pb_file_copy_begin", handle: false, count: 2, wide: 0 },
    0x117: { name: "pb_file_copy_poll", handle: true, count: 2, wide: 1 },
    0x122: { name: "pb_file_select_begin", handle: false, count: 0, wide: 0 },
    0x123: { name: "pb_file_select_poll", handle: true, count: 2, wide: 1 },
    0x124: { name: "pb_file_read_begin", handle: true, count: 3, wide: 3 },
    0x125: { name: "pb_file_read_take", handle: true, count: 4, wide: 1 },
    0x126: { name: "pb_file_write_begin", handle: true, count: 4, wide: 3 },
    0x127: { name: "pb_file_write_take", handle: true, count: 1, wide: 1 },
    0x7: { name: "pb_volumes", handle: false, count: 1, wide: 0 },
    0x12: { name: "pb_volumes_observe", handle: false, count: 1, wide: 1 },
    0x150: { name: "pb_file_open_begin", handle: true, count: 4, wide: 1 },
    0x151: { name: "pb_file_open_take", handle: true, count: 1, wide: 1 },
    0x152: { name: "pb_file_status_begin", handle: true, count: 1, wide: 1 },
    0x153: { name: "pb_file_status_take", handle: true, count: 2, wide: 1 },
    0x154: { name: "pb_file_close_begin", handle: true, count: 2, wide: 1 },
    0x155: { name: "pb_file_close_take", handle: true, count: 2, wide: 1 },
    0x156: { name: "pb_file_list_open_begin", handle: true, count: 2, wide: 1 },
    0x157: { name: "pb_file_list_open_take", handle: true, count: 1, wide: 1 },
    0x158: { name: "pb_file_list_next_begin", handle: true, count: 1, wide: 1 },
    0x159: { name: "pb_file_list_next_take", handle: true, count: 2, wide: 1 },
    0x15a: { name: "pb_file_list_close", handle: true, count: 1, wide: 1 },
    0x15c: { name: "pb_file_dispose", handle: true, count: 1, wide: 1 },
    0x15b: { name: "pb_operation_dispose", handle: true, count: 1, wide: 1 },
    0x136: { name: "pb_object_field_read_authenticated_begin", handle: false, count: 3, wide: 0 },
    0x137: { name: "pb_object_field_read_authenticated_take", handle: true, count: 2, wide: 1 },
    0x1a0: { name: "pb_audio_open_begin", handle: false, count: 1, wide: 0 },
    0x1a1: { name: "pb_audio_open_take", handle: true, count: 1, wide: 1 },
    0x1a2: { name: "pb_audio_close", handle: true, count: 1, wide: 1 },
    0x1a3: { name: "pb_audio_sample_rate", handle: true, count: 1, wide: 1 },
    0x1a4: { name: "pb_audio_volume", handle: true, count: 1, wide: 1 },
    0x1a5: { name: "pb_audio_set_volume", handle: true, count: 2, wide: 1 },
    0x1a6: { name: "pb_sound_open_pcm", handle: true, count: 5, wide: 1 },
    0x1a7: { name: "pb_sound_close", handle: true, count: 1, wide: 1 },
    0x1a8: { name: "pb_sound_play", handle: true, count: 6, wide: 25 },
    0x1a9: { name: "pb_sound_tell", handle: true, count: 3, wide: 1 },
    0x1aa: { name: "pb_sound_seek", handle: true, count: 3, wide: 5 },
    0x1ab: { name: "pb_sound_set_gain", handle: true, count: 3, wide: 1 },
    0x1ac: { name: "pb_sound_set_balance", handle: true, count: 3, wide: 1 },
    0x1ad: { name: "pb_sound_stop", handle: true, count: 2, wide: 1 },
    0xc: { name: "pb_host_platform", handle: false, count: 0, wide: 0 },
    0xd: { name: "pb_host_limit", handle: false, count: 1, wide: 0 },
    0xe: { name: "pb_quit_on_last_window", handle: false, count: 1, wide: 0 },
    0x1: { name: "pb_capabilities", handle: false, count: 1, wide: 0 },
    0x6: { name: "pb_startup_arguments", handle: false, count: 1, wide: 0 },
    0xa: { name: "pb_presence", handle: false, count: 4, wide: 0 },
    0x2: { name: "pb_log", handle: false, count: 2, wide: 0 },
    0x3: { name: "pb_view_submit", handle: false, count: 2, wide: 0 },
    0x4: { name: "pb_event_poll", handle: false, count: 3, wide: 0 },
    0x15: { name: "pb_event_poll_deadline", handle: false, count: 4, wide: 8 },
    0x5: { name: "pb_identity", handle: false, count: 1, wide: 0 },
    0xb: { name: "pb_wake_after", handle: false, count: 1, wide: 0 },
    0x9: { name: "pb_update_defer", handle: false, count: 1, wide: 1 },
    0xf: { name: "pb_trace_record", handle: false, count: 4, wide: 12 },
    0x10: { name: "pb_clock_read", handle: false, count: 0, wide: 0 },
    0x14: { name: "pb_random_fill", handle: false, count: 2, wide: 0 },
};
// The monotonic clock is built in. Other operations require a host callback.
export function operationSupported(operations: GuestOperations, opcode: number): boolean {
    const layout = layouts[opcode >>> 0];
    return (opcode >>> 0) === 0x10
        || (layout !== undefined && typeof operations[layout.name] === 'function');
}
function dispatch(operations: GuestOperations, opcode: number, values: bigint[], deadline: boolean): bigint | Promise<bigint> {
    const args = values.map(value => BigInt.asUintN(64, value));
    const layout = layouts[opcode >>> 0];
    if (!layout) return -7n; // Unknown operations have no handler.
    for (let index = 0; index < 6; index++) {
        if ((index >= layout.count && args[index] !== 0n)
            || (!(layout.wide & (1 << index)) && args[index] > 0xffffffffn)) return -3n;
    }
    if (layout.name === "pb_event_poll_deadline" && !deadline) return -7n;
    if (layout.name === "pb_clock_read") {
        return operations.pb_clock_read?.() ?? clockNs();
    }
    const handler = operations[layout.name];
    if (!handler) return -7n;
    const authority = operations.admitOperation?.(opcode >>> 0) ?? 0;
    if (authority) return BigInt(authority);
    const parameters: (number | bigint)[] = args.slice(0, layout.count)
        .map((value, index) => layout.wide & (1 << index) ? value : Number(value));
    if (layout.handle) parameters.splice(0, 1, args[0] >> 32n, Number(args[0] & 0xffffffffn));
    // The explicit layout table supplies the checked callback signature.
    const result = (handler as (...args: (number | bigint)[]) => number | bigint | Promise<number | bigint>)(...parameters);
    if (result instanceof Promise) {
        if (layout.name === "pb_event_poll_deadline" && supportsDeadline()) return result.then(BigInt);
        throw new Error("Guest syscall returned pending work");
    }
    return BigInt(result);
}
