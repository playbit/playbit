// Run against an already joined TODO in an isolated, debug-enabled Chrome profile.
// This measures API entry through guest completion and synchronous DOM readiness.
// It does not measure native input dispatch or physical display presentation.
const [debugOrigin, pageOrigin] = process.argv.slice(2);
if (!debugOrigin || !pageOrigin) throw new Error('Usage: node todo-latency.mjs DEBUG_ORIGIN PAGE_ORIGIN');
const pages = await (await fetch(new URL('/json/list', debugOrigin))).json();
const matching = pages.filter(page => page.type === 'page' && new URL(page.url).origin === new URL(pageOrigin).origin);
if (matching.length !== 1) throw new Error('Open exactly one matching TODO tab in the isolated web profile');
const socket = new WebSocket(matching[0].webSocketDebuggerUrl);
await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
let nextId = 0;
const pending = new Map();
socket.onmessage = ({ data }) => {
    const message = JSON.parse(data), request = pending.get(message.id);
    if (!request) return;
    pending.delete(message.id); clearTimeout(request.timer);
    if (message.error) request.reject(new Error(message.error.message)); else request.resolve(message.result);
};
socket.onclose = () => {
    for (const request of pending.values()) { clearTimeout(request.timer); request.reject(new Error('Web browser debugging connection closed')); }
    pending.clear();
};
const call = (method, params) => new Promise((resolve, reject) => {
    const id = ++nextId;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error('Web browser benchmark deadline exceeded')); }, 120000);
    pending.set(id, { resolve, reject, timer }); socket.send(JSON.stringify({ id, method, params }));
});
try {
    const response = await call('Runtime.evaluate', { awaitPromise: true, returnByValue: true, expression: `
        (async () => {
            const check = document.querySelector('#app input[type=checkbox]');
            if (!check || !globalThis.playbit?.input) throw new Error('Join a real TODO before measuring');
            const rows = document.querySelectorAll('#app input[type=checkbox]').length, samples = [];
            for (let i = 0; i < 1000; i++) {
                if (document.querySelector('#app input[type=checkbox]') !== check) throw new Error('Native checkbox identity changed');
                const before = check.checked, started = performance.now();
                await playbit.input(2, Number(check.dataset.control), new Uint8Array());
                samples.push(performance.now() - started);
                if (check.checked === before) throw new Error('Committed checkbox state did not change');
            }
            const sorted = samples.slice().sort((a, b) => a - b);
            return { boundary: 'input API entry to guest completion and DOM readiness; excludes native input dispatch and physical presentation',
                rows, count: samples.length, p50_ms: sorted[499], p95_ms: sorted[949], p99_ms: sorted[989], max_ms: sorted[999], samples_ms: samples };
        })()` });
    if (response.exceptionDetails) throw new Error(response.exceptionDetails.exception?.description ?? response.exceptionDetails.text);
    console.log(JSON.stringify(response.result.value));
} finally { socket.close(); }
