import { readFile } from "node:fs/promises";
import { GuestWork } from "../src/guest-work.ts";

const path = process.env.PLAYBIT_WEB_HOST_WASM ?? new URL("../../../../target/wasm32-unknown-unknown/debug/playbit.wasm", import.meta.url);
const module = await WebAssembly.compile(await readFile(path));
function exports() {
    const imports = {};
    for (const item of WebAssembly.Module.imports(module)) {
        if (item.kind !== "function") throw new Error(`Unexpected work fixture import ${item.kind}`);
        (imports[item.module] ??= {})[item.name] = () => { throw new Error(`Memory-only work called ${item.name}`); };
    }
    return new WebAssembly.Instance(module, imports).exports;
}
// Lifecycle mocks still execute the real Rust accounting, independently of
// their mock data backend. No OPFS or account initialization is necessary.
export class FixtureWork extends GuestWork {
    constructor(events) { const host = exports(); super(events, () => host); this.reset(); }
}
export const createWork = (events = []) => new FixtureWork(events);
