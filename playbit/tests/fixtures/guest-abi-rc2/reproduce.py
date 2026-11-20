#!/usr/bin/env python3
"""Verify retained candidate bytes; optionally reproduce them in disposable storage."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile


MANIFEST_SHA256 = "7c90dad9e096db75d6c9a5e7df99d20eee95274db11ceef79d70a885bb0628e3"


def checked(data, record):
    if len(data) != record["bytes"] or hashlib.sha256(data).hexdigest() != record["sha256"]:
        raise ValueError("Retained baseline digest differs: " + record["file"])
    return data


def retained(root):
    raw = (root / "manifest.json").read_bytes()
    if hashlib.sha256(raw).hexdigest() != MANIFEST_SHA256:
        raise ValueError("The frozen candidate manifest changed")
    manifest = json.loads(raw)
    if manifest["status"] != "release_candidate" or manifest["abi"] != "playbit.syscall/1":
        raise ValueError("Unexpected candidate contract")
    for record in manifest["artifacts"] + [manifest["source_snapshot"]]:
        checked((root / record["file"]).read_bytes(), record)
    return manifest


def extract_sources(root, manifest, work):
    inputs = {record["file"]: record for record in manifest["inputs"]}
    packed = (root / manifest["source_snapshot"]["file"]).read_bytes()
    with tarfile.open(fileobj=io.BytesIO(packed), mode="r:gz") as archive:
        members = archive.getmembers()
        if len(members) != len(inputs) or len({item.name for item in members}) != len(inputs):
            raise ValueError("Source inventory differs")
        for member in members:
            if not member.isfile() or member.name not in inputs:
                raise ValueError("Unexpected archived source")
            path = Path(member.name)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError("Source path escapes reproduction directory")
            data = checked(archive.extractfile(member).read(), inputs[member.name])
            target = work / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)


def reproduce(root, manifest):
    flags = {"clang": ["--version"], "wasm-ld": ["--version"],
             "rustc": ["--version", "--verbose"]}
    for name, expected in manifest["toolchains"].items():
        actual = subprocess.check_output([name, *flags[name]], text=True).strip()
        if actual.splitlines()[0] != expected.splitlines()[0]:
            raise ValueError("Compiler identity differs: " + name)
    with tempfile.TemporaryDirectory(prefix="playbit-guest-rc2-reproduce-") as directory:
        work = Path(directory)
        extract_sources(root, manifest, work)
        (work / "out").mkdir()
        for command in manifest["commands"]:
            subprocess.run(command, cwd=work, check=True)
        for record in manifest["artifacts"]:
            data = checked((work / "out" / record["file"]).read_bytes(), record)
            if data != (root / record["file"]).read_bytes():
                raise ValueError("Reproduction is not byte-identical: " + record["file"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rebuild", action="store_true",
                        help="Compile archived sources in a temporary directory and compare bytes")
    arguments = parser.parse_args()
    root = Path(__file__).resolve().parent
    manifest = retained(root)
    if arguments.rebuild:
        reproduce(root, manifest)
    else:
        with tempfile.TemporaryDirectory(prefix="playbit-guest-rc2-sources-") as directory:
            extract_sources(root, manifest, Path(directory))
    print("v1-rc2: 13 retained guest binaries, manifest and exact sources verified")


if __name__ == "__main__":
    main()
