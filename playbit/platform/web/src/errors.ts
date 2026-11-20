// Only transport failures and explicit replica gaps admit automatic recovery.
export type Failure = Error & { code: string };
// Errors reach these helpers as `unknown` from catch clauses; the optional chains
// below read fields off any value, so the cast keeps primitives on the same path.
type Errorish = { message?: unknown; code?: unknown } | null | undefined;
export function failure(code: string, message: string): Failure { return Object.assign(new Error(message), { code }); }
export function transportFailure(error: unknown): Failure {
    return failure('ap-transport', String((error as Errorish)?.message ?? error));
}
export function retryable(error: unknown): boolean { return (['ap-transport', 'replica-gap'] as readonly unknown[]).includes((error as Errorish)?.code); }
export function corruptStoreOpenMessage(message: string): boolean {
    return /^(Corrupt database|Corrupt local profile|corrupt Archipelago store|corrupt database|incompatible database version|store error: corrupt database|store error: incompatible database version)/.test(message);
}
