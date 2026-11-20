#!/usr/bin/env python3
"""Assemble relocatable SDK tar.xz files. Inputs are preinstalled pinned toolchains."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request

REPO = Path(__file__).resolve().parents[3]

def run(*command, **options):
    return subprocess.check_output([str(value) for value in command], **options).decode().strip()

def copy(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source.resolve(), destination)

def dependencies(binary):
    result = run('otool', '-L', binary).splitlines()[1:]
    return [line.strip().split(' (', 1)[0] for line in result if line.startswith('\t')]

def relocate(root):
    binaries = [p for p in root.rglob('*') if p.is_file() and (p.stat().st_mode & 0o111 or p.suffix == '.dylib') and run('file', '-b', p).startswith('Mach-O')]
    pending = list(binaries)
    while pending:
        binary = pending.pop()
        if binary.suffix == ".dylib":
            subprocess.run(["install_name_tool", "-id", "@rpath/" + binary.name, str(binary)], check=True)
        for dependency in dependencies(binary):
            if dependency.startswith(('/usr/lib/', '/System/', '@')):
                continue
            source = Path(dependency)
            if not source.is_file():
                raise ValueError(f'Missing dependency: {dependency}')
            target = root / 'lib/vendor' / source.name
            if not target.exists():
                copy(source, target)
                pending.append(target)
                binaries.append(target)
            relative = '@loader_path/' + os.path.relpath(target, binary.parent)
            subprocess.run(['install_name_tool', '-change', dependency, relative, binary], check=True)
    for binary in binaries:
        versions=[line.split()[1] for line in run('/usr/bin/vtool','-show-build',binary).splitlines() if line.strip().startswith('minos ')]
        if not versions or any(tuple(map(int,v.split('.'))) > (15,0,0) for v in versions):
            raise ValueError(f'SDK binary exceeds macOS 15 deployment target: {binary}: {versions}')
        subprocess.run(['codesign', '--force', '--sign', os.environ.get('PB_SDK_SIGN_IDENTITY', '-'), str(binary)], check=True)
        unresolved = [d for d in dependencies(binary) if not d.startswith(('/usr/lib/', '/System/', '@'))]
        if unresolved:
            raise ValueError(f'Non-relocatable binary: {binary}: {unresolved}')
        for dependency in dependencies(binary):
            candidates=[]
            if dependency.startswith('@loader_path/'):
                candidates=[binary.parent/dependency.removeprefix('@loader_path/')]
            elif dependency.startswith('@executable_path/'):
                candidates=[root/'bin'/dependency.removeprefix('@executable_path/')]
            elif dependency.startswith('@rpath/'):
                relative=dependency.removeprefix('@rpath/')
                candidates=[root/'lib'/relative,root/'lib/vendor'/relative,binary.parent/relative]
            if candidates and not any(p.is_file() and p.resolve().is_relative_to(root.resolve()) for p in candidates):
                raise ValueError(f'Missing packaged library: {binary}: {dependency}')

def assemble(language, source, compiler, binaryen, output, opam_root=None):
    with tempfile.TemporaryDirectory(prefix='playbit-sdk-') as temporary:
        root = Path(temporary) / 'sdk'
        root.mkdir()
        if language == 'ocaml':
            version = run(source / 'bin/ocamlc.opt', '-version')
            if version != '5.3.0': raise ValueError('Require OCaml 5.3.0')
            if run(binaryen/'bin/wasm-opt', '--version') != 'wasm-opt version 133 (version_133)':
                raise ValueError('Require Binaryen 133')
            for name in ('ocamlc', 'ocamldep'):
                copy(source / f'bin/{name}.opt', root / 'bin' / name)
            copy(compiler / '_build/default/compiler/bin-wasm_of_ocaml/wasm_of_ocaml.exe', root / 'bin/wasm_of_ocaml')
            for library in (binaryen/'lib').glob('*.dylib'):
                copy(library,root/'lib'/library.name)
            for name in ('wasm-opt', 'wasm-merge', 'wasm-metadce'):
                copy(binaryen / 'bin' / name, root / 'bin' / name)
            for file in sorted((source / 'lib/ocaml').rglob('*')):
                if file.is_file() and (file.suffix in ('.cmi', '.cma', '.cmo') or file.name.startswith('camlheader') or file.name == 'runtime-launch-info'):
                    copy(file, root / 'lib/ocaml' / file.relative_to(source / 'lib/ocaml'))
            revision = run('git', '-C', compiler, 'rev-parse', 'HEAD')
            if run('git', '-C', compiler, 'status', '--porcelain'):
                raise ValueError('Commit compiler changes locally before assembling a release')
            sources = root / 'sources/js_of_ocaml.tar'
            sources.parent.mkdir()
            with sources.open('wb') as stream:
                subprocess.run(['git', '-C', str(compiler), 'archive', 'HEAD'], stdout=stream, check=True)
            provenance = dict(ocaml='5.3.0', binaryen='133', compiler_revision=revision,
                              deployment_target='15.0',configure=['--without-zstd','ac_cv_func_pipe2=no','ac_cv_func_dup3=no'])
            if opam_root is None: raise ValueError('OCaml assembly requires --opam-root for dependency provenance/licenses')
            copy(REPO/'playbit/sdk/ocaml/toolchain/ocaml.opam-packages',root/'sources/ocaml.opam-packages')
            for file in (opam_root/'playbit/doc').rglob('*'):
                if file.is_file() and file.name.lower().startswith(('license','copying','copyright','notice')):
                    copy(file,root/'licenses/opam'/file.relative_to(opam_root/'playbit/doc'))
            ocaml_source=source.parent/'ocaml-5.3.0.tar.gz'
            if not ocaml_source.is_file() or hashlib.sha256(ocaml_source.read_bytes()).hexdigest() != 'eb9eab2f21758d3cfb1e78c7f83f0b4dd6302824316aba4abee047a5a4f85029':
                raise ValueError('Require the verified original OCaml source archive beside the installed prefix')
            copy(ocaml_source,root/'sources/ocaml-5.3.0.tar.gz')
            for component, url in [('ocaml','https://raw.githubusercontent.com/ocaml/ocaml/5.3.0/LICENSE'),('binaryen','https://raw.githubusercontent.com/WebAssembly/binaryen/version_133/LICENSE')]:
                with urllib.request.urlopen(url,timeout=30) as response: license_bytes=response.read(1024*1024+1)
                if len(license_bytes) > 1024*1024: raise ValueError('License file exceeds limit')
                (root/'licenses').mkdir(exist_ok=True)
                (root/'licenses'/f'{component}-LICENSE').write_bytes(license_bytes)
                provenance[component+'_license_sha256']=hashlib.sha256(license_bytes).hexdigest()

            for name in ('LICENSE', 'COPYING', 'LICENSE.md'):
                if (compiler / name).is_file(): copy(compiler / name, root / 'licenses' / ('js_of_ocaml-' + name))
            for name, directory in [('ocaml', source / 'lib/ocaml'), ('binaryen', binaryen)]:
                for file in directory.glob('*'):
                    if file.is_file() and file.name.lower().startswith(('license', 'copying')):
                        copy(file, root / 'licenses' / (name + '-' + file.name))
        else:
            if not run(source / 'bin/rustc', '--version').startswith('rustc 1.99.0 '): raise ValueError('Require Rust 1.99.0')
            for name in ('cargo', 'rustc', 'rustdoc'):
                copy(source / 'bin' / name, root / 'bin' / name)
            (root/'lib').mkdir()
            for file in (source/'lib').iterdir():
                if file.is_file(): copy(file,root/'lib'/file.name)
            for target in ('aarch64-apple-darwin','wasm32-unknown-unknown'):
                shutil.copytree(source/'lib/rustlib'/target,root/'lib/rustlib'/target,symlinks=False)

            for file in (source / 'share/doc/rust').glob('*'):
                if file.is_file() and file.name.startswith(('LICENSE', 'COPYRIGHT')):
                    copy(file, root / 'licenses' / file.name)
            provenance = dict(rust=run(source / 'bin/rustc', '-vV'))
        (root / 'sdk.json').write_text(json.dumps(dict(language=language, platform='macos-arm64', driver=1, sources=provenance), sort_keys=True) + '\n')
        relocate(root)
        files = sorted(root.rglob('*'))
        expanded = sum(p.stat().st_size for p in files if p.is_file())
        if len(files) + 1 > 50000 or expanded > 4 * 1024**3: raise ValueError('SDK exceeds client limits')
        def normalize(info):
            if not (info.isfile() or info.isdir()): raise ValueError('Only files/directories are permitted')
            info.uid = info.gid = info.mtime = 0
            info.uname = info.gname = ''
            info.mode = 0o755 if info.isdir() or info.mode & 0o111 else 0o644
            return info
        output.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(output, 'w:xz', preset=6, format=tarfile.USTAR_FORMAT) as archive:
            archive.add(root, arcname='sdk', recursive=False, filter=normalize)
            for file in files: archive.add(file, arcname=str(Path('sdk') / file.relative_to(root)), recursive=False, filter=normalize)
        if output.stat().st_size > 1024**3: raise ValueError('Compressed SDK exceeds limit')
        result = dict(language=language, version=('5.3.0-playbit.1' if language == 'ocaml' else '1.99.0-playbit.1'), platform='macos-arm64', driver=1, bytes=output.stat().st_size, expanded_bytes=expanded)
        output.with_suffix(output.suffix + '.json').write_text(json.dumps(result, sort_keys=True) + '\n')
        print(json.dumps(dict(archive=str(output), **result)))

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('language', choices=['ocaml', 'rust'])
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--compiler', type=Path)
    parser.add_argument('--binaryen', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--opam-root', type=Path)
    options = parser.parse_args()
    assemble(options.language, options.source, options.compiler, options.binaryen, options.output, options.opam_root)
