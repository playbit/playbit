// Lifecycle VM fixtures own compatible metadata and no quarantined account.
// Real IndexedDB/OPFS transitions and authority fences use the web browser probes.
import { AbiError } from '../src/guest-memory.ts';
import { incompatibleStorage } from '../src/storage-format.ts';
import { unsupportedAccount } from '../src/account-format.ts';

export function compatibleWorkerStorage() {
    let observer;
    return {
        AbiError, incompatibleStorage, unsupportedAccount,
        observeStorageFormat(listener) {
            if (observer) throw Error('Storage format already has an observer in this worker');
            observer = listener;
        },
        storageFormatAvailable: () => true,
        requireStorageFormat() {},
        async admitWebProfile() {},
        accountOperationStatus: () => 0,
        async requireAccountAuthority() {},
        async withAccountAuthority(_account, callback) { return callback(); },
    };
}
