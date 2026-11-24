import { encodeId } from '../src/identifier.ts';
// Production account owner with actual Rust kvdb, OPFS, IndexedDB and WebCrypto.
// This is component evidence; transport and UI are qualified separately.
import { AccountReplica } from '../src/account-replica.ts';
import { accountPlain, frame } from './account-wire-fixture.mjs';
import { OpfsStore, OpfsResources } from '../src/opfs-store.ts';
import { createStorageBridge } from '../src/opfs-bridge.ts';
import { accountCipher } from '../src/account-record.ts';
import { personalSelections } from '../src/personal.ts';
import { accountOperationStatus } from '../src/guest-authority.ts';
import { bindOperations } from '../src/guest-imports.ts';
import { openAccountMetadata, readAccountMutation, commitAccountMutation, adoptAccountMutation, pendingAccountRecord, removedAppsIn, accountAuthorityAllowed, withAccountAuthority } from '../src/account-outbox.ts';
const encoder = new TextEncoder(), decoder = new TextDecoder();
const check = (condition, message) => { if (!condition) throw Error(message); };
const hex = bytes => Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('');
const summary = values => {
    values = values.slice().sort((a, b) => a - b);
    return { n: values.length, p50_ms: values[Math.ceil(values.length * .5) - 1],
        p99_ms: values[Math.ceil(values.length * .99) - 1], max_ms: values.at(-1) };
};
// Exercise the recovery worker with an actual account database at the other
// end of the pull, then deliver a newer record on the same subscription.
async function recoveryListCase(account, sender, app, restore) {
    const worker = new Worker('/runtime-worker.js?channel=public', { type: 'module' });
    const done = Promise.withResolvers();
    const epoch = new Uint8Array(16).fill(7);
    let sequence = 0, live = 0, reader, subscribed = false;
    const frame = (kind, delta = new Uint8Array()) => {
        const bytes = new Uint8Array(52 + delta.length), view = new DataView(bytes.buffer);
        bytes[0] = 1; bytes[1] = kind; bytes.set(epoch, 4);
        view.setBigUint64(20, BigInt(++sequence), true); view.setBigUint64(28, 1n, true);
        bytes.set(delta, 52); return bytes;
    };
    const respond = (id, result) => worker.postMessage({ apReply: { id, result } });
    const dispatch = async ({ data }) => {
        if (data.error || data.engineFailure) throw Error(data.error ?? data.engineFailure);
        if (data.id === 2) { check(data.result.closed, 'Recovery worker did not retire'); done.resolve(); return; }
        if (data.replication) {
            if (data.replication.kind === 'catching_up') return;
            check(data.replication.kind === 'live', JSON.stringify(data.replication));
            const removed = await removedAppsIn(account, [app]);
            if (++live === 1) {
                check(removed.size === 1, 'Fresh recovery list missed the removal in its initial account snapshot');
                respond(reader, frame(1, sender.call('pb_account_stage', restore.bytes)));
            } else {
                check(!removed.size, 'Open recovery list missed the live rejoin');
                worker.postMessage({ id: 2, method: 'close' });
            }
        }
        if (!data.ap) return;
        const { id, method, args } = data.ap;
        let result;
        switch (method) {
            case 'connect': result = {}; break;
            case 'subscribeReplica':
                check(!subscribed && args[0].op === 'subscribe_account', 'Recovery list requested a guest subscription');
                subscribed = true; result = { handle: 1, ack: { epoch: encodeId(epoch), sequence: 0n, policy_revision: 1n } }; break;
            case 'begin':
                check(subscribed && args[0].op === 'sync_account' && args[0].direction === 'pull', 'Recovery list requested unexpected synchronization');
                sender.invoke('pb_sync_begin', 0, 1); result = 2; break;
            case 'streamRead': result = sender.invoke('pb_sync_output'); break;
            case 'streamWrite': sender.call('pb_sync_step', args[1]); break;
            case 'streamResponse': result = {}; break;
            case 'streamFinish': sender.invoke('pb_sync_finish', 1); break;
            case 'streamReplica': reader = id; return;
            case 'checkpointReplica': respond(reader, frame(2)); break;
            case 'streamClose': if (args[0] === 1) respond(reader, new Uint8Array()); break;
            case 'accountChanged': check(args[0] === account, 'Recovery notice named another account'); break;
            default: throw Error('Unexpected recovery-list operation: ' + method);
        }
        respond(id, result);
    };
    try {
        worker.onmessage = event => dispatch(event).catch(done.reject);
        worker.onerror = event => done.reject(Error(event.message));
        worker.postMessage({ id: 1, method: 'watch_recovery', args: { account } });
        await done.promise;
    } finally { worker.terminate(); }
}
export async function webAccountReplicaCases(accounts, { benchmark = false } = {}) {
    const account = encodeId(crypto.getRandomValues(new Uint8Array(16))); accounts.push(account);
    const secret = new Uint8Array(16).fill(19), version = encodeId(new Uint8Array(32).fill(0x12));
    const root = await navigator.storage.getDirectory(), name = `account-replica-${crypto.randomUUID()}`;
    const directory = await root.getDirectoryHandle(name, { create: true });
    const module = await WebAssembly.compile(await (await fetch('/playbit.wasm')).arrayBuffer());
    const metadata = await openAccountMetadata('recovery'), replicas = [];
    const cipher = await accountCipher(account, secret), times = [];
    let maxDelta = 0;
    const open = async (prefix, ownerAccount = account) => {
        const handles = [];
        const file = async suffix => {
            const handle = await (await directory.getFileHandle(`${prefix}-${suffix}`, { create: true })).createSyncAccessHandle();
            handles.push(handle); return handle;
        };
        const resources = new OpfsResources([await file('writer0'), await file('writer1')], [await file('stage0'), await file('stage1')]);
        const store = await OpfsStore.open(await file('account'), resources, async () => {});
        let host;
        const bridge = createStorageBridge(new Map([[0, store]]), resources, () => host.exports.memory,
            bytes => crypto.getRandomValues(bytes), () => Math.floor(Date.now() / 1000), error => { throw Error(error); });
        host = await WebAssembly.instantiate(module, { opfs: { ...bridge.imports, engine_panic() { throw new Error("Unexpected engine panic"); } } });
        const e = host.exports;
        const invoke = (operation, ...args) => {
            const length = e[operation](...args);
            if (length < 0) { const n = e.pb_error(); throw Error(decoder.decode(new Uint8Array(e.memory.buffer, e.pb_output(), n))); }
            return new Uint8Array(e.memory.buffer, e.pb_output(), length).slice();
        };
        const call = (operation, bytes = new Uint8Array(), current = () => true) => {
            check(current(), 'Retired account call');
            const pointer = e.pb_input(bytes.length);
            check(pointer || !bytes.length, 'Wasm input allocation failed');
            new Uint8Array(e.memory.buffer, pointer, bytes.length).set(bytes);
            return invoke(operation, bytes.length);
        };
        call('pb_account_open', encoder.encode(JSON.stringify({ account: ownerAccount, replica_id: prefix === 'sender' ? 7 : 8 })));
        const owner = await AccountReplica.open(ownerAccount, call);
        const result = { owner, call, invoke, close() {
            owner.close(); bridge.close(); store.close();
            for (const handle of handles) { try { handle.close(); } catch { /* Resource owner already closed it. */ } }
        } };
        replicas.push(result); return result;
    };
    const record = async (logicalKey, clock, value, corrupt = false) => {
        const key = await cipher.recordKey(encoder.encode(logicalKey));
        const ciphertext = await cipher.seal(key, clock, value instanceof Uint8Array ? value : accountPlain(value));
        if (corrupt) ciphertext[ciphertext.length - 1] ^= 1;
        const bytes = new Uint8Array(40 + ciphertext.length);
        bytes.set(key); new DataView(bytes.buffer).setBigUint64(32, clock, true); bytes.set(ciphertext, 40);
        return { key, bytes };
    };
    const apply = async (receiver, delta) => {
        maxDelta = Math.max(maxDelta, delta.length);
        const start = benchmark ? performance.now() : 0;
        const displayName = await receiver.owner.apply(delta, () => true);
        if (benchmark) times.push(performance.now() - start);
        return displayName;
    };
    const rejected = async (receiver, delta) => {
        let error;
        try { await apply(receiver, delta); } catch (caught) { error = caught; }
        check(error, 'Malformed account receipt was accepted');
    };
    try {
        check(await AccountReplica.open(account, () => { throw Error('Locked account reached Wasm'); }) === null,
            'Locked private credentials prevented invitation-only use');
        const initial = await readAccountMutation(metadata, account);
        await commitAccountMutation(metadata, account, initial.control, { secret: Array.from(secret), acknowledged: true,
            revision: 1, catalog: { account, volumes: [], invitations: [] } });
        const sender = await open('sender'); let receiver = await open('receiver');
        const app = { publisher: account, app_id: 'todo' }, appName = `${account}/todo`, appKey = `optional/app_visibility/${appName}`;
        if (!benchmark) {
            const listed = { ...app, app_id: 'recovery-list' }, key = `optional/app_visibility/${account}/recovery-list`;
            sender.call('pb_account_stage', (await record(key, 1n, { kind: 'app_visibility', app: listed, removed: true })).bytes);
            const restore = await record(key, 2n, { kind: 'app_visibility', app: listed, removed: false });
            try { await recoveryListCase(account, sender, listed, restore); }
            finally { await root.removeEntry(`playbit-recovery-list.${account}`, { recursive: true }).catch(error => { if (error.name !== 'NotFoundError') throw error; }); }
        }
        const removal = await record(appKey, 1n, { kind: 'app_visibility', app, removed: true });
        const removalDelta = sender.call('pb_account_stage', removal.bytes);
        const beforeRemoval = await readAccountMutation(metadata, account);
        // Old readers retained the optional ciphertext before they understood it.
        await adoptAccountMutation(metadata, account, beforeRemoval.control, beforeRemoval.state,
            { key: hex(removal.key), clock: 1n, ciphertext: removal.bytes.slice(40) });
        check(!(await removedAppsIn(account, [app])).size, 'An opaque record changed the app list');
        await apply(receiver, removalDelta);
        check((await removedAppsIn(account, [app, { ...app, app_id: 'notes' }])).has(appName),
            'Replay of a previously unknown app removal did not hide the app');
        const restore = await record(appKey, 2n, { kind: 'app_visibility', app, removed: false });
        await apply(receiver, sender.call('pb_account_stage', restore.bytes));
        await apply(receiver, removalDelta);
        check(!(await removedAppsIn(account, [app])).size, 'A stale removal hid a rejoined app');
        await personalSelections(account, version, []);
        const head = await sender.owner.head(() => true);
        check(head?.source === 'account' && head.database === 'index' && head.bytes.length, 'Account intent did not stage');
        await apply(receiver, head.bytes);
        check(await pendingAccountRecord('personal', account), 'Exact replay cleared unacknowledged intent');
        await personalSelections(account, version, [{ volume: encodeId(new Uint8Array(16).fill(0x34)), write: false }]);
        await sender.owner.acknowledge(head.ticket.account, () => true);
        check((await pendingAccountRecord('personal', account)).clock > head.ticket.account.clock, 'Old receipt cleared newer intent');
        const newer = await sender.owner.head(() => true);
        await apply(receiver, newer.bytes);
        await sender.owner.acknowledge(newer.ticket.account, () => false);
        check(await pendingAccountRecord('personal', account), 'Retired publication cleared intent');
        await sender.owner.acknowledge(newer.ticket.account, () => true);
        check(!await pendingAccountRecord('personal', account), 'Exact receipt left intent pending');
        const stale = await record('settings/display-name', 10n, { kind: 'display_name', name: 'Forged stale name' }, true);
        const staleDelta = sender.call('pb_account_stage', stale.bytes);
        const olderName = await record('settings/display-name', 15n, { kind: 'display_name', name: 'Old name' });
        const olderDelta = sender.call('pb_account_stage', olderName.bytes);
        const nameRecord = await record('settings/display-name', 20n, { kind: 'display_name', name: 'Remote name' });
        const validDelta = sender.call('pb_account_stage', nameRecord.bytes);
        check(await apply(receiver, validDelta), 'Display name receipt did not wake publication');
        const receivedName = await receiver.owner.displayNameRecord(() => true);
        check(receivedName?.name === 'Remote name' && receivedName.clock === 20n && hex(receivedName.key) === hex(nameRecord.key)
            && hex(receivedName.ciphertext) === hex(nameRecord.bytes.subarray(40)),
            'Central publication lost the name, clock, or exact winning envelope');
        await apply(receiver, olderDelta);
        const afterLateDelivery = await receiver.owner.displayNameRecord(() => true);
        check(afterLateDelivery && hex(afterLateDelivery.ciphertext) === hex(receivedName.ciphertext), 'An older account delta supplied a losing name for publication');
        const before = receiver.call('pb_account_read', nameRecord.key);
        await rejected(receiver, staleDelta);
        check(hex(receiver.call('pb_account_read', nameRecord.key)) === hex(before), 'Invalid losing record altered the account tree');
        const mismatch = await record('settings/wrong-name', 30n, { kind: 'display_name', name: 'Wrong logical key' });
        await rejected(receiver, sender.call('pb_account_stage', mismatch.bytes));
        const { state, control } = await readAccountMutation(metadata, account);
        const profile = await receiver.owner.profile();
        check(state.display_name === 'Remote name' && profile.displayName === state.display_name && profile.revision === control.revision,
            'Guest profile did not read the committed remote name and revision together');
        receiver.close(); receiver = await open('receiver');
        check(hex(receiver.call('pb_account_read', nameRecord.key)) === hex(before), 'Restart lost durable receipt');
        await receiver.owner.adoptTree(() => true);
        const existingName = await receiver.owner.displayNameRecord(() => true);
        check(existingName?.name === receivedName.name && existingName.clock === receivedName.clock
            && hex(existingName.ciphertext) === hex(receivedName.ciphertext), 'Upgrade did not backfill the previously projected name');
        const replay = await record('settings/display-name', 40n, { kind: 'display_name', name: 'Repair after interruption' });
        receiver.call('pb_account_apply', sender.call('pb_account_stage', replay.bytes));
        await receiver.owner.adoptTree(() => true);
        check((await readAccountMutation(metadata, account)).state.display_name === 'Repair after interruption', 'Restart replay did not repair projection');
        for (let index = 0; index < (benchmark ? 1000 : 4); index++) {
            const value = await record('settings/display-name', BigInt(100 + index), { kind: 'display_name', name: String(Boolean(index % 2)) });
            await apply(receiver, sender.call('pb_account_stage', value.bytes));
        }
        const final = (await readAccountMutation(metadata, account)).state;
        check(final.display_name === "true" && final.acknowledged && final.revision === 1,
            'Account receipt damaged unlocked recovery state');
        const optionalKey = 'optional/future.presentation/one';
        const optional = await record(optionalKey, 2000n, frame('future.presentation', optionalKey, false, Uint8Array.of(255, 7)));
        await apply(receiver, sender.call('pb_account_stage', optional.bytes));
        const retainedOptional = receiver.call('pb_account_read', optional.key);
        check(retainedOptional.length === optional.bytes.length + 4, 'Unknown optional ciphertext was not retained');
        check((await readAccountMutation(metadata, account)).state.display_name === final.display_name, 'Unknown optional record changed projection');
        // Actual AccountReplica staging holds real shared Web Locks while its
        // synchronous Wasm call waits on the serial worker actor. Quarantine in
        // another Worker queues exclusive authority before that actor resumes.
        const lockAccount = encodeId(crypto.getRandomValues(new Uint8Array(16))); accounts.push(lockAccount);
        const lockState = await readAccountMutation(metadata, lockAccount);
        await commitAccountMutation(metadata, lockAccount, lockState.control, { secret: Array.from(secret), acknowledged: true,
            revision: 1, catalog: { account: lockAccount, volumes: [], invitations: [] } });
        const contended = await open('lock-order', lockAccount);
        await personalSelections(lockAccount, version, []);
        const intent = await pendingAccountRecord('personal', lockAccount), effects = [];
        const gate = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
        const blocked = gate(), release = gate(), stageHeld = gate(), enqueueStage = gate(), exclusiveQueued = gate(), exclusiveDone = gate();
        let actor = Promise.resolve(), holdDecode = true;
        const step = callback => { const next = actor.catch(() => {}).then(callback); actor = next.catch(() => {}); return next; };
        const originalCall = contended.owner.call;
        contended.owner.call = async (name, bytes, current) => {
            if (name === 'pb_account_stage') { stageHeld.resolve(); await enqueueStage.promise; }
            const result = await step(() => originalCall(name, bytes, current));
            if (name === 'pb_account_decode' && holdDecode) {
                holdDecode = false; void step(async () => { blocked.resolve(); await release.promise; });
            }
            if (name === 'pb_account_stage') effects.push('stage');
            return result;
        };
        const staging = contended.owner.stage(intent, () => true);
        await blocked.promise; await stageHeld.promise;
        const guestStep = withAccountAuthority(lockAccount, () => step(() => effects.push('guest')));
        const quarantine = new Worker(new URL('./account-outbox-browser.js', import.meta.url), { type: 'module' });
        try {
            quarantine.onmessage = ({ data }) => {
                if (data.error) { exclusiveQueued.resolve(data.error); exclusiveDone.resolve(data.error); }
                if (data.quarantineQueued) exclusiveQueued.resolve();
                if (data.quarantined) exclusiveDone.resolve();
            };
            quarantine.onerror = event => { exclusiveQueued.resolve(event.message); exclusiveDone.resolve(event.message); };
            quarantine.postMessage({ quarantineWrite: { account: lockAccount, record: intent } });
            const queueError = await exclusiveQueued.promise; check(!queueError, queueError);
            const locks = await navigator.locks.query();
            check(locks.pending.some(lock => lock.name === `playbit-account-authority:${lockAccount}` && lock.mode === 'exclusive'), 'Quarantine did not queue behind staged authority');
            let lateRan = false;
            const late = withAccountAuthority(lockAccount, () => step(() => { lateRan = true; }));
            const lateResult = late.then(() => false, error => error.code === 'unsupported-account-semantics');
            enqueueStage.resolve(); release.resolve();
            const [, , quarantineError] = await Promise.all([staging, guestStep, exclusiveDone.promise]);
            check(!quarantineError, quarantineError);
            check(await lateResult && !lateRan, 'New guest work passed queued quarantine');
            check(effects.includes('stage') && effects.includes('guest'), 'Compatible queued work did not finish before quarantine');
        } finally { enqueueStage.resolve(); release.resolve(); quarantine.terminate(); }
        const afterQuarantine = new Worker(new URL('./account-outbox-browser.js', import.meta.url), { type: 'module' });
        try {
            const persisted = await new Promise((resolve, reject) => {
                afterQuarantine.onmessage = ({ data }) => resolve(data);
                afterQuarantine.onerror = event => reject(Error(event.message));
                afterQuarantine.postMessage({ quarantineCheck: lockAccount });
            });
            check(persisted.quarantined && persisted.code === 'unsupported-account-semantics', 'New worker admitted quarantined authority');
        } finally { afterQuarantine.terminate(); }
        await personalSelections(account, version, [{ volume: encodeId(new Uint8Array(16).fill(0x34)), write: true }]);
        const pendingBeforeQuarantine = await pendingAccountRecord('personal', account);
        let dispatched = 0;
        const guest = bindOperations(new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0)), {
            admitOperation: opcode => accountOperationStatus(account, opcode),
            pb_kv_put_begin() { dispatched++; return 0; },
            pb_file_dispose() { return 0; },
        });
        check(guest.pb_syscall(0x144, 0n, 0n, 0n, 0n, 0n, 0n) === 0n && dispatched === 1, 'Compatible account guest dispatch was blocked');
        const beforeQuarantine = receiver.call('pb_account_read', nameRecord.key);
        const mandatory = await record(optionalKey, 2001n, frame('future.presentation', optionalKey, true, Uint8Array.of(255, 7)));
        await rejected(receiver, sender.call('pb_account_stage', mandatory.bytes));
        check(!accountAuthorityAllowed(account), 'Cached authority remained available after unsupported mandatory semantics');
        check(guest.pb_syscall(0x144, 0n, 0n, 0n, 0n, 0n, 0n) === -2n && dispatched === 1, 'Compiled guest scalar bridge invoked a cached-authority handler after quarantine');
        check(guest.pb_syscall(0x15c, 0n, 0n, 0n, 0n, 0n, 0n) === 0n, 'Quarantine prevented owner cleanup');
        check(hex(receiver.call('pb_account_read', optional.key)) === hex(retainedOptional)
            && hex(receiver.call('pb_account_read', nameRecord.key)) === hex(beforeQuarantine), 'Unsupported authority changed retained account tree');
        check((await pendingAccountRecord('personal', account)).clock === pendingBeforeQuarantine.clock, 'Quarantine lost pending consent intent');
        let consentRefused = false;
        try { await personalSelections(account); } catch (error) { consentRefused = error.code === 'unsupported-account-semantics'; }
        check(consentRefused, 'Retained consent remained usable after quarantine');
        const restarted = new Worker(new URL('./account-outbox-browser.js', import.meta.url), { type: 'module' });
        const persisted = await new Promise((resolve, reject) => {
            restarted.onmessage = ({ data }) => resolve(data);
            restarted.onerror = event => reject(Error(event.message));
            restarted.postMessage({ quarantineCheck: account });
        });
        restarted.terminate();
        check(persisted.quarantined === true && persisted.code === 'unsupported-account-semantics', 'A new worker lost the persistent account quarantine');
        return { checks: 'real account owner; recovery-list snapshot, live rejoin and teardown without opening a guest; durable consent outbox; newer intent survives old ACK; retired ACK; authenticated losing records; logical-key binding; OPFS restart; interrupted projection replay; typed metadata receipt; optional ciphertext retained; shared-authority/actor ordering and cross-worker exclusive quarantine; new requests and new workers refuse quarantined authority; unsupported authority retains tree/intent and fences consent after new-worker restart',
            max_delta_bytes: maxDelta, ...(benchmark ? { apply_and_project: summary(times) } : {}) };
    } finally {
        metadata.database.close();
        for (const replica of replicas) replica.close();
        await root.removeEntry(name, { recursive: true });
    }
}
