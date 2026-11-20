import { ChannelHost } from "./channel-host.ts";
import { createHost,bootstrap } from "../tests/host-fixture.mjs";
import { encodeId } from "./identifier.ts";
import test from 'node:test';
import assert from 'node:assert/strict';
import { SessionChannels } from './session-channels.ts';
import { AP } from './ap-codec.ts';
import { AbiError } from './guest-memory.ts';
import { retryable } from './errors.ts';

// Model only AP's raw stream envelope; encryption and channel lifecycle run in production code.
async function fixture() {
    const {host}=await createHost(bootstrap(encodeId(new Uint8Array(16).fill(1))));
    const protocol=new ChannelHost(()=>host.exports);
    let next=1;
    const streams=new Map(),ciphertexts=[];
    const key=new Uint8Array(32).fill(9),session=new Uint8Array(32).fill(7);
    const same=(a,b)=>Buffer.from(a).equals(Buffer.from(b));
    const client=byte=>{
        const events=[],status=[],controls=[];
        const identity={session,account:new Uint8Array(16).fill(byte),device:new Uint8Array(16),instance:new Uint8Array(16).fill(byte+20),build:new Uint8Array(32).fill(8),proof:new Uint8Array(64)};
        const authenticatedDevice=new Uint8Array(16).fill(byte+10);
        let generation=1;
        const transport={
            async connect(){return {device:authenticatedDevice.slice(),generation};},
            consumed(){},
            async open(frame,device,admission){
                assert.equal(admission,generation,"a new stream uses current transport readiness");
                assert.deepEqual(device,authenticatedDevice,"ciphertext uses the admitted device");
                const handle=next++;
                const stream={frame,queue:[],waiting:undefined};streams.set(handle,stream);
                if(frame.type===AP.PUBLISH){
                    const source=Uint8Array.from([...identity.account,...identity.device,...frame.source]);
                    const body=frame.body.slice(64);ciphertexts.push(body);
                    for(const target of streams.values())if(target.frame.type===AP.SUBSCRIBE&&same(target.frame.destination,frame.destination)) {
                        const message={type:AP.PUBLISH,flags:0,source,destination:frame.destination,body};
                        if(target.waiting){target.waiting.resolve(message);target.waiting=undefined;}else target.queue.push(message);
                    }
                }
                return handle;
            },
            read(handle){
                const stream=streams.get(handle);if(!stream)return Promise.reject(new Error('closed'));
                if(stream.queue.length)return Promise.resolve(stream.queue.shift());
                return new Promise((resolve,reject)=>{stream.waiting={resolve,reject};});
            },
            async close(handle){const stream=streams.get(handle);streams.delete(handle);stream?.waiting?.reject(new Error('closed'));},
        };
        const channels=new SessionChannels(identity,key,protocol,transport,(name,source,message,data)=>events.push({name,source,message,data}),()=>controls.push("presence"),error=>status.push(error),()=>controls.push("update"));
        return {channels,events,status,controls,transport,reconnect:async()=>{generation++;await channels.reconnect(authenticatedDevice,generation);}};
    };
    return {client,ciphertexts};
}
const settle=()=>new Promise(resolve=>setTimeout(resolve,20));

test('a superseded channel connection remains retryable while its replacement starts', async () => {
    const f = await fixture(), a = f.client(1);
    let release;
    const held = new Promise(resolve => { release = resolve; });
    try {
        await a.channels.start();
        const connect = a.transport.connect;
        a.transport.connect = async () => { await held; return connect(); };
        const old = assert.rejects(a.reconnect(), retryable);
        const replacement = a.reconnect();
        release();
        await old; await replacement;
        await a.channels.command('live', 2, new Uint8Array());
        await a.channels.command('live', 1, Uint8Array.of(7)); await settle();
        assert.equal(a.events.length, 1);
    } finally { release(); a.channels.close(); }
});

test('encrypted channels deliver to subscribed publishers and preserve exact independent names',async()=>{
    const f=await fixture(),a=f.client(1),b=f.client(2);
    try {
        await a.channels.start();await b.channels.start();
        await a.channels.command('One',2,new Uint8Array());
        await a.channels.command('One',2,new Uint8Array());
        await b.channels.command('One',2,new Uint8Array());
        await b.channels.command('one',2,new Uint8Array());
        const bytes=new TextEncoder().encode('private preview');
        await a.channels.command('One',1,bytes);await settle();
        assert.deepEqual(a.events.map(e=>e.name),['One']);
        assert.deepEqual(b.events.map(e=>e.name),['One']);
        assert.deepEqual(a.events[0].data,bytes);
        assert.deepEqual(a.events[0].source.slice(0,16),a.channels.identity.account);
        assert.equal(f.ciphertexts.some(cipher=>Buffer.from(cipher).includes(bytes)),false);
        await a.channels.command('one',1,bytes);await settle();
        assert.deepEqual(b.events.map(e=>e.name),['One','one']);
        await b.channels.command('One',3,new Uint8Array());
        await a.channels.command('One',1,bytes);await settle();
        assert.equal(b.events.length,2);
        assert.throws(()=>a.channels.destination(''),AbiError);
        assert.equal(a.channels.destination('é'.repeat(111)+'a').length,255);
        assert.throws(()=>a.channels.destination('é'.repeat(112)),AbiError);
    } finally {a.channels.close();b.channels.close();}
});

test('overflow is explicit and disconnected publications never replay after subscription restoration',async()=>{
    const f=await fixture(),a=f.client(1);
    try {
        const own=await a.channels.start();await a.channels.command('data',2,new Uint8Array());
        await a.channels.command('data',1,new Uint8Array(32768));await settle();
        await a.channels.command('data',1,new Uint8Array(32768));await settle();
        assert.deepEqual(a.status,[-9]);
        await a.transport.close(own);await settle();
        assert.deepEqual(a.status,[-9,-5]);
        await assert.rejects(a.channels.command('data',1,Uint8Array.of(99)),error=>error.status===-5);
        a.channels.consumed(32872);
        await a.channels.reconnect(a.channels.identity.device,1);
        await a.channels.command('data',1,Uint8Array.of(1));await settle();
        assert.equal(a.events.length,2);
        assert.deepEqual(a.events[1].data,Uint8Array.of(1));
        assert.equal(f.ciphertexts.length,3);
    } finally {a.channels.close();}
});


test('reconnect replaces prior streams even before their read failures arrive',async()=>{
    const f=await fixture(),a=f.client(1);
    try {
        await a.channels.start();await a.channels.command('live',2,new Uint8Array());
        await a.reconnect();
        await a.channels.command('live',1,Uint8Array.of(7));await settle();
        assert.equal(a.events.length,1);assert.deepEqual(a.events[0].data,Uint8Array.of(7));
    } finally {a.channels.close();}
});

test('unsubscribe and close fence subscriptions suspended during connection startup',async()=>{
    const f=await fixture(),a=f.client(1),opened=[];
    let release;
    const hold=new Promise(resolve=>{release=resolve;});
    const connect=a.transport.connect,open=a.transport.open;
    a.transport.connect=async()=>{await hold;return connect();};
    a.transport.open=async(...args)=>{opened.push(args[0]);return open(...args);};
    const pending=[];
    try {
        for(let i=0;i<16;i++){
            pending.push(assert.rejects(a.channels.command(`closed-${i}`,2,new Uint8Array()),error=>error.status===-7));
            await a.channels.command(`closed-${i}`,i%2?3:4,new Uint8Array());
        }
        release();await Promise.all(pending);
        assert.equal(opened.filter(frame=>frame.destination.length>32).length,0);
        await a.channels.command('fresh',2,new Uint8Array());
        await a.channels.command('fresh',1,Uint8Array.of(7));await settle();
        assert.deepEqual(a.events.map(event=>event.name),['fresh']);
        await a.reconnect();
        assert.equal(opened.filter(frame=>frame.type===AP.SUBSCRIBE&&frame.destination.length>32).length,2,'only the fresh channel is restored');
    } finally {release();a.channels.close();}
});

test('a late subscription cannot replace a new handle for the same name',async()=>{
    const f=await fixture(),a=f.client(1);
    let release,entered,holdFirst=true;
    const hold=new Promise(resolve=>{release=resolve;}),opening=new Promise(resolve=>{entered=resolve;});
    const open=a.transport.open;
    a.transport.open=async(...args)=>{
        if(holdFirst&&args[0].type===AP.SUBSCRIBE&&args[0].destination.length>32){holdFirst=false;entered();await hold;}
        return open(...args);
    };
    try {
        await a.channels.start();
        const old=assert.rejects(a.channels.command('same',2,new Uint8Array()),error=>error.status===-7);
        await opening;
        await a.channels.command('same',4,new Uint8Array());
        await a.channels.command('same',2,new Uint8Array());
        release();await old;
        await a.channels.command('same',1,Uint8Array.of(9));await settle();
        assert.equal(a.events.length,1);assert.deepEqual(a.events[0].data,Uint8Array.of(9));
    } finally {release();a.channels.close();}
});


test('retired control streams cannot deliver an already resolved update or presence frame', async () => {
    for (const kind of ['update_available', 'presence']) {
        const f = await fixture(), a = f.client(1);
        let release;
        const original = a.transport.read;
        a.transport.read = handle => new Promise(resolve => { release = frame => { a.transport.read = original; resolve(frame); }; });
        try {
            await a.channels.start();
            const body = new TextEncoder().encode(JSON.stringify(kind === 'presence' ? { kind, participants: [] } : { kind }));
            release({ type: AP.PUBLISH, flags: 0, destination: a.channels.identity.session, source: new Uint8Array(), body });
            // The old promise has resolved, but its await continuation has not run.
            a.channels.close();
            await new Promise(setImmediate);
            assert.deepEqual(a.controls, []);
            assert.deepEqual(a.status, []);
        } finally { a.channels.close(); }
    }
});
