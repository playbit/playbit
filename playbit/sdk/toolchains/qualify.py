#!/usr/bin/env python3
"""Functionally qualify an SDK archive with packaged pb and no developer tool paths."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

REPO=Path(__file__).resolve().parents[3]

def qualify(app, archive, output):
    output.unlink(missing_ok=True)
    if app.is_relative_to(REPO): raise ValueError('Move the packaged app outside the checkout before qualification')
    metadata=json.loads(archive.with_suffix(archive.suffix+'.json').read_text())
    language=metadata['language']
    with tempfile.TemporaryDirectory(prefix='SDK qualification ') as temporary:
        root=Path(temporary)
        with tarfile.open(archive,'r:xz') as source: source.extractall(root,filter='data')
        sdk=root/'sdk'; project=root/'project with spaces';project.mkdir()
        resources=app/'Contents/Resources'
        template=resources/('ocaml-template' if language=='ocaml' else 'project-template')
        shutil.copytree(template,project,dirs_exist_ok=True)
        if language=='rust':
            cargo=project/'Cargo.toml'
            cargo.write_text(cargo.read_text().replace('@PLAYBIT_API@','target/playbit/sdk/playbit-api').replace('@PLAYBIT_NATIVE_MAIN@','target/playbit/sdk/native-main.rs'))
            (project/'playbit.toml').write_text('name = "SDK Rust test"\nbuild_cmd = "cargo rustc --lib --crate-type cdylib"\nvolume = "1111111111111111"\n[release]\nbuild_cmd = "cargo rustc --release --lib --crate-type cdylib"\n')
        environment={k:v for k,v in os.environ.items() if k in ('HOME','TMPDIR','USER','LOGNAME','SCCACHE_SERVER_UDS')}
        (root/'home').mkdir()
        environment.update(HOME=str(root/'home'),PATH=str(sdk/'bin')+':/usr/bin:/bin',OCAMLLIB=str(sdk/'lib/ocaml'),OCAMLFIND_CONF='/dev/null',CARGO_HOME=str(root/'cargo-home'),RUSTUP_AUTO_INSTALL='0',CARGO_BUILD_JOBS='2',PLAYBIT_PROFILE=str(root/'profile'),PLAYBIT_CHANNEL='testing')
        pb=app/'Contents/MacOS/pb'
        deny=[str(REPO),str(Path.home()/'.opam'),str(Path.home()/'.rustup'),'/opt/homebrew','/Applications/Xcode.app','/Library/Developer/CommandLineTools']
        policy='(version 1) (allow default) '+''.join('(deny file-read* (subpath '+json.dumps(path)+'))' for path in deny)
        def build(release=False,success=True):
            command=['/usr/bin/sandbox-exec','-p',policy,str(pb),'build','--sdk=system']+(['--release'] if release else [])
            result=subprocess.run(command,cwd=project,env=environment,text=True,capture_output=True)
            if (result.returncode==0)!=success: raise ValueError(result.stdout+'\n'+result.stderr)
            print(result.stderr,end='',flush=True)
        build();build(True)
        product=next((project/'target/debug').glob('*.pbapp'))
        previous=hashlib.sha256(product.read_bytes()).hexdigest()
        source=project/'src'/('app.ml' if language=='ocaml' else 'app.rs')
        original=source.read_text();source.write_text(original+'\nthis is invalid syntax !!!\n')
        build(success=False)
        if hashlib.sha256(product.read_bytes()).hexdigest()!=previous:raise ValueError('Failed build replaced working bundle')
        source.write_text(original)
        if language=='ocaml':
            (project/'src/helper.mli').write_text('val label : string\n')
            (project/'src/helper.ml').write_text('let label = "OCaml on Playbit"\n')
            source.write_text(original.replace('"OCaml on Playbit"','Helper.label'))
            build()
            (project/'src/helper.ml').write_text('let label = "A changed label"\n');build()
            (project/'src/helper.ml').write_text('let label = App.label\n');build(success=False)
        else: build()
    with archive.open('rb') as stream: checksum=hashlib.file_digest(stream,'sha256').hexdigest()
    with (app/'Contents/MacOS/pb').open('rb') as stream: pb_checksum=hashlib.file_digest(stream,'sha256').hexdigest()
    output.write_text(json.dumps(dict(status='success',language=language,archive=str(archive),sha256=checksum,pb_sha256=pb_checksum,bytes=archive.stat().st_size,scenarios=['minimal-PATH','developer-paths-denied','debug','release','failed-build-retains-bundle','rebuild']+(['multi-module','interface','dependency-cycle'] if language=='ocaml' else [])),sort_keys=True)+'\n')

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app',type=Path,required=True)
    parser.add_argument('--archive',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();qualify(args.app.resolve(),args.archive.resolve(),args.output.resolve())
