import { encodeId } from '../src/identifier.ts';
// Cases shared by Node and real web browsers, using the compiled Rust decoder and WebCrypto.
import { accountCipher, openAccountRecord, MAX_ACCOUNT_RECORD_BYTES } from '../src/account-record.ts';
import { deriveSessionKeys } from '../src/invitation.ts';
import { accountPlain, frame, field, object, text, magic, join } from './account-wire-fixture.mjs';
const encoder = new TextEncoder();
const canonical = value => JSON.stringify(value, (_, item) => item && typeof item === 'object' && !Array.isArray(item) ? Object.fromEntries(Object.entries(item).sort(([a], [b]) => a.localeCompare(b))) : item);
const check = (value, message) => { if (!value) throw Error(message); };
const summary = samples => {
    const values = samples.slice().sort((a, b) => a - b);
    return { n: values.length, p50_ms: values[Math.ceil(values.length * .5) - 1],
        p99_ms: values[Math.ceil(values.length * .99) - 1], max_ms: values.at(-1) };
};
export async function accountRecordCases(decode, { benchmark = false } = {}) {
    const account = encodeId(new Uint8Array(16).fill(0x12)), version = encodeId(new Uint8Array(32).fill(0x34));
    const cipher = await accountCipher(account, new Uint8Array(16).fill(3));
    const volume = { id: encodeId(new Uint8Array(16).fill(0x56)), owner: account, name: 'Personal', share_key_with_service: true, personal: true };
    const credentials = { database_key: Array(32).fill(1), blob_key: Array(32).fill(2) };
    const seed = new Uint8Array(16).fill(5), { session_id: id } = await deriveSessionKeys(seed);
    const invitation = { seed: Array.from(seed), session: { id, creator: account, app: { publisher: account, app_id: 'todo' },
        root_volume: volume.id,  resolved_build_id: version,
        selector: { kind: 'exact', build_id: version }, update_authority: 'creator' },
        root: volume, root_credentials: credentials};
    const records = [
        ['settings/display-name', { kind: 'display_name', name: "false" }],
        [`volume/${volume.id}`, { kind: 'volume', volume, credentials }],
        [`session-mounts/${version}`, { kind: 'personal_volumes', session: version, grants: [{ volume: volume.id, write: true, incarnation: encodeId(new Uint8Array(16).fill(0x19)) }] }],
        [`session/${id}`, { kind: 'invitation', app_name: null, invitation }],
        [`session/${id}`, { kind: 'invitation', app_name: 'Notes 雨 🌱', invitation }],
        ['settings/display-name', { kind: 'display_name', name: 'Robin 雨 🌱' }],
    ];
    const open = async (logical, plain, decoder = decode) => {
        const key = await cipher.recordKey(encoder.encode(logical)), clock = 1n;
        return openAccountRecord(cipher, key, clock, await cipher.seal(key, clock, plain), decoder);
    };
    let accepted = 0, rejected = 0;
    for (const [logical, record] of records) {
        const parsed = await open(logical, accountPlain(record));
        check(canonical(parsed.record) === canonical(record), `Record round trip failed: ${record.kind}`);
        check(record.kind !== 'invitation' || parsed.invitation.invitation.session.id === id, 'Missing verified invitation');
        accepted++;
    }
    // These values have valid account encryption. Typed validation must reject
    // them even if kvdb would discard the entry as a last-writer-wins loser.
    const invalid = [
        ['settings/display-name', accountPlain({ kind: 'display_name', name: '' })],
        ['settings/display-name', accountPlain({ kind: 'display_name', name: ' Robin' })],
        ['settings/display-name', accountPlain({ kind: 'display_name', name: 'x'.repeat(257) })],
        ['settings/display-name', accountPlain({ kind: 'display_name', name: 'Ro\nbin' })],
        ['settings/display-name', accountPlain({ kind: 'display_name', name: 1 })],
        ['settings/wrong-key', accountPlain(records[0][1])],
        [`volume/${volume.id}`, accountPlain({ ...records[1][1], credentials: { ...credentials, blob_key: credentials.database_key } })],
        [`volume/${volume.id}`, accountPlain({ ...records[1][1], credentials: { ...credentials, database_key: [1] } })],
        [`volume/${volume.id}`, accountPlain({ ...records[1][1], volume: { ...volume, name: ' ' } })],
        [`session-mounts/${version}`, accountPlain({ ...records[2][1], session: 'invalid' })],
        [`session-mounts/${version}`, accountPlain({ ...records[2][1], grants: Array(2).fill(records[2][1].grants[0]) })],
        [`session/${id}`, accountPlain({ kind: 'invitation', app_name: null, invitation: { ...invitation, seed: Array(16).fill(0) } })],
        [`session/${id}`, accountPlain({ kind: 'invitation', app_name: null, invitation: { ...invitation, seed: [1] } })],
        [`session/${id}`, accountPlain({ kind: 'invitation', app_name: null, invitation: { ...invitation, session: { ...invitation.session, root_volume: encodeId(new Uint8Array(16).fill(0xab)) } } })],
    ];
    const duplicatePayload = join(magic, Uint8Array.of(10, 1, 0), object([1, text('One')]), field(0xc001, text('Two')));
    invalid.push(['settings/display-name', frame('display_name', 'settings/display-name', false, duplicatePayload)]);
    const duplicateHeader = join(accountPlain(records[0][1]), field(0xc006, Uint8Array.of(6, 0)));
    invalid.push(['settings/display-name', duplicateHeader]);
    // Optional whole records have a disjoint non-authoritative key namespace.
    const optional = frame('future.presentation', 'optional/future.presentation/one', false, Uint8Array.of(255, 1));
    check((await open('optional/future.presentation/one', optional)).record === null, 'Optional unknown record projected authority');
    accepted++;
    invalid.push(['settings/display-name', frame('future.presentation', 'settings/display-name', false, Uint8Array.of(255))]);
    invalid.push(['optional/future.presentation/one', frame('future.presentation', 'optional/future.presentation/one', true, Uint8Array.of(255))]);
    invalid.push(['settings/display-name', frame('display_name', 'settings/display-name', false, duplicatePayload, ['account.future.read'])]);
    const omitted = { kind: 'invitation', invitation };
    check((await open(`session/${id}`, accountPlain(omitted))).record.app_name === null, 'Missing app_name did not default to None');
    accepted++;
    for (const [logical, plain] of invalid) {
        let failed = false;
        try { await open(logical, plain); } catch { failed = true; }
        check(failed, `Invalid authenticated account record accepted: ${logical}`); rejected++;
    }
    // Verify authentication precedes the Rust plaintext boundary.
    const key = await cipher.recordKey(encoder.encode(records[0][0]));
    const encrypted = await cipher.seal(key, 1n, accountPlain(records[0][1]));
    encrypted[encrypted.length - 1] ^= 1;
    let decoded = false, failed = false;
    try { await openAccountRecord(cipher, key, 1n, encrypted, () => { decoded = true; throw Error('Unexpected decode'); }); }
    catch { failed = true; }
    check(failed && !decoded, 'Unauthenticated plaintext reached decoder');
    for (const plain of [new Uint8Array(), new Uint8Array(MAX_ACCOUNT_RECORD_BYTES + 1), Uint8Array.of(255)]) {
        let failed = false; try { decode(plain); } catch { failed = true; }
        check(failed, 'Invalid size/encoding accepted'); rejected++;
    }
    const small = accountPlain(records[0][1]);
    const prefix = frame('future.presentation', 'optional/future.presentation/large', false, new Uint8Array());
    const large = frame('future.presentation', 'optional/future.presentation/large', false, new Uint8Array(MAX_ACCOUNT_RECORD_BYTES - prefix.length));
    check(large.length === MAX_ACCOUNT_RECORD_BYTES, 'Wrong maximum size fixture');
    const timings = [];
    for (const plain of [small, large]) {
        const samples = [];
        for (let index = 0; index < (benchmark ? 1000 : 1); index++) {
            const start = benchmark ? performance.now() : 0;
            const normalized = decode(plain);
            if (benchmark) samples.push(performance.now() - start);
            check(normalized.length <= MAX_ACCOUNT_RECORD_BYTES + 256, 'Typed output exceeds bounded scratch');
        }
        if (benchmark) timings.push({ plaintext_bytes: plain.length, ...summary(samples) });
    }
    return { checks: { valid_kinds: accepted, rejected_records: rejected, authentication_before_decode: true,
        signed_invitation_verified: true, duplicate_payload_fields_rejected: true, optional_unknown_retained: true, unsupported_authority_rejected: true },
        boundary: 'Wasm input copy, shared typed decoding and canonical output copy; excludes encryption, storage, AP, guest/UI and presentation', ...(benchmark ? { timings } : {}) };
}

// A decoder-only host must not perform any storage calls during validation.
export async function webAccountRecordCases(options) {
    const module = await WebAssembly.compile(await (await fetch(new URL('../playbit.wasm', import.meta.url))).arrayBuffer());
    const imports = {};
    for (const item of WebAssembly.Module.imports(module)) {
        check(item.kind === 'function', 'Unexpected host import');
        (imports[item.module] ??= {})[item.name] = () => { throw Error(`Decoder performed I/O: ${item.name}`); };
    }
    const { exports: e } = await WebAssembly.instantiate(module, imports);
    return accountRecordCases(plain => {
        const pointer = e.pb_input(plain.length);
        check(pointer || !plain.length, 'Decoder input allocation failed');
        new Uint8Array(e.memory.buffer, pointer, plain.length).set(plain);
        const length = e.pb_account_decode(plain.length);
        if (length < 0) {
            const n = e.pb_error();
            throw Error(new TextDecoder().decode(new Uint8Array(e.memory.buffer, e.pb_output(), n)) || `Host error ${length}`);
        }
        return new Uint8Array(e.memory.buffer, e.pb_output(), length).slice();
    }, options);
}
