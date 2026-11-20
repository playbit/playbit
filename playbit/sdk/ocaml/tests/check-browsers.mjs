// Run the same trusted-module harness in installed browsers. Evidence goes to stdout.
import { createServer } from 'node:http';
import { readFile, mkdtemp, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import net from 'node:net';

const root = fileURLToPath(new URL('../../../../', import.meta.url));
const deadline = (promise, ms, reason) => {
    let timer;
    return Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(Error(reason)), ms); })]).finally(() => clearTimeout(timer));
};
const files = {
    '/harness.js': ['text/javascript', 'playbit/sdk/ocaml/_build/browser.js'],
    '/counter.wasm': ['application/wasm', 'playbit/sdk/ocaml/_build/wasm/debug/counter.wasm'],
    '/host.wasm': ['application/wasm', 'target/wasm32-unknown-unknown/debug/playbit.wasm'],
};
const server = createServer(async (request, response) => {
    try {
        if (request.url === '/') { response.setHeader('Content-Type', 'text/html'); response.end('<!doctype html><title>OCaml Playbit compatibility</title><script type="module" src="/harness.js"></script>'); return; }
        const file = files[request.url]; if (!file) { response.writeHead(404).end(); return; }
        response.setHeader('Content-Type', file[0]); response.end(await readFile(path.join(root, file[1])));
    } catch { response.writeHead(500).end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const url = `http://127.0.0.1:${server.address().port}/`;
async function stop(child) {
    if (!child || child.exitCode !== null) return;
    const ended = new Promise(resolve => child.once('exit', resolve)); child.kill('SIGTERM');
    try { await deadline(ended, 5000, 'browser shutdown timed out'); }
    catch { child.kill('SIGKILL'); await ended; }
}
async function chromium() {
    const profile = await mkdtemp(path.join(tmpdir(), 'ocaml-chrome-'));
    let child, socket;
    try {
        child = spawn(process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
            ['--headless=new', '--no-first-run', '--no-default-browser-check', '--disable-background-networking', '--remote-debugging-port=0', `--user-data-dir=${profile}`, 'about:blank'], { stdio: ['ignore','ignore','pipe'] });
        let stderr = '';
        const endpoint = await deadline(new Promise((resolve, reject) => {
            child.once('error', reject); child.once('exit', () => reject(Error(stderr || 'Chrome exited')));
            child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-8192); const match = stderr.match(/DevTools listening on (ws:\/\/\S+)/); if (match) resolve(match[1]); });
        }), 20000, 'Chrome startup timed out');
        socket = new WebSocket(endpoint); let sequence = 0; const pending = new Map();
        socket.addEventListener('message', event => { const message=JSON.parse(event.data), waiter=pending.get(message.id); if (!waiter) return; pending.delete(message.id); message.error ? waiter.reject(Error(message.error.message)) : waiter.resolve(message.result); });
        await deadline(new Promise((resolve,reject) => { socket.addEventListener('open',resolve,{once:true}); socket.addEventListener('error',reject,{once:true}); }),10000,'Chrome connection timed out');
        const send=(method,params={},sessionId)=>new Promise((resolve,reject)=>{const id=++sequence;pending.set(id,{resolve,reject});socket.send(JSON.stringify({id,method,params,...(sessionId?{sessionId}:{})}));});
        const {targetId}=await send('Target.createTarget',{url:'about:blank'});
        const {sessionId}=await send('Target.attachToTarget',{targetId,flatten:true});
        await send('Page.enable',{},sessionId);
        const loaded=new Promise(resolve=>{const listener=event=>{const message=JSON.parse(event.data);if(message.sessionId===sessionId&&message.method==='Page.loadEventFired'){socket.removeEventListener('message',listener);resolve();}};socket.addEventListener('message',listener);});
        await send('Page.navigate',{url},sessionId);await deadline(loaded,10000,'Harness load timed out');
        const result=await deadline(send('Runtime.evaluate',{expression:'window.testDone',awaitPromise:true,returnByValue:true},sessionId),60000,'Harness timed out');
        if(result.exceptionDetails)throw Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
        return result.result.value;
    } finally {socket?.close();await stop(child);await rm(profile,{recursive:true,force:true,maxRetries:5,retryDelay:100});}
}
async function webkit() {
    const child=spawn(path.join(root,'playbit/sdk/ocaml/_build/check-webkit'),[url],{stdio:['ignore','pipe','pipe']});
    let output='',error='';
    child.stdout.on('data',chunk=>{output=(output+chunk).slice(-32768);});
    child.stderr.on('data',chunk=>{error=(error+chunk).slice(-8192);});
    try {
        const code=await deadline(new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',resolve);}),65000,'WebKit harness timed out');
        if(code!==0)throw Error(error||output);return JSON.parse(output);
    } finally {await stop(child);}
}
async function freePort() { const socket=net.createServer();await new Promise(resolve=>socket.listen(0,'127.0.0.1',resolve));const port=socket.address().port;await new Promise(resolve=>socket.close(resolve));return port; }
async function webdriver(browser) {
    const port=await freePort(); let session, child, output='';
    const base=`http://127.0.0.1:${port}`;
    const request=async(method,route,value)=>{const result=await fetch(base+route,{method,headers:{'Content-Type':'application/json'},...(value===undefined?{}:{body:JSON.stringify(value)})});const json=await result.json();if(!result.ok||json.value?.error)throw Error(json.value?.message??JSON.stringify(json));return json.value;};
    try {
        const executable=browser==='firefox'?path.join(root,'playbit/sdk/ocaml/_build/geckodriver'):'/usr/bin/safaridriver';
        child=spawn(executable,browser==='firefox'?['--port',String(port),'--host','127.0.0.1']:['--port',String(port)],{stdio:['ignore','pipe','pipe']});
        child.stdout.on('data',chunk=>{output=(output+chunk).slice(-8192);});child.stderr.on('data',chunk=>{output=(output+chunk).slice(-8192);});
        await deadline(new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',()=>reject(Error(output||'driver exited')));let attempts=0;const connect=()=>{const socket=net.createConnection(port,'127.0.0.1');socket.once('connect',()=>{socket.end();resolve();});socket.once('error',()=>{socket.destroy();if(++attempts===100)reject(Error(output||'driver unavailable'));else setTimeout(connect,50);});};connect();}),10000,'Driver startup timed out');
        const capabilities={browserName:browser,...(browser==='firefox'?{'moz:firefoxOptions':{binary:'/Applications/Firefox.app/Contents/MacOS/firefox',args:['-headless']}}:{})};
        const created=await deadline(request('POST','/session',{capabilities:{alwaysMatch:capabilities}}),30000,'Browser session timed out');session=created.sessionId;
        await request('POST',`/session/${session}/timeouts`,{script:60000,pageLoad:30000});
        await request('POST',`/session/${session}/url`,{url});
        const result=await deadline(request('POST',`/session/${session}/execute/async`,{script:'const done=arguments[arguments.length-1]; Promise.resolve(window.testDone).then(value=>done({value}),error=>done({error:String(error)}));',args:[]}),65000,'Harness timed out');
        if(result.error)throw Error(result.error);if(!result.value?.checks)throw Error('No harness results');return {version:created.capabilities.browserVersion,...result.value};
    } finally {if(session)await request('DELETE',`/session/${session}`).catch(()=>{});await stop(child);}
}
try {
    const results={};
    for(const browser of process.argv.length>2?process.argv.slice(2):['chromium','firefox','webkit']) {
        try {results[browser]=await (browser==='chromium'?chromium():browser==='webkit'?webkit():webdriver(browser));}
        catch(error){results[browser]={error:String(error)};process.exitCode=1;}
        console.log(JSON.stringify({browser,result:results[browser]}));
    }
} finally {await new Promise(resolve=>server.close(resolve));}
