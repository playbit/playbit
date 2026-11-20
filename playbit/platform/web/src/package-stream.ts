import type { ApOperation, ApResponse } from './ap-protocol.ts';

export const MAX_PACKAGE_BYTES = 512 * 1024 * 1024;
export const MAX_PACKAGE_WASM_BYTES = 64 * 1024 * 1024;
export const PACKAGE_CHUNK_BYTES = 65536;
const TRANSFER_MS = 600000;

interface Transport {
    begin(operation: ApOperation): Promise<number>;
    streamFinish(handle: number): Promise<unknown>;
    streamResponse(handle: number): Promise<ApResponse>;
    streamRead(handle: number, maxBody: number): Promise<Uint8Array>;
    streamClose(handle: number): void | Promise<unknown>;
}

// Pull one bounded frame only after the previous frame reached OPFS. Neither
// the account worker nor the page retains a complete-package transfer buffer.
export async function* downloadPackage(transport: Transport, operation: ApOperation): AsyncGenerator<Uint8Array> {
    const expires = Date.now() + TRANSFER_MS;
    const handle = await transport.begin(operation);
    const close = async () => { try { await transport.streamClose(handle); } catch { /* Preserve the transfer result. */ } };
    const timer = setTimeout(() => { void close(); }, Math.max(1, expires - Date.now()));
    try {
        await transport.streamFinish(handle);
        const response = await transport.streamResponse(handle);
        const length = (response.data as { content_length?: unknown } | undefined)?.content_length;
        if (typeof length !== 'number' || !Number.isSafeInteger(length) || length < 0 || length > MAX_PACKAGE_BYTES) {
            throw new Error('App exceeds download limit');
        }
        let received = 0;
        while (received < length) {
            if (Date.now() >= expires) throw new Error('App download timed out');
            const bytes = await transport.streamRead(handle, PACKAGE_CHUNK_BYTES);
            if (!bytes.length || bytes.length > PACKAGE_CHUNK_BYTES || bytes.length > length - received) {
                throw new Error('Invalid app download byte count');
            }
            received += bytes.length;
            yield bytes;
        }
        await transport.streamResponse(handle);
        if (Date.now() >= expires) throw new Error('App download timed out');
    } finally {
        clearTimeout(timer);
        await close();
    }
}

export async function* fetchPackage(url: string): AsyncGenerator<Uint8Array> {
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(new Error('App download timed out')), TRANSFER_MS);
    let reader: ReadableStreamBYOBReader | undefined;
    try {
        const response = await fetch(url, { signal: abort.signal });
        if (!response.ok || !response.body) throw new Error('Cannot load app package');
        reader = response.body.getReader({ mode: 'byob' });
        let buffer = new ArrayBuffer(PACKAGE_CHUNK_BYTES);
        for (;;) {
            const { value, done } = await reader.read(new Uint8Array(buffer));
            if (value?.byteLength) {
                buffer = value.buffer;
                yield value;
            } else if (!done) throw new Error('Empty app download chunk');
            if (done) return;
        }
    } finally {
        abort.abort();
        clearTimeout(timer);
        if (reader) {
            await reader.cancel().catch(() => {});
            reader.releaseLock();
        }
    }
}

export async function writePackage(
    file: Pick<FileSystemSyncAccessHandle, 'write' | 'truncate' | 'flush'>,
    source: AsyncIterable<Uint8Array>,
    yieldWork: () => Promise<void>,
): Promise<void> {
    try {
        file.truncate(0);
        let offset = 0;
        for await (const bytes of source) {
            if (bytes.length > MAX_PACKAGE_BYTES - offset) throw new Error('App exceeds download limit');
            for (let start = 0; start < bytes.length; start += PACKAGE_CHUNK_BYTES) {
                const chunk = bytes.subarray(start, start + PACKAGE_CHUNK_BYTES);
                if (file.write(chunk, { at: offset }) !== chunk.length) throw new Error('Incomplete app write; web browser storage may be full');
                offset += chunk.length;
                await yieldWork();
            }
        }
        file.flush();
    } catch (error) {
        if ((error as { name?: unknown })?.name === 'QuotaExceededError') {
            throw new Error('Not enough web browser storage to install this app', { cause: error });
        }
        throw error;
    }
}
