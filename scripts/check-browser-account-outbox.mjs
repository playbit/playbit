// Real Chromium IndexedDB/WebCrypto regression, without npm dependencies.
// Run after playbit/platform/web/build.sh. CHROME may select the installed executable.
import { createServer } from 'node:http';
import { readFile, mkdtemp, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { linuxChromeStartupSnapshot } from './chrome-startup-diagnostics.mjs';

if (process.argv.length > 3 || (process.argv.length === 3 && !['--benchmark', '--format-only'].includes(process.argv[2]))) {
    throw Error('usage: node scripts/check-browser-account-outbox.mjs [--benchmark|--format-only]');
}
const benchmark = process.argv[2] === '--benchmark';
const repo = fileURLToPath(new URL('../', import.meta.url));
const profile = await mkdtemp(path.join(tmpdir(), 'playbit-outbox-browser-'));
const chrome = process.env.CHROME ?? (process.platform === 'darwin'
    ? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' : '/usr/bin/google-chrome');
let child, socket, childExited, sequence = 0;
const pending = new Map();
const page = `<!doctype html><title>Account outbox regression</title><script>
window.testDone = new Promise((resolve, reject) => {
    const worker = new Worker('/tests/account-outbox-browser.js', {type:'module'});
    let result, error;
    worker.onmessage = ({data}) => {
        if (data.result) result = data.result;
        if (data.error) error = data.error;
        if (data.cleaned) error ? reject(Error(error)) : resolve(result);
    };
    worker.onerror = event => reject(Error(event.message));
    worker.postMessage(${JSON.stringify(benchmark ? {benchmark:true,samples:1000} : process.argv[2] === '--format-only' ? {storageFormatCheck:true} : {})});
});</script>`;
const server = createServer(async (request, response) => {
    try {
        const pathname = new URL(request.url, 'http://localhost').pathname;
        if (pathname === '/') { response.setHeader('Content-Type', 'text/html'); response.end(page); return; }
        const root = path.join(repo, 'target/web');
        const filename = path.resolve(root, `.${decodeURIComponent(pathname)}`);
        if (!filename.startsWith(root + path.sep) || !/\.(js|wasm)$/.test(filename)) { response.writeHead(404).end(); return; }
        response.setHeader('Content-Type', filename.endsWith('.wasm') ? 'application/wasm' : 'text/javascript');
        response.end(await readFile(filename));
    } catch { response.writeHead(404).end(); }
});
const deadline = (promise, ms, message) => {
    let timer;
    return Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(Error(message)), ms); })]).finally(() => clearTimeout(timer));
};
try {
    await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
    const url = `http://127.0.0.1:${server.address().port}/`;
    const launchStarted = performance.now();
    let spawnedMs = null, firstStderrMs = null;
    child = spawn(chrome, ['--headless=new', '--no-first-run', '--no-default-browser-check', '--disable-background-networking',
        '--remote-debugging-port=0', `--user-data-dir=${profile}`, 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] });
    child.once('spawn', () => { spawnedMs = performance.now() - launchStarted; });
    childExited = new Promise(resolve => child.once('exit', resolve));
    let stderr = '';
    const endpoint = await deadline(new Promise((resolve, reject) => {
        child.once('error', reject); child.once('exit', () => reject(Error('Chrome exited before debugging was ready')));
        child.stderr.on('data', chunk => {
            firstStderrMs ??= performance.now() - launchStarted;
            stderr = (stderr + chunk.toString()).slice(-8192);
            const match = stderr.match(/DevTools listening on (ws:\/\/\S+)/);
            if (match) resolve(match[1]);
        });
    }), 30000, 'Chrome did not start').catch(async error => {
        let diagnostic = '';
        if (process.platform === 'linux') {
            const timing = { elapsed_ms: performance.now() - launchStarted, spawned_ms: spawnedMs,
                first_stderr_ms: firstStderrMs, exit_code: child.exitCode, signal: child.signalCode };
            // Observe only this failed launch; do not retry Chrome or extend readiness.
            const snapshot = await deadline(linuxChromeStartupSnapshot(child.pid), 1500, 'Startup diagnostics timed out')
                .catch(() => ({ unavailable: true }));
            let record = JSON.stringify({ ...timing, snapshot });
            if (Buffer.byteLength(record) > 32768) record = JSON.stringify({ ...timing, snapshot: { unavailable: 'Diagnostic size limit' } });
            diagnostic = '\nChrome startup diagnostics: ' + record;
        }
        throw Error(`${error.message}\n${stderr}${diagnostic}`, { cause: error });
    });
    socket = new WebSocket(endpoint);
    socket.addEventListener('message', event => {
        const message = JSON.parse(event.data), waiter = pending.get(message.id);
        if (!waiter) return;
        pending.delete(message.id);
        message.error ? waiter.reject(Error(message.error.message)) : waiter.resolve(message.result);
    });
    socket.addEventListener('close', () => { for (const waiter of pending.values()) waiter.reject(Error('Browser connection closed')); pending.clear(); });
    await deadline(new Promise((resolve, reject) => { socket.addEventListener('open', resolve, { once: true }); socket.addEventListener('error', reject, { once: true }); }), 10000, 'Browser connection timed out');
    const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
        const id = ++sequence; pending.set(id, { resolve, reject }); socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    });
    const { targetId } = await send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await send('Target.attachToTarget', { targetId, flatten: true });
    await send('Page.enable', {}, sessionId);
    // Await the load event rather than polling page readiness.
    const loaded = new Promise(resolve => {
        const listener = event => { const message = JSON.parse(event.data); if (message.sessionId === sessionId && message.method === 'Page.loadEventFired') { socket.removeEventListener('message', listener); resolve(); } };
        socket.addEventListener('message', listener);
    });
    await send('Page.navigate', { url }, sessionId);
    await deadline(loaded, 10000, 'Test page did not load');
    const result = await deadline(send('Runtime.evaluate', { expression: 'window.testDone', awaitPromise: true, returnByValue: true }, sessionId), 60000, 'Account outbox test timed out');
    if (result.exceptionDetails) throw Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
    if (!result.result?.value?.checks) throw Error('Browser returned no account outbox evidence');
    console.log(JSON.stringify(result.result.value));
} finally {
    socket?.close();
    if (child && child.exitCode === null) {
        child.kill('SIGTERM');
        try { await deadline(childExited, 5000, 'Chrome shutdown timed out'); }
        catch { child.kill('SIGKILL'); await childExited; }
    }
    await new Promise(resolve => server.close(resolve));
    // Chromium's helper processes can finish writing after the main process
    // exits. Bound cleanup retries; a persistent failure still fails the check.
    await rm(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
