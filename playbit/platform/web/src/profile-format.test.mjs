import test from 'node:test';
import assert from 'node:assert/strict';
import { admitWebProfile } from './profile-format.ts';
import { storageFormat, validateStorageFormat } from './storage-format.ts';
const bytes = value => new TextEncoder().encode(typeof value === 'string' ? value : JSON.stringify(value));
function directory(failure, records = new Map()) {
    const dir = {
        async *keys() { yield* records.keys(); },
        async getFileHandle(name, options) {
            if (!records.has(name)) {
                if (!options?.create) throw Object.assign(Error('Missing'), { name: 'NotFoundError' });
                records.set(name, new Uint8Array());
            }
            return {
                async getFile() { const data = records.get(name); return { size: data.length, arrayBuffer: async () => data.slice().buffer }; },
                async createSyncAccessHandle() { return {
                    truncate() { records.set(name, new Uint8Array()); },
                    write(data) { if (failure === 'write') throw Error('QuotaExceeded'); records.set(name, data.slice()); return data.length; },
                    flush() { if (failure === 'flush') throw Error('Flush failed'); }, close() {},
                }; },
                async move(next) { if (failure === 'move') throw Error('Publication failed'); records.set(next, records.get(name)); records.delete(name); },
            };
        },
    };
    return { dir, records };
}
test('profile gate preserves unsupported retained bytes and accepts the same JSON meaning', async () => {
    const baseline = storageFormat('playbit.web.profile');
    const reordered = { required_write_features: [], required_read_features: [], revision: 1, format: baseline.format };
    validateStorageFormat(reordered, baseline.format);
    for (const descriptor of [
        { ...baseline, format: 'future.profile' }, { ...baseline, revision: 2 },
        { ...baseline, required_read_features: ['future.authority'] },
        { ...baseline, required_write_features: ['future.authority'] },
        '{"format":"playbit.web.profile","revision":1,"required_read_features":["hidden"],"required_read_features":[],"required_write_features":[]}',
    ]) {
        const { dir, records } = directory(); records.set('profile-format.json', bytes(descriptor)); records.set('profile.json', bytes('retained'));
        const before = structuredClone(records);
        await assert.rejects(admitWebProfile(dir)); assert.deepEqual(records, before);
    }
    const { dir, records } = directory(); records.set('profile-format.json', bytes(reordered));
    assert.equal(await admitWebProfile(dir), false);
    assert.equal(await admitWebProfile(dir, true), true);
    const promoted = JSON.parse(new TextDecoder().decode(records.get('profile-format.json')));
    assert.deepEqual(promoted.required_read_features, ['shared-session.v1']);
    assert.deepEqual(promoted.required_write_features, ['shared-session.v1']);
    assert.equal(await admitWebProfile(dir), true);
    records.delete('profile-format.json'); records.set('account.kvdb', bytes('retained'));
    const before = structuredClone(records);
    await assert.rejects(admitWebProfile(dir)); assert.deepEqual(records, before);
});
test('first Root and Shared publications declare exactly their active features', async () => {
    for (const shared of [false, true]) {
        const { dir, records } = directory();
        assert.equal(await admitWebProfile(dir, shared), shared);
        const descriptor = JSON.parse(new TextDecoder().decode(records.get('profile-format.json')));
        assert.deepEqual(descriptor.required_read_features, shared ? ['shared-session.v1'] : []);
        assert.deepEqual(descriptor.required_write_features, descriptor.required_read_features);
    }
});

test('failed publication preserves both fresh and existing Root profiles until retry', async () => {
    for (const boundary of ['write', 'flush', 'move']) {
        for (const promote of [false, true]) {
            const { dir, records } = directory();
            if (promote) await admitWebProfile(dir);
            const before = records.get('profile-format.json');
            await assert.rejects(admitWebProfile(directory(boundary, records).dir, promote));
            assert.deepEqual(records.get('profile-format.json'), before);
            assert.equal(await admitWebProfile(dir, promote), promote);
            assert.equal(records.size, 1);
            assert.equal(await admitWebProfile(dir), promote);
        }
    }
});
