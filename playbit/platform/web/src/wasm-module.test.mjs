import test from "node:test";
import assert from "node:assert/strict";
import { boundedGuest } from "./wasm-module.ts";
const module = section => Uint8Array.from([0,97,115,109,1,0,0,0,...section,7,10,1,6,109,101,109,111,114,121,2,0]);
test("the real web browser engine cannot grow a bounded guest past 16 MiB", async () => {
    const original = module([5,3,1,0,1]);
    const { instance } = await WebAssembly.instantiate(boundedGuest(original));
    assert.equal(instance.exports.memory.grow(255), 1);
    assert.throws(() => instance.exports.memory.grow(1), RangeError);
});
test("pixel guests can grow to 32 MiB, honoring a smaller declared maximum", async () => {
    const unconstrained = module([5,3,1,0,1]);
    const allowed = (await WebAssembly.instantiate(boundedGuest(unconstrained, true))).instance.exports.memory;
    assert.equal(allowed.grow(511), 1);
    assert.equal(allowed.buffer.byteLength, 32 * 1024 * 1024);
    assert.throws(() => allowed.grow(1), RangeError);

    const capped = module([5,5,1,1,1,128,1]); // The package declares 128 pages.
    const smaller = (await WebAssembly.instantiate(boundedGuest(capped, true))).instance.exports.memory;
    assert.equal(smaller.grow(127), 1);
    assert.throws(() => smaller.grow(1), RangeError);

    assert.throws(() => boundedGuest(module([5,4,1,0,129,4]), true), /32 MiB/);
});
test("implicit starts, shared memory and excessive minimums fail before execution", () => {
    assert.throws(() => boundedGuest(module([5,4,1,3,1,2])), /Shared/);
    assert.throws(() => boundedGuest(module([5,4,1,0,129,2])), /16 MiB/);
    assert.throws(() => boundedGuest(module([5,3,1,0,1,8,1,0])), /start function/);
});
