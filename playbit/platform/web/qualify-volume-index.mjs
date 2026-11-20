// Actual packaged UI/OPFS evidence. Uses an isolated Chrome profile and CDP.
// The input boundary excludes OS input delivery, paint and physical presentation.
import { spawn } from 'node:child_process';
import { readFile, writeFile, mkdtemp } from 'node:fs/promises';
import { resolve, join } from 'node:path';
import { once } from 'node:events';
import { createHash } from 'node:crypto';
import assert from 'node:assert/strict';
const evidence = await mkdtemp(resolve('target/volume-index-web-evidence-'));
const origin = process.argv[2] ?? 'http://127.0.0.1:8787';
const report = { status: 'running', evidence, origin, artifacts: {}, inputBoundary: 'JS input admission through worker response, DOM patch and forced layout; no OS event, paint or physical presentation' };
const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
async function cdp(url) {
    const socket = new WebSocket(url); await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
    const pending = new Map(); let sequence = 0;
    socket.onmessage = ({ data }) => { const message = JSON.parse(data), request = pending.get(message.id); if (!request) return; pending.delete(message.id); if (message.error) request.reject(new Error(message.error.message)); else request.resolve(message.result); };
    return { close: () => socket.close(), call: (method, params = {}) => new Promise((resolve, reject) => { const id = ++sequence; pending.set(id, { resolve, reject }); socket.send(JSON.stringify({ id, method, params })); }) };
}
let chrome, browser, debugging;
async function launch() {
    chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new', '--no-first-run', '--no-default-browser-check', '--disable-background-timer-throttling', '--disable-renderer-backgrounding', `--user-data-dir=${join(evidence, 'chrome')}`, '--remote-debugging-port=0', 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] });
    let stderr = ''; chrome.stderr.on('data', bytes => { stderr += bytes; });
    for (let attempt = 0; attempt < 100; attempt++) {
        const match = stderr.match(/DevTools listening on (ws:\/\/[^\s]+)/);
        if (match) { debugging = new URL(match[1]); browser = await cdp(match[1]); return; }
        if (chrome.exitCode !== null) throw new Error(`Chrome exited: ${stderr}`); await delay(100);
    }
    throw new Error('Isolated Chrome debugger unavailable');
}
async function stop() {
    if (!chrome || chrome.exitCode !== null) return;
    const exited = once(chrome, 'exit'); await browser.call('Browser.close'); browser.close(); await exited;
}
async function open(app = 'todo', profile = 'volume-index-qualification') {
    const { targetId } = await browser.call('Target.createTarget', { url: 'about:blank' });
    const target = (await (await fetch(`http://${debugging.host}/json/list`)).json()).find(item => item.id === targetId);
    const tab = await cdp(target.webSocketDebuggerUrl); await tab.call('Page.enable'); await tab.call('Runtime.enable');
    await tab.call('Emulation.setDeviceMetricsOverride', { width: 1024, height: 768, deviceScaleFactor: 1, mobile: false });
    const evaluate = async expression => { const result = await tab.call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }); if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text); return result.result.value; };
    await tab.call('Page.navigate', { url: `${origin}/?app=${app}&profile=${profile}` });
    for (let attempt = 0; attempt < 100; attempt++) { if (await evaluate('Boolean(window.playbit)')) break; await delay(50); }
    const opened = await evaluate('window.playbit.ready'); assert(opened?.identity, JSON.stringify(await evaluate('document.body.innerText')));
    return { tab, evaluate, opened };
}
async function screenshot(page, name) { const result = await page.tab.call('Page.captureScreenshot', { format: 'png' }); await writeFile(join(evidence, name), Buffer.from(result.data, 'base64')); }
const opening = value => ({ identity: value.identity, timing: value.timing });
try {
    for (const filename of ['playbit.wasm', 'todo.pbapp', 'playbit.js', 'runtime-worker.js']) { const bytes = await readFile(resolve('target/web', filename)); report.artifacts[filename] = { bytes: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex') }; }
    await launch(); let page = await open(); report.initial = opening(page.opened);
    await screenshot(page, 'todo-initial.png');
    report.workloads = await page.evaluate(`(async () => {
        await document.fonts.ready;
        const reports = [];
        for (const rows of [1, 32, 64]) {
            while (document.querySelectorAll('.guest-check').length < rows) {
                const count = document.querySelectorAll('.guest-check').length;
                await window.playbit.input(3, 1, new TextEncoder().encode('Indexed task ' + count));
                if (document.querySelectorAll('.guest-check').length !== count + 1) throw new Error('Named database insertion did not render');
            }
            const target = Number(document.querySelector('.guest-check input').dataset.control), samples = [];
            for (let i = 0; i < 1000; i++) { const began = performance.now(); await window.playbit.input(2, target, new Uint8Array()); document.querySelector('main').getBoundingClientRect(); samples.push(performance.now() - began); }
            samples.sort((a, b) => a - b); reports.push({ rows, count: samples.length, p50_ms: samples[499], p95_ms: samples[949], p99_ms: samples[989], max_ms: samples.at(-1) });
        }
        return reports;
    })()`);
    await screenshot(page, 'todo-64-rows.png');
    report.recovery = await page.evaluate(`(async () => {
        const reports = [];
        for (const rows of [500, 10000]) {
            const worker = new Worker('/tests/opfs-recovery-browser.js', { type:'module' }), samples = [];
            let probe, sequence = 0, probing = false;
            const result = await new Promise((resolve, reject) => {
                worker.onerror = event => reject(new Error(event.message));
                const request = () => { probe = { id:++sequence, start:performance.now() }; worker.postMessage({probe:probe.id}); };
                worker.onmessage = ({data}) => {
                    if (data.error) { reject(new Error(data.error)); return; }
                    if (data.opening === rows && data.probe === undefined) { probing = true; request(); return; }
                    if (data.done) { probing = false; resolve(data.done); return; }
                    if (probe && data.probe === probe.id) {
                        if (data.opening) samples.push(performance.now() - probe.start);
                        if (probing) request();
                    }
                };
                worker.postMessage({rows});
            });
            worker.postMessage({close:true}); samples.sort((a,b) => a-b);
            if (!samples.length) throw new Error('No same-worker probes overlapped recovery');
            result.readiness = { n:samples.length, p99_ms:samples[Math.ceil(samples.length * .99)-1], max_ms:samples.at(-1) };
            reports.push(result);
        }
        return reports;
    })()`);
    const kart = await open('kart', 'volume-index-concurrent-kart');
    await kart.evaluate(`(() => {
        window.pbConcurrentStop = false; window.pbConcurrentFrames = 0;
        window.pbConcurrent = (async () => {
            const target = Number(document.querySelector('main canvas').dataset.control);
            const frame = new Uint8Array(4); new DataView(frame.buffer).setUint32(0, 16667, true);
            while (!window.pbConcurrentStop) {
                await window.playbit.input(13, target, frame);
                document.querySelector('main').getBoundingClientRect(); window.pbConcurrentFrames++;
                await new Promise(resolve => setTimeout(resolve, 16));
            }
        })();
    })()`);
    const concurrent = await page.evaluate(`(async () => {
        const target = Number(document.querySelector('.guest-check input').dataset.control), samples = [];
        for (let i = 0; i < 1000; i++) { const began = performance.now(); await window.playbit.input(2, target, new Uint8Array()); document.querySelector('main').getBoundingClientRect(); samples.push(performance.now() - began); }
        samples.sort((a, b) => a - b);
        return { rows: 64, count: samples.length, p50_ms: samples[499], p95_ms: samples[949], p99_ms: samples[989], max_ms: samples.at(-1) };
    })()`);
    await kart.evaluate('window.pbConcurrentStop = true; window.pbConcurrent');
    concurrent.kartFrames = await kart.evaluate('window.pbConcurrentFrames');
    assert(concurrent.kartFrames > 0, 'Concurrent app executed frames during the input cohort');
    report.concurrent = { ...concurrent, workload: 'Independent packaged Micro Kart worker at nominal 60 Hz, including DOM/layout; web browser background timer throttling disabled' };
    await screenshot(kart, 'concurrent-micro-kart.png'); await kart.evaluate('window.playbit.close()'); kart.tab.close();
    report.files = await page.evaluate(`(async () => {
        const root = await navigator.storage.getDirectory(), directory = await root.getDirectoryHandle('playbit-volume-index-qualification'), names = [];
        for await (const [name, entry] of directory.entries()) {
            names.push(name);
            if (entry.kind === 'directory') for await (const child of entry.keys()) names.push(name + '/' + child);
        }
        return names;
    })()`);
    assert(report.files.some(name => /^[0-9a-f]{32}-[0-9a-f]{32}\/[0-9a-f]{30}\.kvdb$/.test(name)), 'Generation directories contain stable named entry identities');
    await page.evaluate('window.playbit.close()'); page.tab.close(); await stop();
    await launch(); page = await open(); report.reopened = opening(page.opened);
    assert.deepEqual(page.opened.identity, report.initial.identity);
    assert.equal(await page.evaluate("document.querySelectorAll('.guest-check').length"), 64);
    await screenshot(page, 'todo-reopened.png'); await page.evaluate('window.playbit.close()'); page.tab.close();
    report.status = 'passed';
} catch (error) { report.status = 'failed'; report.error = String(error.stack ?? error); process.exitCode = 1; }
finally { await stop().catch(() => chrome?.kill()); await writeFile(join(evidence, 'result.json'), JSON.stringify(report, null, 4)); console.log(JSON.stringify(report, null, 4)); }
