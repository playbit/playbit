// Real Chrome/OPFS qualification; no web browser automation package is required.
import { spawn } from "node:child_process";
import { build } from "esbuild";
import { createServer } from "node:http";
import { readFile, writeFile, mkdir, mkdtemp, readdir } from "node:fs/promises";
import { resolve, join, extname } from "node:path";
import { fileURLToPath } from "node:url";
import { once } from "node:events";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";

// Diagnostic-only source instrumentation. Counters retain no keys, values or IDs.
function replaceBoundary(source, boundary, replacement) {
    if (source.split(boundary).length !== 2) throw new Error(`Qualification boundary changed: ${boundary.slice(0, 80)}`);
    return source.replace(boundary, replacement);
}
const opfsQualification = `
    const qualificationNames = ['begin','end','get','value_length','read_value','next_key','next','put','put_start','put_part','put_finish','delete','pin','reader_length','reader_read','reader_end','stage_begin','stage_clear','stage_append','stage_length','stage_read','stage_end','random','now','snapshot','key_encode','key_decode','map_lookup','raw_read','raw_write','raw_flush','raw_truncate'];
    globalThis.pbQualification = {
        active:false,slot:0,phase:0,names:qualificationNames,
        counters:new Float64Array(3 * 8 * qualificationNames.length * 3),
        reset(){this.counters.fill(0);this.slot=0;this.phase=0;this.active=true;},
        record(name,elapsed,units=0){
            if(!this.active)return;
            const method=this.names.indexOf(name);if(method<0)throw new Error('Unknown qualification counter');
            const index=((this.phase*8+this.slot)*this.names.length+method)*3;
            this.counters[index]++;this.counters[index+1]+=elapsed;this.counters[index+2]+=units;
        },
        result(){
            const result={};
            for(let phase=0;phase<3;phase++)for(let slot=0;slot<8;slot++)for(let method=0;method<this.names.length;method++){
                const index=((phase*8+slot)*this.names.length+method)*3;
                if(!this.counters[index])continue;
                const key=['other','edit','query'][phase]+'.'+['unknown','slot1_policy_index','slot2_revocations','slot3_data','slot4_policy_index','slot5_revocations','slot6_data','other_slot'][slot]+'.'+this.names[method];
                result[key]={calls:this.counters[index],ms:this.counters[index+1],units:this.counters[index+2]};
            }
            return result;
        }
    };
    function qualificationImports(imports){
        const transactions=new Map(),readers=new Map(), q=globalThis.pbQualification;
        const transactional=new Set(['end','get','value_length','read_value','next_key','next','put','put_start','put_part','put_finish','delete','pin']);
        const readerMethods=new Set(['reader_length','reader_read','reader_end']);
        return Object.fromEntries(Object.entries(imports).map(([name,fn])=>[name,(...args)=>{
            const previous=q.slot;
            q.slot=name==='begin'?(args[0]>=1&&args[0]<=6?args[0]:7):transactional.has(name)?transactions.get(args[0])??0:readerMethods.has(name)?readers.get(args[0])??0:0;
            const began=q.active?performance.now():0;
            try{
                const result=fn(...args);
                if(name==='begin'&&result>0){transactions.set(result,q.slot);if(transactions.size>32)throw new Error('Qualification transaction map exceeded32');}
                if(name==='pin'&&result>0){readers.set(result,q.slot);if(readers.size>32)throw new Error('Qualification reader map exceeded32');}
                return result;
            }finally{
                if(q.active)q.record(name,performance.now()-began);
                if(name==='end')transactions.delete(args[0]);
                if(name==='reader_end')readers.delete(args[0]);
                q.slot=previous;
            }
        }]));
    }
`;
function profileOpfsStore(source) {
    source = replaceBoundary(source, 'const keyHex = (bytes: Uint8Array): string => {', 'const keyHexRaw = (bytes: Uint8Array): string => {');
    source = replaceBoundary(source, 'const fromHex = (value: string): Uint8Array =>', 'const fromHexRaw = (value: string): Uint8Array =>');
    source = replaceBoundary(source, 'const hashBytes = (hash: number, bytes: Uint8Array): number => {', `
        const keyHex = bytes => {const q=globalThis.pbQualification,t=q?.active?performance.now():0;try{return keyHexRaw(bytes);}finally{if(q?.active)q.record('key_encode',performance.now()-t,bytes.length);}};
        const fromHex = value => {const q=globalThis.pbQualification,t=q?.active?performance.now():0;try{return fromHexRaw(value);}finally{if(q?.active)q.record('key_decode',performance.now()-t,value.length/2);}};
        const hashBytes = (hash: number, bytes: Uint8Array): number => {
    `);
    source = replaceBoundary(source, 'this.snapshot = new Map(store.index); this.keys = store.keys.slice();', `
        const q=globalThis.pbQualification, t=q?.active?performance.now():0;
        this.snapshot = new Map(store.index); this.keys = store.keys.slice();
        if(q?.active)q.record('snapshot',performance.now()-t,store.index.size);
    `);
    return replaceBoundary(source, 'return this.changes.has(hex) ? this.changes.get(hex)!.value : this.snapshot.get(hex) ?? null;', `
        const q=globalThis.pbQualification,t=q?.active?performance.now():0;
        try{return this.changes.has(hex) ? this.changes.get(hex)!.value : this.snapshot.get(hex) ?? null;}
        finally{if(q?.active)q.record('map_lookup',performance.now()-t);}
    `);
}

const runtimeQualification = opfsQualification + `
    let qualification = null, qualificationSequence = 0, qualifiedExports, qualifiedHost;
    for (const name of ['read','write','flush','truncate']) {
        const original = FileSystemSyncAccessHandle.prototype[name];
        FileSystemSyncAccessHandle.prototype[name] = function(...args) {
            if (!qualification) return original.apply(this,args);
            const began = performance.now();
            try { return original.apply(this,args); }
            finally { const elapsed=performance.now()-began; qualification[name+'Ms']+=elapsed; qualification[name+'Count']++; globalThis.pbQualification.record('raw_'+name,elapsed); }
        };
    }
    function qualifyHost(exports) {
        if (exports === qualifiedExports) return qualifiedHost;
        // Wasm exports are immutable. This is a separate plain method table,
        // used only by the actual async backend executor, never by guest imports.
        qualifiedExports = exports; qualifiedHost = {...exports};
        for (const [operation,name,phase] of [['pb_web_object_edit','edit',1],['pb_web_object_query','query',2]]) {
            const original = exports[operation];
            if (typeof original !== 'function') throw new Error('Qualification backend boundary changed: '+operation);
            qualifiedHost[operation] = (...args) => {
                if (!qualification) return original(...args);
                const began=performance.now(), q=globalThis.pbQualification, previous=q.phase; q.phase=phase;
                try { return original(...args); }
                finally { qualification[name+'Ms']+=performance.now()-began; qualification[name+'Count']++; q.phase=previous; }
            };
        }
        return qualifiedHost;
    }
    function beginInputQualification() {
        if (qualification) { qualification.inputs++; return; }
        qualification = {sequence:++qualificationSequence,inputs:1,started:performance.now(),admissionMs:0,guestMs:0,
            queryMs:0,editMs:0,queryCount:0,editCount:0,readMs:0,readCount:0,writeMs:0,writeCount:0,flushMs:0,flushCount:0,truncateMs:0,truncateCount:0};
        globalThis.pbQualification.reset();
    }
    async function enterGuest() {
        const captured=qualification, began=captured?performance.now():0;
        try { return await enterGuestRaw(); }
        finally { if(captured)captured.guestMs+=performance.now()-began; }
    }
    function completeInputQualification() {
        if (!qualification || guestWork.pending || operations.data.waiting) return;
        const {started,...result}=qualification;
        result.completedMs=performance.now()-started;
        result.failed=guestWork.failed;
        result.opfs=globalThis.pbQualification.result();
        globalThis.pbQualification.active=false; qualification=null;
        return result;
    }
`;
function profileRuntimeWorker(source) {
    source = replaceBoundary(source, 'return { ...storageBridge.imports,', 'return { ...qualificationImports(storageBridge.imports),');
    source = replaceBoundary(source, 'async function enterGuest() {', 'async function enterGuestRaw() {');
    source = replaceBoundary(source, '\n    host: () => host.exports,', '\n    host: () => qualifyHost(host.exports),');
    source = replaceBoundary(source, 'if (scene) self.postMessage({ replicaScene: presentation() });', `
        if (scene) self.postMessage({ replicaScene: presentation() });
        const measured = completeInputQualification();
        if (measured) self.postMessage({ qualification: measured });
    `);
    source = replaceBoundary(source, 'await enterGuest(); result = presentation();', `
        beginInputQualification(); const began=performance.now();
        await enterGuest(); qualification.admissionMs+=performance.now()-began;
        result = presentation();
        const measured=completeInputQualification();
        if(measured)result.qualification=measured;
    `);
    return runtimeQualification + source;
}

// One latest result per isolated page; no request is delayed, changed or replayed.
function installQualificationReceiver() {
    const original=Worker.prototype.postMessage, observed=new WeakSet();
    let waiting;
    globalThis.qualificationResult={latest:null};
    globalThis.waitQualification=() => {
        if(globalThis.qualificationResult.latest)return Promise.resolve(globalThis.qualificationResult.latest);
        if(waiting)return Promise.reject(new Error('A qualification waiter is already active'));
        return new Promise((resolve,reject) => {
            const deadline=setTimeout(() => {waiting=undefined;reject(new Error('Accepted work did not complete; input was not replayed'));},5000);
            waiting={resolve:result => {clearTimeout(deadline);waiting=undefined;resolve(result);}};
        });
    };
    Worker.prototype.postMessage=function(message,...options) {
        if(message.method==='input') {
            globalThis.qualificationResult.latest=null;
            if(!observed.has(this)) {
                observed.add(this);
                this.addEventListener('message', ({data}) => {
                    const result=data.qualification ?? data.result?.qualification;
                    if(result) {
                        globalThis.qualificationResult.latest=result;
                        waiting?.resolve(result);
                    }
                });
            }
        }
        return original.call(this,message,...options);
    };
}
// Observe DOM changes, with a failure deadline rather than a freshness timer.
function installCommittedWait() {
    globalThis.waitCommitted=predicate => new Promise((resolve,reject) => {
        if(predicate()) { resolve(); return; }
        const observer=new MutationObserver(() => {
            if(predicate()) { observer.disconnect(); clearTimeout(deadline); resolve(); }
        });
        const deadline=setTimeout(() => {observer.disconnect();reject(new Error('Committed visible state did not arrive; input was not replayed'));},5000);
        observer.observe(document.querySelector('main'),{subtree:true,attributes:true,childList:true,characterData:true});
    });
}

// Runs only in isolated control-check pages, installed by CDP before playbit.js.
// At most one Click and one following text request are withheld. No request is
// modified or retried, and the failed Click never reaches the runtime worker.
function installCheckboxFailureHarness() {
    const original = Worker.prototype.postMessage;
    let armedTarget, heldClick, heldDraft, gateDraft = false, worker;
    let textCalls = 0, lastKind = 0, lastTarget = 0;
    const hold = (target, message, options) => {
        if ((message.args?.data?.byteLength ?? 0) > 65536) throw new Error("Failure fixture payload exceeds limit");
        return { target, message, options };
    };
    Worker.prototype.postMessage = function(message, ...options) {
        if (message.method === "input") {
            worker = this; lastKind = message.args.kind; lastTarget = message.args.target;
            if (lastKind === 14) textCalls++;
        }
        if (message.method === "input" && message.args.kind === 2 && message.args.target === armedTarget) {
            if (heldClick) throw new Error("Failure fixture already holds a Click");
            heldClick = hold(this, message, options); worker = this; armedTarget = undefined; return;
        }
        if (gateDraft && message.method === "input" && message.args.kind === 14) {
            if (heldDraft) throw new Error("Failure fixture already holds text");
            heldDraft = hold(this, message, options); return;
        }
        return original.call(this, message, ...options);
    };
    globalThis.checkboxFailureHarness = {
        arm(target) {
            if (heldClick || heldDraft || armedTarget !== undefined) throw new Error("Failure fixture is occupied");
            armedTarget = target; gateDraft = true;
        },
        state() { return { clickHeld: Boolean(heldClick), draftHeld: Boolean(heldDraft), textCalls, lastKind, lastTarget }; },
        acceptScene(presentation) {
            if (!worker) throw new Error("Failure fixture has no worker");
            worker.dispatchEvent(new MessageEvent("message", { data: { adopted: presentation } }));
        },
        reject(message) {
            if (!heldClick) throw new Error("Failure fixture has no Click");
            const request = heldClick; heldClick = undefined;
            request.target.dispatchEvent(new MessageEvent("message", { data: { id: request.message.id, error: message } }));
        },
        releaseDraft() {
            if (!heldDraft) throw new Error("Failure fixture has no text");
            const request = heldDraft; heldDraft = undefined; gateDraft = false;
            original.call(request.target, request.message, ...request.options);
        },
        restore() {
            if (heldClick || heldDraft || armedTarget !== undefined) throw new Error("Failure fixture still holds work");
            Worker.prototype.postMessage = original;
        },
    };
}

function finishQualification(report) {
    report.functionalStatus = "passed";
    const cohorts = [{ rows: 3, ...report.controls.toggle }, ...report.largerControls.reports];
    report.localPerformance = {
        mode: report.profiling ? "intrusive_profile_diagnostic" : "local_admission_to_committed_layout_check",
        budgetMs: 8,
        asserted: !report.profiling,
        cohorts: cohorts.map(({ rows, n, p99, max }) => ({ rows, n, p99, max, belowBudget: Number.isFinite(p99) && p99 < 8 })),
        limitation: "These local cohorts exclude native event receipt, concurrent sessions/work, paint and physical presentation. They do not qualify Phase 3b or a supported platform. Profile timing is diagnostic and is never asserted.",
    };
    if (report.profiling) {
        report.status = "functional_passed_profile_diagnostic_phase_unverified";
        return;
    }
    const failed = report.localPerformance.cohorts.filter(cohort => !cohort.belowBudget);
    if (failed.length) {
        report.status = "functional_passed_local_performance_failed";
        assert.fail(`Local admission-to-committed-layout p99 must be below 8 ms: ${failed.map(cohort => `${cohort.rows} rows=${cohort.p99}ms`).join(', ')}`);
    }
    report.status = "functional_and_local_checks_passed_phase_unverified";
}

const output = resolve("target/web"), evidence = await mkdtemp(resolve("target/web-evidence-"));
// Drive the host with the same typed edit encoding as the installed UI.
const textInput = await build({ entryPoints: [fileURLToPath(new URL("./src/text-input.ts", import.meta.url))],
    bundle: true, write: false, format: "iife", globalName: "qualificationText" });
const webBrowserProfile = join(evidence, "chrome");
await mkdir(webBrowserProfile);
// Intrusive profiling instruments the worker sources before bundling, so the
// boundaries above match the checked-in files rather than esbuild output.
let profiledWorker;
if (process.argv.includes("--profile")) {
    const esbuild = await import("esbuild");
    const built = await esbuild.build({
        entryPoints: [fileURLToPath(new URL("./src/runtime-worker.ts", import.meta.url))], bundle: true, format: "esm", write: false, logLevel: "warning",
        plugins: [{ name: "qualification", setup(build) {
            build.onLoad({ filter: /[\\/]src[\\/](runtime-worker|opfs-store)\.ts$/ }, async ({ path }) => {
                const source = await readFile(path, "utf8");
                return { contents: path.endsWith("opfs-store.ts") ? profileOpfsStore(source) : profileRuntimeWorker(source), loader: "ts" };
            });
        } }],
    });
    profiledWorker = built.outputFiles[0].text;
}
const server = createServer(async (request, response) => {
    try {
        const path = new URL(request.url, "http://localhost").pathname;
        if (!/^\/[a-zA-Z0-9_.-]*$/.test(path)) throw new Error("Invalid path");
        const file = join(output, path === "/" ? "index.html" : path);
        response.setHeader("Content-Type", ({ ".js": "text/javascript", ".wasm": "application/wasm", ".html": "text/html", ".json": "application/json", ".css": "text/css", ".svg": "image/svg+xml", ".ttf": "font/ttf" })[extname(file)] ?? "application/octet-stream");
        let content = await readFile(file);
        if (profiledWorker && path === "/runtime-worker.js") content = profiledWorker;
        response.end(content);
    } catch { response.writeHead(404).end(); }
});
server.listen(0, "127.0.0.1"); await once(server, "listening");
const origin = `http://127.0.0.1:${server.address().port}`;
const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));

export async function cdp(url) {
    const socket = new WebSocket(url);
    await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
    let sequence = 0;
    const pending = new Map();
    socket.onmessage = ({ data }) => {
        const message = JSON.parse(data), request = pending.get(message.id);
        if (!request) return;
        pending.delete(message.id);
        if (message.error) request.reject(new Error(message.error.message)); else request.resolve(message.result);
    };
    return { close: () => socket.close(), call: (method, params = {}) => new Promise((resolve, reject) => {
        const id = ++sequence; pending.set(id, { resolve, reject }); socket.send(JSON.stringify({ id, method, params }));
    }) };
}
let chrome, browser, debugging;
async function launch() {
    chrome = spawn("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", ["--headless=new", "--no-first-run", "--no-default-browser-check", `--user-data-dir=${webBrowserProfile}`, "--remote-debugging-port=0", "about:blank"], { stdio: ["ignore", "ignore", "pipe"] });
    let stderr = ""; chrome.stderr.on("data", data => { stderr += data; });
    for (let attempt = 0; attempt < 100; attempt++) {
        const match = stderr.match(/DevTools listening on (ws:\/\/[^\s]+)/);
        if (match) { debugging = new URL(match[1]); browser = await cdp(match[1]); return; }
        if (chrome.exitCode !== null) throw new Error(`Chrome exited: ${stderr}`);
        await delay(100);
    }
    throw new Error("Chrome debugger unavailable");
}
async function stop() {
    if (!chrome || chrome.exitCode !== null) return;
    const exited = once(chrome, "exit");
    await browser.call("Browser.close"); browser.close(); await exited;
}
async function page(url, initialization) {
    const { targetId } = await browser.call("Target.createTarget", { url: "about:blank" });
    const target = (await (await fetch(`http://${debugging.host}/json/list`)).json()).find(item => item.id === targetId);
    const tab = await cdp(target.webSocketDebuggerUrl);
    await tab.call("Page.enable"); await tab.call("Runtime.enable");
    await tab.call("Emulation.setDeviceMetricsOverride", { width: 800, height: 900, deviceScaleFactor: 1, mobile: false });
    await tab.call("Page.addScriptToEvaluateOnNewDocument", { source:
        textInput.outputFiles[0].text + `(${installCommittedWait.toString()})();` +
        (profiledWorker ? `(${installQualificationReceiver.toString()})();` : "") + (initialization ?? "") });
    await tab.call("Page.navigate", { url });
    const evaluate = async expression => {
        const result = await tab.call("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
        if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
        return result.result.value;
    };
    for (let attempt = 0; attempt < 100; attempt++) {
        if (await evaluate("Boolean(window.playbit)")) break;
        await delay(50);
    }
    const opened = await evaluate("window.playbit.ready");
    return { tab, evaluate, opened };
}
async function waitFor(page, expression) {
    for (let attempt = 0; attempt < 100; attempt++) { if (await page.evaluate(expression)) return; await delay(50); }
    throw new Error(`Web browser condition failed: ${expression}`);
}
async function screenshot(page, filename) {
    const result = await page.tab.call("Page.captureScreenshot", { format: "png" });
    await writeFile(join(evidence, filename), Buffer.from(result.data, "base64"));
}
async function key(page, key, type) {
    await page.tab.call("Input.dispatchKeyEvent", { type, key, code: key === "Enter" ? "Enter" : key, windowsVirtualKeyCode: ({ Enter: 13, ArrowUp: 38, ArrowRight: 39 })[key] ?? 0 });
}
async function todoPage(profile, initialization) {
    const ui=await page(`${origin}/?app=todo&profile=${profile}`,initialization);
    // These profiles start with a seeded task; wait for its committed checkbox.
    await ui.evaluate("waitCommitted(() => document.querySelector('.guest-check input'))");
    return ui;
}
async function checkboxFailureCheck() {
    const ui = await todoPage("checkbox-failure", `(${installCheckboxFailureHarness.toString()})()`);
    const cases = [];
    try {
        // Use the real guest to create a row if this local fixture starts empty.
        await ui.evaluate(`(async () => {
            if (!document.querySelector('.guest-check input')) {
                await window.playbit.input(3, 1, new TextEncoder().encode('Checkbox failure fixture'));
                await waitCommitted(() => document.querySelector('.guest-check input'));
                if(globalThis.waitQualification)await waitQualification();
            }
        })()`);
        const rows = await ui.evaluate("document.querySelectorAll('.guest-check input').length");
        for (const [name, initialState, newerState] of [["off", 0, null], ["newer-on", 0, 1], ["mixed", 2, null]]) {
            // Scene injection exercises the existing adopted-presentation handler.
            // Mixed state is a semantic fixture; the TODO guest does not emit it.
            const initial = await ui.evaluate(`(async () => {
                const opened = await window.playbit.ready, accepted = await window.playbit.input(14, 1, qualificationText.encodeTextInput('', null)), field = document.querySelector('.guest-field'), check = document.querySelector('.guest-check input');
                globalThis.checkboxFailureElements = { field, check };
                const fixture = structuredClone(accepted);
                fixture.build_id = opened.identity.version;
                const visit = node => { if (node.kind === 'checkbox') node.state = ${initialState}; if (node.kind === 'input') node.value = ''; node.children?.forEach(visit); };
                visit(fixture.scene); globalThis.checkboxFailureScene = fixture;
                document.querySelector('#error').textContent = '';
                field.value = ''; field.focus();
                const r = check.getBoundingClientRect();
                return {target:Number(check.dataset.control),x:r.x+r.width/2,y:r.y+r.height/2};
            })()`);
            await ui.evaluate(`checkboxFailureHarness.acceptScene(checkboxFailureScene); checkboxFailureHarness.arm(${initial.target})`);
            // Intercept the actual native checkbox Click after the accepted state is visible.
            await ui.tab.call("Input.dispatchMouseEvent", { type: "mousePressed", x: initial.x, y: initial.y, button: "left", clickCount: 1 });
            await ui.tab.call("Input.dispatchMouseEvent", { type: "mouseReleased", x: initial.x, y: initial.y, button: "left", clickCount: 1 });
            await waitFor(ui, "checkboxFailureHarness.state().clickHeld");
            await ui.evaluate("checkboxFailureElements.field.focus()");
            const draft = `Newer draft ${name}`;
            await ui.tab.call("Input.insertText", { text: draft });
            await ui.evaluate(`checkboxFailureElements.field.setSelectionRange(2, 8, 'backward')`);
            if (newerState !== null) await ui.evaluate(`{
                const visit = node => {if(node.kind==='checkbox')node.state=${newerState};node.children?.forEach(visit)};
                visit(checkboxFailureScene.scene);checkboxFailureHarness.acceptScene(checkboxFailureScene);
            }`);
            const message = `Qualification rejected native checkbox: ${name}`;
            await ui.evaluate(`checkboxFailureHarness.reject(${JSON.stringify(message)})`);
            await waitFor(ui, `checkboxFailureHarness.state().draftHeld && document.querySelector('#error').textContent === ${JSON.stringify(message)}`);
            const expected = newerState ?? initialState;
            const restored = await ui.evaluate(`(() => {
                const {field,check}=checkboxFailureElements;
                if (document.querySelector('.guest-field')!==field || document.querySelector('.guest-check input')!==check) throw new Error('Rejected input replaced native controls');
                if (check.checked!==${expected === 1} || check.indeterminate!==${expected === 2}) throw new Error('Rejected input did not restore latest accepted checkbox state');
                if (field.value!==${JSON.stringify(draft)} || field.selectionStart!==2 || field.selectionEnd!==8 || field.selectionDirection!=='backward' || document.activeElement!==field) throw new Error('Rejected input damaged newer native editing');
                return {checked:check.checked,mixed:check.indeterminate,draftAndSelectionRetained:true};
            })()`);
            await ui.evaluate(`checkboxFailureHarness.releaseDraft()`);
            await ui.evaluate(`window.playbit.input(14, 1, qualificationText.encodeTextInput(checkboxFailureElements.field.value, null))`);
            assert.equal(await ui.evaluate(`document.querySelector('#error').textContent`), message);
            const before = await ui.evaluate("checkboxFailureElements.check.checked");
            // Visible failure feedback above the guest can change its position.
            const retry = await ui.evaluate("(()=>{const r=checkboxFailureElements.check.getBoundingClientRect();return{x:r.x+r.width/2,y:r.y+r.height/2}})()");
            await ui.tab.call("Input.dispatchMouseEvent", { type: "mousePressed", ...retry, button: "left", clickCount: 1 });
            await ui.tab.call("Input.dispatchMouseEvent", { type: "mouseReleased", ...retry, button: "left", clickCount: 1 });
            await ui.evaluate(`window.playbit.input(14, 1, qualificationText.encodeTextInput(checkboxFailureElements.field.value, null))`);
            assert.equal(await ui.evaluate("checkboxFailureElements.check.checked"), !before);
            assert.equal(await ui.evaluate(`document.querySelector('#error').textContent`), '');
            cases.push({ name, ...restored, subsequentRealToggleWorked: true, failureClearedAfterRetry: true });
        }
        await ui.evaluate("checkboxFailureHarness.restore()");
        return { cases, rows, workload: "Three rejected native checkbox activations; one isolated local TODO session, first row only, one newer draft per rejection",
            boundary: "Native HTML change → intercepted worker request → injected accepted scene/error → retained DOM checked/mixed state and editor selection; followed by an actual worker commit",
            limitation: "Harness-injected rejection and mixed/updated scene are UI ordering fixtures, not proof of real storage failure or live-replication behavior. Excluded from performance cohorts." };
    } finally { ui.tab.close(); }
}


// Functional fixture only, after all timing samples. Scenes are injected through
// the existing worker-adoption handler; native controls and real commits remain
// the production implementation. No DOM replacement or input replay is injected.
async function siblingReconciliationCheck() {
    const ui = await todoPage("sibling-reconciliation", `(${installCheckboxFailureHarness.toString()})()`);
    const result = { status: "running", workload: "One isolated local TODO session; three rows, focused sibling reorder/insertion/removal, selected editor and one web browser engine composition",
        boundary: "Accepted scene injection → native DOM state-preserving moves → actual native input and worker commit",
        limitation: "Injected scene order is a UI fixture, not live-replication evidence. CDP engine composition does not qualify physical OS IME, accessibility, presentation, concurrent work or reconciliation heap usage. Excluded from timing cohorts." };
    try {
        result.moveBefore = await ui.evaluate("typeof Element.prototype.moveBefore");
        assert.equal(result.moveBefore, "function");
        await ui.evaluate(`(async () => {
            while (document.querySelectorAll('.guest-check input').length < 3) {
                const count=document.querySelectorAll('.guest-check input').length;
                await window.playbit.input(3,1,new TextEncoder().encode('Reconciliation row '+count));
                await waitCommitted(() => document.querySelectorAll('.guest-check input').length===count+1);
            }
            const accepted = await window.playbit.input(14, 1, qualificationText.encodeTextInput('', null));
            accepted.build_id = (await window.playbit.ready).identity.version;
            globalThis.reconciliation = { accepted, field: document.querySelector('.guest-field'), button: document.querySelector('.guest-button'), checks: Array.from(document.querySelectorAll('.guest-check input')) };
            const q = reconciliation; q.checks[1].focus(); q.before = q.checks.map(check => check.checked);
            const scene = structuredClone(accepted), rows = scene.scene.children.filter(node => node.kind === 'checkbox');
            let index = 0; const order = [rows[1], rows[2], rows[0]];
            scene.scene.children = scene.scene.children.map(node => node.kind === 'checkbox' ? order[index++] : node);
            checkboxFailureHarness.acceptScene(scene);
            if (document.querySelector('.guest-check input') !== q.checks[1] || document.activeElement !== q.checks[1]) throw new Error('Reordered checkbox lost native identity or focus');
        })()`);
        await screenshot(ui, "todo-siblings-focused.png");
        const clickRetained = async () => {
            const point = await ui.evaluate("(()=>{const r=reconciliation.checks[1].getBoundingClientRect();return{x:r.x+r.width/2,y:r.y+r.height/2}})()");
            await ui.tab.call("Input.dispatchMouseEvent", { type: "mousePressed", ...point, button: "left", clickCount: 1 });
            await ui.tab.call("Input.dispatchMouseEvent", { type: "mouseReleased", ...point, button: "left", clickCount: 1 });
        };
        await clickRetained();
        await waitFor(ui, "document.querySelector('.guest-check input') === reconciliation.checks[0]");
        await ui.evaluate(`{
            const q = reconciliation, last = checkboxFailureHarness.state();
            if (q.checks[1].checked === q.before[1] || q.checks[0].checked !== q.before[0] || q.checks[2].checked !== q.before[2] || document.activeElement !== q.checks[1]) throw new Error('Reordered checkbox committed to the wrong control or lost focus');
            if (last.lastKind !== 2 || last.lastTarget !== Number(q.checks[1].dataset.control)) throw new Error('Moved checkbox emitted a different target');
        }`);
        result.checkboxReorder = { retainedDOMAndFocus: true, correctTargetCommitted: true, guestOrderRestored: true };
        await ui.evaluate(`(async () => {
            const q = reconciliation; q.accepted = await window.playbit.input(14, 1, qualificationText.encodeTextInput('', null)); q.accepted.build_id = (await window.playbit.ready).identity.version;
            const scene = structuredClone(q.accepted), children = scene.scene.children;
            const last = children.findIndex(node => node.id === Number(q.checks[2].dataset.control)); children.splice(last, 1);
            const position = children.findIndex(node => node.id === Number(q.checks[1].dataset.control));
            children.splice(position, 0, {kind:'checkbox',id:4294967294,label:'Inserted qualification control',state:0,disabled:false});
            checkboxFailureHarness.acceptScene(scene);
            if (!document.querySelector('main').contains(q.checks[1]) || document.activeElement !== q.checks[1] || document.querySelector('main').contains(q.checks[2])) throw new Error('Sibling insertion/removal damaged retained focus or failed removal');
        })()`);
        await clickRetained();
        await waitFor(ui, "!document.querySelector('[data-control=\"4294967294\"]')");
        await ui.evaluate(`{
            const q = reconciliation;
            if (q.checks[1].checked !== q.before[1] || document.activeElement !== q.checks[1]) throw new Error('Retained checkbox failed after sibling insertion/removal');
        }`);
        result.checkboxInsertionRemoval = { retainedDOMAndFocus: true, subsequentCommitWorked: true };
        await ui.evaluate("reconciliation.field.focus()");
        await ui.tab.call("Input.insertText", { text: "Selected native draft" });
        await ui.evaluate(`(async () => {
            const q = reconciliation; q.accepted = await window.playbit.input(14, 1, qualificationText.encodeTextInput(q.field.value, null)); q.accepted.build_id = (await window.playbit.ready).identity.version;
            q.field.setSelectionRange(2, 8, 'backward');
            const scene = structuredClone(q.accepted), row = scene.scene.children.find(node => node.kind === 'row'); row.children.reverse();
            checkboxFailureHarness.acceptScene(scene);
            if (document.querySelector('.guest-field') !== q.field || document.querySelector('.guest-button') !== q.button || document.activeElement !== q.field || q.field.selectionStart !== 2 || q.field.selectionEnd !== 8 || q.field.selectionDirection !== 'backward' || q.field.value !== 'Selected native draft' || q.field.previousElementSibling !== q.button) throw new Error('Reordered field/Add lost native identity, selection or focus');
            q.accepted = await window.playbit.input(14, 1, qualificationText.encodeTextInput('', null)); q.accepted.build_id = (await window.playbit.ready).identity.version;
            const reverse = structuredClone(q.accepted); reverse.scene.children.find(node => node.kind === 'row').children.reverse(); checkboxFailureHarness.acceptScene(reverse);
            q.composition = {starts:0,ends:0}; q.field.addEventListener('compositionstart', () => q.composition.starts++); q.field.addEventListener('compositionend', () => q.composition.ends++);
            q.textCalls = checkboxFailureHarness.state().textCalls;
        })()`);
        result.editorSelection = { retainedDOMFocusAndBackwardSelection: true, fieldAndAddOrderChanged: true };
        try {
            await ui.tab.call("Input.imeSetComposition", { text: "に", selectionStart: 1, selectionEnd: 1 });
        } catch (error) {
            if (!/wasn't found|not found|not supported|unsupported/i.test(error.message)) throw error;
            result.composition = { status: "unverified", reason: `CDP Input.imeSetComposition unavailable: ${error.message}` };
        }
        if (!result.composition) {
            await ui.evaluate(`{
                const q = reconciliation;
                if (q.composition.starts !== 1 || q.composition.ends !== 0 || q.field.value !== 'に') throw new Error('CDP did not establish native web browser composition');
                checkboxFailureHarness.acceptScene(structuredClone(q.accepted));
                if (document.querySelector('.guest-field') !== q.field || document.activeElement !== q.field || q.field.value !== 'に' || q.field.selectionStart !== 1 || q.field.selectionEnd !== 1 || q.composition.ends !== 0 || checkboxFailureHarness.state().textCalls !== q.textCalls || q.field.nextElementSibling !== q.button) throw new Error('Moving composed editor lost composition, native state, or emitted premature guest text');
            }`);
            await ui.tab.call("Input.imeSetComposition", { text: "日本", selectionStart: 1, selectionEnd: 2 });
            await ui.evaluate(`{
                const q = reconciliation;
                if(q.composition.starts !== 1 || q.composition.ends !== 0 || q.field.value !== '日本' || q.field.selectionStart !== 1 || q.field.selectionEnd !== 2 || checkboxFailureHarness.state().textCalls !== q.textCalls) throw new Error('Composition did not continue on the moved editor');
            }`);
            await screenshot(ui, "todo-siblings-composition.png");
            await ui.tab.call("Input.insertText", { text: "日本語" });
            await waitFor(ui, "reconciliation.composition.ends === 1 && checkboxFailureHarness.state().textCalls > reconciliation.textCalls");
            const textCalls = await ui.evaluate(`(() => {
                const q = reconciliation, calls = checkboxFailureHarness.state();
                if (document.activeElement !== q.field || q.field.value !== '日本語' || calls.lastKind !== 14 || calls.lastTarget !== 1) throw new Error('Composition commit lost its native control target');
                return calls.textCalls - q.textCalls;
            })()`);
            await key(ui, "Enter", "keyDown"); await key(ui, "Enter", "keyUp");
            await waitFor(ui, "reconciliation.field.value === '' && Array.from(document.querySelectorAll('.guest-check')).some(label => label.innerText === '日本語')");
            assert.equal(await ui.evaluate("document.querySelector('.guest-field') === reconciliation.field && document.activeElement === reconciliation.field"), true);
            result.composition = { status: "passed", source: "CDP Input.imeSetComposition + Input.insertText; real Chromium editing engine, not physical OS IME", continuedAfterNativeMove: true, emittedTextChangesAfterCommit: textCalls, returnCreatedTask: true };
        }
        await ui.evaluate("checkboxFailureHarness.restore()");
        result.status = result.composition.status === "passed" ? "passed" : "partially_unverified";
        return result;
    } finally { ui.tab.close(); }
}

const report = { origin, evidence, status: "running", functionalStatus: "running", qualificationStatus: "unverified", profiling: process.argv.includes("--profile") };
report.artifacts = {};
for (const filename of (await readdir(output)).filter(name => /\.(js|wasm|pbapp|css|html|json|svg|ttf)$/.test(name)).sort()) {
    const bytes = await readFile(join(output, filename));
    report.artifacts[filename] = { bytes: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") };
}
try {
    await launch();
    let todo = await todoPage("release-todo");
    report.jspi = await todo.evaluate("({suspending:typeof WebAssembly.Suspending,promising:typeof WebAssembly.promising})");
    assert.deepEqual(report.jspi,{suspending:"function",promising:"function"});
    report.initial = { identity: todo.opened.identity, timing: todo.opened.timing, storageBytes: todo.opened.storageBytes };
    await todo.tab.call("Network.enable");
    await todo.tab.call("Network.emulateNetworkConditions", { offline: true, latency: 0, downloadThroughput: -1, uploadThroughput: -1 });
    await todo.evaluate("document.querySelector('input[type=text]').focus()");
    await todo.tab.call("Input.insertText", { text: "Web release gate" });
    await key(todo, "Enter", "keyDown"); await key(todo, "Enter", "keyUp");
    await waitFor(todo, "document.body.innerText.includes('Web release gate')");
    const point = await todo.evaluate("(()=>{const input=Array.from(document.querySelectorAll('label')).find(e=>e.innerText==='Web release gate').querySelector('input');const r=input.getBoundingClientRect();return{x:r.x+r.width/2,y:r.y+r.height/2}})()");
    await todo.tab.call("Input.dispatchMouseEvent", { type: "mousePressed", ...point, button: "left", clickCount: 1 });
    await todo.tab.call("Input.dispatchMouseEvent", { type: "mouseReleased", ...point, button: "left", clickCount: 1 });
    await waitFor(todo, "Array.from(document.querySelectorAll('label')).some(e=>e.innerText==='Web release gate'&&e.querySelector('input').checked)");
    await todo.evaluate("document.querySelector('.guest-field').focus()");
    await todo.tab.call("Input.insertText", { text: "Added by button" });
    await waitFor(todo, "!document.querySelector('.guest-button').disabled");
    const addPoint = await todo.evaluate("(()=>{const r=document.querySelector('.guest-button').getBoundingClientRect();return{x:r.x+r.width/2,y:r.y+r.height/2}})()");
    await todo.tab.call("Input.dispatchMouseEvent", { type: "mousePressed", ...addPoint, button: "left", clickCount: 1 });
    await screenshot(todo, "todo-add-pressed.png");
    await todo.tab.call("Input.dispatchMouseEvent", { type: "mouseReleased", ...addPoint, button: "left", clickCount: 1 });
    await waitFor(todo, "Array.from(document.querySelectorAll('label')).some(e=>e.innerText==='Added by button')");
    report.controls = await todo.evaluate(`(async () => {
        await document.fonts.ready;
        const field = document.querySelector('.guest-field');
        const button = document.querySelector('.guest-button');
        if (getComputedStyle(field).fontFamily.indexOf('Inter') < 0 || field.tagName !== 'INPUT' || button.tagName !== 'BUTTON') throw new Error('Styled semantic HTML controls missing');
        const original = field;
        field.focus(); field.value = 'Selected draft'; field.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: 'Selected draft' }));
        await window.playbit.input(14, 1, qualificationText.encodeTextInput(field.value, null));
        field.setSelectionRange(2, 8, 'backward');
        const checkbox = document.querySelector('.guest-check input');
        const target = Number(checkbox.dataset.control);
        const desired = !checkbox.checked;
        await window.playbit.input(2, target, new Uint8Array());
        await waitCommitted(() => checkbox.checked === desired && !checkbox.disabled);
        if (document.querySelector('.guest-field') !== original || document.activeElement !== original
            || field.selectionStart !== 2 || field.selectionEnd !== 8 || field.selectionDirection !== 'backward') throw new Error('Editor identity or selection lost');
        field.value = ''; field.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'deleteContentBackward' }));
        await window.playbit.input(14, 1, qualificationText.encodeTextInput('', null));
        const samples = [], admissions = [];
        for (let i = 0; i < 1000; i++) {
            const desired = !checkbox.checked, start = performance.now();
            await window.playbit.input(2, target, new Uint8Array());
            admissions.push(performance.now() - start);
            await waitCommitted(() => checkbox.checked === desired && !checkbox.disabled);
            document.querySelector('main').getBoundingClientRect();
            samples.push(performance.now() - start);
            if(globalThis.waitQualification)await waitQualification();
        }
        admissions.sort((a,b) => a-b);
        samples.sort((a,b) => a-b);
        return { retainedIdentitySelectionFocus: true, font: getComputedStyle(field).fontFamily,
            fieldHeight: field.getBoundingClientRect().height, buttonHeight: button.getBoundingClientRect().height,
            checks: Array.from(document.querySelectorAll('.guest-check input'), check => ({checked:check.checked, image:getComputedStyle(check).backgroundImage})),
            toggle: { admission: {p50:admissions[499],p95:admissions[949],p99:admissions[989],max:admissions.at(-1)}, n: samples.length, p50: samples[499], p95: samples[949], p99: samples[989], max: samples.at(-1),
                boundary: 'JS input dispatch to committed visible state and forced layout; admission ACK reported separately; excludes native event receipt, paint and physical presentation',
                workload: '3 rows, one local offline session, headless Chrome; not device or concurrent-work qualification' } };
    })()`);
    await screenshot(todo, "todo-before-restart.png");
    report.largerControls = await todo.evaluate(`(async () => {
        const reports = [];
        for (const rows of [32, 64]) {
            let count = document.querySelectorAll('.guest-check').length;
            while (count < rows) {
                const before=count;
                await window.playbit.input(3, 1, new TextEncoder().encode('Capacity task ' + count));
                await waitCommitted(() => document.querySelectorAll('.guest-check').length === before + 1);
                if(globalThis.waitQualification)await waitQualification();
                count=document.querySelectorAll('.guest-check').length;
            }
            if (document.querySelectorAll('.guest-check').length !== rows) throw new Error('Creation count mismatch');
            const checkbox = document.querySelector('.guest-check input');
            const target = Number(checkbox.dataset.control), samples = [], admissions = [], phases = [];
            for (let i = 0; i < 1000; i++) {
                const desired=!checkbox.checked, start=performance.now();
                const result=await window.playbit.input(2,target,new Uint8Array()), admitted=performance.now();
                admissions.push(admitted-start);
                await waitCommitted(() => checkbox.checked===desired && !checkbox.disabled);
                const patched=performance.now(); document.querySelector('main').getBoundingClientRect();
                const end=performance.now(); samples.push(end-start);
                const measured=result.qualification ?? (globalThis.waitQualification ? await waitQualification() : undefined);
                if (measured) {
                    if(measured.failed)throw new Error('A measured input failed; it was not replayed');
                    phases.push({...measured,admission:admitted-start,committedPatch:patched-start,layout:end-patched});
                }
            }
            admissions.sort((a,b) => a-b);
            samples.sort((a,b) => a-b);
            reports.push({ rows, n:1000, p50:samples[499], p95:samples[949], p99:samples[989], max:samples.at(-1),
                admission:{p50:admissions[499],p95:admissions[949],p99:admissions[989],max:admissions.at(-1)}, phases });
        }
        const field = document.querySelector('.guest-field'); field.value = 'Retained at capacity';
        field.dispatchEvent(new InputEvent('input', {bubbles:true, inputType:'insertText', data:field.value}));
        await window.playbit.input(14, 1, qualificationText.encodeTextInput(field.value, null));
        if (!document.querySelector('.guest-button').disabled || field.disabled) throw new Error('Capacity controls incorrect');
        await window.playbit.input(3, 1, new TextEncoder().encode(field.value));
        if (field.value !== 'Retained at capacity' || document.querySelectorAll('.guest-check').length !== 64) throw new Error('Capacity discarded draft or added a row');
        return { reports, capacityRetainsDraft:true, boundary:'JS dispatch to committed visible state and forced layout, separate admission ACK; one local session, no concurrent work' };
    })()`);
    // The untimed selection/focus check toggles the first sorted row once.
    // Random object IDs can place any title first; retain the actual final cut.
    const checkboxSnapshot = `Array.from(document.querySelectorAll('.guest-check'), label => ({title:label.innerText,checked:label.querySelector('input').checked})).sort((a,b)=>a.title.localeCompare(b.title))`;
    const checksBeforeRestart = await todo.evaluate(checkboxSnapshot);
    todo.tab.close(); await stop();
    await launch();
    todo = await todoPage("release-todo");
    assert.deepEqual(todo.opened.identity, report.initial.identity);
    await waitFor(todo, "document.querySelectorAll('.guest-check').length===64 && Array.from(document.querySelectorAll('.guest-check input')).every(check=>!check.disabled)");
    assert.deepEqual(await todo.evaluate(checkboxSnapshot), checksBeforeRestart);
    report.restart = { identity: todo.opened.identity, timing: todo.opened.timing, storageBytes: todo.opened.storageBytes, allRowsAndStatesRetained: true };
    await screenshot(todo, "todo-after-restart.png");
    report.checkboxFailure = await checkboxFailureCheck();
    report.siblingReconciliation = await siblingReconciliationCheck();
    const kart = await page(`${origin}/?app=kart&profile=release-kart`);
    await kart.evaluate("document.querySelector('canvas').focus()");
    const before = await kart.evaluate("document.querySelector('canvas').toDataURL()");
    await key(kart, "ArrowUp", "keyDown"); await key(kart, "ArrowRight", "keyDown");
    await delay(600);
    await key(kart, "ArrowRight", "keyUp"); await key(kart, "ArrowUp", "keyUp");
    const after = await kart.evaluate("document.querySelector('canvas').toDataURL()");
    assert.notEqual(before, after);
    const position = await kart.evaluate("(()=>{const r=document.querySelector('canvas').getBoundingClientRect();return{x:r.x+80,y:r.y+160}})()");
    await kart.tab.call("Input.dispatchMouseEvent", { type: "mousePressed", ...position, button: "left", buttons: 1, clickCount: 1 });
    await kart.tab.call("Input.dispatchMouseEvent", { type: "mouseMoved", x: position.x + 120, y: position.y, button: "left", buttons: 1 });
    await delay(200);
    await kart.tab.call("Input.dispatchMouseEvent", { type: "mouseReleased", x: position.x + 120, y: position.y, button: "left", buttons: 0, clickCount: 1 });
    report.kart = { keyboardChangedPixels: true, pointerChangedPixels: (await kart.evaluate("document.querySelector('canvas').toDataURL()")) !== after,
        state: await kart.evaluate("document.querySelector('[data-speed]').innerText"), timing: kart.opened.timing };
    assert.equal(report.kart.pointerChangedPixels, true);
    assert(!report.kart.state.includes("Distance 0"));
    await screenshot(kart, "micro-kart.png");
    for (const [filename, artifact] of Object.entries(report.artifacts)) {
        assert.equal(createHash("sha256").update(await readFile(join(output, filename))).digest("hex"), artifact.sha256,
            `Build artifact changed during qualification: ${filename}`);
    }
    finishQualification(report);
} catch (error) {
    if (report.functionalStatus !== "passed") { report.functionalStatus = "failed"; report.status = "functional_failed"; }
    report.error = String(error.stack ?? error); process.exitCode = 1;
}
finally { await stop().catch(() => chrome?.kill()); server.close(); await writeFile(join(evidence, "result.json"), JSON.stringify(report, null, 4)); console.log(JSON.stringify(report, null, 4)); }
