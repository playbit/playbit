// Account-owned binary mutation boundary. Native owns full payload validation;
// this writer originates only the web client's personal-volume decision records.
import { decodeId, encodeId, validId } from './identifier.ts';
import type { PersonalSelection } from './account-record.ts';

export const UNSUPPORTED_ACCOUNT = 'This account requires a newer version of Playbit. Its saved data has been kept; account access is paused.';
export function unsupportedAccount(): Error & { code: string } {
    return Object.assign(Error(UNSUPPORTED_ACCOUNT), { code: 'unsupported-account-semantics' });
}
const magic = new Uint8Array([80, 66, 82, 1]), required = 0xc000, maxBytes = 48 * 1024;
const encoder = new TextEncoder(), decoder = new TextDecoder('utf-8', { fatal: true });
interface Field { tag: number; value: Uint8Array }
const same = (a: Uint8Array, b: Uint8Array): boolean => a.length === b.length && a.every((byte, index) => byte === b[index]);
const invalid = (): never => { throw Error('Invalid binary account record'); };
function join(...values: Uint8Array[]): Uint8Array<ArrayBuffer> {
    const size = values.reduce((sum, value) => sum + value.length, 0);
    if (size > maxBytes) throw Error('Account record capacity exceeded');
    const result = new Uint8Array(size); let offset = 0;
    for (const value of values) { result.set(value, offset); offset += value.length; }
    return result;
}
function number(value: number, width: number): Uint8Array<ArrayBuffer> {
    const result = new Uint8Array(width), view = new DataView(result.buffer);
    if (width === 2) view.setUint16(0, value, true); else view.setUint32(0, value, true);
    return result;
}
function take(bytes: Uint8Array, offset: number, length: number): Uint8Array {
    if (!Number.isSafeInteger(length) || length < 0 || offset + length > bytes.length) return invalid();
    return bytes.subarray(offset, offset + length);
}
function integer(bytes: Uint8Array, offset: number, width: number): number {
    const value = take(bytes, offset, width), view = new DataView(value.buffer, value.byteOffset, value.length);
    return width === 2 ? view.getUint16(0, true) : view.getUint32(0, true);
}
function body(bytes: Uint8Array, kind: number): Uint8Array {
    if (bytes[0] !== kind) return invalid();
    return bytes.subarray(1);
}
function root(bytes: Uint8Array): Uint8Array {
    if (!same(take(bytes, 0, 4), magic)) return invalid();
    return bytes.subarray(4);
}
function fields(bytes: Uint8Array, known?: number[], limit = 4096): Map<number, Field> {
    bytes = body(bytes, 8);
    const result = new Map<number, Field>(); let offset = 0, previous = 0, extensions = 0;
    while (offset < bytes.length) {
        const tag = integer(bytes, offset, 2), id = tag & 0x3fff, length = integer(bytes, offset + 2, 4);
        const value = take(bytes, offset + 6, length);
        if (!id || id <= previous || !length || result.size === limit) return invalid();
        if (known?.includes(id)) { if (tag !== (id | required)) return invalid(); }
        else if (known) {
            if (tag & required) throw unsupportedAccount();
            extensions += length + 6;
            if (extensions > 4096) throw Error('Account extension capacity exceeded');
        }
        result.set(id, { tag, value }); previous = id; offset += 6 + length;
    }
    return result;
}
function get(values: Map<number, Field>, id: number): Uint8Array {
    return values.get(id)?.value ?? invalid();
}
function field(tag: number, value: Uint8Array): Uint8Array<ArrayBuffer> {
    return join(number(tag, 2), number(value.length, 4), value);
}
function object(values: [number, Uint8Array][]): Uint8Array<ArrayBuffer> {
    return join(new Uint8Array([8]), ...values.map(([tag, value]) => field(tag | required, value)));
}
const text = (value: string): Uint8Array<ArrayBuffer> => join(new Uint8Array([5]), encoder.encode(value));
const binary = (value: Uint8Array): Uint8Array<ArrayBuffer> => join(new Uint8Array([6]), value);
const boolean = (value: boolean): Uint8Array<ArrayBuffer> => new Uint8Array([1, Number(value)]);
const readText = (value: Uint8Array): string => decoder.decode(body(value, 5));
function readBoolean(value: Uint8Array): boolean {
    const bytes = body(value, 1);
    if (bytes.length !== 1 || bytes[0] > 1) return invalid();
    return bytes[0] === 1;
}
function sequence(bytes: Uint8Array, limit: number): Uint8Array[] {
    bytes = body(bytes, 7); const result = []; let offset = 0;
    while (offset < bytes.length) {
        const length = integer(bytes, offset, 4), value = take(bytes, offset + 4, length);
        if (!length || result.length === limit) return invalid();
        result.push(value); offset += 4 + length;
    }
    return result;
}
function features(value: Uint8Array): void {
    let previous = '';
    const names = sequence(value, 16);
    for (const name of names) {
        const text = readText(name);
        if (!/^[\x21-\x7e]{1,64}$/.test(text) || text <= previous) return invalid();
        previous = text;
    }
    if (names.length) throw unsupportedAccount();
}
function personalPayload(session: string, grants: PersonalSelection[]): Uint8Array<ArrayBuffer> {
    const items = grants.map(grant => object([[1, binary(decodeId(grant.volume))], [2, boolean(grant.write)], [3, binary(decodeId(grant.incarnation))]]));
    return join(magic, new Uint8Array([10, 5, 0]), object([[1, text(session)], [2, join(new Uint8Array([7]), ...items.map(value => join(number(value.length, 4), value)))]]));
}
function header(session: string, payload: Uint8Array): Uint8Array<ArrayBuffer> {
    return join(magic, object([[1, text('personal_volumes')], [2, text(`session-mounts/${session}`)], [3, boolean(true)],
        [4, new Uint8Array([7])], [5, new Uint8Array([7])], [6, binary(payload)]]));
}
function selection(value: Uint8Array): PersonalSelection {
    const values = fields(value, [1, 2, 3]);
    const volume = body(get(values, 1), 6), incarnation = body(get(values, 3), 6);
    if (volume.length !== 16 || incarnation.length !== 16 || incarnation.every(byte => byte === 0)) return invalid();
    return { volume: encodeId(volume), write: readBoolean(get(values, 2)), incarnation: encodeId(incarnation) };
}
export function decodePersonalRecord(plain: Uint8Array): { session: string; grants: PersonalSelection[]; payload: Uint8Array } {
    if (!plain.length || plain.length > maxBytes) return invalid();
    const values = fields(root(plain), [1, 2, 3, 4, 5, 6], 64);
    if (readText(get(values, 1)) !== 'personal_volumes' || !readBoolean(get(values, 3))) return invalid();
    features(get(values, 4)); features(get(values, 5));
    const payload = body(get(values, 6), 6), variant = body(root(payload), 10);
    if (integer(variant, 0, 2) !== 5) return invalid();
    const record = fields(variant.subarray(2), [1, 2]);
    const session = readText(get(record, 1));
    if (!validId(session, 32) || readText(get(values, 2)) !== `session-mounts/${session}`) return invalid();
    const grants = sequence(get(record, 2), 30).map(selection);
    validate(session, grants);
    return { session, grants, payload };
}
function validate(session: string, grants: PersonalSelection[]): void {
    if (!validId(session, 32) || grants.length > 30) return invalid();
    const volumes = new Set(), incarnations = new Set();
    for (const grant of grants) {
        if (!validId(grant.volume, 16) || !validId(grant.incarnation, 16) || /^1+$/.test(grant.incarnation)
            || typeof grant.write !== 'boolean' || volumes.has(grant.volume) || incarnations.has(grant.incarnation)) return invalid();
        volumes.add(grant.volume); incarnations.add(grant.incarnation);
    }
}
// Exactly the native three-way preservation rule: only corresponding struct
// fields and unchanged containers can safely retain uninterpreted nested bytes.
function merge(old: Uint8Array, known: Uint8Array, next: Uint8Array, depth = 0): Uint8Array {
    if (depth >= 16) throw Error('Account nesting capacity exceeded');
    if (same(old, known)) return next;
    if (same(known, next)) return old;
    if (old[0] === 10 && known[0] === 10 && next[0] === 10
        && integer(old, 1, 2) === integer(known, 1, 2) && integer(old, 1, 2) === integer(next, 1, 2)) {
        return join(old.subarray(0, 3), merge(old.subarray(3), known.subarray(3), next.subarray(3), depth + 1));
    }
    if (old[0] !== 8 || known[0] !== 8 || next[0] !== 8) throw Error('Cannot preserve optional account data through this edit');
    const before = fields(old), projection = fields(known), after = fields(next), result = [];
    for (const id of [...new Set([...before.keys(), ...after.keys()])].sort((a, b) => a - b)) {
        const old = before.get(id), known = projection.get(id), next = after.get(id);
        if (old && next) {
            if (!known) throw Error('Account field identity collision');
            result.push(field(next.tag, merge(old.value, known.value, next.value, depth + 1)));
        } else if (old && !known) result.push(field(old.tag, old.value));
        else if (old && known) { if (!same(old.value, known.value)) throw Error('Edit removes optional account data'); }
        else if (next) result.push(field(next.tag, next.value));
    }
    return join(new Uint8Array([8]), ...result);
}
export function encodePersonalRecord(session: string, grants: PersonalSelection[], original?: Uint8Array | null): Uint8Array<ArrayBuffer> {
    validate(session, grants);
    const nextPayload = personalPayload(session, grants);
    if (!original) return header(session, nextPayload);
    const previous = decodePersonalRecord(original);
    if (previous.session !== session) return invalid();
    const payload = join(magic, merge(root(previous.payload), root(personalPayload(session, previous.grants)), root(nextPayload)));
    return join(magic, merge(root(original), root(header(session, previous.payload)), root(header(session, payload))));
}
