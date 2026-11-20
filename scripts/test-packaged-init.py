#!/usr/bin/env python3
"""Check tool-free packaged initialization and durable, offline project recovery."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
from identifier import decode as decode_identifier

repo = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--app", type=Path, default=repo / "target/debug/Playbit.app")
args = parser.parse_args()
app = args.app.resolve()
bin_dir = app / "Contents/MacOS"
# Keep profile/control socket paths below macOS's Unix socket limit.
work = Path(tempfile.mkdtemp(prefix="pb-init-", dir="/tmp")).resolve()
source = work / "source"
source.mkdir()
profile = work / "profile"
tools = work / "tools"
tools.mkdir()
env = dict(os.environ, PLAYBIT_PROFILE=str(profile), PATH=str(tools),
           DEVELOPER_DIR=str(work / "missing-developer-tools"), GIT_CONFIG_NOSYSTEM="1",
           GIT_CONFIG_GLOBAL="/dev/null")
policy = '(version 1) (allow default) (deny network-outbound (remote ip "*:*"))'
sandbox = ["/usr/bin/sandbox-exec", "-p", policy]
engine = None
results = {}


def pb(*arguments, extra_policy=""):
    command_sandbox = ["/usr/bin/sandbox-exec", "-p", policy + extra_policy]
    return subprocess.run(command_sandbox + [str(bin_dir / "pb"), *arguments], cwd=source,
                          env=env, capture_output=True, text=True, timeout=30)


def start_engine(log):
    global engine
    ready, output = socket.socketpair()
    try:
        engine = subprocess.Popen(sandbox + [str(app / "Contents/Helpers/playbit-engine.app/Contents/MacOS/playbit-engine")],
                                  env=env, stdout=output, stderr=log)
    finally:
        output.close()
    with ready:
        ready.settimeout(15)
        expected = b'{"ready":true}\n'
        received = bytearray()
        while len(received) < len(expected):
            part = ready.recv(len(expected) - len(received))
            assert part, "engine exited before readiness: " + (work / "engine.log").read_text()
            received.extend(part)
        assert received == expected


def stop_engine():
    global engine
    engine.terminate()
    engine.wait(timeout=10)
    engine = None


try:
    # The packaged version is the canonical ID of the actual compiled guest icon.
    icon = app / "Contents/Resources/guest-icon"
    version = (icon / "version").read_text().removesuffix("\n")
    with (icon / "Assets.car").open("rb") as assets:
        digest = hashlib.file_digest(assets, "sha256").digest()
    assert decode_identifier(version, 32) == digest, "guest icon version must identify Assets.car"
    # Keep PATH empty of Git, Cargo, rustc and OCaml throughout initialization
    # and recovery. These operations must only need the installed app.
    with (work / "engine.log").open("wb") as log:
        start_engine(log)
        before = pb("volume", "list")
        assert before.returncode == 0, before.stderr
        assert json.loads(before.stdout) == []
        started = time.monotonic()
        initialized = pb("init", "foo/bar")
        elapsed_ms = (time.monotonic() - started) * 1000
        assert initialized.returncode == 0, initialized.stderr
        result = json.loads(initialized.stdout)
        assert not (source / "playbit.toml").exists()
        source = source / "foo/bar"
        assert result["source"] == str(source)
        assert (source / ".git").is_dir()
        assert (source / ".git/HEAD").read_text() == "ref: refs/heads/main\n"
        assert (source / "src/app.rs").is_file(), "Rust remains the default language"
        assert "volume =" not in (source / "playbit.toml").read_text()
        assert (source / "playbit-volumes.json").is_file()
        volumes = pb("volume", "list")
        assert volumes.returncode == 0, volumes.stderr
        volumes = json.loads(volumes.stdout)
        assert volumes == [], "source init must leave every scope lazy"
        assert 'app_id = "bar"' in (source / "playbit.toml").read_text()
        protected = pb("init", "--name", "Offline initialization")
        assert protected.returncode != 0 and "existing source identity differs" in protected.stderr
        assert json.loads(pb("volume", "list").stdout) == volumes
        source = work / "ocaml"
        source.mkdir()
        initialized_ocaml = pb("init", "--name", "Offline OCaml", "--app-id=custom.ocaml", "--lang", "ocaml")
        assert initialized_ocaml.returncode == 0, initialized_ocaml.stderr
        ocaml = json.loads(initialized_ocaml.stdout)
        assert (source / "src/app.ml").is_file()
        assert 'app_id = "custom.ocaml"' in (source / "playbit.toml").read_text()
        assert (source / ".git/HEAD").read_text() == "ref: refs/heads/main\n"
        assert "volume =" not in (source / "playbit.toml").read_text()
        assert (source / "playbit-volumes.json").is_file()
        initialized_volumes = json.loads(pb("volume", "list").stdout)
        assert initialized_volumes == volumes, "OCaml init must not create data backing"
        results["ocaml"] = ocaml
        name_results = []
        for case, argument, expected_name, expected_app_id in (
            ("trailing-dot", "foo/bar/.", "bar", "bar"),
            ("explicit-cwd", ".", "cat", "cat"),
            ("implicit-cwd", None, "cat", "cat"),
            ("sanitized-id", "My App_2.testing", "My App_2.testing", "myapp2.testing"),
        ):
            source = work / case / "foo/cat"
            source.mkdir(parents=True)
            initialized_name = pb("init", *([argument] if argument is not None else []))
            assert initialized_name.returncode == 0, initialized_name.stderr
            named = json.loads(initialized_name.stdout)
            named_source = Path(named["source"])
            assert ('name = "' + expected_name + '"') in (named_source / "playbit.toml").read_text()
            assert ('app_id = "' + expected_app_id + '"') in (named_source / "playbit.toml").read_text()
            named_volumes = json.loads(pb("volume", "list").stdout)
            assert named_volumes == volumes, "name selection must not create scope data"
            source = named_source
            repeated = pb("init")
            assert repeated.returncode == 0, repeated.stderr
            assert json.loads(repeated.stdout)["volume"] == named["volume"], "same app identity keeps its lazy World address"
            name_results.append(dict(argument=argument, name=expected_name, app_id=expected_app_id, source=str(named_source)))
        results["directory_names"] = name_results
        # An absolute destination beneath a search-only ancestor must behave
        # like the same destination selected from its writable working directory.
        source = work / "outside-caller"
        source.mkdir()
        restricted = work / "search-only"
        writable = restricted / "writable"
        writable.mkdir(parents=True)
        destination = writable / "new/project"
        restricted.chmod(0o111)
        try:
            external = pb("init", str(destination))
        finally:
            restricted.chmod(0o700)
        assert external.returncode == 0, external.stderr
        assert (destination / "playbit.toml").is_file()
        results["search_only_destination"] = json.loads(external.stdout)
        recovery_results = []
        for failure in ("file_creation", "git_init"):
            source = work / failure
            source.mkdir()
            before = json.loads(pb("volume", "list").stdout)
            if failure == "file_creation":
                # Refuse publication of Cargo.toml after the intent and earlier
                # files have landed. No test-only fault switch in the binary.
                deny = ' (deny file-write* (literal ' + json.dumps(str(source / "Cargo.toml")) + '))'
                interrupted = pb("init", "--name", "Retained attempt", "--app-id", "retained.app", extra_policy=deny)
            else:
                # Interrupt embedded Git after its directory has been created.
                deny = ' (deny file-write* (literal ' + json.dumps(str(source / ".git/config")) + '))'
                interrupted = pb("init", "--name", "Retained attempt", "--app-id", "retained.app", extra_policy=deny)
                assert (source / ".git").is_dir(), "expected a partially initialized repository"
            assert interrupted.returncode != 0 and not interrupted.stdout, interrupted
            assert "run `pb init`" in interrupted.stderr, interrupted.stderr
            intent = json.loads((source / ".pb-init.json").read_text())
            manifest = bytes(intent["files"]["playbit.toml"]).decode()
            assert 'app_id = "retained.app"' in manifest
            assert "volume =" not in manifest
            assert Path(intent["profile"]) == profile
            planned_initializers = bytes(intent["files"]["playbit-volumes.json"])
            during = json.loads(pb("volume", "list").stdout)
            assert during == before, "an interrupted init must not allocate data"
            if failure == "git_init":
                assert (source / "playbit-volumes.json").read_bytes() == planned_initializers
                (source / "src/app.rs").write_text("// User code retained\n")
            else:
                # Explicit conflict recovery preserves user bytes before the
                # entire scaffold has been marked prepared.
                (source / "Cargo.toml").write_text("user work")
                conflict = pb("init")
                assert conflict.returncode != 0 and "Move that file aside" in conflict.stderr
                assert (source / "Cargo.toml").read_text() == "user work"
                (source / "Cargo.toml").rename(source / "Cargo.saved")
            conflict = pb("init", "--app-id", "different.app")
            assert conflict.returncode != 0 and "different app_id" in conflict.stderr
            # Both the CLI and engine recover from disk, while Internet access
            # remains denied for every engine and CLI subprocess.
            stop_engine()
            start_engine(log)
            recovered = pb("init")
            assert recovered.returncode == 0, recovered.stderr
            recovered_result = json.loads(recovered.stdout)
            root = recovered_result["volume"]
            assert (source / "playbit-volumes.json").read_bytes() == planned_initializers
            assert 'app_id = "retained.app"' in (source / "playbit.toml").read_text()
            after = json.loads(pb("volume", "list").stdout)
            assert after == before, "recovery must leave scope data lazy"
            assert not (source / ".pb-init.json").exists()
            assert (source / ".git").is_dir()
            assert (source / ".git/HEAD").read_text() == "ref: refs/heads/main\n"
            if failure == "git_init":
                assert (source / "src/app.rs").read_text() == "// User code retained\n"
            else:
                assert (source / "Cargo.saved").read_text() == "user work"
            repeated = pb("init")
            assert repeated.returncode == 0, repeated.stderr
            assert json.loads(repeated.stdout)["volume"] == root
            stop_engine()
            start_engine(log)
            reopened = pb("init")
            assert reopened.returncode == 0, reopened.stderr
            assert json.loads(reopened.stdout)["volume"] == root, "reopened profile preserves publisher/app identity"
            assert json.loads(pb("volume", "list").stdout) == after
            recovery_results.append(dict(failure=failure, root=root, error=interrupted.stderr,
                                         timing_us=recovered_result["timing_us"]))
        results["recovery"] = recovery_results
        assert not (profile / "instance.json").exists(), "tool-free init must not enroll or obtain a signer"
        results.update(passed=True, lazy_world=result["volume"], timing_us=result["timing_us"],
                       elapsed_ms=elapsed_ms, success_stderr=initialized.stderr,
                       nonempty_stderr=protected.stderr, developer_tools_on_path=False,
                       evidence=str(work), timing_scope="one diagnostic sample; shared machine, not p99 qualification")
        (work / "evidence.json").write_text(json.dumps(results, indent=2) + "\n")
        print(json.dumps(results, indent=2))
finally:
    if engine is not None:
        engine.terminate()
        engine.wait(timeout=10)
