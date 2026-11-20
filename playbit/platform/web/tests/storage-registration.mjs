import { encodeId } from '../src/identifier.ts';
// Web Worker benchmark for the real compiled host and synchronous OPFS.
// Post { samples: 1000 } to this module worker on an isolated test origin. It
// creates/removes only its own random benchmark directory. This component
// boundary excludes UI admission, AP, rendering and physical presentation.
import { OpfsStore, OpfsResources } from '../src/opfs-store.ts';
import { createStorageBridge } from '../src/opfs-bridge.ts';

const encoder = new TextEncoder(), decoder = new TextDecoder();
const summary = values => {
    const sorted = values.slice().sort((a,b) => a-b), at = fraction => sorted[Math.ceil(sorted.length*fraction)-1];
    return { count: values.length, p50_ms: at(.5), p95_ms: at(.95), p99_ms: at(.99), max_ms: sorted.at(-1) };
};
self.onmessage = async ({ data }) => {
    self.onmessage = null;
    let directory;
    const handles = new Set();
    const name = `registration-benchmark-${crypto.randomUUID()}`;
    try {
        const count = data.samples;
        if (!Number.isSafeInteger(count) || count < 1 || count > 1000) throw new Error('Use 1–1,000 samples');
        const root = await navigator.storage.getDirectory();
        directory = await root.getDirectoryHandle(name,{create:true});
        const module = await WebAssembly.compile(await (await fetch(new URL('../playbit.wasm',import.meta.url))).arrayBuffer());
        const samples = [], account = encodeId(new Uint8Array(16).fill(0x01)), key = Array(32).fill(1);
        let measuring;
        const file = async filename => {
            const handle = await (await directory.getFileHandle(filename,{create:true})).createSyncAccessHandle();
            handles.add(handle);
            const timed = (kind, callback) => {
                if (!measuring) return callback();
                measuring[`${kind}_calls`]++;
                const start = performance.now();
                try { return callback(); } finally { measuring[`${kind}_ms`] += performance.now()-start; }
            };
            return { getSize: () => handle.getSize(), read: (bytes,options) => timed('read',()=>handle.read(bytes,options)),
                write: (bytes,options) => timed('write',()=>handle.write(bytes,options)),
                truncate: length => timed('truncate',()=>handle.truncate(length)),
                flush: () => timed('flush',()=>handle.flush()), close: () => { if(handles.delete(handle))handle.close(); } };
        };
        for (let group = 0; samples.length < count; group++) {
            const stores = new Map(), resources = new OpfsResources(
                [await file(`${group}-writer-0`),await file(`${group}-writer-1`)],
                [await file(`${group}-stage-0`),await file(`${group}-stage-1`)]);
            let instance, bridge;
            try {
                for (let slot=1;slot<=2;slot++) stores.set(slot,OpfsStore.empty(await file(`${group}-db-${slot}`),resources));
                bridge = createStorageBridge(stores,resources,()=>instance.exports.memory,
                    bytes=>crypto.getRandomValues(bytes),()=>Math.floor(Date.now()/1000),error=>{throw new Error(error)});
                instance = await WebAssembly.instantiate(module,{opfs: { ...bridge.imports, engine_panic() { throw new Error("Unexpected engine panic"); } }});
                const call = (operation, bytes) => {
                    const pointer = instance.exports.pb_input(bytes.length);
                    new Uint8Array(instance.exports.memory.buffer,pointer,bytes.length).set(bytes);
                    const result = instance.exports[operation](bytes.length);
                    if (result<0) {
                        const length=instance.exports.pb_error();
                        throw new Error(decoder.decode(new Uint8Array(instance.exports.memory.buffer,instance.exports.pb_output(),length))||`Host error ${result}`);
                    }
                };
                const volume = id => ({id,owner:account,name:'Benchmark',share_key_with_service:true, personal: true});
                const rootVolume=volume(encodeId(new Uint8Array(16).fill(0x02))),version=encodeId(new Uint8Array(32).fill(0x04));
                call('pb_initialize',encoder.encode(JSON.stringify({account,installation:encodeId(new Uint8Array(16).fill(0x2a)),root:rootVolume,
                    session:{id:encodeId(new Uint8Array(32).fill(0x05)),app:{publisher:account,app_id:'todo'},selector:{kind:'exact',build_id:version},resolved_build_id:version,
                        root_volume:rootVolume.id,creator:account,update_authority:'creator'},
                    root_key:key,replica_ids:[1,2],local:null})));
                for (let index=0;index<30 && samples.length<count;index++) {
                    const opened = performance.now();
                    for (let database=0;database<2;database++) {
                        const slot=3+index*2+database;
                        stores.set(slot,OpfsStore.empty(await file(`${group}-db-${slot}`),resources));
                    }
                    const open_ms = performance.now() - opened;
                    const id=encodeId(Buffer.from((index+100).toString(16).padStart(32,'0'), 'hex'));
                    const bytes=encoder.encode(JSON.stringify({volume:volume(id),key,replica_ids:[7,8]}));
                    measuring={open_ms,read_calls:0,read_ms:0,write_calls:0,write_ms:0,flush_calls:0,flush_ms:0,truncate_calls:0,truncate_ms:0};
                    const start=performance.now();
                    call('pb_register_personal',bytes);
                    measuring.total_ms=performance.now()-start;
                    measuring.open_and_register_ms=performance.now()-opened;
                    samples.push(measuring);measuring=null;
                }
            } finally {
                measuring=null;bridge?.close();for(const store of stores.values())store.close();resources.close();
            }
            self.postMessage({progress:samples.length});
        }
        const results={};for(const key of ['open_ms','total_ms','open_and_register_ms','read_ms','write_ms','flush_ms','truncate_ms'])results[key]=summary(samples.map(sample=>sample[key]));
        self.postMessage({result:{boundary:'host input copy through registration of two empty kvdb databases on real OPFS; instrumented I/O; excludes handle acquisition and UI/AP/presentation',
            openBoundary:'separately timed async acquisition/recovery of both OPFS handles; open_and_register_ms includes both boundaries and JSON preparation',
            userAgent:navigator.userAgent,isolated:crossOriginIsolated,results,samples}});
    } catch(error) { self.postMessage({error:error.message}); }
    finally {
        try {
            for(const handle of handles)handle.close();
            if(directory)await (await navigator.storage.getDirectory()).removeEntry(name,{recursive:true});
        } finally { self.close(); }
    }
};
