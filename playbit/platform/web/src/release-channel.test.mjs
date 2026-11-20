import test from 'node:test';
import assert from 'node:assert/strict';
import { channel, configuration, deploymentChannel, workerURL, workerChannel } from './release-channel.ts';
import { parseSessionLink, sessionLink, sessionUri } from './invitation.ts';
import { nativeInvitation } from './native-handoff.ts';

test('deployment and workers bind one channel before credentials can open', () => {
    for (const selected of ['public','testing']) {
        const value=configuration(selected);
        assert.equal(deploymentChannel(selected,value.website),selected);
        const url=workerURL('./ap-worker.js',value.website+'/assets/fixture/playbit.js',selected);
        assert.equal(workerChannel(url.href),selected);
        assert.equal(url.searchParams.get('channel'),selected);
        assert.throws(()=>workerChannel(value.website+'/ap-worker.js'),/missing/);
        assert.throws(()=>workerChannel(url.href+'&channel='+selected),/ambiguous/);
    }
    assert.equal(deploymentChannel('public','http://localhost:8787'),'public');
    for (const value of ['',undefined,null,'Testing','preview']) assert.throws(()=>channel(value),/public or testing/);
    assert.throws(()=>deploymentChannel('testing','https://playbit.app'),/different environment/);
    assert.throws(()=>deploymentChannel('public','https://testing.playbit.app'),/different environment/);
    assert.throws(()=>deploymentChannel('testing','http://localhost:8787'),/different environment/);
    assert.throws(()=>workerChannel('https://playbit.app/ap-worker.js?channel=testing'),/different environment/);
});

test('links retain authority and reject another channel before key derivation', () => {
    const seed=new Uint8Array(16).fill(7);
    for (const selected of ['public','testing']) {
        const other=selected==='public'?'testing':'public';
        const url=sessionLink(seed,selected),uri=sessionUri(seed,selected);
        assert.deepEqual(parseSessionLink(url,selected),seed);
        assert.deepEqual(parseSessionLink(uri,selected),seed);
        assert.throws(()=>parseSessionLink(url,other),/belongs to/);
        assert.throws(()=>parseSessionLink(uri,other),/belongs to/);
        assert.equal(nativeInvitation('/s',new URL(url).hash,selected),uri);
    }
});
