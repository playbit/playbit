import { accountAuthorityAllowed } from './account-outbox.ts';
import { storageFormatAvailable } from './storage-format.ts';

// Quarantine revokes account-derived handles, including cached grants. Local
// presentation, scheduling and owner cleanup remain usable to report failure.
export function accountOperationStatus(account: string | undefined, opcode: number): number {
    if (storageFormatAvailable() && (account === undefined || accountAuthorityAllowed(account))) return 0;
    switch (opcode) {
        case 0x1: case 0x2: case 0x3: case 0x4: case 0x9:
        case 0xb: case 0xc: case 0xd: case 0xe: case 0xf: case 0x10: case 0x11: case 0x14: case 0x15:
        case 0x15a: case 0x15b: case 0x15c: return 0;
        default: return -2;
    }
}
