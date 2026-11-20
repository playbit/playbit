// Recovery protocol tests use this revision-aware store. Actual IndexedDB
// durability and transaction aborts are exercised by the web browser suite.
export function recoveryMemory() {
    const values = new Map();
    return {
        async read(account) {
            return structuredClone(values.get(account) ?? { state: undefined, control: { revision: 0n, clock: 0n } });
        },
        async write(account, expected, state) {
            const current = values.get(account)?.control ?? { revision: 0n, clock: 0n };
            if (current.revision !== expected.revision) throw Error('Account state changed');
            values.set(account, structuredClone({ state, control: { revision: current.revision + 1n, clock: current.clock } }));
        },
    };
}
