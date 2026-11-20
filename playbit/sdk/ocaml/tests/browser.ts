// Compatibility harness only: actual Playbit syscall adapters and Rust work owner.
// No account, AP connection, durable user data or production app admission.
import { boundedGuest } from '../../../platform/web/src/wasm-module.ts';
import { bindOperations, validateEntryPoint } from '../../../platform/web/src/guest-imports.ts';
import { guestOperations } from '../../../platform/web/src/guest-operations.ts';
import { GuestWork } from '../../../platform/web/src/guest-work.ts';
import { decodeScene } from '../../../platform/web/src/scene.ts';
import { createStorageBridge } from '../../../platform/web/src/opfs-bridge.ts';
import { OpfsStore, OpfsResources } from '../../../platform/web/src/opfs-store.ts';
import { encodeId } from '../../../platform/web/src/identifier.ts';

class File {
    bytes = new Uint8Array();
    getSize() { return this.bytes.length; }
    read(out: Uint8Array, { at = 0 } = {}) { const bytes = this.bytes.subarray(at, at + out.length); out.set(bytes); return bytes.length; }
    write(bytes: Uint8Array, { at = 0 } = {}) { const next = new Uint8Array(Math.max(at + bytes.length, this.bytes.length)); next.set(this.bytes); next.set(bytes, at); this.bytes = next; return bytes.length; }
    truncate(length: number) { this.bytes = this.bytes.slice(0, length); }
    flush() {}
    close() {}
}
function assert(value: unknown, message: string): asserts value { if (!value) throw Error(message); }
async function run() {
    const started = performance.now();
    let host: WebAssembly.Instance;
    const resources = new OpfsResources([new File(), new File()], [new File(), new File()]);
    const stores = new Map([1, 2].map(slot => [slot, OpfsStore.empty(new File(), resources)]));
    const bridge = createStorageBridge(stores, resources, () => host.exports.memory as WebAssembly.Memory,
        bytes => crypto.getRandomValues(bytes), () => 1000, error => { throw error; });
    host = (await WebAssembly.instantiate(await (await fetch('/host.wasm')).arrayBuffer(), {
        opfs: { ...bridge.imports, engine_panic() { throw Error('Rust host panic'); } },
    })).instance;
    const id = (length: number, byte: number) => encodeId(new Uint8Array(length).fill(byte));
    const profile = {
        installation: id(16, 42), account: id(16, 1),
        root: { id: id(16, 4), owner: id(16, 1), name: 'Root', share_key_with_service: false, personal: false },
        session: { id: id(32, 3), selector: { kind: 'exact', build_id: id(32, 2) }, resolved_build_id: id(32, 2), root_volume: id(16, 4), creator: id(16, 1), update_authority: 'creator' },
        root_key: Array(32).fill(9), replica_ids: [1, 2],
        local: { blobs: [], databases: [], permissions: { grants: [] } },
    };
    const exports = host.exports as any;
    const input = new TextEncoder().encode(JSON.stringify(profile));
    const pointer = exports.pb_input(input.length);
    new Uint8Array(exports.memory.buffer, pointer, input.length).set(input);
    assert(exports.pb_initialize(input.length) === 0, 'initialize isolated Rust host');
    const events: any[] = [], work = new GuestWork(events, () => exports);
    let guest: WebAssembly.Instance, scene: ReturnType<typeof decodeScene>;
    const logs: string[] = [];
    const operations = guestOperations({
        memory: () => guest.exports.memory as WebAssembly.Memory,
        host: () => exports, events, work,
        log: text => logs.push(text), submit: bytes => { scene = decodeScene(bytes); },
    });
    const module = await WebAssembly.compile(boundedGuest(new Uint8Array(await (await fetch('/counter.wasm')).arrayBuffer())));
    guest = await WebAssembly.instantiate(module, { playbit: bindOperations(module, operations) });
    validateEntryPoint(guest.exports.pb_main);
    const turn = () => { work.beginTurn(); try { (guest.exports.pb_main as Function)(); } finally { work.endTurn(); } };
    turn();
    assert(operations.quitOnLastWindow && scene!.controls.size === 2, 'real scene and quit intent');
    const samples: number[] = [];
    for (let count = 1; count <= 1000; count++) {
        const start = performance.now();
        work.queue({ kind: 2, target: 1, data: count === 1 ? new Uint8Array(512) : new Uint8Array() });
        turn();
        assert(logs.at(-1) === `Count: ${count} (${count} events)`, 'increment and retained OCaml state');
        samples.push(performance.now() - start);
    }
    work.queue({ kind: 2, target: 2, data: new Uint8Array() }); turn();
    assert(logs.at(-1) === 'Count: 0 (1001 events)', 'reset');
    samples.sort((a, b) => a - b);
    return { checks: 1004, userAgent: navigator.userAgent, total_ms: performance.now() - started,
        turns_ms: { p50: samples[500], p95: samples[950], p99: samples[990], max: samples[999] },
        boundary: 'Trusted OCaml module, production browser syscall/scene adapters and Rust work owner; compatibility only, no GC heap enforcement or UI timing claim' };
}
const result = run();
(window as any).testDone = result;
const receiver = (window as any).webkit?.messageHandlers.result;
if (receiver) result.then(value => receiver.postMessage(value), error => receiver.postMessage({ error: String(error) }));
