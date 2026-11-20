import type { Frame } from "./ap-codec.ts";
// Only operation-private durations are accepted. These are annotations, not
// positions on the web browser clock or evidence of another account's activity.
export const AP_TIMING_STAGES = ["ap.admission_check", "ap.authority_check", "ap.durable_merge",
    "ap.storage_sync", "ap.mail_send", "ap.storage_wait", "ap.request_read", "ap.operation"] as const;
export interface ApTiming { stage: string; duration_ns: string }
export function validateApTiming(value: unknown): asserts value is ApTiming[] {
    if (!Array.isArray(value) || value.length > 8) throw new Error("Invalid AP timing array");
    const seen = new Set<string>();
    for (const entry of value) {
        if (!entry || typeof entry !== "object" || Object.keys(entry).sort().join() !== "duration_ns,stage"
            || typeof entry.stage !== "string" || entry.stage.length > 64
            || !entry.stage.startsWith("ap.") || entry.stage.length === 3
            || /[^a-z0-9_.]/.test(entry.stage) || seen.has(entry.stage)
            || typeof entry.duration_ns !== "string" || entry.duration_ns.length > 20
            || !entry.duration_ns.length || /[^0-9]/.test(entry.duration_ns)
            || (entry.duration_ns.length > 1 && entry.duration_ns.startsWith("0"))
            || BigInt(entry.duration_ns) > (1n << 64n) - 1n) throw new Error("Invalid AP timing stage");
        seen.add(entry.stage);
    }
    if (new TextEncoder().encode(JSON.stringify(value)).length + 10 > 1024) throw new Error("Invalid AP timing encoding");
}

// Every binary authentication reply has this envelope; stripping it never
// adopts credentials. The caller validates status and credential semantics.
export function authenticationReply(frame: Frame): Frame {
    if (![0, 1, 5].includes(frame.type) || frame.source.length || frame.destination.length
        || frame.body.length < 4 || frame.body.length > 4096) throw new Error("Invalid AP authentication envelope");
    const length = new DataView(frame.body.buffer, frame.body.byteOffset).getUint16(0, true);
    if (length < 2 || length > 1024 || 2 + length > frame.body.length) throw new Error("Invalid AP authentication timing length");
    const timing: unknown = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(frame.body.subarray(2, 2 + length)));
    validateApTiming(timing);
    return { ...frame, body: frame.body.slice(2 + length) };
}
