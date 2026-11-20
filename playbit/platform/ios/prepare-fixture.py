#!/usr/bin/env python3
"""Build a starter through pb init, Cargo and pb bundle.

An explicit isolated offline fixture issuer signs this temporary profile's
packages. This does not qualify AP enrollment or recovery. Only the signed
.pbapp is copied; no development scope data or credentials enter the iOS app.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("app", choices=("todo", "shared-note", "ui-gallery", "micro-kart"))
parser.add_argument("output", type=Path)
parser.add_argument("--kit", type=Path, required=True)
parser.add_argument("--profile", choices=("debug", "release"), required=True)
args = parser.parse_args()
repo = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(repo / "scripts"))
from offline_fixture import prepare_offline_profile
guest, destination = args.app, args.output.resolve()
flags = ["--release"] if args.profile == "release" else []
binary = args.kit.resolve() / "Contents/MacOS"
engine_path = args.kit.resolve() / "Contents/Helpers/playbit-engine.app/Contents/MacOS/playbit-engine"
for tool in (binary / "pb", engine_path):
    if not tool.is_file():
        parser.error(f"kit is missing {tool}")
# Profile-owned Unix sockets must fit macOS SUN_LEN even when TMPDIR is long.
with tempfile.TemporaryDirectory(prefix="playbit-ios-source-", dir="/tmp") as temporary:
    work = Path(temporary).resolve()
    init_kit = work / "Playbit.app"
    subprocess.run([sys.executable, str(repo / "playbit/platform/macos/developer-resources.py"),
                    "--todo-fixture", str(args.kit.resolve()), str(init_kit)], check=True)
    source = work / "source"
    source.mkdir()
    profile = work / "profile"
    prepare_offline_profile(profile)
    environment = dict(os.environ, PLAYBIT_PROFILE=str(profile))
    policy = '(version 1) (allow default) (deny network-outbound (remote ip "*:*"))'
    sandbox = ["/usr/bin/sandbox-exec", "-p", policy]
    ready, output = socket.socketpair()
    with (work / "engine.stderr").open("wb") as log:
        engine = subprocess.Popen(sandbox + [str(engine_path)],
            env=environment, stdout=output, stderr=log)
        output.close()
        try:
            with ready:
                ready.settimeout(10)
                expected = b'{"ready":true}\n'
                observed = bytearray()
                while len(observed) < len(expected):
                    chunk = ready.recv(len(expected) - len(observed))
                    if not chunk:
                        raise RuntimeError("fixture engine exited before readiness: " + (work / "engine.stderr").read_text())
                    observed.extend(chunk)
                assert observed == expected
            subprocess.run(sandbox + [str(init_kit / "Contents/MacOS/pb"), "init", "--name", guest],
                cwd=source, env=environment, check=True, stdout=subprocess.DEVNULL)
            guest_source = repo / "guests" / guest
            # The init fixture supplies TODO. Other fixtures
            # replace Cargo's actual library entry point, not an unused lib.rs.
            if guest != "todo":
                shutil.copyfile(guest_source / "src/lib.rs", source / "src/app.rs")
            if (guest_source / "assets").is_dir():
                shutil.copytree(guest_source / "assets", source / "assets", dirs_exist_ok=True)
            if guest != "todo":
                # The replacement app has no TODO instrumentation or compiled fingerprint.
                manifest = source / "playbit.toml"
                lines = []
                trace_table = False
                for line in manifest.read_text().splitlines(keepends=True):
                    if line.startswith("["):
                        table = line.strip().strip("[]")
                        trace_table = table == "trace" or table.startswith("trace.")
                    if not trace_table:
                        lines.append(line)
                manifest.write_text("".join(lines))
            subprocess.run([str(binary / "pb"), "build", *flags], cwd=source, env=environment, check=True)
            result = subprocess.run([str(binary / "pb"), "bundle", *flags],
                cwd=source, env=environment, check=True, stdout=subprocess.PIPE)
            bundle = json.loads(result.stdout)
            destination.parent.mkdir(parents=True, exist_ok=True)
            if Path(bundle["bundle"]).resolve() != destination:
                shutil.copyfile(bundle["bundle"], destination)
            print(json.dumps({"bundle":str(destination), "build_id":bundle["build_id"], "guest":guest}))
        finally:
            engine.terminate()
            engine.wait(timeout=5)
