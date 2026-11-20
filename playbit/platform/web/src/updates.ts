import { validId } from './identifier.ts';
import { boundedGuest } from "./wasm-module.ts";
import { bindOperations, validateEntryPoint } from "./guest-imports.ts";
import type { GuestOperations } from "./guest-imports.ts";

// AP's session selector as stored and as resolved; which id is present depends on kind.
export interface SessionSelector { kind: "exact" | "app"; build_id?: string; app?: { publisher: string; app_id: string } }
// AP ResolveSession result: the authorized build for a session at a revision.
export interface SessionResolution { revision: number; selector: SessionSelector; resolved_build_id: string }
// The session fields resolvedSession reads; the profile's session carries more.
export interface SessionDescriptor { id: string; selector: SessionSelector; resolved_build_id: string }
export interface GuestExports extends WebAssembly.Exports { memory: WebAssembly.Memory; pb_main: () => void }
// An instance prepareGuest has verified: one memory export and a nullary pb_main.
export type GuestInstance = WebAssembly.Instance & { exports: GuestExports };

// Only AP ResolveSession can authorize a new runtime version. An app record
// alone cannot change a session or its immutable identity.
export function resolvedSession<S extends SessionDescriptor>(current: S, resolution: SessionResolution, previous?: SessionResolution | null): S {
    const version = resolution?.resolved_build_id, selector = resolution?.selector;
    if (!Number.isSafeInteger(resolution?.revision) || resolution.revision < 0 || !validId(version, 32)
        || !selector || selector.kind !== current.selector.kind
        || (selector.kind === "exact" && selector.build_id !== version)
        || (selector.kind === "app" && (!selector.app || !validId(selector.app.publisher, 16) || !selector.app.app_id || selector.app.publisher !== current.selector.app?.publisher || selector.app.app_id !== current.selector.app?.app_id))
        || !["exact", "app"].includes(selector.kind)) throw new Error("Invalid authorized session update");
    if (previous && (resolution.revision < previous.revision
        || (resolution.revision === previous.revision && version !== previous.resolved_build_id))) throw new Error("Session update revision moved backwards or changed unexpectedly");
    return { ...current, selector, resolved_build_id: version };
}

// Instantiation has no guest entry point side effects: boundedGuest rejects
// implicit starts. The running instance is replaced only after these checks.
export async function prepareGuest(wasm: Uint8Array, operations: GuestOperations): Promise<GuestInstance> {
    const module = await WebAssembly.compile(boundedGuest(wasm));
    const candidate = await WebAssembly.instantiate(module, { playbit: bindOperations(module, operations) });
    validateEntryPoint(candidate.exports.pb_main);
    if (!(candidate.exports.memory instanceof WebAssembly.Memory) || typeof candidate.exports.pb_main !== "function"
        || candidate.exports.pb_main.length !== 0 || candidate.exports.memory.buffer.byteLength > 16 * 1024 * 1024) throw new Error("This app cannot run on the web. Open it in Playbit");
    // The checks above establish exactly the shape GuestInstance names.
    return candidate as GuestInstance;
}
