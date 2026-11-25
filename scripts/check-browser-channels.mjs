import assert from 'node:assert/strict';
import {createServer} from 'node:https';
import {readFile,writeFile,mkdtemp,rm} from 'node:fs/promises';
import {spawn,execFileSync} from 'node:child_process';
import {createHash,X509Certificate} from 'node:crypto';
// Operator-only real-browser check. Build the website test binary and stage
// verified browser assets first; all profiles, certificates and origins are local.
const [binary,content,output]=process.argv.slice(2);
if(!output) throw Error('usage: node scripts/check-browser-channels.mjs TEST_BINARY CONTENT EVIDENCE');
const temporary=await mkdtemp('/tmp/playbit-browser-channel-');
const deadline=(promise,ms,label)=>{let timer;return Promise.race([promise,new Promise((_,reject)=>timer=setTimeout(()=>reject(Error(label)),ms))]).finally(()=>clearTimeout(timer));};
let fixture,chrome,socket,server,origins,sequence=0,passed=false;
const pending=new Map();
async function stop(child){if(!child||child.exitCode!==null||child.signalCode!==null)return;const exited=new Promise(r=>child.once('exit',r));child.kill('SIGTERM');try{await deadline(exited,5000,'shutdown');}catch{child.kill('SIGKILL');await exited;}}
try{
 fixture=spawn(binary,['-test.run','^TestQualifyBrowserChannels$','-test.v'],{env:{...process.env,PLAYBIT_WEBSITE_QUALIFY_CONTENT:content,PLAYBIT_WEBSITE_CHANNEL_BROWSER:'1'},stdio:['ignore','pipe','pipe']});
 const fixtureOutput=[];fixture.stderr.on('data',x=>fixtureOutput.push(x.toString()));
 origins=await deadline(new Promise((resolve,reject)=>{let text='';fixture.on('error',reject);fixture.on('exit',code=>reject(Error('fixture exited '+code+' '+text)));fixture.stdout.on('data',chunk=>{text+=chunk;fixtureOutput.push(chunk.toString());const match=text.match(/channel_browser_fixture (\{[^\n]+\})/);if(match)resolve(JSON.parse(match[1]));});}),15000,'fixture start');
 execFileSync('/usr/bin/openssl',['req','-x509','-newkey','rsa:2048','-nodes','-subj','/CN=playbit.app','-days','1','-keyout',temporary+'/key.pem','-out',temporary+'/cert.pem'],{stdio:'ignore',timeout:10000});
 const cert=await readFile(temporary+'/cert.pem'),key=await readFile(temporary+'/key.pem');
 const pin=createHash('sha256').update(new X509Certificate(cert).publicKey.export({type:'spki',format:'der'})).digest('base64');
 server=createServer({cert,key},async(req,res)=>{try{
 const selected=req.headers.host==='testing.playbit.app'?'testing':req.headers.host==='playbit.app'?'public':undefined;
 if(!selected){res.writeHead(403).end();return;}
 if(req.url.startsWith('/download/')){
 const channel=req.url==='/download/macos-testing.json'?'testing':'public';
 res.writeHead(200,{'Content-Type':'application/json','Access-Control-Allow-Origin':'https://testing.playbit.app','Cache-Control':'no-store'});
 res.end(JSON.stringify({channel,artifact:{platform:'macos-arm64',filename:'Playbit-macos-arm64.zip',sha256:'ab'.repeat(32),url:'https://blob.playbit.app/'+'1'.repeat(32),sequence:1,bytes:3}}));return;
 }
 const upstream=await fetch(origins[selected]+req.url,{redirect:'manual'});
 res.writeHead(upstream.status,Object.fromEntries(upstream.headers));
 if(upstream.body)for await(const chunk of upstream.body)res.write(chunk);
 res.end();
 }catch(e){res.writeHead(500).end(e.message);}});
 await new Promise(r=>server.listen(0,'127.0.0.1',r));
 const port=server.address().port;
 chrome=spawn(process.env.CHROME??(process.platform==='darwin'?'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome':'/usr/bin/google-chrome'),['--headless=new','--no-first-run','--no-default-browser-check','--disable-background-networking','--no-proxy-server','--remote-debugging-port=0','--user-data-dir='+temporary+'/profile','--ignore-certificate-errors-spki-list='+pin,`--host-resolver-rules=MAP playbit.app 127.0.0.1:${port}, MAP testing.playbit.app 127.0.0.1:${port}, MAP ap.playbit.app ~NOTFOUND, MAP ap.testing.playbit.app ~NOTFOUND`,'about:blank'],{stdio:['ignore','ignore','pipe']});
 const endpoint=await deadline(new Promise((resolve,reject)=>{let text='';chrome.on('error',reject);chrome.on('exit',code=>reject(Error('Chrome exited '+code)));chrome.stderr.on('data',chunk=>{text=(text+chunk).slice(-8192);const m=text.match(/DevTools listening on (ws:\/\/\S+)/);if(m)resolve(m[1]);});}),15000,'Chrome start');
 socket=new WebSocket(endpoint);
 socket.onmessage=({data})=>{const msg=JSON.parse(data),entry=pending.get(msg.id);if(entry){pending.delete(msg.id);msg.error?entry.reject(Error(msg.error.message)):entry.resolve(msg.result);}};
 await new Promise((r,j)=>{socket.onopen=r;socket.onerror=j;});
 const send=(method,params={},sessionId)=>deadline(new Promise((resolve,reject)=>{const id=++sequence;pending.set(id,{resolve,reject});socket.send(JSON.stringify({id,method,params,...(sessionId?{sessionId}:{})}));}),20000,method);
 const {targetId}=await send('Target.createTarget',{url:'about:blank'});
 const {sessionId}=await send('Target.attachToTarget',{targetId,flatten:true});
 const call=(method,params)=>send(method,params,sessionId);
 await call('Page.enable');await call('Emulation.setDeviceMetricsOverride',{width:1000,height:850,deviceScaleFactor:1,mobile:false});
 const evaluate=async expression=>{const r=await call('Runtime.evaluate',{expression,awaitPromise:true,returnByValue:true});if(r.exceptionDetails)throw Error(r.exceptionDetails.exception?.description??r.exceptionDetails.text);return r.result.value;};
 const loaded=()=>deadline(new Promise(resolve=>{const listener=event=>{const msg=JSON.parse(event.data);if(msg.sessionId===sessionId&&msg.method==='Page.loadEventFired'){socket.removeEventListener('message',listener);resolve();}};socket.addEventListener('message',listener);}),20000,'page load');
 const page=async(channel,viaSelector=false)=>{
 const ready=loaded();
 if(viaSelector)await evaluate(`document.querySelector('#environment').value=${JSON.stringify(channel)};document.querySelector('#environment').dispatchEvent(new Event('change'));true`);
 else await call('Page.navigate',{url:'https://'+(channel==='testing'?'testing.':'')+'playbit.app/s?qualification='+sequence+'#'+'1'.repeat(22)});
 await ready;
 await evaluate(`new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(Error('app readiness')),20000);const inspect=()=>{if(window.playbit){clearTimeout(timer);resolve(true);observer.disconnect();}};const observer=new MutationObserver(inspect);observer.observe(document.body,{subtree:true,childList:true,characterData:true});inspect();})`);
 await evaluate('window.playbit.ready');
 const value=await evaluate(`({origin:location.origin,channel:document.querySelector('#environment').value,badge:!document.querySelector('#testing').hidden,status:document.querySelector('#status').textContent,error:document.querySelector('#error').textContent,handoff:document.querySelector('#open-installed').getAttribute('href'),asset:document.querySelector('link[rel="modulepreload"][href="./playbit.js"]').href,install:!document.querySelector('#install-playbit').hidden,form:document.querySelector('#onboarding').textContent})`);
 assert.equal(value.channel,channel);assert.equal(value.badge,channel==='testing');assert.ok(value.form.includes('Email')||value.form.includes('Sign in')||value.form.includes('Join in web browser'),JSON.stringify(value));
 return value;};
 const report={boundary:'isolated Chrome HTTPS origins and staged Go shell; AP DNS unavailable; synthetic immutable installer feeds; no sign-in or live deployment',pages:[]};
 report.pages.push(await page('public'));assert.match(report.pages[0].handoff,/^playbit:session:public:/);
 await evaluate(`(async()=>{localStorage.setItem('channel-check','public');const root=await navigator.storage.getDirectory();const file=await root.getFileHandle('channel-check',{create:true});const writer=await file.createWritable();await writer.write('public');await writer.close();})()`);
 report.pages.push(await page('testing',true));
 assert.equal(await evaluate(`localStorage.getItem('channel-check')`),null);
 assert.equal(await evaluate(`(async()=>{try{await(await navigator.storage.getDirectory()).getFileHandle('channel-check');return true;}catch(e){if(e.name==='NotFoundError')return false;throw e;}})()`),false);
 assert.equal(report.pages[1].handoff,null,'switch must not carry invitation');
 report.pages.push(await page('testing'));assert.match(report.pages[2].handoff,/^playbit:session:testing:/);
 await evaluate(`document.querySelector('#copy-environment').click();true`);
 const picture=await call('Page.captureScreenshot',{format:'png'});await writeFile(output+'/testing-channel.png',Buffer.from(picture.data,'base64'));
 report.pages.push(await page('public',true));assert.equal(await evaluate(`localStorage.getItem('channel-check')`),'public');
 assert.equal(await evaluate(`(async()=>{const file=await(await navigator.storage.getDirectory()).getFileHandle('channel-check');return(await file.getFile()).text();})()`),'public');
 assert.equal(new URL(report.pages[0].asset).pathname,new URL(report.pages[2].asset).pathname);
 await writeFile(output+'/browser-channel.json',JSON.stringify(report,null,2)+'\n');
 passed=true;console.log(JSON.stringify(report));
 for(const origin of Object.values(origins))assert.equal((await fetch(origin+'/qualification/complete?passed',{method:'POST'})).status,204);
 if(fixture.exitCode===null&&fixture.signalCode===null)await deadline(new Promise(r=>fixture.once('exit',r)),10000,'fixture completion');assert.equal(fixture.exitCode,0,fixtureOutput.join(''));
}finally{
 if(!passed&&origins)for(const origin of Object.values(origins))await fetch(origin+'/qualification/complete?failed',{method:'POST'}).catch(()=>{});
 socket?.close();await stop(chrome);await stop(fixture);if(server){server.closeAllConnections();await deadline(new Promise(r=>server.close(r)),5000,'HTTPS fixture close');}await rm(temporary,{recursive:true,force:true,maxRetries:5,retryDelay:100});
}
