#!/usr/bin/env python3
"""Bounded corpus bundles and the unprivileged CI fuzz runner."""
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import zipfile

import provenance

TARGETS = ('decode_view', 'abi_reader', 'wasm_operations', 'kvdb_wire',
           'kvdb_encoding', 'kvdb_db_ops', 'kvdb_sync_pair', 'ap_codec', 'store_contract', 'package_contract', 'wasm32_memory', 'trace_decode', 'local_ipc')
TRIPLE = 'aarch64-unknown-linux-gnu'
ROOT = Path('playbit/fuzz')
MAX_INPUT = 1024 * 1024
MAX_INPUTS = 16384
# Thirteen corpus/crash pairs plus ZIP headers stay below the 256-MiB transfer
# budget: 26 * 7 MiB payload plus at most 256 bytes per admitted ZIP entry.
# Keeping both new targets at 8 MiB would exceed the existing transfer bound.
MAX_BYTES = 7 * 1024 * 1024
# Bound the unprivileged worker's scan/merge before enforcing archive limits.
MAX_WORKING_INPUTS = 65536
MAX_WORKING_BYTES = 64 * 1024 * 1024
COMPACT_TIMEOUT = 60
MAX_ARCHIVE = 24 * 1024 * 1024
MAX_CRASHES = 16
MAX_METADATA = 64 * 1024
MAX_BASELINE_TARGETS = 64
MAX_LOCK = 1024 * 1024
BASELINE_KEY = 'fuzz/corpus/latest.json'
STATES = ('not-run', 'running', 'success', 'failure', 'timeout')


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_json(path):
    if path.stat().st_size > MAX_METADATA:
        raise ValueError('fuzz metadata exceeds limit')
    return json.loads(path.read_text())


def write_json(path, value):
    data = (json.dumps(value, sort_keys=True, indent=2) + '\n').encode()
    if len(data) > MAX_METADATA:
        raise ValueError('fuzz metadata exceeds limit')
    temporary = path.with_suffix('.tmp')
    temporary.write_bytes(data)
    temporary.replace(path)


def input_files(directory, crashes=False, working=False):
    result = []
    count = size = 0
    count_limit = MAX_CRASHES if crashes else MAX_WORKING_INPUTS if working else MAX_INPUTS
    byte_limit = MAX_WORKING_BYTES if working else MAX_BYTES
    if directory.is_symlink():
        raise ValueError('fuzz input directory is a symlink')
    if not directory.exists():
        return result
    with os.scandir(directory) as entries:
        for entry in entries:
            count += 1
            if count > count_limit:
                raise ValueError(f'fuzz input count exceeds limit: {count} > {count_limit}')
            info = entry.stat(follow_symlinks=False)
            if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_INPUT:
                raise ValueError('invalid fuzz input file')
            size += info.st_size
            if size > byte_limit:
                raise ValueError(f'fuzz input bytes exceed limit: {size} > {byte_limit}')
            result.append((Path(entry.path), info.st_size))
    return result


def inputs(directory, crashes=False, working=False):
    result = {}
    for path, size in input_files(directory, crashes, working):
        with path.open('rb') as source:
            data = source.read(MAX_INPUT + 1)
        if len(data) != size:
            raise ValueError('fuzz input changed during collection')
        prefix = ''
        if crashes:
            match = re.fullmatch(r'(crash|leak|timeout|oom|slow-unit)-[0-9a-f]+', path.name)
            if not match:
                raise ValueError('unexpected fuzz artifact name')
            prefix = match[1] + '-'
        result[prefix + digest(data)] = data
    return result


def write_bundle(path, values, crashes=False):
    if len(values) > (MAX_CRASHES if crashes else MAX_INPUTS) or sum(map(len, values.values())) > MAX_BYTES:
        raise ValueError('fuzz bundle exceeds capacity; compact the active corpus')
    with zipfile.ZipFile(path, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in sorted(values.items()):
            if len(data) > MAX_INPUT:
                raise ValueError('fuzz input exceeds limit')
            info = zipfile.ZipInfo(name)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (stat.S_IFREG | 0o644) << 16
            archive.writestr(info, data)
    return describe(path, values)


def describe(path, values):
    if path.stat().st_size > MAX_ARCHIVE:
        raise ValueError('fuzz archive exceeds limit')
    return {'sha256': digest(path.read_bytes()), 'bytes': path.stat().st_size,
            'inputs': len(values), 'expanded_bytes': sum(map(len, values.values()))}


def read_bundle(path, crashes=False):
    # Bound the ZIP central directory before ZipFile allocates entry objects.
    limit = MAX_CRASHES if crashes else MAX_INPUTS
    size = path.stat().st_size
    if size > MAX_ARCHIVE or size < 22:
        raise ValueError('invalid fuzz archive size')
    with path.open('rb') as source:
        source.seek(max(0, size - 65557))
        tail = source.read()
    offset = tail.rfind(b'PK\x05\x06')
    if offset < 0 or len(tail) - offset < 22:
        raise ValueError('missing fuzz ZIP directory')
    _, disk, directory_disk, disk_count, count, directory_bytes, _, comment = struct.unpack_from('<4s4H2IH', tail, offset)
    if (disk or directory_disk or disk_count != count or count > limit or
            directory_bytes > 2 * 1024 * 1024 or offset + 22 + comment != len(tail)):
        raise ValueError('fuzz ZIP directory exceeds limit')
    values = {}
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        if len(entries) > limit or sum(item.file_size for item in entries) > MAX_BYTES:
            raise ValueError('fuzz archive exceeds expanded capacity')
        pattern = r'(?:crash|leak|timeout|oom|slow-unit)-[0-9a-f]{64}' if crashes else r'[0-9a-f]{64}'
        for item in entries:
            mode = item.external_attr >> 16
            if (not re.fullmatch(pattern, item.filename) or item.filename in values or
                    item.file_size > MAX_INPUT or item.flag_bits & 1 or
                    (stat.S_IFMT(mode) and not stat.S_ISREG(mode))):
                raise ValueError('invalid fuzz archive entry')
            with archive.open(item) as source:
                data = source.read(MAX_INPUT + 1)
            if len(data) != item.file_size or digest(data) != item.filename[-64:]:
                raise ValueError('fuzz input hash or length mismatch')
            values[item.filename] = data
    return values


def check_descriptor(value, path=None):
    keys = {'sha256', 'bytes', 'inputs', 'expanded_bytes'} | ({'path'} if path is not None else set())
    if (not isinstance(value, dict) or set(value) != keys or
            not re.fullmatch('[0-9a-f]{64}', str(value['sha256']))):
        raise ValueError('invalid fuzz bundle descriptor')
    for key, maximum in [('bytes', MAX_ARCHIVE), ('inputs', MAX_INPUTS), ('expanded_bytes', MAX_BYTES)]:
        if type(value[key]) is not int or not 0 <= value[key] <= maximum:
            raise ValueError('invalid fuzz bundle capacity')
    if path is not None and value['path'] != path.format(sha256=value['sha256']):
        raise ValueError('unexpected fuzz snapshot path')


def check_baseline(value):
    if not isinstance(value, dict) or set(value) != {'targets'} or not isinstance(value['targets'], dict):
        raise ValueError('invalid fuzz baseline')
    if len(value['targets']) > MAX_BASELINE_TARGETS:
        raise ValueError('fuzz baseline target count exceeds limit')
    for target, item in value['targets'].items():
        if not isinstance(target, str) or not re.fullmatch(r'[a-z][a-z0-9_]{0,63}', target):
            raise ValueError('invalid fuzz baseline target name')
        check_descriptor(item, 'fuzz/corpus/' + target + '/{sha256}.zip')
    return value


def fetch(base_url, key, path, limit):
    if not re.fullmatch(r'https://[A-Za-z0-9.-]+', base_url):
        raise ValueError('expected HTTPS corpus origin without a path')
    # No redirects, credentials, shell expansion, or unbounded response body.
    subprocess.run(['curl', '--silent', '--show-error', '--fail', '--proto', '=https',
                    '--max-time', '30', '--max-filesize', str(limit),
                    '--output', str(path), base_url + '/' + key], check=True, timeout=35)
    if path.stat().st_size > limit:
        raise ValueError('fuzz download exceeds limit')


def initialize(transfer):
    provenance.record(transfer)
    write_json(transfer / 'manifest.json', {
        'restore': 'pending', 'baseline': {'targets': {}}, 'target_triple': TRIPLE,
        'toolchain': {}, 'targets': {target: {'status': 'not-run', 'exit_code': None,
                                           'starting_inputs': 0} for target in TARGETS},
    })


def restore(transfer, base_url):
    manifest = read_json(transfer / 'manifest.json')
    with tempfile.TemporaryDirectory() as temporary:
        destination = Path(temporary) / 'baseline.json'
        fetch(base_url, BASELINE_KEY, destination, MAX_METADATA)
        baseline = check_baseline(read_json(destination))
        # The shared index and this checkout can have different target sets.
        # Restore and report only inputs belonging to this checkout's targets.
        baseline = {'targets': {target: item for target, item in baseline['targets'].items()
                                if target in TARGETS}}
        for target, item in baseline['targets'].items():
            path = Path(temporary) / 'restore.zip'
            fetch(base_url, item['path'], path, MAX_ARCHIVE)
            if digest(path.read_bytes()) != item['sha256'] or path.stat().st_size != item['bytes']:
                raise ValueError('fuzz snapshot download hash or length mismatch')
            values = read_bundle(path)
            if describe(path, values) != {key: value for key, value in item.items() if key != 'path'}:
                raise ValueError('fuzz snapshot metadata mismatch')
            directory = ROOT / 'corpus' / target
            directory.mkdir(parents=True, exist_ok=True)
            for name, data in values.items():
                (directory / name).write_bytes(data)
    manifest.update(restore='success', baseline=baseline)
    write_json(transfer / 'manifest.json', manifest)


def toolchain():
    versions = {}
    for name, command in [('rustc', ['rustc', '-Vv']), ('cargo_fuzz', ['cargo', 'fuzz', '--version'])]:
        value = subprocess.check_output(command, text=True, timeout=15).strip()
        if len(value) > 2048:
            raise ValueError('fuzz toolchain description exceeds limit')
        versions[name] = value
    # The distribution date, unlike rustc's commit date, identifies an installable
    # nightly exactly. rustup stores it in the installed channel manifest header.
    sysroot = Path(subprocess.check_output(['rustc', '--print', 'sysroot'], text=True, timeout=15).strip())
    with (sysroot / 'lib/rustlib/multirust-channel-manifest.toml').open() as source:
        header = source.read(256)
    date = re.search(r'^date = "([0-9]{4}-[0-9]{2}-[0-9]{2})"$', header, re.MULTILINE)
    if not date or '-nightly' not in versions['rustc']:
        raise ValueError('expected a rustup nightly distribution manifest')
    versions['channel'] = 'nightly-' + date[1]
    return versions


def run(transfer):
    manifest = read_json(transfer / 'manifest.json')
    if manifest['restore'] != 'success':
        raise ValueError('fuzz baseline was not restored')
    manifest['toolchain'] = toolchain()
    subprocess.run([sys.executable, str(ROOT / 'seed-corpus.py')], check=True, timeout=15)
    failed = False
    for target in TARGETS:
        item = manifest['targets'][target]
        starting = inputs(ROOT / 'corpus' / target, working=True)
        item.update(status='running', starting_inputs=len(starting))
        write_json(transfer / 'manifest.json', manifest)
        print(f'Fuzz {target}: starting with {len(starting)} distinct inputs, '
              f'{sum(map(len, starting.values()))} bytes', flush=True)
        del starting
        process = subprocess.Popen(['cargo', 'fuzz', 'run', '--fuzz-dir', str(ROOT), '--target', TRIPLE,
                                    '--debug-assertions', target, '--', '-max_total_time=60'], start_new_session=True)
        try:
            code = process.wait(timeout=180)
            item.update(status='success' if code == 0 else 'failure', exit_code=code)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=15)
            item.update(status='timeout', exit_code=None)
        failed |= item['status'] != 'success'
        write_json(transfer / 'manifest.json', manifest)
    return 1 if failed else 0


def compact(target):
    directory = ROOT / 'corpus' / target
    files = input_files(directory, working=True)
    print(f'Fuzz {target}: before compaction {len(files)} files, '
          f'{sum(size for _, size in files)} bytes', flush=True)
    if not files:
        return {}
    # Use cmin's merge operation with a separate output directory. The original
    # corpus survives failure, and cargo-fuzz run propagates the merge exit code.
    with tempfile.TemporaryDirectory() as temporary:
        destination = Path(temporary) / 'corpus'
        destination.mkdir()
        process = subprocess.Popen([
            'cargo', 'fuzz', 'run', '--fuzz-dir', str(ROOT), '--target', TRIPLE,
            '--debug-assertions', target, str(destination), str(directory.resolve()), '--',
            '-merge=1', f'-merge_control_file={temporary}/merge', '-rss_limit_mb=2048',
        ], start_new_session=True)
        try:
            code = process.wait(timeout=COMPACT_TIMEOUT)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=15)
            raise ValueError(f'corpus compaction exceeded {COMPACT_TIMEOUT} seconds') from None
        if code:
            raise ValueError(f'corpus compaction failed with exit code {code}')
        values = inputs(destination)
        print(f'Fuzz {target}: after compaction {len(values)} inputs, '
              f'{sum(map(len, values.values()))} bytes', flush=True)
        return values


def collect(transfer):
    manifest = read_json(transfer / 'manifest.json')
    failed = False
    for target in TARGETS:
        item = manifest['targets'][target]
        # Merge can itself discover a crash. Collect reproducers afterwards even
        # if compaction or corpus packing failed.
        for kind, directory in [('corpus', 'corpus'), ('crashes', 'artifacts')]:
            path = transfer / f'{target}-{kind}.zip'
            try:
                if kind == 'corpus' and item['status'] == 'success':
                    values = compact(target)
                else:
                    values = inputs(ROOT / directory / target, crashes=kind == 'crashes')
                item[kind] = write_bundle(path, values, crashes=kind == 'crashes')
                # libFuzzer's crash-resistant merge can return zero after a child
                # fails. Saved reproducers must still fail CI and block promotion.
                if kind == 'crashes' and any(not name.startswith('slow-unit-') for name in values):
                    failed = True
                    if item['status'] == 'success':
                        item.update(status='failure', exit_code=None)
            except (ValueError, OSError, subprocess.SubprocessError) as error:
                print(f'{target} {kind}: collection failed: {error}', file=sys.stderr)
                path.unlink(missing_ok=True)
                item[kind] = None
                failed = True
    lock = ROOT / 'Cargo.lock'
    if lock.exists():
        if lock.is_symlink() or not lock.is_file() or lock.stat().st_size > MAX_LOCK:
            print('fuzz dependency lockfile could not be collected', file=sys.stderr)
            failed = True
        else:
            shutil.copyfile(lock, transfer / 'Cargo.lock')
    write_json(transfer / 'manifest.json', manifest)
    return 1 if failed else 0


if __name__ == '__main__':
    command, transfer = sys.argv[1], Path(sys.argv[2])
    if command == 'initialize':
        initialize(transfer)
    elif command == 'restore':
        restore(transfer, sys.argv[3])
    elif command == 'run':
        sys.exit(run(transfer))
    elif command == 'collect':
        sys.exit(collect(transfer))
    else:
        raise ValueError('unknown fuzz CI command')
