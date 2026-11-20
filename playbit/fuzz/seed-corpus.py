#!/usr/bin/env python3
"""Deterministic valid frames so fuzzing reaches nested decoders and transitions."""
from pathlib import Path
import struct
import runpy
import tomllib

root = Path(__file__).resolve().parent / 'corpus'

def save(target, name, data):
    directory = root / target
    directory.mkdir(parents=True, exist_ok=True)
    (directory / name).write_bytes(data)

# Packed trace inputs and trusted event blocks exercise framing independently.
import zlib
trace_event = struct.pack('<QQQIIHB5x', 2, 1, 0, 0, 0, 0, 2)
save('trace_decode', 'event-end', trace_event)
save('trace_decode', 'end-before-start', trace_event + struct.pack('<QQQIIHB5x', 1, 0, 0, 0, 0, 0, 1))
trace_payload = struct.pack('<IIQ', 1, 1, 1) + trace_event
save('trace_decode', 'trusted-event', trace_payload)
trace_header = bytearray(struct.pack('<4sBBHIIQQQQQQ', b'PBTB', 13, 0, 1,
                                    len(trace_payload), 0, 1, 1, 1, 1, 1, 0))
trace_block = trace_header + trace_payload
struct.pack_into('<I', trace_block, 12, zlib.crc32(trace_block))
save('trace_decode', 'event-block', trace_block)
segment = bytearray(b'PBTRAC01' + struct.pack('<QQQQII', 1, 1, 1, 48, 0, 0))
struct.pack_into('<I', segment, 40, zlib.crc32(segment[:40]))
save('trace_decode', 'segment', segment)

def u32(value):
    return struct.pack('<I', value)

def record(kind, identity=0, payload=b''):
    return bytes([kind]) + u32(identity) + u32(len(payload)) + payload

column = record(1)
end = record(4)
text = record(2, payload=b'\x00hello')
scenes = {
    'empty-column': b'PBV1' + column + end,
    'text': b'PBV1' + column + text + end,
    'button': b'PBV1' + column + record(3, 1, b'\x00\x00press') + end,
    'depth-limit': b'PBV1' + column * 32 + text + end * 32,
    'record-limit': b'PBV1' + column + text * 2046 + end,
    'menu': b'PBV1' + column + record(11, 1, b'\x00' + u32(1) + b'm' + u32(1)
            + b'\x01' + u32(2) + b'\x00' + u32(1) + b'a') + end,
    'description': b'PBV1' + column + record(20, payload=struct.pack('<II', 1, 1)
            + bytes([1]) * 32 + u32(10) + u32(10)) + record(21, payload=b'image') + end,
}
# Generate the canvas seeds locally; corpus outputs are never source files.
for mode, name in [(0, 'static'), (1, 'continuous'), (2, 'invalid-mode')]:
    payload = struct.pack('<4I', 320, 240, mode, 0)
    scenes[f'canvas-{name}'] = b'PBV1' + column + record(22, 17, payload) + end
scenes['canvas-decorative'] = (b'PBV1' + column
    + record(22, 0, struct.pack('<4I', 320, 240, 0, 0)) + end)
for name, label in [('unicode', '日本語 →'.encode()), ('invalid-utf8', b'\xff'), ('newline', b'\n')]:
    item = struct.pack('<9I', 1, 2, 3, 100, 24, 0xffffffff, 16, 1, len(label)) + label
    payload = struct.pack('<4I', 320, 240, 0, 1) + item
    scenes[f'canvas-label-{name}'] = b'PBV1' + column + record(22, 17, payload) + end
for name, data in scenes.items():
    save('decode_view', name, data)
for index in range(14):
    save('abi_reader', str(index), bytes((index + n) % 14 for n in range(32))
         + u32(1) + b'a' + bytes(range(256)))
save('abi_reader', 'surface', struct.pack('<IIfI', 368, 448, 1.0, 4))
# Scalar operation harness: public u32 opcode, six u64 arguments, guest memory.
def abi_record(memory, pointer, size):
    struct.pack_into('<II', memory, pointer, size, 0)

def buffer(memory, offset, pointer, capacity, stride=None):
    struct.pack_into('<III', memory, offset, pointer, capacity, 0)
    if stride is not None:
        struct.pack_into('<I', memory, offset + 12, stride)
        for index in range(capacity):
            abi_record(memory, pointer + index * stride, stride)

def operation_memory():
    memory = bytearray(16384)
    for pointer, value in [(16, 2), (32, 2), (48, 3)]:
        memory[pointer:pointer+16] = bytes([value]) * 16
    for pointer, value in [(64, 1), (112, 2)]:
        abi_record(memory, pointer, 32)
        struct.pack_into('<Q', memory, pointer + 8, 1 | (1 << 32))
        memory[pointer+16:pointer+31] = bytes([value]) * 15
    memory[192:196] = b'test'
    return memory

def operation_seed(opcode, args, memory):
    return u32(opcode) + struct.pack('<6Q', *(args + [0] * (6-len(args)))) + memory

handle = 2 | (1 << 32)
operation_cases = {
    1:[512], 2:[192,4], 3:[192,len(scenes['text'])], 4:[512,8192,64],
    5:[512], 6:[512], 7:[512], 9:[2], 10:[1024,8,88,512], 11:[1000], 12:[],
    0x102:[64,512], 0x103:[handle,512], 0x104:[64,0,0,48,512], 0x105:[handle],
    0x106:[handle,192,4,512], 0x107:[handle,512],
    0x114:[64,16,256,1,40,512], 0x115:[handle,512],
    0x116:[64,112,512], 0x117:[3 | (1 << 32),512], 0x122:[512], 0x123:[4 | (1 << 32),512],
    0x124:[handle,0,16,512], 0x125:[handle,8192,16,512],
    0x126:[handle,0,192,4,512], 0x127:[handle],
    0x130:[64,32,48,1,512], 0x131:[handle,512],
    0x132:[64,32,256,1,40,512], 0x133:[handle],
    0x134:[64,256,512], 0x135:[handle,512],
    0x136:[64,32,48,512], 0x137:[handle,512],
    0x140:[64,256,0,0,512], 0x141:[handle,8192,4096,512],
    0x142:[64,192,4,256,512], 0x143:[handle,8192,4096,512],
    0x144:[64,192,4,192,4,512], 0x145:[handle],
    0x146:[64,192,4,512], 0x147:[handle],
    0x150:[handle,192,4,1,512], 0x151:[handle,512],
    0x152:[handle,512], 0x153:[handle,512], 0x154:[handle,0,512], 0x155:[handle,512],
    0x156:[handle,0,512], 0x157:[handle,512], 0x158:[handle,512], 0x159:[handle,512],
    0x15a:[handle], 0x15b:[handle], 0x15c:[handle],
}
for opcode in (0x102, 0x104, 0x106, 0x114, 0x116, 0x122, 0x124, 0x126,
               0x130, 0x132, 0x134, 0x136, 0x140, 0x142, 0x144, 0x146,
               0x150, 0x152, 0x154, 0x156, 0x158, 0x151, 0x157):
    operation_cases[opcode].pop()
for opcode, args in operation_cases.items():
    memory = operation_memory()
    if opcode == 3:
        memory[192:192+len(scenes['text'])] = scenes['text']
    if opcode in (1, 6):
        abi_record(memory, 512, 36)
        buffer(memory, 520, 1024, 32, 16 if opcode == 1 else 24)
        buffer(memory, 536, 8192, 4096)
    elif opcode in (4, 5, 0x107, 0x123, 0x153, 0x155, 0x159):
        size = {4:24, 5:104, 0x107:32, 0x123:40, 0x153:32, 0x155:24, 0x159:28}[opcode]
        abi_record(memory, 512, size)
        if opcode == 0x159:
            buffer(memory, 528, 8192, 255)
    elif opcode == 7:
        abi_record(memory, 512, 64)
        buffer(memory, 528, 1024, 2, 80)
        buffer(memory, 544, 4096, 64, 16)
        buffer(memory, 560, 8192, 4096)
    elif opcode == 10:
        for index in range(8):
            abi_record(memory, 1024 + index * 88, 88)
    elif opcode == 0x103:
        abi_record(memory, 512, 52)
        buffer(memory, 520, 1024, 8, 48)
        buffer(memory, 536, 4096, 64, 56)
        buffer(memory, 552, 8192, 4096)
    elif opcode == 0x131:
        abi_record(memory, 512, 68)
        buffer(memory, 536, 1024, 8, 40)
        buffer(memory, 552, 4096, 16, 80)
        buffer(memory, 568, 8192, 4096)
    elif opcode == 0x135:
        abi_record(memory, 512, 92)
        buffer(memory, 544, 1024, 8, 48)
        buffer(memory, 560, 2048, 16, 40)
        buffer(memory, 576, 4096, 16, 80)
        buffer(memory, 592, 8192, 4096)
    elif opcode == 0x137:
        abi_record(memory, 512, 40)
        struct.pack_into('<I', memory, 520, 1536)
        abi_record(memory, 1536, 168)
        buffer(memory, 524, 4096, 16, 80)
        buffer(memory, 540, 8192, 4096)
    if opcode in (0x114, 0x132):
        abi_record(memory, 256, 40)
        memory[264:280] = bytes([3]) * 16
        struct.pack_into('<IIII', memory, 280, 320, 1, 80, 1)
        abi_record(memory, 320, 80)
        struct.pack_into('<IIq', memory, 328, 3, 0, 42)
    elif opcode == 0x134:
        abi_record(memory, 256, 60)
        memory[264:280] = bytes([2]) * 16
        struct.pack_into('<I', memory, 300, 64)
    elif opcode in (0x140, 0x142):
        abi_record(memory, 256, 56 if opcode == 0x140 else 64)
        struct.pack_into('<I', memory, 304 if opcode == 0x140 else 272, 64)
    save('wasm_operations', f'{opcode:x}-valid', operation_seed(opcode, args, memory))
    for argument in range(len(args)):
        for invalid in [0, 0xffffffff, 0xffffffffffffffff]:
            changed = args.copy()
            changed[argument] = invalid
            save('wasm_operations', f'{opcode:x}-{argument}-{invalid}', operation_seed(opcode, changed, memory))

# A retained pending Selection clears only its valid initialized extent.
memory = operation_memory()
abi_record(memory, 512, 48)
struct.pack_into('<I', memory, 516, 0xffffffff)
memory[552:560] = bytes([0xa5]) * 8
save('wasm_operations', 'selection-future-output', operation_seed(0x123, [4 | (1 << 32),512], memory))
for name, data in scenes.items():
    save('wasm_operations', 'view-' + name, operation_seed(3, [16,len(data)], bytearray(16) + data))
save('wasm_operations', 'unknown-opcode', u32(0xffffffff) + bytes([255]) * 48)

# AP service operation fixtures also drive the Rust validation contract tests.
import json
requests = json.loads((Path(__file__).resolve().parent.parent / 'tests/fixtures/ap-requests.json').read_text())
for request in requests:
    save('ap_codec', request['op'], json.dumps(request).encode())
baseline = json.loads((Path(__file__).resolve().parent.parent / 'tests/fixtures/ap-baseline.json').read_text())
for case in baseline['cases']:
    save('ap_codec', 'baseline-' + case['name'], case['request'].encode())
for kind in [0, 1, 2, 3, 4, 200, 255]:
    save('ap_codec', f'frame-{kind}', u32(16) + bytes([kind, 0, 0, 1]) + b'x' + bytes(7))
save('ap_codec', 'hello', u32(49) + bytes([0, 1, 0, 0]) + bytes(8) + bytes([1]) * 32 + b'x')
# Structurally valid registration replies reach borrowed-field decoding. These
# public fixture bytes are not certificates or authentication credentials.
registration = bytes([72, 32, 4, 0]) + bytes([1]) * 72 + b'a' * 32 + b'rootinstance'
save('ap_codec', 'registration-response', u32(16 + len(registration))
     + bytes([1, 1, 0, 0]) + bytes(8) + registration)
for status in (0, 2, 3, 4):
    save('ap_codec', f'registration-status-{status}', u32(20)
         + bytes([1, status, 0, 0]) + bytes(12))

# Mandatory wire envelope is distinct from the borrowed credential payload above.
for kind, status, payload in [(0, 0, bytes([1])*72), (1, 1, registration), (1, 2, bytes(4)), (5, 0, bytes(32))]:
    timing = json.dumps([{'stage':'ap.operation','duration_ns':'18446744073709551615'}]).encode()
    body = len(timing).to_bytes(2,'little') + timing + payload
    save('ap_codec', f'authentication-timing-{kind}-{status}', u32(16+len(body)) + bytes([kind,status,0,0]) + bytes(8) + body)

# Service replies exercise envelope, operation correlation and error handling.
for name, reply in [
    ('success', {'ok': True, 'operation_id': '12' * 16, 'data': {'receipt': 7}}),
    ('unrelated', {'ok': True, 'operation_id': '34' * 16}),
    ('rejected', {'ok': False, 'operation_id': '12' * 16, 'error': 'access denied'}),
]:
    reply['timing'] = [{'stage': 'ap.operation', 'duration_ns': '18446744073709551615'}]
    body = json.dumps(reply).encode()
    save('ap_codec', 'service-' + name, u32(16 + len(body))
         + bytes([2, 0, 7, 1]) + b'servicex' + body)

for name, timing in [
    ('empty', []),
    ('maximum', [{'stage': 'ap.operation', 'duration_ns': '18446744073709551615'}]),
    ('overflow', [{'stage': 'ap.operation', 'duration_ns': '18446744073709551616'}]),
    ('private', [{'stage': 'ap.operation', 'duration_ns': '1', 'account': 'private'}]),
]:
    save('ap_codec', 'timing-' + name, json.dumps(timing).encode())

# Version 1 events carry a tagged 16-byte database identity and scoped terminals.
def replica_event(kind, database, payload=b''):
    return (bytes([1, kind, 0, 0]) + bytes([1]) * 16
            + struct.pack('<QQ', 1, 1) + database + payload)
index_database = bytes(16)
entry_database = bytes([2]) + bytes([3]) * 15
for name, kind, database, payload in [
    ('index', 1, index_database, b'x'),
    ('revocations', 1, bytes([1]) + bytes(15), b'x'),
    ('entry', 1, entry_database, b'x'),
    ('caught-up', 2, index_database, b''),
    ('gap', 3, index_database, b''),
    ('bloblog', 6, entry_database, b''),
]:
    save('ap_codec', 'event-' + name, replica_event(kind, database, payload))
for cause in range(1, 5):
    save('ap_codec', f'event-terminal-{cause}',
         replica_event(4, index_database, bytes([4]) * 16 + bytes([cause])))
# Native stream packets share this decoder target with ordinary AP frames.
for kind, body in [(1, b'x'), (1, bytes(16384)), (2, b''), (3, b''),
                   (4, u32(0)), (4, u32(0xffffffff))]:
    save('ap_codec', f'websocket-{kind}-{len(body)}-{body[:1].hex()}',
         bytes([kind]) + u32(1) + body)
# Exact wire maxima reach boundary checks without waiting for corpus growth.
maximum_frame = 1024 * 1024
save('ap_codec', 'frame-maximum', u32(maximum_frame) + bytes([2, 0, 0, 1])
     + b'x' + bytes(7) + bytes(maximum_frame - 16))
save('ap_codec', 'hello-maximum', u32(16 + 32 + 4096) + bytes([0, 1, 0, 0])
     + bytes(8) + bytes([1]) * 32 + bytes(4096))
save('ap_codec', 'event-maximum', replica_event(1, entry_database, bytes(65536)))

# Store contracts: mode byte followed by the native wire/JSON representation.
import base64
vector = json.loads((Path(__file__).resolve().parent.parent / 'tests/fixtures/session-link-v1.json').read_text())
client_record = json.loads((Path(__file__).resolve().parent.parent / 'tests/fixtures/client-record-v1.json').read_text())
volume_record = bytes.fromhex(''.join(client_record['volume_hex_parts']))
save('store_contract', 'client-record-volume', b'\7' + volume_record)
account_plaintext = json.loads((Path(__file__).resolve().parent.parent / 'tests/fixtures/account-plaintext-v1.json').read_text())
for kind in ['personal', 'display', 'optional']:
    save('store_contract', f'account-plaintext-{kind}', b'\7' + bytes.fromhex(account_plaintext[f'{kind}_hex']))
for flags in [0, 0x4000, 0x8000, 0xc000]:
    extension = struct.pack('<HI', 99 | flags, 3) + bytes([255, 1, 2])
    save('store_contract', f'client-record-extension-{flags}', b'\7' + volume_record + extension)
save('store_contract', 'invitation-valid', b'\0' + bytes.fromhex(vector['seed'])
     + base64.urlsafe_b64decode(vector['envelope'] + '=='))
save('store_contract', 'session-code', b'\0' + vector['code'].encode())
for length in [4095, 4096, 4097, 4098]:
    save('store_contract', f'envelope-{length}', b'\0' + base64.urlsafe_b64encode(bytes(length)).rstrip(b'='))
def identity(byte):
    return f'{byte:02x}' * 16
schema = {'types': {identity(3): {'name': 'Record', 'deprecated': False, 'fields': {
    identity(4): {'name': 'value', 'value_type': {'kind': 'text'}, 'repeated': False,
                  'deprecated': False}}}}}
save('store_contract', 'schema-add', b'\1' + json.dumps([{'types': {}}, schema]).encode())
save('store_contract', 'schema-repeat', b'\1' + json.dumps([schema, schema]).encode())
save('store_contract', 'acl', b'\1' + json.dumps({'grants': [{'subject': {'kind': 'public'},
     'permissions': ['read']}]}).encode())
for kind, value in [('text', 'hello'), ('boolean', True), ('integer', 42), ('float', 1.25),
                    ('bytes', [1, 2]), ('reference', {'volume_id': identity(1), 'database_id': '03' * 15,
                                   'object_id': identity(5)}),
                    ('file', {'volume_id': identity(1), 'entry_id': '04' * 15})]:
    field = schema['types'][identity(3)]['fields'][identity(4)]
    field['value_type'] = {'kind': kind}
    if kind == 'reference':
        field['value_type']['type_id'] = identity(3)
    fields = {'object_id': identity(5), 'type_id': identity(3), 'fields': {}}
    edit = {'object_id': identity(5), 'type_id': identity(3), 'field_id': identity(4),
            'stamp': {'clock': 1, 'device_id': identity(2)},
            'values': [{'kind': kind, 'value': value}]}
    save('store_contract', 'field-' + kind, b'\2' + json.dumps([schema, fields, edit]).encode())
clock = struct.pack('>Q', 1 << 32)
entries = []
for key in [f'field/{identity(5)}/{identity(4)}', f'object/{identity(5)}', f'object/{identity(6)}']:
    stored = b'\0\0' + clock + b'value'
    key = key.encode()
    entries.append(struct.pack('<HI', len(key), len(stored)) + key + stored)
payload = b''.join(entries)
header = struct.pack('<IHHHBB', 32 + len(payload), 6, 0, 1, 16, 0) + bytes(8) + clock + u32(len(entries))
save('store_contract', 'delta-projection', b'\3' + header + payload)
save('store_contract', 'delta-oversized', b'\3' + bytes(65537))
for length in [0, 1, 1024, 65504]:
    save('store_contract', f'blob-{length}', b'\4' + bytes(range(32)) + bytes(length))

# Keep initialization seeds aligned with the shipped named-database template.
initializers = json.loads((Path(__file__).resolve().parents[2] /
                           'guests/todo/playbit-volumes.json').read_bytes())
save('store_contract', 'root-initialization', b'\5' +
     json.dumps(initializers['session'], separators=(',', ':')).encode())

# Source manifests and canonical bundle headers; payloads remain bounded.
manifest = 'app_id="test"\nname="App"\nbuild_cmd="cargo build"\n'
save('package_contract', 'manifest', b'\1' + manifest.encode())
native_manifest = manifest + '[native]\nbuild_cmd="cargo build --features native --target aarch64-apple-darwin"\n[native.release]\nbuild_cmd="cargo build --release --features native --target aarch64-apple-darwin"\n'
save('package_contract', 'manifest-native', b'\1' + native_manifest.encode())
save('package_contract', 'manifest-native-long-command', b'\1' + native_manifest.replace('cargo build --release --features native --target aarch64-apple-darwin', 'x' * 8193).encode())
for length in [65536, 65537]:
    source = manifest + '#' + 'x' * (length - len(manifest) - 1)
    save('package_contract', f'manifest-{length}', b'\1' + source.encode())
for count in [0, 1, 1024, 1025]:
    payloads = {'app/manifest.json': b'{"app_id":"test","name":"App"}',
                'app/main.wasm': b'\0asm\1\0\0\0',
                'app/volumes.json': b'{}'}
    payloads.update({f'files/{n:04}': b'' for n in range(count)})
    names = sorted(payloads)
    index_size = sum((24 + len(name.encode()) + 7) & ~7 for name in names)
    offset = 32 + index_size
    index, payload = bytearray(), bytearray()
    for name in names:
        encoded = name.encode(); data = payloads[name]
        aligned = (offset + 7) & ~7
        payload.extend(b'\0' * (aligned - offset)); offset = aligned
        index.extend(struct.pack('<IIQQ', len(encoded), 0, offset, len(data)))
        index.extend(encoded); index.extend(b'\0' * ((-len(index)) % 8))
        payload.extend(data); offset += len(data)
    header = b'PBAPP\0\0\0' + struct.pack('<IIQQ', 1, len(names), len(index), offset)
    save('package_contract', f'bundle-{count}', b'\0' + header + index + payload)
save('package_contract', 'construct', b'\2\3Appicon bytes')

# Declared app names, stage ownership and generated-identifier boundaries.
schematic = tomllib.loads((Path(__file__).resolve().parents[2] / 'guests/todo/playbit.toml').read_text())['trace']
save('package_contract', 'schematic-todo', b'\3' + json.dumps(schematic).encode())
for count in [0, 256, 257]:
    value = {'version': 1, 'names': [{'name': f'n{n}', 'kind': 'mark', 'plane': 'data',
        'description': ''} for n in range(count)], 'boxes': []}
    save('package_contract', f'schematic-{count}', b'\3' + json.dumps(value).encode())
for name in ['', 'a' * 48, 'a' * 49, 'todo_turn', '\";panic!();']:
    value = json.loads(json.dumps(schematic))
    value['names'][1]['name'] = name
    save('package_contract', f'schematic-name-{len(name)}', b'\3' + json.dumps(value).encode())

# Scalar wasm32 C records: six selector words followed by untrusted memory.
for kind in range(1, 8):
    memory = bytearray(1024)
    abi_record(memory, 32, 40)
    memory[40:56] = bytes([7]) * 16
    struct.pack_into('<IIII', memory, 56, 128, 1, 80, 1)
    abi_record(memory, 128, 80)
    struct.pack_into('<I', memory, 136, kind)
    if kind in (1, 5):
        struct.pack_into('<I', memory, 140, 3)
        struct.pack_into('<I', memory, 200, 256)
        memory[256:259] = b'abc'
    for output in [0, 512, 32, 128, 256]:
        selectors = struct.pack('<6I', 32, 1, 40, output, int(output != 0), 1000000)
        save('wasm32_memory', f'value-{kind}-output-{output}', selectors + memory)

for field_data, value_data, byte_data in [(256,512,768),(32,512,768),(256,256,768),(256,512,512),(0,512,768)]:
    memory = bytearray(1024)
    abi_record(memory, 32, 68)
    buffer(memory, 56, field_data, 1, 40)
    buffer(memory, 72, value_data, 1, 80)
    buffer(memory, 88, byte_data, 1)
    save('wasm32_memory', f'outputs-{field_data}-{value_data}-{byte_data}',
         struct.pack('<6I',32,1,40,0,0,1000000) + memory)

for kind in range(1, 8):
    memory = bytearray(1024)
    abi_record(memory, 32, 48)
    memory[40:56] = bytes([8]) * 16
    struct.pack_into('<6I', memory, 56, 512, 4, 128, 1, 56, 0)
    abi_record(memory, 128, 56)
    memory[136:152] = bytes([7]) * 16
    struct.pack_into('<4I', memory, 152, 516, 4, kind, 3)
    if kind == 6:
        memory[168:184] = bytes([8]) * 16
    memory[512:520] = b'Itemdone'
    save('wasm32_memory', f'schema-kind-{kind}', struct.pack('<6I',32,1,48,0,0,1000000) + memory)

# Local IPC mutation selectors. The Rust target constructs valid archives with
# the current production serializer; Python does not duplicate rkyv layouts.
for mode in range(6):
    for seed in range(6):
        save('local_ipc', f'mode-{mode}-seed-{seed}', bytes([mode, seed, 0, 0]))
        save('local_ipc', f'mode-{mode}-seed-{seed}-corrupt',
             bytes([mode, seed, 0, 0]) + bytes([255]) * 16)

# Rewriting an accepted frame changes diagnostic tokens, not its archive.
save('local_ipc', 'diagnostic-token-rewrite', bytes([5]) + bytes([153]) * 7)

print('Wrote component fuzz seeds to', root)

# Executable metadata admission: valid section framing with a mismatching hash,
# plus a minimal Mach-O header so mutations reach both executable decoders.
metadata_name = b'playbit.metadata'
metadata_section = bytes([len(metadata_name)]) + metadata_name + bytes(32)
save('package_contract', 'wasm-metadata', b'\x04\0asm\x01\0\0\0\0'
     + bytes([len(metadata_section)]) + metadata_section)
save('package_contract', 'macho-header', b'\x04'
     + struct.pack('<8I', 0xfeedfacf, 0x100000c, 0, 2, 0, 0, 0, 0))
