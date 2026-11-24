// Independent host checks the actual compiled SDK's records, handles and copies.
import fs from 'node:fs';
const [wasmPath, abiPath] = process.argv.slice(2);
const abi = JSON.parse(fs.readFileSync(abiPath));
const assert = (value, message) => { if (!value) throw Error(message); };
const volume = 0x7edcba9880000001n, operation = 0x7123456780000002n, sound = 0x6234567880000003n, audio = 0x5234567880000004n;
const channel = 0x6edcba9880000005n, subscription = 0x6123456780000006n;
const publish = 0x6123456780000007n, uncertain = 0x6123456780000008n;
const unsubscribe = 0x6123456780000009n, resubscribe = 0x612345678000000an;
const close = 0x612345678000000bn, reopenedClose = 0x612345678000000cn;
const reopenedChannel = 0x6edcba9980000005n;
let liveChannel = null, subscribed = false, unsubscribeTakes = 0, closes = 0, staleCalls = 0;
const clock = 0xfedcba9876543210n;
let instance, entryCalls = 0, volumeCalls = 0, passed = false, submitted = 0, pending = null;
let identityCalls = 0, presenceCalls = 0, sessionCalls = 0, channelOpens = 0;
let infoCalls = 0;
let subscribeTakes = 0, publicationCalls = 0, wakeCalls = 0, clockCalls = 0, eventRetries = 0;
const events = [], channelOperations = new Set();
const liveAliases = new Set();
let disposedAliases = 0, audioTakes = 0, soundPlays = 0;
let keyPages = 0, keyTakes = 0;
const text = new TextDecoder();
const words = (...values) => {
    const result = Buffer.alloc(values.length * 4);
    values.forEach((value, i) => result.writeUInt32LE(value, i * 4));
    return result;
};
const word64 = value => {
    const result = Buffer.alloc(8);
    result.writeBigUInt64LE(value);
    return result;
};
const command = (kind, id, body) => Buffer.concat([Buffer.of(kind), words(id, body.length), body]);
const identity = index => Buffer.from(Array.from({length: 48}, (_, i) => index + i));
const enqueue = (name, body, target = 0) => events.push({kind: abi[`EventKind::${name}`], target, body});
const messageBytes = Buffer.from(Array.from({length: 130}, (_, i) => (i * 17) & 255));
const menuAction = (id, label) => {
    const body = Buffer.from(label);
    return Buffer.concat([Buffer.of(abi.MENU_ACTION), words(id), Buffer.of(0, 0), words(body.length), body]);
};
const u64 = value => BigInt.asUintN(64, value);
const opcode = (name, op) => abi[`Opcode::${name}`] === op;
const module = new WebAssembly.Module(fs.readFileSync(wasmPath));
assert(WebAssembly.Module.imports(module).length === 1, 'single scalar import');
instance = new WebAssembly.Instance(module, {playbit: {pb_syscall(op, ...slots) {
    const data = new DataView(instance.exports.memory.buffer);
    const bytes = (p,n) => new Uint8Array(instance.exports.memory.buffer, Number(p), Number(n));
    const u32 = p => data.getUint32(Number(p), true);
    const set32 = (p,v) => data.setUint32(Number(p), v, true);
    const set64 = (p,v) => data.setBigUint64(Number(p), v, true);
    if (opcode('ViewSubmit', op)) {
        const scene = bytes(slots[0], slots[1]);
        submitted++;
        if (submitted === 2) {
            const actions = Buffer.concat([words(2), menuAction(0x80000001, 'Réessayer'), menuAction(0x7fffffff, 'Quit')]);
            const canvas = words(321, 217, abi.CANVAS_FLAG_VIEWPORT, 0);
            const expected = Buffer.concat([Buffer.from('PBV1'), command(abi.VIEW_CONTEXT_MENU_START, 0, actions),
                command(abi.VIEW_CANVAS, 0, canvas), command(abi.VIEW_CONTEXT_MENU_END, 0, Buffer.alloc(0))]);
            assert(Buffer.from(scene).equals(expected), 'exact native context menu and static viewport canvas');
            return 0n;
        }
        if (submitted === 3) {
            const canvas = words(640, 479, abi.CANVAS_FLAG_CONTINUOUS | abi.CANVAS_FLAG_VIEWPORT, 0);
            const expected = Buffer.concat([Buffer.from('PBV1'), command(abi.VIEW_CANVAS, 19, canvas)]);
            assert(Buffer.from(scene).equals(expected), 'exact continuous viewport canvas');
            return 0n;
        }
        if (submitted === 4) {
            const style = words(abi['WindowStyle::TITLEBAR'] | abi['WindowStyle::CLOSABLE'] |
                abi['WindowStyle::TRANSPARENT_TITLEBAR'] | abi['WindowStyle::HIDE_SESSION_BUTTON']);
            const constraints = Buffer.alloc(20);
            [320.25, 0.5, 0, 180.140625, 16 / 9].forEach((value, i) => constraints.writeFloatLE(value, i * 4));
            const theme = Buffer.concat([Buffer.of(abi['ThemeColor::WindowBackground']), words(0x132d41ff)]);
            const canvas = words(15360, 9000, abi.CANVAS_FLAG_WINDOW | abi.CANVAS_FLAG_CLEAR, 1,
                0xa1b2c3d4, 0, 15359, 8999, 1, 1, 0xffffffff);
            const expected = Buffer.concat([Buffer.from('PBV1'), command(abi.VIEW_WINDOW_STYLE, 0, style),
                command(abi.VIEW_WINDOW_SIZE_CONSTRAINTS, 0, constraints),
                command(abi.VIEW_THEME, 0, theme), command(abi.VIEW_CANVAS, 19, canvas)]);
            assert(Buffer.from(scene).equals(expected), 'styled window with a clear color behind its canvas items');
            return 0n;
        }
        assert(submitted === 1, 'unexpected scene');
        assert(text.decode(scene.slice(0,4)) === 'PBV1', 'view framing');
        let at=4, found=false;
        while (at<scene.length) {
            const d=new DataView(scene.buffer, scene.byteOffset+at);
            const kind=scene[at], length=d.getUint32(5,true);
            if (kind===22) {
                assert(d.getUint32(1,true)===9, 'canvas identity');
                const expected=new Uint8Array(16+24+36+5+36+573440);
                const record=new DataView(expected.buffer);
                let offset=0;
                const words=values => {for(const value of values) {record.setUint32(offset,value,true);offset+=4;}};
                words([512,280,1,3]);
                words([0,1,2,3,4,0xff123456]);
                words([1,5,6,20,12,0xffabcdef,9,1,5]);
                expected.set(new TextEncoder().encode('Pixel'),offset);offset+=5;
                words([2,0,0,512,280,0,512,280,573440]);
                expected.fill(255,offset);expected.set([18,52,86],offset);
                const actual=scene.subarray(at+9,at+9+length);
                assert(actual.length===expected.length && actual.every((byte,i)=>byte===expected[i]), 'exact mixed canvas wire bytes');
                found=true;
            }
            at+=9+length;
        }
        assert(found && at===scene.length, 'complete pixel scene'); return 0n;
    }
    if (opcode('AvailableVolumes',op)) {
        assert(liveAliases.size===0, 'no File aliases survive into another enumeration');
        const p=Number(slots[0]); assert(u32(p)===64 && u32(p+28)===88 && u32(p+44)===16,'volume record layouts');
        ++volumeCalls;
        const enumeration=Math.floor((volumeCalls-1)/2);
        if (volumeCalls%2===1) {
            assert(u32(p+16)===0 && u32(p+20)===0 && u32(p+32)===0 && u32(p+36)===0 && u32(p+48)===0 && u32(p+52)===0,'zero-capacity volume probe');
            set32(p+24,9);set32(p+40,576);set32(p+56,enumeration<10 ? 65537 : abi.MAX_DATA_BYTES);
            return BigInt(abi['Error::BufferTooSmall']);
        }
        assert(enumeration<10 && u32(p+20)===9 && u32(p+36)===576 && u32(p+52)===65537,'exact bounded capacity retry');
        const records=u32(p+16), tags=u32(p+32), names=u32(p+48);
        const selected=[0,8,4,2,6,1,0,-1,3,8][enumeration];
        assert(records===p+64 && tags===records+9*88 && names===tags+576*16,'disjoint descriptor spans');
        for(let i=0;i<9;i++) {
            assert(u32(records+i*88)===88 && u32(records+i*88+4)===0,'initialized Volume capacity');
            const file=0x4123456780000000n+BigInt(enumeration*16+i+1);
            assert(!liveAliases.has(file), 'unique enumeration File alias');liveAliases.add(file);
            const role=i===selected ? [1,2,3,4,5,6,0,0,1,1][enumeration] : 0;
            const legacy=enumeration===6;
            const rights = [3,1,5,2,0,7,3,0,1,1][enumeration];
            set32(records+i*88+4,legacy ? 80 : 84);set64(records+i*88+8,i===selected ? volume : BigInt(i+1));
            set32(records+i*88+48,i===selected ? rights : 0);
            set32(records+i*88+52,role===1 || (legacy && i===selected) ? 0x101 : 0x100);
            set64(records+i*88+56,file);set32(records+i*88+80,legacy ? 0xffffffff : role);
        }
        for(let i=0;i<576;i++) assert(u32(tags+i*16)===16 && u32(tags+i*16+4)===0,'initialized Range capacity');
        set32(p+4,60);set32(p+24,9);set32(p+40,576);set32(p+56,65537);return 0n;
    }
    if (opcode('FileDispose',op)) {
        assert(slots.slice(1).every(slot=>slot===0n), 'File disposal has no borrowed spans');
        assert(liveAliases.delete(u64(slots[0])), 'dispose every full-width File alias exactly once');
        ++disposedAliases;return 0n;
    }
    if (opcode('Identity',op)) {
        const p = Number(slots[0]);
        assert(u32(p) === 360 && u32(p+4) === 0 && slots.slice(1).every(s => s === 0n), 'identity record capacity');
        set32(p+4, identityCalls === 0 ? 104 : 360);
        bytes(p+104, 256).set(Buffer.from("Zoë"));
        set64(p+8, 0x1234567800000001n);
        bytes(p+16, 48).set(identity(0));
        bytes(p+64, 32).fill(99);
        set32(p+96, 2);
        if (++identityCalls === 3) set32(p+100, 0x80000000);
        return 0n;
    }
    if (opcode('SessionInfo',op)) {
        const call = ++infoCalls, p = Number(slots[0]);
        assert(u32(p) === 64 && u32(p+4) === 0 && slots.slice(1).every(s => s === 0n), 'session info record capacity');
        if (call === 4) return BigInt(abi['Error::NotFound']);
        if (call === 5) return BigInt(abi['Error::Unsupported']);
        set32(p+4, 64);
        set32(p+8, call <= 2 ? 0x80000001 : 0x80000000);
        bytes(p+16, 32).fill(99);
        if (call === 1) bytes(p+48, 16).set(Buffer.from('session-seed-v1!'));
        if (call === 6) set32(p+12, 0x80000000);
        return 0n;
    }
    if (opcode('Presence',op)) {
        const call = ++presenceCalls;
        const records = Number(slots[0]), count = Number(slots[3]);
        assert(sessionCalls === 1 && channelOpens === 0, 'roster-only membership');
        const capacity = call === 3 || call === 5 ? 18 : 16;
        assert(slots[1] === BigInt(capacity) && slots[2] === 344n && records === count+4
               && slots.slice(4).every(s => s === 0n), 'presence exact scalar layout');
        assert(u32(count) === 0, 'presence output count initialized');
        for (let i=0; i<capacity; i++) {
            assert(u32(records+344*i) === 344 && u32(records+344*i+4) === 0, 'presence input capacity');
        }
        if (call === 2 || call === 4) {
            set32(count, 18);
            return BigInt(abi['Error::BufferTooSmall']);
        }
        if (call === 1 || call === 3) enqueue('PresenceChanged', Buffer.alloc(0));
        const actual = call === 3 || call === 5 ? 17 : 0;
        set32(count, actual);
        for (let i=0; i<actual; i++) {
            const p = records + i*344;
            set32(p+4, i === 0 ? 88 : 344);
            bytes(p+88, 256).set(Buffer.from(call === 5 && i === 1 ? "月" : "小"));
            bytes(p+8, 48).set(identity(i));
            bytes(p+56, 32).fill(99);
        }
        return 0n;
    }
    if (opcode('Session',op)) {
        assert(slots.every(s => s === 0n), 'session has no scalar arguments');
        sessionCalls++;
        return 7n;
    }
    if (opcode('ChannelOpen',op)) {
        assert(sessionCalls === channelOpens+2 && slots[1] === 7n && slots.slice(2).every(s => s === 0n), 'channel open name extent and session');
        assert(text.decode(bytes(slots[0], slots[1])) === 'live/é', 'channel open exact UTF-8 name');
        assert(liveChannel === null, 'channel must be closed before allocating a fresh handle');
        liveChannel = ++channelOpens === 1 ? channel : reopenedChannel;
        return liveChannel;
    }
    if (opcode('ChannelBegin',op)) {
        const handle = u64(slots[0]), action = slots[1];
        assert(slots.slice(4).every(s => s === 0n), 'channel begin unused slots');
        if (handle !== liveChannel) {
            assert(handle === channel && action === 2n && closes === 1, 'only the retired handle is stale');
            staleCalls++;
            return BigInt(abi['Error::StaleHandle']);
        }
        let operation;
        if (action === 2n) {
            assert(!subscribed && slots.slice(2).every(s => s === 0n), 'subscribe has no payload');
            operation = subscribeTakes === 0 ? subscription : resubscribe;
            enqueue('DataReady', word64(operation));
        } else if (action === 3n) {
            assert(handle === channel && subscribed && slots.slice(2).every(s => s === 0n), 'unsubscribe has no payload');
            operation = unsubscribe;
            enqueue('DataReady', word64(operation));
        } else if (action === 4n) {
            assert(slots.slice(2).every(s => s === 0n), 'close has no payload');
            assert(!subscribed || handle === channel, 'close accepts either subscription state');
            operation = ++closes === 1 ? close : reopenedClose;
            liveChannel = null;
            enqueue('DataReady', word64(operation));
        } else {
            assert(action === 1n, 'publish action');
            if (++publicationCalls === 1) {
                assert(slots[3] === 130n && Buffer.from(bytes(slots[2], slots[3])).equals(messageBytes), 'publish exact binary payload');
                operation = publish;
                enqueue('ChannelMessage', Buffer.concat([word64(channel), identity(1), Buffer.alloc(48, 33), messageBytes]));
                enqueue('DataReady', word64(operation));
            } else {
                assert(publicationCalls === 2 && slots[3] === 0n, 'empty publish payload');
                operation = uncertain;
                enqueue('ChannelStatus', words(abi['Error::Unavailable'] >>> 0));
                enqueue('Click', Buffer.alloc(0), 0x80000001);
                enqueue('SurfaceChanged', words(641, 480, 0x40200000, 3));
            }
        }
        assert(!channelOperations.has(operation), 'one live command per handle');
        channelOperations.add(operation);
        return operation;
    }
    if (opcode('ChannelTake',op)) {
        const id = u64(slots[0]);
        assert(channelOperations.has(id) && slots.slice(1).every(s => s === 0n), 'full-width channel operation take');
        if (id === subscription && ++subscribeTakes === 1) return BigInt(abi['Error::WouldBlock']);
        if (id === unsubscribe && ++unsubscribeTakes === 1) return BigInt(abi['Error::WouldBlock']);
        if (id === subscription || id === resubscribe) subscribed = true;
        if (id === unsubscribe || id === close || id === reopenedClose) subscribed = false;
        channelOperations.delete(id);
        return id === uncertain ? BigInt(abi['Error::Uncertain']) : 0n;
    }
    if (opcode('WakeAfter',op)) {
        assert(slots[0] === [1000n, 86400000n, 999n][wakeCalls] && slots.slice(1).every(s => s === 0n), 'wake delay and unused slots');
        ++wakeCalls;
        if (wakeCalls === 3) return BigInt(abi['Error::InvalidArgument']);
        if (wakeCalls === 2) enqueue('Wake', Buffer.alloc(0));
        return 0n;
    }
    if (opcode('ClockRead',op)) {
        assert(slots.every(s => s === 0n), 'clock takes no slots');
        clockCalls++;
        return BigInt.asIntN(64, clock);
    }
    if (opcode('EventPoll',op)) {
        if (events.length === 0) return BigInt(abi['Error::WouldBlock']);
        const event = events[0], p = Number(slots[0]);
        assert(u32(p) === 24 && u32(p+4) === 0 && slots.slice(3).every(s => s === 0n), 'event capacity and unused slots');
        if (Number(slots[2]) < event.body.length) {
            eventRetries++;
            return BigInt(event.body.length);
        }
        set32(p+4, 24);
        set64(p+8, 0x100000001n);
        set32(p+16, event.kind);
        set32(p+20, event.target);
        bytes(slots[1], event.body.length).set(event.body);
        events.shift();
        return BigInt(event.body.length);
    }
    if (opcode('EntryResolveBegin',op)) {
        assert(pending === null, 'previous operation consumed'); pending = 'entry';
        enqueue('DataReady', word64(operation));
        assert(u64(slots[0])===volume && text.decode(bytes(slots[1],slots[2]))==='pixel-plays','full-width volume and name');assert(slots.slice(3).every(slot=>slot===0n),'entry begin unused slots');return operation;
    }
    if (opcode('EntryResolveTake',op)) {
        assert(u64(slots[0])===operation && pending === 'entry','full-width live operation');if (++entryCalls===1) return -6n;
        pending = null;
        const p=Number(slots[1]);assert(u32(p)===32,'entry capacity');set32(p+4,32);set64(p+8,volume);bytes(p+16,15).fill(7);return 0n;
    }
    if (opcode('KvKeysBegin',op)) {
        const query = Number(slots[1]);
        assert(pending === null && data.getBigUint64(Number(slots[0]) + 8, true) === volume, 'keys entry');
        assert(u32(query) === 56 && u32(query + 48) === 64, 'bounded keys query');
        if (keyPages === 0) {
            assert(u32(query + 44) === 0 && slots[2] === 0n && slots[3] === 0n, 'first page has no cursor');
        } else {
            assert(u32(query + 44) === 1 && u32(query + 8) === 7 && bytes(query + 12, 32).every(b => b === 42), 'pinned snapshot');
            assert(slots[3] === 2n && bytes(slots[2], 2).join(',') === '98,0', 'binary continuation key');
        }
        keyPages++;
        pending = 'keys';
        return operation;
    }
    if (opcode('KvKeysTake',op)) {
        assert(pending === 'keys' && u64(slots[0]) === operation, 'live keys operation');
        if (++keyTakes === 1) return BigInt(abi['Error::WouldBlock']);
        if (keyPages === 3) {
            pending = null;
            return BigInt(abi['Error::Conflict']);
        }
        const page = Number(slots[1]);
        const size = keyPages === 1 ? 65 : 40;
        assert(slots[2] >= BigInt(size), 'page capacity');
        bytes(page, size).fill(0);
        bytes(page, 2).set([1, 7]);
        bytes(page + 2, 32).fill(42);
        if (keyPages === 1) {
            set32(page + 34, 2);
            data.setUint16(page + 38, 2, true);
            data.setUint16(page + 40, 1, true);
            bytes(page + 50, 1).set([97]);
            data.setUint16(page + 51, 2, true);
            bytes(page + 61, 4).set([98, 0, 98, 0]);
        }
        set32(slots[3], size);
        pending = null;
        return 0n;
    }
    if (opcode('KvReadBegin',op)) {
        assert(pending === null, 'previous operation consumed'); pending = 'read';
        const p=Number(slots[0]), q=Number(slots[3]);assert(data.getBigUint64(p+8,true)===volume && bytes(p+16,15).every(x=>x===7),'entry handle copied');
        assert(text.decode(bytes(slots[1],slots[2]))==='best/stari-most' && u32(q)===64 && u32(q+16)===8,'read key/query');assert(slots.slice(4).every(slot=>slot===0n),'read begin unused slots');return operation;
    }
    if (opcode('KvReadTake',op)) {
        assert(pending === 'read', 'live read completion'); pending = null;
        assert(u64(slots[0])===operation && slots[2]===57n,'read take capacity');const p=Number(slots[1]);bytes(p,57).fill(0);set64(p+33,8n);set64(p+49,123n);set32(slots[3],57);return 0n;
    }
    if (opcode('KvPutBegin',op)) {
        assert(pending === null, 'previous operation consumed'); pending = 'put';
        assert(data.getBigUint64(Number(slots[0])+8,true)===volume && text.decode(bytes(slots[1],slots[2]))==='best/stari-most','put entry/key');
        assert(slots[4]===8n && data.getBigUint64(Number(slots[3]),true)===123n,'put exact bytes');assert(slots[5]===0n,'put begin unused slot');return operation;
    }
    if (opcode('KvPutTake',op) || opcode('OperationDispose',op)) {
        assert(u64(slots[0])===operation,'completion handle');
        assert(pending === (opcode('KvPutTake',op) ? 'put' : 'read'), 'completion/disposal consumes a live operation');
        pending = null; return 0n;
    }
    if (opcode('AudioOpenBegin',op)) {
        assert(pending===null && slots[0]===44100n, 'asynchronous audio sample rate preference');
        pending='audio';assert(slots.slice(1).every(slot=>slot===0n),'audio begin unused slots');return operation;
    }
    if (opcode('AudioOpenTake',op)) {
        assert(pending==='audio' && u64(slots[0])===operation, 'pending audio operation');
        if (++audioTakes===1) return -6n;
        pending=null;assert(slots.slice(1).every(slot=>slot===0n),'audio take unused slots');return audio;
    }
    if (opcode('AudioSampleRate',op)) {assert(u64(slots[0])===audio,'audio rate handle');return 48000n;}
    if (opcode('AudioVolume',op)) {assert(u64(slots[0])===audio,'audio volume handle');return 0x3f000000n;}
    if (opcode('AudioSetVolume',op)) {assert(u64(slots[0])===audio && slots[1]===0x3f400000n,'master volume float bits');return 0n;}
    if (opcode('SoundOpenPcm',op)) {
        assert(u64(slots[0])===audio && slots[1]===2n && slots[2]===44100n,'PCM channel count/source rate');
        assert(slots[4]===32n && bytes(slots[3],128).every(x=>x===1),'interleaved stereo frame count/copy');
        assert(slots[5]===0n,'sound open unused slot');return sound;
    }
    if (opcode('SoundPlay',op)) {
        assert(u64(slots[0])===sound,'full-width sound capability');++soundPlays;
        if (slots[1]===0x80000000n) {
            assert(slots[2]===32768n && slots[3]===0x123456789n && slots[4]===0x23456789an && slots[5]===1n,'arbitrary unsigned instance, gain, full-width source timestamps and flags');
            return 0n;
        }
        assert((slots[1]===1234n || slots[1]===1235n) && slots[2]===65536n && slots[3]===0n && slots[4]===1000000n && slots[5]===0n,'independent sound instance and original amplitude');
        return slots[1]===1235n ? BigInt(abi['Error::LimitExceeded']) : 0n;
    }
    if (opcode('SoundSetGain',op)) {assert(u64(slots[0])===sound && slots[1]===0x80000000n && slots[2]===16384n,'instance fixed gain');return 0n;}
    if (opcode('SoundSetBalance',op)) {assert(u64(slots[0])===sound && slots[1]===0x80000000n && slots[2]===0xbf000000n,'negative balance zero-extended float bits');return 0n;}
    if (opcode('SoundTell',op)) {assert(u64(slots[0])===sound && slots[1]===0x80000000n,'tell instance');set64(slots[2],0xfedcba9876543210n);return 0n;}
    if (opcode('SoundSeek',op)) {assert(u64(slots[0])===sound && slots[1]===0x80000000n && slots[2]===0x100000001n,'seek full-width source timestamp');return 0n;}
    if (opcode('SoundStop',op)) {assert(u64(slots[0])===sound && (slots[1]===0x80000000n || slots[1]===1234n),'stop independent sound instance');return 0n;}
    if (opcode('SoundClose',op)) {assert(u64(slots[0])===sound,'close sound capability');return 0n;}
    if (opcode('AudioClose',op)) {assert(u64(slots[0])===audio,'close audio capability');return 0n;}
    if (opcode('Log',op)) {assert(text.decode(bytes(slots[0],slots[1]))==='expanded bindings passed','completion log');passed=true;return 0n;}
    throw Error(`Unexpected syscall ${op}`);
}}});
instance.exports.pb_main();
assert(passed && submitted===4 && entryCalls===2 && volumeCalls===21 && pending===null && audioTakes===2 && soundPlays===3, 'all expanded bindings exercised and consumed');
assert(disposedAliases===90 && liveAliases.size===0, 'all aliases released across six automatic roles, released Root prefix, missing roots and excessive capacity');
assert(identityCalls===3 && infoCalls===6 && presenceCalls===6 && sessionCalls===3 && channelOpens===2 && subscribeTakes===2
       && unsubscribeTakes===2 && closes===2 && staleCalls===1 && liveChannel===null && !subscribed
       && publicationCalls===2 && wakeCalls===3 && clockCalls===1 && eventRetries===1
       && channelOperations.size===0 && events.length===0, 'live session and wake fixtures complete');
console.log('OCaml menus, viewport and pixel scenes, input, live session, wake and clock, async KV and PCM adapters passed');
