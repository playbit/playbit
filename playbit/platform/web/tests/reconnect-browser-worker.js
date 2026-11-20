// Appended to the actual runtime-worker module by the isolated browser test page.
// Replace only external AP/storage/guest boundaries; retain owner/retry/live code.
const sendEvidence = self.postMessage.bind(self);
const testReaders = new Map(), testSequences = new Map(), testFeed = [], testPublished = [];
let testHandle = 0, testAttempts = 0, testFailures = 0, testBootstraps = 0, testStage = 0;
const testSource = '03'.repeat(16), testText = new TextEncoder().encode('Saved during interruption');
const testStates = [], testDelays = [];
function testAssert(condition, message) { if (!condition) throw Error(message); }
function testEvent(handle, kind) {
    const bytes=new Uint8Array(40), view=new DataView(bytes.buffer);
    bytes[0]=1; bytes[1]=kind; bytes.fill(7,4,20);
    const sequence=(testSequences.get(handle) ?? 0n)+1n; testSequences.set(handle,sequence);
    view.setBigUint64(20,sequence,true); view.setBigUint64(28,1n,true);
    const reader=testReaders.get(handle); testReaders.delete(handle); reader.resolve(bytes);
}
managed=true; profile={root:{id:testSource},session:{id:'05'.repeat(32),resolved_build_id:'06'.repeat(32)}};
proof='test';
synchronize=async()=>{testBootstraps++;}; refreshScene=async()=>{};
syncDatabase=async()=>{};
accountReplica={head:async()=>null,adoptTree:async()=>{},stagePending:async()=>{},close(){}};
host={exports:{pb_live_rebase:()=>0,pb_live_peek:()=>testFeed.length?1:0,pb_live_ack:()=>{testFeed.shift();return 0;},pb_sync_finish:()=>0}};
hostOutput=()=>new Uint8Array();
liveOutput=length=>{
    if(!length)return new Uint8Array();
    const bytes=new Uint8Array(28+testFeed[0].length),view=new DataView(bytes.buffer);
    bytes.fill(3,0,16);view.setUint32(16,3,true);view.setBigUint64(20,1n,true);bytes.set(testFeed[0],28);return bytes;
};
ap=async(method,...args)=>{
    if(method==='connect') { testAttempts++; if(testFailures-->0)throw Object.assign(Error('AP temporarily unavailable'),{code:'ap-transport'});return {}; }
    if(method==='request'||method==='accountChanged')return {};
    if(method==='subscribeReplica'){const handle=++testHandle;return {handle,ack:{epoch:Array(16).fill(7),sequence:0n,policy_revision:1n}};}
    if(method==='streamReplica')return new Promise((resolve,reject)=>testReaders.set(args[0],{resolve,reject}));
    if(method==='streamClose'){testReaders.get(args[0])?.reject(Object.assign(Error('closed'),{code:'ap-transport'}));testReaders.delete(args[0]);return;}
    if(method==='checkpointReplica'){queueMicrotask(()=>testEvent(args[0],2));return;}
    if(method==='publishReplica'){testPublished.push(new TextDecoder().decode(args[1]));return {};}
    throw Error(`Unexpected AP operation ${method}`);
};
const testSetTimeout=self.setTimeout.bind(self);
self.setTimeout=(callback,delay,...args)=>{if(delay<=30000)testDelays.push(delay);return testSetTimeout(callback,delay,...args);};
self.postMessage=value=>{
    if(value.replication) {
        testStates.push(value.replication.kind);
        if(value.replication.kind==='live'&&testStage===0) {
            testStage=1;testFailures=3;
            queueMicrotask(()=>{
                for(const reader of testReaders.values())reader.reject(Object.assign(Error('Connection lost'),{code:'ap-transport'}));
                testReaders.clear();testFeed.push(testText);replicas.notify();
            });
        } else if(value.replication.kind==='live'&&testStage===1) {
            testStage=2;
            queueMicrotask(()=>{try {
                testAssert(testAttempts===5,'Expected initial connect, three failed attempts and recovery');
                testAssert(testBootstraps===2,'Unexpected steady-state bootstrap');
                testAssert(testFeed.length===0&&testPublished.includes('Saved during interruption'),'Pending edit did not converge');
                testEvent(testReaders.keys().next().value,4);
            }catch(error){sendEvidence({failure:error.stack});}});
        } else if(value.replication.kind==='action_required') {
            queueMicrotask(async()=>{try {
                await connectReplication();
                testAssert(testAttempts===5&&!recoveryTimer,'Revoked authority retried');
                self.onmessage({data:{method:'close',id:999}});
            }catch(error){sendEvidence({failure:error.stack});}});
        }
    }
    if(value.id===999)sendEvidence({evidence:{passed:!value.error,attempts:testAttempts,bootstraps:testBootstraps,
        retryDelays:testDelays,states:testStates,published:testPublished,closed,retryCanceled:!recoveryTimer,
        boundary:'native browser Worker; synthetic AP/local store; no guest/OPFS/device timing claim'}});
};
void connectReplication();
