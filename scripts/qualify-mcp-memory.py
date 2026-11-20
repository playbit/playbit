"""Operator-only MCP payload memory qualification; never a CI benchmark.

Uses optimized packaged MCP/engine binaries, a fresh isolated profile, and OS
wait4 peak RSS. Pipe round trips are diagnostics, not codec-only measurements.
"""
import argparse
import concurrent.futures
import hashlib
import json
import math
import os
from pathlib import Path
import select
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time
LIMIT = 4 * 1024 * 1024

def frame(arguments):
    return b'{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"playbit","arguments":' + arguments + b'}}'

def padded(arguments):
    message = frame(arguments)
    assert len(message) <= LIMIT
    return message + b' ' * (LIMIT - len(message)) + b'\n'

def fixtures():
    for op in [b'status', b'not-an-operation']:
        prefix = b'{"op":"' + op + b'","extra":['
        count = (LIMIT - len(frame(prefix + b'0]}'))) // 2
        yield (op.decode() + '-dense-array', padded(prefix + b'0,' * count + b'0]}'), None)
    prefix = b'{"op":"status","extra":"'
    size = LIMIT - len(frame(prefix + b'"}'))
    yield ('status-long-string', padded(prefix + b'x' * size + b'"}'), None)
    entries = b','.join((b'"k%06d":0' % index for index in range(250000)))
    yield ('status-many-keys', padded(b'{"op":"status","extra":{' + entries + b'}}'), None)
    arguments = dict(op='syscall', context=dict(kind='tool', binding=dict(handle='0' * 32, incarnation='0' * 32)), operation=1, mutation=1, call=dict(volume=[0] * 16, operation=dict(ChannelCommand=dict(instance=[1] * 16, name='qualification', action=1, data=[]))))
    encoded = json.dumps(arguments, separators=(',', ':')).encode()
    prefix, suffix = encoded.split(b'[]')
    prefix += b'['
    suffix = b']' + suffix
    count = (LIMIT - len(frame(prefix + b'0' + suffix))) // 2
    yield ('syscall-dense-array', padded(prefix + b'0,' * count + b'0' + suffix), None)
    arguments = dict(op='syscall', context=dict(kind='tool', binding=dict(handle='0' * 32, incarnation='0' * 32)), operation=1, mutation=0, call=dict(volume=[0] * 16, operation=dict(BloblogRead=dict(log=[0] * 16, cursor=None))))
    yield ('synthetic-response-bytes', frame(json.dumps(arguments, separators=(',', ':')).encode()) + b'\n', 'bytes')
    yield ('synthetic-response-text', frame(b'{"op":"local-protocol"}') + b'\n', 'text')

def response_server(profile, fixtures, name, count):
    """Same-user typed archive fixture, explicitly not real engine data."""
    profile.mkdir(mode=448, exist_ok=True)
    (profile / 'control').mkdir(mode=448, exist_ok=True)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(profile / 'control/engine.sock'))
    listener.listen(2)
    listener.settimeout(10)
    errors = []

    def receive(reader):
        header = reader.read(4)
        assert len(header) == 4
        length, = struct.unpack('<I', header)
        assert length <= LIMIT
        body = reader.read(length)
        assert len(body) == length
        return header + body

    def serve():
        try:
            for _ in range(count):
                for request, response in [('protocol.request', 'protocol.frame'), ('bytes.request' if name == 'bytes' else 'protocol.request', name + '.frame')]:
                    with listener.accept()[0] as stream:
                        stream.settimeout(10)
                        with stream.makefile('rb') as reader:
                            actual = receive(reader)
                            expected = (fixtures / request).read_bytes()
                            assert actual == expected
                            stream.sendall((fixtures / response).read_bytes())
        except BaseException as error:
            errors.append(repr(error))
    thread = threading.Thread(target=serve)
    thread.start()
    return (listener, thread, errors)

def reply(process, maximum=16 * LIMIT):
    deadline, data = (time.monotonic() + 30, bytearray())
    while b'\n' not in data:
        remaining = deadline - time.monotonic()
        assert remaining > 0 and select.select([process.stdout], [], [], remaining)[0], 'reply deadline'
        chunk = os.read(process.stdout.fileno(), 65536)
        assert chunk, 'MCP closed before reply'
        data.extend(chunk)
        assert len(data) <= maximum, 'fixture reply bound'
    assert data.endswith(b'\n') and data.count(b'\n') == 1
    return (json.loads(data), len(data))

def peak(process):
    _, status, usage = os.wait4(process.pid, 0)
    process.returncode = os.waitstatus_to_exitcode(status)
    assert process.returncode == 0, process.returncode
    return int(usage.ru_maxrss) * (1 if sys.platform == 'darwin' else 1024)

def denied_syscall(result):
    assert not result.get('isError'), result
    completion = json.loads(result['content'][0]['text'])
    assert completion['status'] == -8 and completion['result'] is None, completion
    assert completion['operation'] == 1 and completion['mutation'] == 1, completion

def concurrent_calls(binary, env, output, log, repeats):
    _, message, _ = next((case for case in fixtures() if case[0] == 'syscall-dense-array'))
    barrier = threading.Barrier(4)

    def measure(index):
        child_env = env
        profile = Path(env['PLAYBIT_PROFILE'])
        child = subprocess.Popen([str(binary)], env=child_env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log)
        try:
            child.stdin.write(b'{"jsonrpc":"2.0","id":2,"method":"initialize"}\n')
            child.stdin.flush()
            assert reply(child)[0]['id'] == 2
            barrier.wait(timeout=30)
            samples = []
            for _ in range(repeats):
                start = time.monotonic_ns()
                child.stdin.write(message)
                child.stdin.flush()
                result, _ = reply(child)
                samples.append(time.monotonic_ns() - start)
                assert result['id'] == 1
                denied_syscall(result['result'])
            child.stdin.write(frame(b'{"op":"status"}') + b'\n')
            child.stdin.flush()
            assert not reply(child)[0]['result'].get('isError'), 'small runtime call failed after saturation'
            child.stdin.close()
            ordered = sorted(samples)
            return dict(case=f'concurrent-call-client-{index}', count=len(samples), request_bytes=len(message) - 1, peak_rss_bytes=peak(child), samples_ns=samples, endpoint='one shared packaged local engine', roundtrip={key: ordered[math.ceil(len(samples) * p) - 1] for key, p in [('p50_ns', 0.5), ('p95_ns', 0.95), ('p99_ns', 0.99), ('max_ns', 1)]})
        finally:
            if child.returncode is None:
                child.kill()
                child.wait()
            child.stdout.close()
            if not child.stdin.closed:
                child.stdin.close()
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        rows = list(pool.map(measure, range(4)))
    for row in rows:
        print(json.dumps({key: value for key, value in row.items() if key != 'samples_ns'}), flush=True)
    return rows

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path, help='Packaged Contents/MacOS/playbit-mcp')
    parser.add_argument('output', type=Path, help='Fresh external artifact directory')
    parser.add_argument('--repeats', type=int, default=1, help='Explicit reference-device cohort size')
    parser.add_argument('--concurrent-calls', action='store_true', help='Instead run four maximum-input clients against one engine')
    args = parser.parse_args()
    assert sys.platform in ('darwin', 'linux'), 'verify OS peak-RSS units before adding a platform'
    args.binary = args.binary.resolve()
    engine_path = args.binary.parent.parent / 'Helpers/playbit-engine.app/Contents/MacOS/playbit-engine' if sys.platform == 'darwin' and args.binary.parent.name == 'MacOS' and (args.binary.parent.parent.name == 'Contents') else args.binary.with_name('playbit-engine')
    args.output = args.output.resolve()
    args.output.mkdir(mode=448)
    assert 1 <= args.repeats <= 1000
    fixture_dir = args.output / 'archives'
    standalone = args.output / 'standalone-mcp'
    if not args.concurrent_calls:
        repo = Path(__file__).resolve().parents[1]
        subprocess.run(['cargo', 'build', '--locked', '--offline', '--release', '-p', 'playbit', '--no-default-features', '--features', 'process', '--example', 'mcp-memory-fixtures'], cwd=repo, check=True)
        generator = repo / 'target/release/examples/mcp-memory-fixtures'
        payload_size = int(subprocess.check_output([str(generator), str(fixture_dir)]))
        shutil.copy2(args.binary, standalone)
    profile = args.output / 'profile'
    env = dict(os.environ, PLAYBIT_PROFILE=str(profile))
    engine = None
    results = {'binary_sha256': hashlib.sha256(args.binary.read_bytes()).hexdigest(), 'engine_sha256': hashlib.sha256(engine_path.read_bytes()).hexdigest(), 'platform': sys.platform, 'rss_boundary': 'OS wait4 lifetime maximum resident bytes', 'timing_boundary': 'write/flush maximum-size request through complete reply decode; Python/stdio/engine included, not codec-only', 'cases': []}
    if not args.concurrent_calls:
        results['fixture_generator_sha256'] = hashlib.sha256(generator.read_bytes()).hexdigest()
        results['fixture_sha256'] = {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in fixture_dir.iterdir()}
    with (args.output / 'stderr.log').open('wb') as log:
        try:
            engine = subprocess.Popen([str(engine_path)], env=env, stdout=subprocess.PIPE, stderr=log)
            assert select.select([engine.stdout], [], [], 30)[0], 'engine readiness deadline'
            assert engine.stdout.readline() == b'{"ready":true}\n'
            for name, message, synthetic in [] if args.concurrent_calls else fixtures():
                invalid = name.startswith('not-an-operation')
                absent = args.output / ('absent-' + name)
                child_env = dict(env, PLAYBIT_PROFILE=str(absent)) if invalid else env
                endpoint = None
                measured_profile = absent if invalid else profile
                if synthetic is not None:
                    measured_profile = args.output / name
                child = None
                try:
                    if synthetic is not None:
                        endpoint = response_server(measured_profile, fixture_dir, synthetic, args.repeats)
                    child_env = dict(env, PLAYBIT_PROFILE=str(measured_profile))
                    child = subprocess.Popen([str(standalone if synthetic is not None else args.binary)], env=child_env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log)
                    samples = []
                    for _ in range(args.repeats):
                        start = time.monotonic_ns()
                        child.stdin.write(message)
                        child.stdin.flush()
                        response, response_bytes = reply(child)
                        samples.append(time.monotonic_ns() - start)
                        assert response['id'] == 1
                        result = response['result']
                        if synthetic is not None:
                            assert not result.get('isError'), result
                            decoded = json.loads(result['content'][0]['text'])
                            if synthetic == 'bytes':
                                data = decoded['result']['BloblogPage']['entries'][0]['data']
                                assert len(data) == payload_size and all((byte == 255 for byte in data))
                            else:
                                assert decoded['version'] == '\n' * payload_size
                        elif name.startswith('syscall-'):
                            denied_syscall(result)
                        elif invalid:
                            assert result.get('isError'), result
                        else:
                            assert not result.get('isError'), result
                            assert 'volumes' in json.loads(result['content'][0]['text'])
                    child.stdin.write(b'{"jsonrpc":"2.0","id":2,"method":"initialize"}\n')
                    child.stdin.flush()
                    followup, _ = reply(child)
                    assert followup['id'] == 2 and followup['result']['serverInfo']['name'] == 'playbit'
                    if invalid:
                        assert not absent.exists(), 'malformed operation created a profile'
                        assert not (measured_profile / 'control/engine.sock').exists(), 'malformed operation dispatched to an engine'
                    child.stdin.close()
                    ordered = sorted(samples)
                    row = dict(case=name, request_bytes=len(message) - 1, response_bytes=response_bytes, endpoint='synthetic typed archive, beyond engine operation limits' if synthetic is not None else 'no runtime dispatch' if invalid else 'packaged local engine', count=len(samples), peak_rss_bytes=peak(child), samples_ns=samples, roundtrip={key: ordered[math.ceil(len(samples) * p) - 1] for key, p in [('p50_ns', 0.5), ('p95_ns', 0.95), ('p99_ns', 0.99), ('max_ns', 1)]})
                    results['cases'].append(row)
                    print(json.dumps({key: value for key, value in row.items() if key != 'samples_ns'}), flush=True)
                finally:
                    if child is not None:
                        if child.returncode is None:
                            child.kill()
                            child.wait()
                        child.stdout.close()
                        if not child.stdin.closed:
                            child.stdin.close()
                    if endpoint is not None:
                        listener, thread, errors = endpoint
                        listener.close()
                        thread.join(timeout=15)
                        assert not thread.is_alive() and (not errors), errors
            if args.concurrent_calls:
                results['cases'] = concurrent_calls(args.binary, env, args.output, log, args.repeats)
                results['aggregate_peak_rss_upper_bound_bytes'] = sum((row['peak_rss_bytes'] for row in results['cases']))
            assert results['cases']
            results['all_below_128_mib'] = all((row['peak_rss_bytes'] <= 128 * 1024 * 1024 for row in results['cases']))
            (args.output / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
        finally:
            if engine is not None:
                engine.terminate()
                try:
                    engine.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    engine.kill()
                    engine.wait()
    if not results['all_below_128_mib']:
        raise SystemExit('MCP decoded-payload RSS allocation exceeded')
if __name__ == '__main__':
    main()
