#!/usr/bin/env python3
"""Init/build/bundle and execute TODO in its matching GUI owner, then reopen it.

This correctness smoke uses the installed SDK, stable target-profile products
and real GUI owners with an explicit isolated offline fixture issuer. It does
not qualify AP enrollment, account recovery or physical UI.
"""
import argparse
import base64
from identifier import decode as decode_identifier
from offline_fixture import prepare_offline_profile
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import socket
import select
import shutil
import sys
import plistlib
import tomllib

repo = Path(__file__).resolve().parents[1]
os.environ.setdefault("SCCACHE_SERVER_UDS", str(repo / "_sccache.sock"))
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--native", action="store_true", help="also check generated native Cargo builds with the installed SDK")
parser.add_argument("--app", type=Path, default=repo / "target/debug/Playbit.app")
args = parser.parse_args()
# Keep the full profile/control socket paths below macOS's Unix socket limit.
# The per-user temporary directory can be too long even with a short prefix.
work = Path(tempfile.mkdtemp(prefix="pb-workflow-", dir="/tmp")).resolve()
app = args.app.resolve()
if args.native and repo in app.parents:
    copied = work / "Playbit.app"
    shutil.copytree(app, copied)
    app = copied
init_kit = work / "init-kit/Playbit.app"
subprocess.run([sys.executable, str(repo / "playbit/platform/macos/developer-resources.py"),
                "--todo-fixture", str(app), str(init_kit)], check=True)
bin_dir = app / "Contents/MacOS"
profile = work / "profile"
env = dict(os.environ, PLAYBIT_PROFILE=str(profile), PATH=str(bin_dir) + os.pathsep + os.environ["PATH"])
policy = ('(version 1) (allow default) (deny network-outbound (remote ip "*:*"))'
          + (' (deny file-read* (subpath ' + json.dumps(str(repo)) + '))' if args.native else ''))
sandbox = ["/usr/bin/sandbox-exec", "-p", policy]
probe = subprocess.run(sandbox + ["/usr/bin/python3", "-c", "import socket; socket.create_connection(('1.1.1.1',443),timeout=2)"], capture_output=True)
assert probe.returncode != 0 and b"Operation not permitted" in probe.stderr, probe.stderr.decode()
source = work / "todo"
source.mkdir()
env["CARGO_TARGET_DIR"] = str(source / "target")
engine = None
mcp = None
known_owner = None
quit_attempted = False
log = (work / "engine.log").open("ab")

def pb(*args, tool_env=None, cwd=source):
    # Launch Services strips arguments from sandboxed callers. The matching
    # GUI launch is trusted native code; guest I/O stays in the offline engine.
    launcher = [] if len(args) == 3 and args[:2] == ("run", "--session") else sandbox
    executable = init_kit / "Contents/MacOS/pb" if args[:1] == ("init",) else bin_dir / "pb"
    result = subprocess.run(launcher + [str(executable), *map(str, args)], env=tool_env or env, cwd=cwd, capture_output=True, timeout=180)
    if result.returncode:
        raise RuntimeError(result.stderr.decode().strip() or "CLI failed without an error")
    return result.stdout.decode().strip()

def request(*args, tool_env=None, cwd=source):
    return json.loads(pb(*args, tool_env=tool_env, cwd=cwd))

def call(operation, success=True):
    # One process retains Session tool-context ownership across syscall requests.
    global mcp
    if mcp is None:
        mcp = subprocess.Popen(sandbox + [str(bin_dir / "playbit-mcp")], env=env, cwd=source,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, bufsize=0)
    call.serial += 1
    message = dict(jsonrpc="2.0", id=call.serial, method="tools/call",
                   params=dict(name="playbit", arguments=operation))
    mcp.stdin.write((json.dumps(message) + "\n").encode())
    mcp.stdin.flush()
    deadline = time.monotonic() + 30
    while b"\n" not in call.buffer:
        remaining = deadline - time.monotonic()
        assert remaining > 0 and select.select([mcp.stdout], [], [], remaining)[0], operation["op"]
        chunk = os.read(mcp.stdout.fileno(), 65536)
        assert chunk, (operation, mcp.poll())
        call.buffer.extend(chunk)
        assert len(call.buffer) <= 4 * 1024 * 1024
    line, _, tail = call.buffer.partition(b"\n")
    call.buffer = bytearray(tail)
    response = json.loads(line)
    assert response["id"] == call.serial and "error" not in response, response
    result = response["result"]
    if success and result.get("isError", False):
        raise RuntimeError(result["content"][0]["text"])
    assert (not result.get("isError", False)) == success, (operation, result)
    return json.loads(result["content"][0]["text"]) if success else result

call.serial = 0
call.buffer = bytearray()


def close_mcp():
    global mcp
    if mcp is not None:
        mcp.stdin.close()
        try:
            assert mcp.wait(timeout=5) == 0
        finally:
            if mcp.poll() is None:
                mcp.kill()
                mcp.wait()
            mcp = None
            call.buffer.clear()


def objects():
    context = call({"op":"tool-context-open", "scope":{"kind":"session", "binding":session_id}})["context"]
    try:
        outcome = call({"op":"syscall", "context":context, "operation":1, "mutation":0,
            "call":{"volume":shared, "operation":{"Query":{"entry_id":database,
                "type_id":type_id, "after":None, "limit":64,
                "projection":[title_field, done_field]}}}})
        assert outcome["context"] == context and outcome["operation"] == 1
        assert outcome["mutation"] == 0
        if outcome["status"] == -5:  # ABI Error::Unavailable; a read may be retired by a commit.
            raise RuntimeError("Tool data operation failed: Unavailable")
        assert outcome["status"] == 0, outcome
        assert outcome["revision"] is not None
        page = outcome["result"]["Page"]
        assert page["next"] is None
        return page["records"]
    finally:
        call({"op":"tool-context-close", "context":context})

def quit_owner():
    global known_owner, quit_attempted
    assert known_owner is not None and not quit_attempted
    # A fresh launch ACK can precede both claim and bootstrap. Read only the
    # original owner until its actual committed controls are ready for Quit.
    deadline = time.monotonic() + 15
    while True:
        try:
            frame = request("frame", known_owner[0], "--instance", known_owner[1])["Frame"]
            controls = json.loads(subprocess.check_output([str(oracle)],
                input=(json.dumps(dict(view=frame["scene"])) + "\n").encode()))
            if isinstance(controls, list) and len(controls) == 2 and all(not c["disabled"] for c in controls):
                break
        except RuntimeError:
            if time.monotonic() >= deadline: raise
        assert time.monotonic() < deadline, "known owner has not finished bootstrap/edit"
        time.sleep(.01)
    deadline = time.monotonic() + 15
    while True:
        quit_attempted = True
        try:
            result = request("stop", known_owner[0], "--instance", known_owner[1])
            break
        except RuntimeError as error:
            # This exact GUI reply confirms no intent was admitted. Never
            # repeat a lost exchange, an accepted Quit or any other error.
            # Trace diagnostics can follow the CLI's actual error record.
            messages = [line.partition("] pb: ")[2] for line in str(error).splitlines()
                        if "] pb: " in line]
            if messages != ["The app lifecycle queue is busy; Quit was not requested"]:
                raise
            quit_attempted = False
            assert time.monotonic() < deadline, "known owner lifecycle queue stayed busy"
            time.sleep(.01)
    assert result["Accepted"]["instance"] == known_owner[1]
    deadline = time.monotonic() + 15
    while request("session", "instances", known_owner[0]):
        assert time.monotonic() < deadline, "accepted Quit has not retired the owner"
        time.sleep(.01)
    known_owner = None
    quit_attempted = False

def start():
    global engine
    ready, output = socket.socketpair()
    try:
        engine = subprocess.Popen(sandbox + [str(bin_dir.parent / "Helpers/playbit-engine.app/Contents/MacOS/playbit-engine")], env=env, stdout=output, stderr=log)
    finally: output.close()
    with ready:
        expected = b'{"ready":true}\n'
        received = bytearray()
        deadline = time.monotonic() + 10
        while len(received) < len(expected):
            remaining = deadline - time.monotonic()
            if remaining <= 0: raise RuntimeError("engine readiness deadline")
            ready.settimeout(remaining)
            part = ready.recv(len(expected) - len(received))
            if not part: raise RuntimeError("engine exited before readiness: " + (work / "engine.log").read_text())
            received.extend(part)
        assert received == expected

def stop():
    global engine
    close_mcp()
    if engine is not None:
        engine.terminate()
        engine.wait(timeout=10)
        engine = None

try:
    # Cleanup needs the decoder even if the first admitted input fails.
    subprocess.run(["cargo", "build", "--locked", "--release", "-p", "playbit",
        "--no-default-features", "--features", "presentation", "--bin", "check-presentation"],
        cwd=repo, check=True, timeout=180)
    oracle = work / "check-presentation"
    shutil.copy2(repo / "target/release/check-presentation", oracle)
    if args.native:
        sdk = request("sdk-path")["path"]
        assert Path(sdk) == bin_dir.parent / "Resources/native-sdk"
        assert not profile.exists(), "SDK discovery must not create an engine profile"
    prepare_offline_profile(profile)
    start()
    world = request("init", "--name", "Offline smoke test")["volume"]
    assert not request("volume", "list"), "source init must leave World data lazy"
    assert sorted(path.name for path in (source / "src").iterdir()) == ["app.rs"]
    assert not (source / ".playbit/sdk").exists()
    assert not (source / ".playbit/native").exists()
    manifest = tomllib.loads((source / "Cargo.toml").read_text())
    api_path = manifest["dependencies"]["playbit-api"]["path"]
    assert api_path == "target/playbit/sdk/playbit-api"
    assert (source / api_path / "src/lib.rs").read_bytes() == (app / "Contents/Resources/playbit-api/src/lib.rs").read_bytes()
    # First-party API dependencies must build without the developer's registry cache.
    build_env = dict(env, CARGO_NET_OFFLINE="true", CARGO_HOME=str(work / "empty-cargo-home"))
    built = request("build", tool_env=build_env)
    assert Path(built["bundle"]) == source / "target/debug/Offline smoke test.pbapp"
    assert not request("volume", "list"), "source build must not allocate scope data"
    assert not request("session", "list"), "source build must not create a session"
    declaration = json.loads((source / "playbit-volumes.json").read_text())["session"]
    database_record = next(item for item in declaration["databases"]
                           if item["name"] == "data" and item["kind"]["type"] == "object_db")
    database = database_record["id"]
    type_id, type_record = next(iter(database_record["kind"]["schema"]["types"].items()))
    title_field = next(key for key, field in type_record["fields"].items() if field["name"] == "title")
    done_field = next(key for key, field in type_record["fields"].items() if field["name"] == "done")
    bundle = request("bundle")["bundle"]
    assert built["bundle"] == bundle
    authenticated = call({"op":"authenticate-package", "bundle":bundle})
    assert authenticated["build_id"] == built["build_id"]
    app_identity = authenticated["app"]
    session = call({"op":"create-session", "bundle":bundle, "root":None, "user":None})
    session_id = session["id"]
    # MCP retains the internal ID; CLI controls require the full launch reference.
    reference = call({"op":"session-reference", "session":session_id})["reference"]
    assert session["root_volume"] == world
    shared = session["shared_volume"]
    decode_identifier(shared, 16)
    assert shared != world, "Shared backing must remain distinct from World"
    assert session["app"] == app_identity
    slots = call({"op":"automatic-volumes", "session":session_id})["slots"]
    assert all(slot["state"] == "lazy" for slot in slots), "session creation must leave scopes lazy"
    assert next(slot["volume"] for slot in slots if slot["role"] == "session") == shared
    opened = request("run", "--session", reference, cwd=work)
    known_owner = (reference, opened["instance"])
    assert opened["session"]["id"] == reference
    assert opened["session"]["resolved_build_id"] == built["build_id"]

    def observed(expected_count):
        # Read-only durable observation after a known input ACK; no input replay.
        deadline = time.monotonic() + 15
        while True:
            try:
                records = objects()
            except RuntimeError as error:
                # The admitted commit can retire a read before its result arrives.
                messages = [line.partition("] pb: ")[2] for line in str(error).splitlines()
                            if "] pb: " in line]
                if str(error) != "Tool data operation failed: Unavailable" and messages != ["Tool data operation failed: Unavailable"]:
                    raise
            else:
                if len(records) == expected_count:
                    return records
            assert time.monotonic() < deadline, "accepted edit did not commit"
            time.sleep(.01)

    # Wait for actual local startup before one input. Only Frame is repeated.
    deadline = time.monotonic() + 15
    while True:
        frame = subprocess.run(sandbox + [str(bin_dir / "pb"), "frame", reference,
            "--instance", known_owner[1]], env=env, cwd=source, capture_output=True, timeout=30)
        if frame.returncode == 0 and b"TODO" in bytes(json.loads(frame.stdout)["Frame"]["scene"]):
            break
        assert time.monotonic() < deadline, "known GUI owner did not become ready"
        time.sleep(.01)
    assert len(observed(1)) == 1, "first authorized use must seed one TODO in Session scope"
    request("input", reference, "submit", 1, "Retained offline task", "--instance", known_owner[1])
    before = observed(2)
    added = next(obj for obj in before if any(field["field_id"] == title_field and field["values"] == [{"Text": "Retained offline task"}] for field in obj["fields"]))
    deadline = time.monotonic() + 15
    while True:
        frame = request("frame", reference, "--instance", known_owner[1])["Frame"]
        if b"Retained offline task" in bytes(frame["scene"]): break
        assert time.monotonic() < deadline
        time.sleep(.01)
    controls = json.loads(subprocess.check_output([str(oracle)],
        input=(json.dumps(dict(view=frame["scene"])) + "\n").encode()))
    targets = [row["id"] for row in controls if row["label"] == "Retained offline task" and not row["disabled"]]
    assert len(targets) == 1
    request("input", reference, "click", targets[0], "--instance", known_owner[1])
    deadline = time.monotonic() + 15
    while True:
        before = observed(2)
        added = next(obj for obj in before if obj["id"] == added["id"])
        if any(field["field_id"] == done_field and field["values"] == [{"Boolean": True}] for field in added["fields"]): break
        assert time.monotonic() < deadline, "accepted toggle did not commit"
        time.sleep(.01)
    assert call({"op":"session-reference", "session":session_id})["reference"] == reference
    invitation = call({"op":"invitation", "session":session_id})["invitation"]
    assert invitation.endswith("/s#" + reference), "Shared invitation must keep the original session capability"
    pending = request("status")["pending"]
    registrations = [op["operation"] for op in pending
                     if op["operation"]["kind"] in ("register_volume", "register_session")]
    # TODO uses Session data. Opening logical Files roots must leave World and
    # the private scopes lazy, without publishing unused backing volumes.
    assert sorted(op["kind"] for op in registrations) == ["register_session", "register_volume"], \
        "Shared and the retained session must each register once"
    volumes = {op["volume"]["id"] for op in registrations if op["kind"] == "register_volume"}
    registered = next(op["session"] for op in registrations if op["kind"] == "register_session")
    assert volumes == {shared}, "Only the used Shared backing must enter publication"
    assert registered == dict(session, id=reference), "CLI status must retain the exact Shared binding"
    used_slots = call({"op":"automatic-volumes", "session":session_id})["slots"]
    assert sorted((slot["role"], slot["state"]) for slot in used_slots) == [
        ("local", "lazy"), ("local-session", "lazy"), ("root", "lazy"),
        ("session", "ready"), ("user", "lazy"), ("user-session", "lazy")], \
        "Only the used Session scope must be initialized"
    assert {volume["id"] for volume in request("volume", "list")} == {shared}
    quit_owner()
    stop()
    start()
    reopened = request("run", "--session", reference, cwd=work)
    known_owner = (reference, reopened["instance"])
    assert reopened["session"]["id"] == reference
    assert reopened["session"]["resolved_build_id"] == built["build_id"]
    assert reopened["instance"] != opened["instance"]
    assert call({"op":"prepare-session", "session":session_id})["session"] == session
    assert objects() == before
    assert call({"op":"session-reference", "session":session_id})["reference"] == reference
    assert call({"op":"invitation", "session":session_id})["invitation"] == invitation
    quit_owner()
    assert request("status")["pending"] == pending
    assert call({"op":"automatic-volumes", "session":session_id})["slots"] == used_slots
    assert {volume["id"] for volume in request("volume", "list")} == {shared}
    output = {"passed": True, "network": "engine/data tools IPv4/IPv6 outbound denied; native Launch Services caller", "profile": str(profile),
        "world": world, "shared": shared, "session": session_id,
        "build_id": session["resolved_build_id"],
        "guest_write_and_process_restart": True, "stable_pending_operations": True,
        "lazy_session_data": True, "local_reference_sha256": hashlib.sha256(reference.encode()).hexdigest(),
        "authority": "explicit isolated offline fixture issuer; no AP enrollment or account-recovery qualification"}
    if args.native:
        native_volumes = request("volume", "list")
        # Native builds use the installed Rust SDK, launcher and C archive. A
        # separate Cargo feature keeps native startup out of ordinary Wasm builds.
        other_tools = work / "other-tools"
        other_tools.mkdir()
        wrong_pb = other_tools / "pb"
        wrong_pb.write_text("#!/bin/sh\nprintf 'wrong Playbit installation\\n' >&2\nexit 99\n")
        wrong_pb.chmod(0o755)
        native_env = dict(build_env, PATH=str(other_tools) + os.pathsep + os.environ["PATH"],
                          CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS="-Dwarnings -C link-arg=-Wl,-fatal_warnings")
        native_debug_app = Path(request("build", "--native", tool_env=native_env)["bundle"])
        assert native_debug_app == source / "target/debug/Offline smoke test.app"
        subprocess.run(["codesign", "--verify", "--deep", "--strict", str(native_debug_app)], check=True)
        native_command = ["cargo", "build", "--offline", "--release", "--features", "native",
                          "--target", "aarch64-apple-darwin"]
        def native_build(extra_env=None, target=None):
            command = [str(bin_dir / "pb"), "build", "--native", "--release"] if target is None else native_command[:-1] + [target]
            return subprocess.run(sandbox + command, cwd=source,
                                  env=dict(native_env, **(extra_env or {})),
                                  capture_output=True, timeout=180)
        result = native_build()
        assert result.returncode == 0, result.stderr.decode()
        native_app = Path(json.loads(result.stdout)["bundle"])
        assert native_app == source / "target/release/Offline smoke test.app"
        assert native_app != native_debug_app
        assert native_app.name == "Offline smoke test.app"
        metadata = plistlib.loads((native_app / "Contents/Info.plist").read_bytes())
        assert metadata["CFBundleName"] == "Offline smoke test"
        native_identifier = metadata["CFBundleIdentifier"]
        # Match AppIdentity::short_bundle_id using the verified publisher, not a data root.
        publisher_suffix = base64.b32hexencode(decode_identifier(app_identity["publisher"])[:10]).decode().lower()
        assert native_identifier == "playbit.native." + app_identity["app_id"] + "." + publisher_suffix
        assert metadata["LSMinimumSystemVersion"] == "15.0"
        assert (native_app / "Contents/Resources/app.icns").read_bytes().startswith(b"icns")
        assert {path.name for path in (native_app / "Contents/Resources/licenses").iterdir()} == {
            "AccessKit-MIT.txt", "playbit-api.txt", "Playbit.txt"}
        subprocess.run(["codesign", "--verify", "--deep", "--strict", str(native_app)], check=True)
        assert request("volume", "list") == native_volumes, "native build must not allocate scope data"
        assert request("bundle", "--native", "--release", tool_env=native_env)["bundle"] == str(native_app)
        bundle_executable = native_app / "Contents/MacOS/app"
        original_bundle = hashlib.sha256(bundle_executable.read_bytes()).hexdigest()
        # A changed role initializer must not package a binary with the old seed.
        root_path = source / "playbit-volumes.json"
        root_bytes = root_path.read_bytes()
        root_path.write_bytes(root_bytes.replace(b"Try Playbit offline", b"Different seed"))
        stale_root = subprocess.run(sandbox + [str(bin_dir / "pb"), "bundle", "--native", "--release"],
                                    cwd=source, env=native_env, capture_output=True, timeout=180)
        assert stale_root.returncode != 0 and b"initializer differs from source" in stale_root.stderr
        assert hashlib.sha256(bundle_executable.read_bytes()).hexdigest() == original_bundle
        root_path.write_bytes(root_bytes)
        # Packaging failure must preserve the signed product at its stable path.
        icon_path = source / "icon.svg"
        icon_bytes = icon_path.read_bytes()
        icon_path.write_bytes(b"not an icon")
        failed_bundle = subprocess.run(sandbox + [str(bin_dir / "pb"), "bundle", "--native", "--release"],
                                       cwd=source, env=native_env, capture_output=True, timeout=180)
        assert failed_bundle.returncode != 0
        assert hashlib.sha256(bundle_executable.read_bytes()).hexdigest() == original_bundle
        assert not list(native_app.parent.glob(".staging-*"))
        icon_path.write_bytes(icon_bytes)
        executable = source / "target/aarch64-apple-darwin/release/playbit-app"
        original = hashlib.sha256(executable.read_bytes()).hexdigest()
        paired = [source / "target/playbit/native-app.pbapp"]
        assert len(paired) == 1, "native build must retain its exact distributable Wasm package"
        paired_bytes = paired[0].read_bytes()
        request("build", "--release", tool_env=build_env)
        release_bundle = Path(request("bundle", "--release")["bundle"])
        assert release_bundle == source / "target/release/Offline smoke test.pbapp"
        assert release_bundle.read_bytes() == paired_bytes, "native and ordinary release builds must have the same package identity"
        # A host configuration error must fail the build and preserve the last
        # working executable. No reset or silent Wasm/native substitution.
        result = native_build({"PLAYBIT_NATIVE_SDK":str(work / "missing-sdk")})
        assert result.returncode != 0
        assert hashlib.sha256(executable.read_bytes()).hexdigest() == original
        assert hashlib.sha256(bundle_executable.read_bytes()).hexdigest() == original_bundle
        result = native_build(target="wasm32-unknown-unknown")
        assert result.returncode != 0 and b"aarch64-apple-darwin only" in result.stderr
        manifest_path = source / "playbit.toml"
        manifest = manifest_path.read_bytes()
        manifest_path.write_bytes(manifest + b"\n#" + b"x" * (320 * 1024))
        result = native_build()
        assert result.returncode != 0 and b"source manifest exceeds 320 KiB" in result.stderr
        manifest_path.write_bytes(manifest)
        result = native_build({"PLAYBIT_NATIVE_SDK":sdk})
        assert result.returncode == 0, result.stderr.decode()
        assert hashlib.sha256(executable.read_bytes()).hexdigest() == original
        # Source edits invalidate the embedded counterpart, and a failed paired
        # Wasm compile preserves the last executable instead of retaining stale code.
        app_source = source / "src/app.rs"
        app_bytes = app_source.read_bytes()
        title = b'TextStyle::Title, "TODO"'
        assert app_bytes.count(title) == 1, "expected one visible title to edit"
        app_source.write_bytes(app_bytes.replace(title, b'TextStyle::Title, "Paired TODO"'))
        result = native_build()
        assert result.returncode == 0, result.stderr.decode()
        assert Path(json.loads(result.stdout)["bundle"]) == native_app
        assert plistlib.loads((native_app / "Contents/Info.plist").read_bytes())["CFBundleIdentifier"] == native_identifier
        assert paired[0].read_bytes() != paired_bytes
        changed_executable = hashlib.sha256(executable.read_bytes()).hexdigest()
        changed_bundle = hashlib.sha256(bundle_executable.read_bytes()).hexdigest()
        assert changed_bundle != original_bundle
        app_source.write_bytes(app_source.read_bytes() + b'\n#[cfg(target_arch = "wasm32")] compile_error!("paired failure fixture");\n')
        result = native_build()
        assert result.returncode != 0 and b"paired failure fixture" in result.stderr
        assert hashlib.sha256(executable.read_bytes()).hexdigest() == changed_executable
        assert hashlib.sha256(bundle_executable.read_bytes()).hexdigest() == changed_bundle
        app_source.write_bytes(app_bytes)
        result = native_build()
        assert result.returncode == 0, result.stderr.decode()
        assert paired[0].read_bytes() == paired_bytes
        assert hashlib.sha256(executable.read_bytes()).hexdigest() == original
        # The generated native executable is independent of cwd and reports
        # invalid arguments without starting AppKit or opening a profile.
        invalid = subprocess.run(sandbox + [str(executable), "one", "two"],
                                 cwd=work, env=env, capture_output=True)
        assert invalid.returncode != 0 and b"at most one profile" in invalid.stderr
        assert not (work / "one").exists()
        bundled_invalid = subprocess.run(sandbox + [str(native_app / "Contents/MacOS/app"), "one", "two"],
                                        cwd=work, env=env, capture_output=True)
        assert bundled_invalid.returncode != 0 and b"at most one profile" in bundled_invalid.stderr
        # Native launchers reject fuel flags before treating any argument as a
        # profile directory, including when the flag follows a positional path.
        for launcher in [executable, native_app / "Contents/MacOS/app"]:
            for flags in [["--gas-limit=1M"], ["--gas-limit", "1000000"],
                          ["unused-profile", "--gas-limit=1"], ["--gas-limit=0"]]:
                rejected = subprocess.run(sandbox + [str(launcher), *flags], cwd=work,
                                          env=env, capture_output=True, timeout=10)
                assert rejected.returncode != 0 and b"cannot enforce Wasm fuel" in rejected.stderr
        assert not (work / "unused-profile").exists()
        subprocess.run(["codesign", "--verify", "--deep", "--strict", str(native_app)], check=True)
        output.update(native_fuel_limit_rejected=True, native_executable=str(executable), native_sdk=sdk,
                      native_app=str(native_app), native_bundle_metadata_icon_licenses=True,
                      native_build_preserves_scope_data=True, native_bundle_replaced=True,
                      native_failure_retains_bundle=True, named_debug_release_products=True,
                      native_build=True, native_failure_retains_executable=True,
                      installed_build_tool_ignores_path=True,
                      paired_wasm_identity=True, source_edits_refresh_pair=True,
                      paired_failure_retains_executable=True,
                      unsupported_target_rejected=True, oversized_manifest_rejected=True)
    (work / "evidence.json").write_text(json.dumps(output, indent=2) + "\n")
    print(json.dumps(output, indent=2))
finally:
    # Accepted/uncertain Quit is attempted once; preserve unknown owners.
    if known_owner is not None and not quit_attempted:
        try:
            quit_owner()
        except Exception as error:
            print(f"Unknown Quit result; retained owner {known_owner}: {error}", file=sys.stderr)
    if known_owner is None:
        stop()
    close_mcp()
    log.close()
