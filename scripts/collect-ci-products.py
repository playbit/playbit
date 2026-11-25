#!/usr/bin/env python3
"""Package client products from an independent source-available checkout."""
import argparse
from pathlib import Path
import shutil
import subprocess
import zipfile


def collect(public, output):
    if (public / 'service').exists() or not (public / 'AGENTS.md').is_file():
        raise ValueError('expected an independent public export')
    output.mkdir(parents=True, exist_ok=False)
    kit = output / 'kit'
    kit.mkdir()
    shutil.copy2(public / 'LICENSE', kit / 'LICENSE.txt')
    # This is an output list, not a second source-publication configuration.
    for name in ['Playbit.app', 'pb', 'playbit-engine', 'playbit-mcp',
                 'todo.pbapp', 'micro-kart.pbapp', 'ui-gallery.pbapp']:
        source = public / 'target/debug' / name
        if source.is_dir():
            shutil.copytree(source, kit / name, symlinks=True)
        else:
            shutil.copy2(source, kit / name)
    for path in kit.rglob('*'):
        if path.is_symlink() and not path.resolve().is_relative_to(kit.resolve()):
            raise ValueError('archive symlink escapes the product')
    (kit / 'README.txt').write_text('Archipelago macOS development kit. Unsigned debug build.\nRequires macOS 15 or newer. See the public source README for local usage.\nWeb examples are experimental; Phase 6 qualification is incomplete.\n')
    subprocess.run(['ditto', '-c', '-k', '--keepParent', str(kit), str(output / 'macos-arm64.zip')], check=True)
    shutil.rmtree(kit)
    # Web build.sh produces this closed, flat set from public source only; the
    # bundled tests/ probes and debug source maps are not products.
    source = public / 'target/web'
    expected = {'index.html', 'playbit.js', 'native-handoff.js', 'runtime-worker.js', 'ap-worker.js', 'controls.css',
                'invitation.css', 'InterVariable.ttf', 'playbit.wasm',
                'todo.pbapp', 'micro-kart.pbapp', 'shared-note.pbapp'}
    expected |= {p.name for p in (public / 'playbit/assets').glob('check-*.svg')}
    expected |= {p.name for p in (public / 'playbit/platform/web/invitation').glob('*.svg')}
    web = output / 'web'
    web.mkdir()
    for name in sorted(expected):
        path = source / name
        if not path.is_file() or path.is_symlink():
            raise ValueError('missing or unsafe web product: ' + name)
        shutil.copy2(path, web / name)
    (web / 'QUALIFICATION.txt').write_text('Experimental web examples. Phase 6 browser qualification is incomplete.\n')
    shutil.copy2(public / 'LICENSE', web / 'LICENSE.txt')
    with zipfile.ZipFile(output / 'web-experimental.zip', 'w', zipfile.ZIP_DEFLATED) as archive:
        for path in sorted(web.iterdir()):
            archive.write(path, 'web/' + path.name)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('public', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    collect(args.public, args.output)
