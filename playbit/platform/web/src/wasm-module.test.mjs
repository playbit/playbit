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
test("implicit starts, shared memory and excessive minimums fail before execution", () => {
    assert.throws(() => boundedGuest(module([5,4,1,3,1,2])), /Shared/);
    assert.throws(() => boundedGuest(module([5,4,1,0,129,2])), /16 MiB/);
    assert.throws(() => boundedGuest(module([5,3,1,0,1,8,1,0])), /start function/);
});
