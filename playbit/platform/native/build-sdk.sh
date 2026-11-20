#!/bin/sh
# Package the concrete native Mac composition behind its C ABI. Build it in an
# isolated target directory so desktop/VM features cannot enter through Cargo.
set -eu
cd "$(dirname "$0")/../../.."
test "$(uname -s)" = Darwin && test "$(uname -m)" = arm64 || {
    echo 'The native Mac SDK currently requires an arm64 Mac.' >&2
    exit 1
}
. scripts/macos-env.sh
# Native callbacks cannot unwind through C; the concrete host is fail-stop.
export CARGO_PROFILE_RELEASE_PANIC=abort
mkdir -p target/native-mac
sdk_log=target/native-mac/native-static-libs.log
# Parse compiler metadata below; CI may otherwise force ANSI color in this log.
if ! cargo rustc --color never --locked -p playbit --lib --release --no-default-features \
    --features connected-engine,native-runtime,desktop-ui --target-dir target/native-mac \
    --crate-type staticlib -- --print native-static-libs > "$sdk_log" 2>&1; then
    cat "$sdk_log" >&2
    exit 1
fi
cat "$sdk_log" >&2
python3 - <<'PY'
from pathlib import Path
import json, os, re, shlex, shutil, subprocess
output = Path('target/native-mac/sdk')
archive = Path('target/native-mac/release/libplaybit.a')
minimum_os = os.environ['MACOSX_DEPLOYMENT_TARGET']
# Installed SDK storage is separate from the small source scaffold. Bound the
# actual optimized archive; do not ship debug artifacts or duplicate target libs.
if archive.stat().st_size > 64 * 1024 * 1024:
    raise SystemExit('native Mac SDK archive exceeds 64 MiB')
lines = re.findall(r'^note: native-static-libs: (.+)$', Path('target/native-mac/native-static-libs.log').read_text(), re.M)
if len(lines) != 1 or len(lines[0]) > 8192:
    raise SystemExit('missing or oversized native linker requirements')
# These are macOS system dynamic libraries/frameworks, not ordered static
# archives. Emit each once so consumers get warning-free linker commands.
tokens = iter(shlex.split(lines[0]))
arguments, seen = [], set()
for token in tokens:
    # Both clang and rustc supply libSystem, which includes libc and libm.
    if token in ('-lSystem', '-lc', '-lm'):
        continue
    if token == '-framework':
        name = next(tokens, None)
        if name is None:
            raise SystemExit('framework link argument has no name')
        unit = (token, name)
    elif token.startswith('-l'):
        unit = (token,)
    else:
        raise SystemExit('unsupported native link argument: ' + token)
    if unit not in seen:
        seen.add(unit)
        arguments.extend(unit)
if output.exists():
    shutil.rmtree(output)
(output / 'aarch64-apple-darwin').mkdir(parents=True, exist_ok=True)
# A Rust staticlib exports its bundled std implementation too. Combine its
# objects, then localize everything except our C entry points: independently
# compiled Rust consumers must never select the SDK's allocator/panic runtime.
# Apple's ld/nmedit resolve internal references before hiding their definitions.
combined = Path('target/native-mac/sdk.o')
sdk_version = subprocess.check_output(['xcrun', '--sdk', 'macosx', '--show-sdk-version'], text=True).strip()
subprocess.run(['xcrun', 'ld', '-r', '-arch', 'arm64', '-platform_version',
                'macos', minimum_os, sdk_version, '-all_load', str(archive),
                '-o', str(combined)], check=True)
# Older Apple linkers retain Rust's embedded LLVM bitcode. Inspect the native
# symbol table: Xcode's LLVM reader may not understand rustc's newer IR.
nm = ['xcrun', 'nm', '--no-llvm-bc', '-gjU']
names = subprocess.check_output(nm + [str(combined)], text=True).splitlines()
exports = sorted(name for name in names if name.startswith('_pb_'))
if not exports or '_pb_mac_run' not in exports or '_pb_mac_run_portable' not in exports:
    raise SystemExit('native Mac SDK has no host entry point')
export_file = Path('target/native-mac/sdk-exports.txt')
export_file.write_text('\n'.join(exports) + '\n')
subprocess.run(['nmedit', '-s', str(export_file), str(combined)], check=True)
if sorted(subprocess.check_output(nm + [str(combined)], text=True).splitlines()) != exports:
    raise SystemExit('native Mac SDK leaks implementation symbols')
packaged = output / 'aarch64-apple-darwin/libplaybit.a'
subprocess.run(['xcrun', 'libtool', '-static', '-D', '-o', str(packaged), str(combined)], check=True)
if packaged.stat().st_size > 64 * 1024 * 1024:
    raise SystemExit('packaged native Mac SDK exceeds 64 MiB')
for source in [Path('playbit/include/playbit'), Path('playbit-api/include/playbit')]:
    shutil.copytree(source, output / 'include/playbit', dirs_exist_ok=True)
licenses = output / 'licenses'
licenses.mkdir(exist_ok=True)
for source, name in [('vendor/accesskit_macos/LICENSE-MIT', 'AccessKit-MIT.txt'),
                     ('playbit-api/LICENSE', 'playbit-api.txt'), ('LICENSE', 'Playbit.txt')]:
    shutil.copyfile(source, licenses / name)
(output / 'aarch64-apple-darwin/link.json').write_text(json.dumps({
    'target': 'aarch64-apple-darwin', 'minimum_os': minimum_os,
    'archive': 'libplaybit.a', 'link_args': arguments,
}, indent=2) + '\n')
print(output.resolve())
PY
