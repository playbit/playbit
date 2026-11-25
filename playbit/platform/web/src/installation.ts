import { encodeId, validId } from './identifier.ts';
import { INSTALLATION_DATABASE, openMetadata, watchMetadata } from './storage-format.ts';
// One durable origin identity per web browser installation, independent of accounts,
// sessions and replica IDs. The creation transaction publishes the store and
// identity atomically. Missing or damaged identities fail; they are never replaced.


export async function installationIdentity(): Promise<string> {
    const database = await openMetadata(INSTALLATION_DATABASE, 'identity', 'playbit.web.installation', store => {
        const bytes = crypto.getRandomValues(new Uint8Array(16));
        if (bytes.every(byte => byte === 0)) throw Error('Invalid installation identity');
        store.add(encodeId(bytes), 'installation');
    });
    try {
        await watchMetadata(INSTALLATION_DATABASE, 'identity', 'playbit.web.installation');
        return await new Promise<string>((resolve, reject) => {
            const transaction = database.transaction('identity', 'readonly');
            const request = transaction.objectStore('identity').get('installation');
            transaction.oncomplete = () => {
                const value: unknown = request.result;
                if (typeof value !== 'string' || !validId(value, 16) || /^1+$/.test(value)) {
                    reject(Error('Corrupt web browser installation identity'));
                } else resolve(value);
            };
            transaction.onabort = () => reject(transaction.error ?? Error('Web browser installation storage failed'));
        });
    } finally {
        database.close();
    }
}

// Every session on this origin observes one monotonic signer history.
// Verification and comparison run synchronously in the admission transaction.
export async function admitPublisherStatus(publisher: string, signer: number[], status: number[],
    keepPrevious: (previous: number[] | undefined) => boolean): Promise<number[]> {
    const database = await openMetadata(INSTALLATION_DATABASE, 'identity', 'playbit.web.installation');
    const key = `publisher/${publisher}/${encodeId(Uint8Array.from(signer))}`;
    try {
        return await new Promise<number[]>((resolve, reject) => {
            const tx = database.transaction('identity', 'readwrite');
            const store = tx.objectStore('identity'), previous = store.get(key);
            const count = store.count(IDBKeyRange.bound('publisher/', 'publisher/\uffff'));
            let selected = status, failure: unknown;
            count.onsuccess = () => {
                try {
                    if (keepPrevious(previous.result)) selected = previous.result;
                    else {
                        if (!previous.result && count.result >= 1024) throw Error('Publisher trust storage is full');
                        store.put(status, key);
                    }
                } catch (error) { failure = error; tx.abort(); }
            };
            tx.oncomplete = () => resolve(selected);
            tx.onabort = () => reject(failure ?? tx.error ?? Error('Publisher trust storage failed'));
        });
    } finally { database.close(); }
}
