import { encodeId, decodeId, validId } from './identifier.ts';
import { replicationDiagnostic } from './live-replication.ts';
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
import { stripTypeScriptTypes } from 'node:module';
// Types are replaced by whitespace, so the slice offsets below still hold.
const source = stripTypeScriptTypes(await readFile(new URL('./app.ts', import.meta.url), 'utf8'), { mode: 'strip' });
const functions = source.slice(source.indexOf('async function refreshAccountPending()'), source.indexOf('window.addEventListener("online"'));
test('live app status retains pending or unknown account intent and local action failures', async () => {
    let pending = {}, failure = false, text; const calls = [];
    const context = vm.createContext({ encodeId, decodeId, validId, engineFailure:undefined, guestFailure:undefined, opened: false, refreshRecoveredAppVisibility: async () => {}, call: async method => calls.push(method), accountPending: undefined, replicationState: {kind:'live'}, enrollment: {account:'fixture'},
        recoveryVisible:false, personalProgress:undefined, status: value => {text=value;}, error:{textContent:'Access could not be connected'},
        pendingAccountRecord: async (kind, account) => {assert.equal(kind,'personal'); assert.equal(account,'fixture'); if(failure) throw Error('IDB failed'); return pending;} });
    vm.runInContext(functions, context);
    await context.refreshAccountPending();
    assert.match(text,/account synchronization pending/);
    assert.deepEqual(calls, []);
    context.opened = true;
    await context.refreshAccountPending();
    assert.deepEqual(calls, ['account_changed']);
    assert.equal(context.error.textContent,'Access could not be connected');
    context.showReplication({kind:'live'});
    assert.match(text,/account synchronization pending/);
    pending=null; await context.refreshAccountPending();
    assert.equal(text,'Up to date · Shared changes received and saved');
    failure=true; await context.refreshAccountPending();
    assert.match(text,/status unavailable/);
    context.recoveryVisible=true; text='Account key required';
    await context.refreshAccountPending(); assert.equal(text,'Account key required');
});

test('the signed-in recovery list follows account projections before a guest opens and keeps invitation actions', async () => {
    const app = { publisher: 'publisher', app_id: 'list' }, retained = { session: { app, id: 'session' } };
    let removed = false, joined = false;
    const makeNode = () => ({ dataset: {}, children: [], isConnected: true,
        querySelectorAll() { return this.children; },
        replaceChildren(...children) { for (const old of this.children) old.isConnected = false; this.children = children; } });
    const onboarding = { ...makeNode(), querySelector(id) { return this.children.find(child => '#' + child.id === id); },
        append(...children) { this.children.push(...children); } };
    const calls = [];
    const context = vm.createContext({
        opened: false, invitation: { session_id: 'fresh-invitation' }, enrollment: { account: 'account' }, onboarding, status() {},
        selectedChannel: 'public', recoveryListAccount: undefined, error: {}, recoveryVisible: true, guestFailure: undefined, replicationState: undefined,
        recoveryClient: { catalog: async () => ({}), state: async () => ({ catalog: { invitations: [retained] } }) },
        removedAppsIn: async () => removed ? new Set(['publisher/list']) : new Set(), pendingAccountRecord: async () => null,
        document: { createElement: makeNode }, textElement: makeNode,
        action: (textContent, onclick) => ({ ...makeNode(), textContent, onclick }), replicaCapability: () => ({ volume: { name: 'Little List' } }),
        call: async (method, args) => calls.push([method, args.account]), retainedInvitation: async entry => ({ session_id: entry.session.id }),
        showJoin: () => { joined = true; }, persistInvitation() {}, showInvitationHeader() {}, paused: false, canJoin: true,
    });
    vm.runInContext(source.slice(source.indexOf('let recoveryRender ='), source.indexOf('function recoveryAction()')) + functions, context);
    await context.showRecoveredApps();
    assert.deepEqual(calls, [['watch_recovery', 'account']]);
    const rows = onboarding.querySelector('#recovered-app-list'), original = rows.children[0];
    assert.equal(original.textContent, 'Little List');
    await context.refreshAccountPending(); assert.equal(rows.children[0], original, 'unrelated notices preserve focus');
    removed = true;
    await original.onclick();
    assert.equal(joined, false, 'a committed removal wins before its notice reaches the visible row');
    assert.equal(context.invitation.session_id, 'fresh-invitation');
    await context.refreshAccountPending(); assert.equal(rows.children.length, 0);
    assert.equal(onboarding.children.at(-1).textContent, 'Continue invitation');
    removed = false; await context.refreshAccountPending(); assert.equal(rows.children.length, 1);
    await rows.children[0].onclick();
    assert.equal(joined, true); assert.equal(context.invitation.session_id, 'session');
    assert.equal(calls.length, 1, 'the recovery list does not start a guest');
});

test('transient reconnect keeps local-save status and terminal failures retain their action', () => {
    let text;
    const document = { documentElement: { dataset: {} } };
    const context = vm.createContext({ encodeId, decodeId, validId, document, replicationDiagnostic, engineFailure:undefined,guestFailure:undefined,replicationState:undefined,recoveryVisible:false,personalProgress:undefined,
        status:value=>text=value,error:{textContent:'Connection lost'}});
    vm.runInContext(functions,context);
    context.showReplication({kind:'reconnecting',stage:'checkpoint',code:'ap-transport',message:'Connection lost'});
    assert.match(text,/Changes saved on this web browser · Reconnecting/);
    assert.equal(context.error.textContent,'');
    assert.match(document.documentElement.dataset.replicationFailure, /^checkpoint:.*\(ap-transport\)$/);
    context.showReplication({kind:'catching_up'});
    assert.match(document.documentElement.dataset.replicationFailure, /^checkpoint:/, 'recovery progress retains the last failure');
    context.showReplication({kind:'action_required',message:'Sign-in expired. Request another email code'});
    assert.equal(context.error.textContent,'Sign-in expired. Request another email code');
});

// A healthy replica says nothing about the halted executable. Retain the app's
// failure until replacement/reload, even when account or replica notices arrive.
test('guest failure survives live replication and account status notifications', async () => {
    let text;
    const context = vm.createContext({ encodeId, decodeId, validId, engineFailure:undefined, guestFailure:'Crash Demo is halted. Reload it to continue.',
        opened:false, refreshRecoveredAppVisibility: async () => {}, replicationState:undefined, recoveryVisible:false, personalProgress:undefined,
        enrollment:{account:'fixture'}, pendingAccountRecord:async()=>null,
        status:value=>text=value, error:{textContent:''} });
    vm.runInContext(functions,context);
    for (const kind of ['live','catching_up','reconnecting','action_required']) {
        context.showReplication({kind});
        assert.equal(text,'App needs attention');
        assert.equal(context.error.textContent,context.guestFailure);
    }
    context.showReplication({kind:'live'});
    await context.refreshAccountPending(); context.accountNoticeFailed();
    assert.equal(text,'App needs attention');
    assert.equal(context.error.textContent,context.guestFailure);
    context.guestFailure=undefined;
    context.showReplication({kind:'live'});
    assert.equal(context.error.textContent,'');
});

test('a halted canvas stops scheduling animation callbacks', async () => {
    let requested = 0, inputs = 0;
    const context = vm.createContext({ encodeId, decodeId, validId, engineFailure:undefined, guestFailure:'Halted', pageClosing:false, appClosing:false, appClosed:false, inputCount:0,
        renderedScene:{kind:'column', children:[{kind:'canvas', id:1, frames:1}]}, document:{hidden:false},
        input:async()=>inputs++, words:()=>new Uint8Array(),
        cancelAnimationFrame:()=>{}, requestAnimationFrame:()=>++requested });
    vm.runInContext(source.slice(source.indexOf('let frameRequest'),
        source.indexOf('const statusElement =')),context);
    context.updateFrames(); await context.animate(100);
    assert.equal(inputs,0); assert.equal(requested,0);
    assert.equal(vm.runInContext('frameTime', context),0);
    context.guestFailure=undefined;
    context.appClosing=true; context.updateFrames(); await context.animate(150);
    assert.equal(inputs,0); assert.equal(requested,0);
    context.appClosing=false; context.appClosed=true; context.updateFrames(); await context.animate(175);
    assert.equal(inputs,0); assert.equal(requested,0);
    context.appClosed=false; context.updateFrames();
    await context.animate(200);
    assert.equal(inputs,1); assert.equal(requested,2);
});
