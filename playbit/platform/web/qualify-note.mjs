// Real Chrome/OPFS qualification; no web browser automation package is required.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { readFile, writeFile, mkdir, mkdtemp } from "node:fs/promises";
import { resolve, join, extname } from "node:path";
import { once } from "node:events";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";

const output = resolve("target/web"), evidence = await mkdtemp(resolve("target/shared-note-evidence-"));
const webBrowserProfile = join(evidence, "chrome");
await mkdir(webBrowserProfile);
const server = createServer(async (request, response) => {
    try {
        const path = new URL(request.url, "http://localhost").pathname;
        if (!/^\/[a-zA-Z0-9_.-]*$/.test(path)) throw new Error("Invalid path");
        const file = join(output, path === "/" ? "index.html" : path);
        response.setHeader("Content-Type", ({ ".js": "text/javascript", ".wasm": "application/wasm", ".html": "text/html", ".json": "application/json", ".svg": "image/svg+xml", ".ttf": "font/ttf" })[extname(file)] ?? "application/octet-stream");
        response.end(await readFile(file));
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
async function page(url) {
    const { targetId } = await browser.call("Target.createTarget", { url: "about:blank" });
    const target = (await (await fetch(`http://${debugging.host}/json/list`)).json()).find(item => item.id === targetId);
    const tab = await cdp(target.webSocketDebuggerUrl);
    await tab.call("Page.enable"); await tab.call("Runtime.enable");
    await tab.call("Emulation.setDeviceMetricsOverride", { width: 800, height: 900, deviceScaleFactor: 1, mobile: false });
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
const report = { origin, evidence, status: "running" };
report.artifacts = {};
for (const filename of ["target/web/shared-note.pbapp", "target/web/playbit.wasm", "target/web/playbit.js", "target/web/runtime-worker.js"]) {
    const bytes = await readFile(resolve(filename));
    report.artifacts[filename] = { bytes: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") };
}
try {
    await launch();
    let note = await page(`${origin}/?app=note&profile=shared-note`);
    report.jspi = await note.evaluate("({suspending:typeof WebAssembly.Suspending,promising:typeof WebAssembly.promising})");
    assert.deepEqual(report.jspi,{suspending:"function",promising:"function"});
    report.initial = note.opened.identity;
    await waitFor(note, "Boolean(document.querySelector('main input[type=text]:not([readonly]):not(:disabled)'))");
    await note.evaluate("document.querySelector('main input[type=text]').focus()");
    await note.tab.call("Input.insertText", { text: "Web encrypted-file fixture" });
    await waitFor(note,"document.querySelector('main input[type=text]').value==='Web encrypted-file fixture'");
    await key(note,"Enter","keyDown"); await key(note,"Enter","keyUp");
    await waitFor(note,"Array.from(document.querySelectorAll('main p')).some(node=>node.textContent==='Web encrypted-file fixture')");
    await waitFor(note,"document.querySelector('main').textContent.includes('26 bytes')");
    await screenshot(note,"shared-note-saved.png");
    const files = await note.evaluate(`(async()=>{const root=await navigator.storage.getDirectory();const dir=await root.getDirectoryHandle('playbit-shared-note');const names=[];for await(const name of dir.keys())names.push(name);return names})()`);
    assert.equal(files.filter(name=>/^blob-[0-9a-f]{64}$/.test(name)).length,2,
        'Retain both the independent empty seed and the saved content revision');
    assert(note.opened.identity.shared && note.opened.identity.shared !== note.opened.identity.root);
    const other = await page(`${origin}/?app=note&profile=independent-note`);
    assert.equal(other.opened.identity.root, note.opened.identity.root);
    assert.notEqual(other.opened.identity.shared, note.opened.identity.shared);
    assert.equal(await other.evaluate("document.querySelector('main input[type=text]').value"), '');
    other.tab.close();
    report.files = files;
    note.tab.close(); await stop();
    await launch();
    note = await page(`${origin}/?app=note&profile=shared-note`);
    await waitFor(note,"Array.from(document.querySelectorAll('main p')).some(node=>node.textContent==='Web encrypted-file fixture')");
    assert.deepEqual(note.opened.identity,report.initial);
    assert.equal(await note.evaluate("document.querySelector('main').textContent.includes('Could not')"),false);
    await screenshot(note,"shared-note-reopened.png");
    report.restored = note.opened.identity;
    report.status = "passed";
    note.tab.close();
} catch (error) { report.status = "failed"; report.error = String(error.stack ?? error); process.exitCode = 1; }
finally { await stop().catch(() => chrome?.kill()); server.close(); await writeFile(join(evidence, "result.json"), JSON.stringify(report, null, 4)); console.log(JSON.stringify(report, null, 4)); }
