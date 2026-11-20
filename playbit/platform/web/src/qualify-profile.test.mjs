import { compatibleWorkerStorage } from '../tests/worker-storage-fixture.mjs';
import { encodeId, decodeId, validId } from './identifier.ts';
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { build } from "esbuild";
import { fileURLToPath } from "node:url";
import { stripTypeScriptTypes } from "node:module";

const source = await readFile(new URL("../qualify.mjs", import.meta.url), "utf8");
const helper = vm.createContext({ encodeId, decodeId, validId, assert });
vm.runInContext(source.slice(source.indexOf("// Diagnostic-only source instrumentation."), source.indexOf("const output ="))
    + "globalThis.profile = { opfsQualification, runtimeQualification, profileOpfsStore, profileRuntimeWorker, replaceBoundary, installCheckboxFailureHarness, installQualificationReceiver, finishQualification };", helper);
function counters() {
    let clock = 0;
    const context = vm.createContext({ ...compatibleWorkerStorage(), encodeId, decodeId, validId, performance: { now: () => ++clock } });
    vm.runInContext(helper.profile.opfsQualification + "globalThis.wrap = qualificationImports;", context);
    return context;
}

test("qualification tracks transaction slots across resets and retires them on end", () => {
    const context = counters(), q = context.pbQualification;
    let sequence = 0;
    const imports = context.wrap({ begin: () => ++sequence, get: () => 7, end: () => 0 });
    const data = imports.begin(3, 0); // Created outside the measured turn.
    q.reset(); q.phase = 1;
    const policy = imports.begin(1, 0);
    imports.get(policy); imports.get(data); imports.end(policy); imports.end(data);
    const result = q.result();
    assert.equal(result["edit.slot1_policy_index.begin"].calls, 1);
    assert.equal(result["edit.slot1_policy_index.get"].calls, 1);
    assert.equal(result["edit.slot3_data.get"].calls, 1);
    assert.equal(result["edit.slot3_data.end"].calls, 1);
    assert.equal(q.counters.byteLength, 18432);
    q.reset(); imports.get(policy);
    assert.deepEqual(Object.keys(q.result()), ["other.unknown.get"]);
});

test("qualification labels pinned reads with their original database slot", () => {
    const context = counters(), q = context.pbQualification;
    const imports = context.wrap({ begin: () => 1, pin: () => 2, end: () => 0, reader_read: () => 8, reader_end: () => 0 });
    const transaction = imports.begin(4, 0), reader = imports.pin(transaction);
    imports.end(transaction); q.reset(); q.phase = 2;
    imports.reader_read(reader); imports.reader_end(reader);
    const result = q.result();
    assert.equal(result["query.slot4_policy_index.reader_read"].calls, 1);
    assert.equal(result["query.slot4_policy_index.reader_end"].calls, 1);
});

test("instrumented key codecs preserve all byte values and only record lengths", async () => {
    const store = await readFile(new URL("./opfs-store.ts", import.meta.url), "utf8");
    const context = counters();
    vm.runInContext(stripTypeScriptTypes(helper.profile.profileOpfsStore(store), { mode: "strip" }).replace(/^import .*;$/gm, "").replaceAll("export ", "")
        + "globalThis.codec = { keyHex, fromHex };", context);
    const bytes = Uint8Array.from({ length: 256 }, (_, i) => i);
    const q = context.pbQualification; q.reset(); q.slot = 3;
    const encoded = context.codec.keyHex(bytes);
    assert.equal(encoded, Buffer.from(bytes).toString("hex"));
    assert.deepEqual(Buffer.from(context.codec.fromHex(encoded)), Buffer.from(bytes));
    const result = q.result();
    assert.equal(result["other.slot3_data.key_encode"].units, 256);
    assert.equal(result["other.slot3_data.key_decode"].units, 256);
    assert.deepEqual(Object.keys(result["other.slot3_data.key_encode"]), ["calls", "ms", "units"]);
});


test("failure harness withholds one click and draft, rejects without dispatch, then resumes real requests", () => {
    class Worker {
        sent = [];
        events = [];
        postMessage(...args) { this.sent.push(args); }
        dispatchEvent(event) { this.events.push(event); }
    }
    class MessageEvent {
        constructor(type, options) { this.type = type; this.data = options.data; }
    }
    const original = Worker.prototype.postMessage;
    const context = vm.createContext({ encodeId, decodeId, validId, Worker, MessageEvent });
    vm.runInContext(`(${helper.profile.installCheckboxFailureHarness.toString()})()`, context);
    const harness = context.checkboxFailureHarness, worker = new Worker();
    const message = (id, kind) => ({ id, method: "input", args: { kind, target: 100, data: new Uint8Array() } });
    worker.postMessage(message(1, 14));
    assert.equal(harness.state().textCalls, 1);
    assert.equal(harness.state().lastKind, 14);
    assert.equal(harness.state().lastTarget, 100);
    harness.arm(100);
    const click = message(2, 2), draft = message(3, 14), options = { transfer: [] };
    worker.postMessage(click);
    worker.postMessage(draft, options);
    assert.equal(harness.state().clickHeld, true);
    assert.equal(harness.state().draftHeld, true);
    assert.equal(worker.sent.length, 1);
    assert.throws(() => harness.arm(100), /occupied/);
    assert.throws(() => harness.restore(), /still holds work/);
    const presentation = { scene: { kind: "checkbox", state: 2 } };
    harness.acceptScene(presentation);
    assert.equal(worker.events[0].data.adopted, presentation);
    harness.reject("Injected rejection");
    assert.equal(worker.events[1].data.id, click.id);
    assert.equal(worker.events[1].data.error, "Injected rejection");
    assert.equal(worker.sent.length, 1); // Rejected click never reaches the worker.
    harness.releaseDraft();
    assert.equal(worker.sent.length, 2);
    assert.equal(worker.sent[1][0], draft);
    assert.equal(worker.sent[1][1], options);
    assert.throws(() => harness.releaseDraft(), /no text/);
    harness.restore();
    assert.equal(Worker.prototype.postMessage, original);
    worker.postMessage(message(4, 2));
    assert.equal(worker.sent.length, 3);
});

function reportFor(p99, profiling = false) {
    const sample = p99 => ({ n: 1000, p99, max: p99 + 1 });
    return { profiling, qualificationStatus: "unverified", controls: { toggle: sample(2) },
        largerControls: { reports: [{ rows: 32, ...sample(5) }, { rows: 64, ...sample(p99) }] } };
}

test("local timing checks keep functional and phase qualification status separate", () => {
    const report = reportFor(7.9), controls = report.controls, larger = report.largerControls;
    helper.profile.finishQualification(report);
    assert.equal(report.functionalStatus, "passed");
    assert.equal(report.qualificationStatus, "unverified");
    assert.equal(report.status, "functional_and_local_checks_passed_phase_unverified");
    assert.equal(report.localPerformance.asserted, true);
    assert.equal(report.controls, controls);
    assert.equal(report.largerControls, larger);
    for (const p99 of [8, 9, NaN]) {
        const failed = reportFor(p99);
        assert.throws(() => helper.profile.finishQualification(failed), /p99 must be below 8 ms/);
        assert.equal(failed.functionalStatus, "passed");
        assert.equal(failed.qualificationStatus, "unverified");
        assert.equal(failed.status, "functional_passed_local_performance_failed");
        assert.equal(failed.localPerformance.cohorts[2].belowBudget, false);
    }
});

test("intrusive profile timing is never asserted against the local budget", () => {
    const report = reportFor(100, true);
    assert.doesNotThrow(() => helper.profile.finishQualification(report));
    assert.equal(report.functionalStatus, "passed");
    assert.equal(report.qualificationStatus, "unverified");
    assert.equal(report.status, "functional_passed_profile_diagnostic_phase_unverified");
    assert.equal(report.localPerformance.asserted, false);
    assert.equal(report.localPerformance.cohorts[2].belowBudget, false);
});


test("profiling transforms and bundles the actual asynchronous worker sources", async () => {
    const worker = await readFile(new URL("./runtime-worker.ts", import.meta.url), "utf8");
    const instrumented = helper.profile.profileRuntimeWorker(worker);
    assert.match(instrumented, /host: \(\) => qualifyHost\(host\.exports\)/);
    assert.match(instrumented, /completeInputQualification\(\)/);
    assert.throws(() => helper.profile.profileRuntimeWorker(worker.replace("host: () => host.exports,", "host: changed,")), /boundary changed/);
    const result = await build({ entryPoints: [fileURLToPath(new URL("./runtime-worker.ts", import.meta.url))],
        bundle: true, format: "esm", write: false, logLevel: "silent",
        plugins: [{ name: "actual-qualification", setup(bundle) {
            bundle.onLoad({ filter: /[\\/]src[\\/](runtime-worker|opfs-store)\.ts$/ }, async ({ path }) => {
                const source = await readFile(path, "utf8");
                return { contents: path.endsWith("opfs-store.ts") ? helper.profile.profileOpfsStore(source)
                    : helper.profile.profileRuntimeWorker(source), loader: "ts" };
            });
        } }],
    });
    assert.equal(result.outputFiles.length, 1);
    assert.match(result.outputFiles[0].text, /pb_web_object_edit/);
});

test("async backend phases persist past admission and never mutate Wasm exports", async () => {
    let clock = 0, sequence = 0;
    class AccessHandle { read() {} write() {} flush() {} truncate() {} }
    const guestWork = { pending: true, failed: false }, operations = { data: { waiting: true } };
    const context = vm.createContext({ encodeId, decodeId, validId, performance: { now: () => ++clock }, FileSystemSyncAccessHandle: AccessHandle,
        guestWork, operations, enterGuestRaw: async () => {} });
    vm.runInContext(helper.profile.runtimeQualification +
        "globalThis.profile={qualifyHost,beginInputQualification,enterGuest,completeInputQualification};", context);
    const imports = context.qualificationImports({ begin: () => ++sequence, end: () => 0, put: () => 0, get: () => 1 });
    const originalEdit = () => { const tx = imports.begin(3, 1); imports.put(tx); imports.end(tx); return 7; };
    const originalQuery = () => { const tx = imports.begin(3, 0); imports.get(tx); imports.end(tx); return 8; };
    const memory = {}, exports = Object.freeze({ memory, pb_web_object_edit: originalEdit, pb_web_object_query: originalQuery });
    const host = context.profile.qualifyHost(exports);
    assert.notEqual(host, exports); assert.equal(host.memory, memory);
    assert.equal(context.profile.qualifyHost(exports), host);
    context.profile.beginInputQualification();
    await context.profile.enterGuest();
    assert.equal(context.profile.completeInputQualification(), undefined, "Pending ACK cannot finish the measurement");
    assert.equal(context.pbQualification.active, true);
    assert.equal(host.pb_web_object_edit(), 7);
    assert.equal(host.pb_web_object_query(), 8);
    context.profile.beginInputQualification();
    guestWork.pending = false;
    assert.equal(context.profile.completeInputQualification(), undefined, "external execution still owns work");
    operations.data.waiting = false;
    const result = context.profile.completeInputQualification();
    assert.equal(result.inputs, 2); assert.equal(result.editCount, 1); assert.equal(result.queryCount, 1);
    assert.equal(result.opfs["edit.slot3_data.put"].calls, 1);
    assert.equal(result.opfs["query.slot3_data.get"].calls, 1);
    assert.equal(result.failed, false); assert.ok(result.completedMs > result.guestMs);
    assert.equal(context.pbQualification.active, false);
    assert.equal(context.profile.completeInputQualification(), undefined, "one terminal diagnostic result");
    assert.equal(exports.pb_web_object_edit, originalEdit); assert.equal(exports.pb_web_object_query, originalQuery);
    const replacement = Object.freeze({ ...exports, pb_web_object_query: () => 9 });
    assert.equal(context.profile.qualifyHost(replacement).pb_web_object_query(), 9);
    guestWork.failed = true;
    context.profile.beginInputQualification();
    assert.equal(context.profile.completeInputQualification().failed, true);
});

test("diagnostic receiver retains one latest completion without changing requests", async () => {
    class Worker {
        sent = [];
        listeners = [];
        postMessage(...args) { this.sent.push(args); return 7; }
        addEventListener(name, callback) { assert.equal(name, "message"); this.listeners.push(callback); }
    }
    const deadlines = new Map(); let sequence = 0;
    const context = vm.createContext({ encodeId, decodeId, validId, Worker, setTimeout: fn => { deadlines.set(++sequence, fn); return sequence; },
        clearTimeout: id => deadlines.delete(id) });
    vm.runInContext(`(${helper.profile.installQualificationReceiver.toString()})()`, context);
    const worker = new Worker(), request = { method: "input" }, options = { transfer: [] };
    assert.equal(worker.postMessage(request, options), 7);
    assert.equal(worker.sent[0][0], request); assert.equal(worker.sent[0][1], options);
    const first = { sequence: 1 }, second = { sequence: 2 };
    worker.listeners[0]({ data: { result: { qualification: first } } });
    assert.equal(context.qualificationResult.latest, first);
    assert.equal(await context.waitQualification(), first);
    worker.postMessage(request); assert.equal(context.qualificationResult.latest, null);
    assert.equal(worker.listeners.length, 1);
    const completion = context.waitQualification();
    await assert.rejects(context.waitQualification(), /waiter is already active/);
    assert.equal(deadlines.size, 1);
    worker.listeners[0]({ data: { qualification: second } });
    assert.equal(context.qualificationResult.latest, second);
    assert.equal(await completion, second);
    assert.equal(deadlines.size, 0);
    worker.postMessage(request);
    const failed = context.waitQualification();
    deadlines.values().next().value();
    await assert.rejects(failed, /input was not replayed/);
    assert.equal(worker.sent.length, 3);
});
