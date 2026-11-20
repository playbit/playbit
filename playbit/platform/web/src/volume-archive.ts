import { compareId } from './identifier.ts';
// Original files are sealed inside the pending generation before its index
// records the cut. Discard removes incomplete cuts; later generation cleanup
// preserves this archive directory and the index retains its generation ID.
import type { BlobHash, BlobHashFactory } from './blob.ts';
import { VolumePublication, type PreparedVolume } from './volume-publication.ts';

interface Seal { bytes: number; digest: number[] }
interface Cut { generation: string; entries: string[]; bytes: number; files: Record<string, Seal> }
export type ArchiveCall = <T>(name: string, value: unknown) => Promise<T>;
const MAX_ENTRIES = 128, MAX_FILES = 256, COPY_BYTES = 64 * 1024;
const hex = (bytes: number[]) => bytes.map(byte => byte.toString(16).padStart(2, '0')).join('');
const unhex = (value: string) => Array.from(Uint8Array.from(value.match(/../g)!, byte => parseInt(byte, 16)));

export async function retainRejectedCut(root: FileSystemDirectoryHandle, publication: VolumePublication,
    prepared: PreparedVolume, call: ArchiveCall, hash: BlobHashFactory, yieldReady: () => Promise<void>): Promise<void> {
    const entries: string[] = [];
    let after: string | null = null;
    do {
        publication.check(prepared);
        const [page, next]: [string[], string | null] = await call('pb_volume_rejected_page', after);
        if (entries.length + page.length > MAX_ENTRIES) throw new Error('Offline recovery entry capacity exceeded');
        entries.push(...page);
        if (next !== null && after !== null && compareId(next, after, 15) <= 0) throw new Error('Offline recovery scan made no progress');
        after = next;
        if (after) await yieldReady();
    } while (after);
    if (!entries.length) return;
    entries.sort((a, b) => compareId(a, b, 15));
    const plan = await call<{ available: number; reserve: number; backings: string[] }>('pb_volume_archive_plan', entries);
    if (!prepared.previous) throw new Error('Offline recovery requires an original generation');
    const generation = await root.getDirectoryHandle(publication.directory(prepared.volume, prepared.generation));
    const archive = await generation.getDirectoryHandle('rejected', { create: true });
    const cut: Cut = { generation: prepared.generation, entries, bytes: plan.reserve, files: {} };
    const copy = async (name: string, source: File, expected?: { digest: string; bytes: number }) => {
        publication.check(prepared);
        if (Object.hasOwn(cut.files, name)) return;
        if (Object.keys(cut.files).length === MAX_FILES || !Number.isSafeInteger(source.size)
            || source.size < 0 || source.size > plan.available - cut.bytes) throw new Error('Offline recovery storage capacity exceeded');
        if (expected && source.size !== expected.bytes) throw new Error('Offline blob length differs');
        const file = await (await archive.getFileHandle(name, { create: true })).createSyncAccessHandle();
        let digest: BlobHash | undefined;
        try {
            digest = hash();
            if (file.getSize() !== 0) throw new Error('Offline recovery file already exists');
            for (let offset = 0; offset < source.size; offset += COPY_BYTES) {
                publication.check(prepared);
                const bytes = new Uint8Array(await source.slice(offset, Math.min(offset + COPY_BYTES, source.size)).arrayBuffer());
                if (bytes.length !== Math.min(COPY_BYTES, source.size - offset)) throw new Error('Offline recovery source ended early');
                publication.check(prepared);
                if (file.write(bytes, { at: offset }) !== bytes.length) throw new Error('Offline recovery write ended early');
                digest.update(bytes);
                await yieldReady();
            }
            publication.check(prepared);
            const sealed = digest.finish();
            if (expected && sealed !== expected.digest) throw new Error('Offline blob digest differs');
            file.truncate(source.size); file.flush();
            cut.files[name] = { bytes: source.size, digest: unhex(sealed) };
            cut.bytes += source.size;
        } finally { try { digest?.close(); } finally { file.close(); } }
    };
    const index = await publication.file(root, prepared.volume, prepared.previous, 'index', false);
    await copy('index.kvdb', await index.getFile());
    for (const entry of plan.backings) {
        const location = await call<string | null>('pb_volume_local_location', entry);
        const file = await publication.file(root, prepared.volume, location ?? prepared.previous, entry, false);
        await copy(`${entry}.kvdb`, await file.getFile());
    }
    for (const entry of entries) {
        let after: string | null = null;
        do {
            const [blobs, next]: [[number[], number][], string | null] = await call('pb_volume_archive_blobs', { entry, after });
            for (const [bytes, length] of blobs) {
                const digest = hex(bytes), name = `${digest}.blob`;
                if (Object.hasOwn(cut.files, name)) continue;
                let source: File;
                try { source = await (await root.getFileHandle(`blob-${digest}`)).getFile(); }
                catch (error) { if ((error as DOMException).name === 'NotFoundError') continue; throw error; }
                await copy(name, source, { digest, bytes: length });
            }
            if (next !== null && after !== null && next <= after) throw new Error('Offline blob history made no progress');
            after = next;
            if (after) await yieldReady();
        } while (after);
    }
    publication.check(prepared);
    await call('pb_volume_archive_commit', cut);
    publication.check(prepared);
}
