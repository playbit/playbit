// Isolated test origin only. The caller supplies its attached OPFS workspace.
import { openAutomaticStorage } from '../src/automatic-storage.ts';
import { encodeId } from '../src/identifier.ts';
const check = (value, message) => { if (!value) throw Error(message); };
const id = width => encodeId(crypto.getRandomValues(new Uint8Array(width)));
async function rejected(run, matches) {
    try { await run(); } catch (error) { if (matches(error)) return; throw error; }
    throw Error('Automatic volume unexpectedly opened');
}
export async function automaticStorageCases(resources) {
    const volume = id(16), other = id(16), account = id(16), build = id(32), updated = id(32);
    const root = await navigator.storage.getDirectory();
    const open = (address = volume, owner = account, version = build) => openAutomaticStorage(address, owner, version, resources, async () => {});
    const incompatible = error => error.code === 'incompatible-storage-format';
    let opened;
    try {
        opened = await open();
        await opened.close(); opened = await open(volume, account, updated);
        check(opened.creation_build === build && opened.publication.current(volume) === null
            && !opened.publication.initialization(volume), 'Interrupted first use lost its original build before index publication');
        const { directory, credentials, replica_ids, publication } = opened;
        await rejected(() => open(), error => error.status === -12);
        const prepared = publication.prepare(volume); await publication.create(directory, prepared);
        publication.beginInitialization(volume, build, 0);
        const file = await publication.file(directory, volume, prepared.generation, 'index', true);
        const work = await file.createSyncAccessHandle();
        try { work.write(Uint8Array.of(7, 4)); work.flush(); } finally { work.close(); }
        publication.publish(prepared, () => {}, () => {});
        publication.finishInitialization(volume, build);
        const interrupted = await directory.getFileHandle('blob-stage-3.tmp', { create: true });
        const published = await directory.getFileHandle('blob-' + 'ab'.repeat(32), { create: true });
        const unknown = await directory.getFileHandle('blob-stage-unrecognized.tmp', { create: true });
        await opened.close(); opened = undefined;

        await rejected(() => open(volume, id(16)), incompatible);
        check((await interrupted.getFile()).size === 0, 'Mismatched owner removed private data');
        opened = await open(volume, account, updated);
        await rejected(() => directory.getFileHandle(interrupted.name), e => e.name === 'NotFoundError');
        check((await published.getFile()).size === 0 && (await unknown.getFile()).size === 0, 'Removed a non-stage file');
        check(opened.creation_build === build && opened.publication.initialization(volume)?.complete, 'Lost original scope initialization');
        check(JSON.stringify(opened.credentials) === JSON.stringify(credentials)
            && JSON.stringify(opened.replica_ids) === JSON.stringify(replica_ids), 'Lost private scope identity');
        const generation = opened.publication.current(volume);
        const restored = await opened.publication.file(opened.directory, volume, generation, 'index', false);
        check(new Uint8Array(await (await restored.getFile()).arrayBuffer()).join() === '7,4', 'Lost committed volume bytes on reopen');
        await opened.close(); opened = undefined;

        const second = await root.getDirectoryHandle(`playbit-automatic-${other}`, { create: true });
        const retained = await second.getFileHandle('unrecognized-data', { create: true });
        await rejected(() => open(other), incompatible);
        check((await second.getFileHandle(retained.name)).name === retained.name, 'Cleared unrecognized storage');
        await second.removeEntry(retained.name);
        await second.getFileHandle('volume-format.pending', { create: true });
        await second.getFileHandle('volume-generations.kvdb', { create: true });
        opened = await open(other);
        check(opened.publication.current(other) === null
            && JSON.stringify(opened.credentials) !== JSON.stringify(credentials), 'Distinct scope reused another scope’s data or keys');
        await opened.close(); opened = undefined;

        const marker = await directory.getFileHandle('volume-format');
        const handle = await marker.createSyncAccessHandle();
        try { handle.write(Uint8Array.of(0), { at: 0 }); handle.flush(); } finally { handle.close(); }
        await rejected(() => open(), incompatible);
        check((await restored.getFile()).size === 2, 'Rejected scope marker overwrote the backing');
        return 'Busy on contention; original build, keys, replica identities and files reopen; independent volume; unknown data retained; interrupted descriptor publication retried';
    } finally {
        await opened?.close();
        for (const address of [volume, other]) {
            try { await root.removeEntry(`playbit-automatic-${address}`, { recursive: true }); }
            catch (cause) { if (cause.name !== 'NotFoundError') throw cause; }
        }
    }
}
