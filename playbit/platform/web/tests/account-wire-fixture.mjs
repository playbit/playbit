// Independent test construction of the documented account wire format. This is
// deliberately separate from the web client's personal-decision production writer.
import { decodeId } from '../src/identifier.ts';
export const magic = Uint8Array.of(80, 66, 82, 1);
export const join = (...values) => Uint8Array.from(values.flatMap(value => Array.from(value)));
const u16 = value => Uint8Array.of(value & 255, value >>> 8);
const u32 = value => Uint8Array.of(value & 255, value >>> 8 & 255, value >>> 16 & 255, value >>> 24 & 255);
export const field = (tag, value) => join(u16(tag), u32(value.length), value);
export const object = (...items) => join(Uint8Array.of(8), ...items.map(([tag, value]) => field(0xc000 | tag, value)));
export const text = value => typeof value === 'string' ? join(Uint8Array.of(5), new TextEncoder().encode(value)) : Uint8Array.of(2, 1, 0, 0, 0, 0, 0, 0, 0);
export const binary = value => join(Uint8Array.of(6), value);
const boolean = value => Uint8Array.of(1, Number(value));
const id = value => binary(decodeId(value));
const variant = (tag, ...items) => join(Uint8Array.of(10), u16(tag), object(...items));
const sequence = values => join(Uint8Array.of(7), ...values.map(value => join(u32(value.length), value)));
const credentials = value => object([1, binary(value.database_key)], [2, binary(value.blob_key)]);
const volume = value => object([1, id(value.id)], [2, text(value.name)], [3, id(value.owner)],
    [4, boolean(value.share_key_with_service)], [5, boolean(value.personal)]);
const selector = value => variant(value.kind === 'exact' ? 1 : 2, [1, text(value.build_id ?? value.deployment_id)]);
const session = value => object([1, object([1, id(value.app.publisher)], [2, text(value.app.app_id)])], [2, text(value.id)],
    [3, selector(value.selector)], [4, text(value.resolved_build_id)], [5, id(value.root_volume)], [6, id(value.creator)],
    [7, variant(value.update_authority === 'creator' ? 1 : 2)]);
const invitation = value => object([1, binary(value.seed)], [2, session(value.session)], [3, volume(value.root)], [4, credentials(value.root_credentials)]);
export function frame(kind, logicalKey, authority, payload, requiredRead = [], requiredWrite = []) {
    return join(magic, object([1, text(kind)], [2, text(logicalKey)], [3, boolean(authority)],
        [4, sequence(requiredRead.map(text))], [5, sequence(requiredWrite.map(text))], [6, binary(payload)]));
}
export function accountPlain(record) {
    let key, payload;
    switch (record.kind) {
    case 'app_visibility':
        key = `optional/app_visibility/${record.app.publisher}/${record.app.app_id}`;
        payload = variant(8, [1, object([1, id(record.app.publisher)], [2, text(record.app.app_id)])], [2, boolean(record.removed)]);
        break;
    case 'display_name': key = 'settings/display-name'; payload = variant(1, [1, text(record.name)]); break;
    case 'volume': key = `volume/${record.volume.id}`; payload = variant(2, [1, volume(record.volume)], [2, credentials(record.credentials)]); break;
    case 'volume_deleted': key = `deleted-volume/${record.volume}`; payload = variant(3, [1, id(record.volume)]); break;
    case 'invitation': {
        key = `session/${record.invitation.session.id}`;
        payload = variant(4, [1, invitation(record.invitation)]);
        if ('app_name' in record) payload = join(payload, field(2, record.app_name === null ? Uint8Array.of(0) : text(record.app_name)));
        break;
    }
    case 'personal_volumes': key = `session-mounts/${record.session}`; payload = variant(5, [1, text(record.session)],
        [2, sequence(record.grants.map(grant => object([1, id(grant.volume)], [2, boolean(grant.write)], [3, id(grant.incarnation)])))]); break;
    default: throw Error('Unsupported test kind');
    }
    return frame(record.kind, key, record.kind !== 'display_name' && record.kind !== 'app_visibility', join(magic, payload));
}
