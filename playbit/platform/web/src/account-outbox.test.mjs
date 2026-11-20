import test from 'node:test';
import assert from 'node:assert/strict';
import { nextAccountClock } from './account-outbox.ts';

test('account intent clock matches kvdb epoch, rollover and observed-clock rules', () => {
    const epoch = 1728000000;
    assert.equal(nextAccountClock(0n, epoch - 1), 1n << 32n);
    assert.equal(nextAccountClock(0n, epoch + 100), 100n << 32n);
    const current = (100n << 32n) + 7n;
    assert.equal(nextAccountClock(current, epoch + 100), current + 1n);
    assert.equal(nextAccountClock(current, epoch + 50), current + 1n);
    assert.equal(nextAccountClock(current, epoch + 101), 101n << 32n);
    assert.equal(nextAccountClock((100n << 32n) + 0xffffffffn, epoch + 100), 101n << 32n);
    assert.equal(nextAccountClock(0xfffffffffffffffbn, epoch), 0xfffffffffffffffcn);
    assert.throws(() => nextAccountClock(0xfffffffffffffffcn, epoch));
    for (const value of [-1n, 0x10000000000000000n, 1, NaN]) assert.throws(() => nextAccountClock(value, epoch));
    for (const time of [NaN, Infinity, 1.5]) assert.throws(() => nextAccountClock(0n, time));
});
