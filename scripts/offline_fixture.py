#!/usr/bin/env python3
"""Prepare isolated offline signing authority for local examples and tests.

The root test hook uses a synthetic fixture issuer with valid certificates and
production package validation. It does not enroll an AP account or grant consent.
Production builds never invoke this helper implicitly.
"""
from pathlib import Path
import json
import os
import shutil
import subprocess
import sys

from identifier import decode


def prepare_offline_profile(profile):
    """Create exactly this fresh profile under the caller's private directory."""
    profile = Path(profile)
    if not profile.is_absolute():
        raise ValueError("offline fixture profile must be absolute")
    if profile.exists() or profile.is_symlink():
        raise ValueError("offline fixture profile must not exist")
    repo = Path(__file__).resolve().parents[1]
    environment = dict(os.environ, PLAYBIT_PACKAGED_WORKFLOW_PROFILE=str(profile))
    environment.setdefault("SCCACHE_SERVER_UDS", str(repo / "_sccache.sock"))
    environment["CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS"] = "true"
    environment["CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS"] = "true"
    # This profile generator also builds native-runtime dependencies and CLI
    # targets through Cargo's integration-test graph. Link fixture utilities
    # without LTO, retaining the release optimization level and correctness checks.
    environment["CARGO_PROFILE_RELEASE_LTO"] = "off"
    command = ["cargo", "test", "--release", "--locked", "-p", "playbit",
        "--no-default-features", "--features", "network,test-support",
        "--test", "packaged_workflow"]
    # Cold release compilation includes the native runtime. Give compilation its
    # own bounded allowance; it must not consume the fixture's execution deadline.
    subprocess.run([*command, "--no-run"], cwd=repo, env=environment, check=True,
        stdout=sys.stderr, timeout=1200)
    subprocess.run([*command, "prepare_offline_signer_profile", "--",
        "--exact", "--ignored"], cwd=repo, env=environment, check=True,
        stdout=sys.stderr, timeout=600)
    if not profile.is_dir() or (profile / "instance.json").exists():
        raise RuntimeError("offline fixture did not retain its expected local-only authority")


def sign_template(app, source, destination, environment, sandbox):
    """Sign a private template copy through the caller's isolated fixture engine."""
    profile = Path(environment["PLAYBIT_PROFILE"])
    if not profile.is_absolute():
        raise ValueError("offline template signing requires an absolute fixture profile")
    destination = Path(destination).absolute()
    with Path(source).open("rb") as original, os.fdopen(os.open(destination,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as copied:
        shutil.copyfileobj(original, copied)
        copied.flush()
        os.fsync(copied.fileno())
    request = dict(jsonrpc="2.0", id=1, method="tools/call",
        params=dict(name="playbit", arguments=dict(op="sign-package", bundle=str(destination))))
    result = subprocess.run([*sandbox, str(Path(app) / "Contents/MacOS/playbit-mcp")],
        input=json.dumps(request) + "\n", env=environment, capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or "offline template signing failed")
    response = json.loads(result.stdout)
    if response.get("id") != 1 or "error" in response:
        raise RuntimeError("offline template signing returned an invalid response")
    reply = response["result"]
    if reply.get("isError", False):
        raise RuntimeError(reply["content"][0]["text"])
    decode(json.loads(reply["content"][0]["text"])["build_id"], 32)
    return destination


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", type=Path, help="new absolute profile path in a private directory")
    prepare_offline_profile(parser.parse_args().profile)
