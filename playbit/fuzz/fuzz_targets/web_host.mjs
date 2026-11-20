// Deterministic length mutator for the actual compiled Wasm host. Called by
// host-policy.test.mjs; this is not a native libFuzzer/ASan target. The existing
// abi_reader, package_contract and store_contract targets mutate payload bytes.
import assert from 'node:assert/strict';
export function fuzzWebHostLengths(exports, iterations, seed = 0x6d2b79f5) {
    const calls = [
        length => exports.pb_initialize(length),
        pointer => exports.pb_web_object_create(pointer, 0, 0, 0),
        pointer => exports.pb_web_object_edit(pointer, 0, 0, 0),
        pointer => exports.pb_web_object_read(pointer, 0, 0, 0),
        pointer => exports.pb_web_object_query(pointer, 0),
        length => exports.pb_web_file_begin(0xffffffff, 0xffffffff, 23, 0n, length, 1),
        length => exports.pb_web_file_begin(0xffffffff, 0xffffffff, 9, 0n, length, 0),
        length => exports.pb_package(length),
        length => exports.pb_blob_digest(length), length => exports.pb_adopt_session(length),
        length => exports.pb_begin_adoption(length), length => exports.pb_register_personal(length),
        length => exports.pb_select_personal(length), length => exports.pb_catalog_page(length),
        length => exports.pb_sync_step(length), length => exports.pb_live_apply(length),
        ...['open', 'stage', 'entries', 'apply', 'read', 'next', 'decode'].map(
            name => length => exports[`pb_account_${name}`](length)),
    ];
    const random = () => { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; return seed >>> 0; };
    for (let index = 0; index < iterations; index++) {
        const allocated = random() % 256;
        exports.pb_input(allocated);
        const bounds = [allocated + 1, 65536, 262144, 1048576, 67108864, 0xffffffff];
        const length = index % 2 ? bounds[random() % bounds.length] : allocated + 1 + random() % 1048576;
        assert(calls[index % calls.length](length) < 0, `call ${index % calls.length}: ${allocated}/${length}`);
    }
    return iterations;
}
