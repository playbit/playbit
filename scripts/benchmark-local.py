#!/usr/bin/env python3
"""Real packaged TODO input diagnostic; NOT an IPC transport benchmark.

Input ACK and committed scene observation are separate Rust-clock boundaries. Guests run
in matching GUI processes; storage stays in the shared engine. This diagnostic
excludes physical input/display qualification. Rust performs each complete input,
binary frame read and scene decode; JSON/Python are outside every timed sample.

Runs only in a fresh temporary profile. Engine/data tools have outbound IP
denied by the macOS sandbox. Launch Services uses a trusted native caller because
macOS discards app arguments from sandboxed launch callers; Wasm host permissions
remain zero and its service I/O stays in the offline engine. Timed requests use the real Unix IPC codec and compiled guest.
Retains raw samples and failures. Fixture setup uses the packaged CLI and real engine admission.
Use --pause-ms 250 to measure paced interaction; the pause is outside timing.
"""
from identifier import encode as encode_identifier
import argparse
import concurrent.futures
import ctypes
import functools
import hashlib
import json
import math
import os
from pathlib import Path
import runpy
import shutil
import signal
import select
import socket
import subprocess
import sys
import tempfile
import threading
import time


@functools.lru_cache(maxsize=1)
def mach_clock():
    class Timebase(ctypes.Structure):
        _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]
    library = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
    library.mach_absolute_time.restype = ctypes.c_uint64
    library.mach_timebase_info.argtypes = [ctypes.POINTER(Timebase)]
    info = Timebase()
    if library.mach_timebase_info(ctypes.byref(info)) != 0 or not info.denom:
        raise RuntimeError("Mach timebase unavailable")
    return library.mach_absolute_time, info.numer, info.denom


def now_ns():
    # Older macOS Python subtracts its process-local starting epoch from
    # monotonic_ns(). Native counters and the IPC driver use the system epoch.
    # Verify this shared clock against the compiled helper before measuring.
    if sys.platform == "darwin":
        clock, numer, denom = mach_clock()
        return clock() * numer // denom
    return time.monotonic_ns()


def summary(samples):
    ordered = sorted(samples)
    if not ordered:
        return {"count": 0}
    return {
        "count": len(samples),
        **{f"p{p}_ms": ordered[math.ceil(len(ordered) * p / 100) - 1]
           for p in (50, 95, 99)},
        "max_ms": ordered[-1],
    }


def machine_conditions():
    """Observe outside timed work; retain unavailable readings explicitly."""
    result = {"observed_ns": now_ns()}
    commands = {
        "hardware": ["/usr/sbin/sysctl", "hw.model", "machdep.cpu.brand_string", "hw.memsize"],
        "os": ["/usr/bin/sw_vers"],
        "power": ["/usr/bin/pmset", "-g", "batt"],
        "thermal_performance_report": ["/usr/bin/pmset", "-g", "therm"],
    }
    for name, command in commands.items():
        try:
            reading = subprocess.run(command, capture_output=True, text=True, timeout=5)
            result[name] = {"exit_code": reading.returncode,
                            "stdout": reading.stdout, "stderr": reading.stderr}
        except (OSError, subprocess.TimeoutExpired) as error:
            result[name] = {"unavailable": str(error)}
    result["thermal_limit"] = "pmset reports performance warnings/limits; absence of warnings does not establish NSProcessInfo thermal state."
    return result


# Imported qualification readers use the same canonical binary as main().
# Their workflow must build it before starting isolated engine processes.
driver_binary = Path(__file__).resolve().parents[1] / "target/release/examples/local-ipc-driver"
driver_local = threading.local()
drivers = []
drivers_lock = threading.Lock()
MAX_DRIVERS = 8  # Four concurrent inputs plus setup/replica readers.
MAX_THREAD_PROFILES = 2  # The actual paired owner/participant diagnostics.


def discard_driver(process):
    if process.poll() is None:
        process.kill()
    process.wait(timeout=5)
    try:
        process.stdin.close()
    except BrokenPipeError:
        pass
    process.stdout.close()
    for profile, cached in list(getattr(driver_local, "processes", {}).items()):
        if cached is process:
            del driver_local.processes[profile]
    with drivers_lock:
        drivers.remove(process)


def close_thread_drivers():
    for process in list(getattr(driver_local, "processes", {}).values()):
        discard_driver(process)


def close_all_drivers():
    with drivers_lock:
        remaining = list(drivers)
    for process in remaining:
        discard_driver(process)


def driver_exchange(profile, command, timeout=35):
    profile = Path(profile).resolve()
    if not hasattr(driver_local, "processes"):
        driver_local.processes = {}
    if profile not in driver_local.processes:
        if len(driver_local.processes) == MAX_THREAD_PROFILES:
            raise RuntimeError("benchmark thread profile limit exceeded")
        with drivers_lock:
            if len(drivers) == MAX_DRIVERS:
                raise RuntimeError("benchmark driver connection limit exceeded")
            process = subprocess.Popen([str(driver_binary), str(profile)],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            drivers.append(process)
        driver_local.processes[profile] = process
    process = driver_local.processes[profile]
    before = now_ns()
    try:
        process.stdin.write(json.dumps(command) + "\n")
        process.stdin.flush()
        deadline, reply = time.monotonic() + timeout, bytearray()
        while b"\n" not in reply:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([process.stdout], [], [], remaining)[0]:
                raise RuntimeError("benchmark driver response deadline")
            chunk = os.read(process.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError("benchmark driver closed before replying")
            reply.extend(chunk)
            if len(reply) > 32 * 1024 * 1024:
                raise RuntimeError("benchmark driver JSON response exceeds limit")
        result = json.loads(reply)
    except (OSError, RuntimeError, ValueError):
        discard_driver(process)
        raise
    if "error" in result:
        raise RuntimeError(result["error"])
    if not before <= result["start_ns"] <= result["end_ns"] <= now_ns():
        raise RuntimeError("Rust driver and Python timestamps do not share an epoch")
    return result


def exchange(profile, request, waiters=0, after=None):
    return driver_exchange(profile, dict(kind="management", request=request,
                                        waiters=waiters, after=after))


def call(profile, request):
    return exchange(profile, request)["response"]


def app_exchange(profile, binding, operation, timeout=35):
    return driver_exchange(profile, dict(kind="app", **binding, operation=operation), timeout)


def read_presentation(stream):
    deadline, reply = time.monotonic() + 15, bytearray()
    while b"\n" not in reply:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([stream], [], [], remaining)[0]:
            raise RuntimeError("presentation response deadline")
        chunk = os.read(stream.fileno(), 65536)
        if not chunk:
            raise RuntimeError("presentation helper closed before replying")
        reply.extend(chunk)
        if len(reply) > 1024 * 1024:
            raise RuntimeError("presentation JSON response exceeds limit")
    return json.loads(reply)


def idle_observation(before, after):
    """Process-counter deltas; short smoke runs do not qualify five-minute CPU."""
    if before["clock"] != after["clock"]:
        raise ValueError("idle observations use different clocks")
    elapsed = after["observed_ns"] - before["observed_ns"]
    if elapsed <= 0:
        raise ValueError("idle observation must have positive duration")
    counters = ("cpu_ns", "interrupt_wakeups", "package_idle_wakeups", "read_bytes", "written_bytes")
    deltas = {key: after[key] - before[key] for key in counters}
    if any(value < 0 for value in deltas.values()):
        raise ValueError("idle process counters went backwards")
    cpu_percent = 100 * deltas["cpu_ns"] / elapsed
    return {
        "before": before, "after": after, "duration_ns": elapsed,
        "counter_deltas": deltas, "cpu_percent_one_core": cpu_percent,
        "five_minute_cpu_under_0_5_percent": None if elapsed < 300_000_000_000 else cpu_percent < .5,
        "sampled_rss_at_most_256_mib": max(before["resident_bytes"], after["resident_bytes"]) <= 256 * 1024 * 1024,
        "rss_boundary": "endpoint snapshots, not continuous peak RSS",
        "allocation_count": None,
    }


def known_owner(owners, binding):
    matching = [owner for owner in owners if owner["instance"]["id"] == binding["instance"]]
    if (len(matching) != 1 or matching[0]["process"] is None
            or matching[0]["instance"]["owner_generation"] != binding["generation"]):
        raise RuntimeError("known owner has no exact process/generation")
    return matching[0]




def main():
    global driver_binary
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--tools", type=Path,
                        help="directory containing the matching local-ipc-driver; otherwise build it")
    parser.add_argument("--samples", type=int, default=1000)
    parser.add_argument("--rows", type=int, default=32, choices=(1, 32, 64))
    parser.add_argument("--sessions", type=int, default=1, choices=(1, 4, 8))
    parser.add_argument("--same-session", action="store_true",
                        help="independent owners share one session; each toggles a distinct row")
    parser.add_argument("--renames", action="store_true",
                        help="rename the first root concurrently, once per input sample")
    parser.add_argument("--idle-seconds", type=int, default=0)
    parser.add_argument("--pause-ms", type=int, default=0,
                        help="idle pause before each input, outside timing (0..1000 ms)")
    args = parser.parse_args()
    if not 1 <= args.samples <= 10000:
        parser.error("samples must be between 1 and 10000")
    if args.same_session and args.rows < args.sessions:
        parser.error("same-session concurrent input requires one distinct row per owner")
    if not 0 <= args.idle_seconds <= 3600:
        parser.error("idle-seconds must be between 0 and 3600")
    if not 0 <= args.pause_ms <= 1000:
        parser.error("pause-ms must be between 0 and 1000")
    repo = Path(__file__).resolve().parents[1]
    if args.tools is None:
        subprocess.run(["cargo", "build", "--locked", "--release", "-p", "playbit",
                        "--no-default-features", "--features", "process",
                        "--example", "local-ipc-driver"], cwd=repo, check=True)
    tools = args.tools.resolve() if args.tools else repo / "target/release/examples"
    driver_binary = tools / "local-ipc-driver"
    # macOS's default temporary directory can exceed sockaddr_un.sun_path.
    work = Path(tempfile.mkdtemp(prefix="pb-bench-", dir="/tmp"))
    work.chmod(0o700)
    print(f"benchmark artifact: {work}", file=sys.stderr, flush=True)
    resource_tool = work / "process-resources"
    compile_native = runpy.run_path(str(repo / "scripts/native-build.py"))["compile_native"]
    compile_native(repo, "clang", "-std=c17", "-Wall", "-Wextra", "-Werror", "-O2",
                   str(repo / "scripts/process-resources.c"), "-o", str(resource_tool))
    clock_before = now_ns()
    clock_probe = json.loads(subprocess.check_output([str(resource_tool), "0"]))
    clock_after = now_ns()
    if not clock_before <= clock_probe["observed_ns"] <= clock_after:
        raise RuntimeError("native and benchmark monotonic clocks do not share an epoch")
    cancelled = threading.Event()
    signal.signal(signal.SIGINT, lambda *_: cancelled.set())
    profile = work / "profile"
    if registered := os.environ.get("PLAYBIT_PERFORMANCE_PROFILE"):
        staged = Path(registered).with_suffix(".tmp")
        staged.write_text(str(profile.resolve()))
        staged.replace(registered)
    binaries = args.app.resolve() / "Contents/MacOS"
    env = dict(os.environ, PLAYBIT_PROFILE=str(profile))
    policy = '(version 1) (allow default) (deny network-outbound (remote ip "*:*"))'
    sandbox = ["/usr/bin/sandbox-exec", "-p", policy]
    # Prove the fixture cannot accidentally connect to a real account/AP.
    probe = subprocess.run(sandbox + ["/usr/bin/python3", "-c", """
import errno, json, socket
try:
    socket.create_connection(('1.1.1.1', 443), timeout=2).close()
except OSError as error:
    print(json.dumps({'outbound_denied': error.errno == errno.EPERM}))
else:
    print(json.dumps({'outbound_denied': False}))
"""], capture_output=True)
    # A failure to enter sandbox-exec can itself say "Operation not permitted".
    # Require a successful probe process reporting denial of the actual socket.
    if probe.returncode != 0 or probe.stdout.strip() != b'{"outbound_denied": true}':
        raise RuntimeError("offline sandbox verification failed")

    def pb(*arguments):
        cli_env = dict(env)
        launcher = [] if arguments[:2] == ("session", "join") else sandbox
        result = subprocess.run(launcher + [str(binaries / "pb"), *map(str, arguments)],
                                env=cli_env, cwd=source, capture_output=True, check=True, timeout=60)
        return result.stdout.decode().strip()

    evidence = {"boundary": "Rust-only input ACK and committed scene observation over binary app IPC, including 1 ms frame polling and native scene decoding; JSON/Python only orchestrate/export batches; no physical presentation qualification",
                "app": str(args.app.resolve()),
                "rows": args.rows, "sessions": args.sessions, "requested_samples_per_session": args.samples,
                "waiter_pressure": "not measured",
                "concurrent_renames": args.renames,
                "pause_before_request_ms": args.pause_ms,
                "network": "engine/data tools outbound IP denied; trusted native Launch Services caller; Wasm host permissions zero", "errors": [], "results": []}
    evidence["fixture_title_bytes"] = {"seed": 64, "added": 64}
    evidence["fixture_roots"] = "one shared root/session" if args.same_session else "one independent root per session"
    evidence["clock"] = clock_probe["clock"]
    evidence["clock_resolution_ns"] = math.ceil(clock_probe["timebase_numer"] / clock_probe["timebase_denom"])
    evidence["binary_sha256"] = {name: hashlib.sha256((binaries.parent / name).read_bytes()).hexdigest()
                                 for name in ("MacOS/Playbit", "Helpers/playbit-engine.app/Contents/MacOS/playbit-engine", "MacOS/pb")}
    evidence["driver_sha256"] = hashlib.sha256(driver_binary.read_bytes()).hexdigest()
    evidence["todo_wasm_sha256"] = hashlib.sha256(
        (repo / "target/wasm32-unknown-unknown/release/todo.wasm").read_bytes()).hexdigest()
    source = work / "source"
    source.mkdir()
    initializers = json.loads((repo / "guests/todo/playbit-volumes.json").read_text())
    declaration = initializers["session"]
    database = next(item for item in declaration["databases"]
                    if item["name"] == "data" and item["kind"]["type"] == "object_db")
    seed = database["kind"]["objects"][0]
    database["kind"]["objects"] = []
    for number in range(args.rows):
        item = json.loads(json.dumps(seed))
        item["id"] = encode_identifier((number + 1).to_bytes(16, "big"))
        item["fields"][encode_identifier(bytes([2] * 16))][0]["value"] = f"task-{number:04d}" + "x" * 55
        database["kind"]["objects"].append(item)
    (source / "playbit-volumes.json").write_text(json.dumps(initializers))
    manifest = (repo / "guests/todo/playbit.toml").read_text()
    (source / "src").mkdir()
    (source / "src/lib.rs").write_text("// Artifact fixture; compiled TODO is copied below.\n")
    (source / "Cargo.toml").write_text('[package]\nname = "todo"\nversion = "0.1.0"\nedition = "2024"\n[lib]\ncrate-type = ["cdylib"]\n')
    artifact = source / "target/wasm32-unknown-unknown/release/todo.wasm"
    artifact.parent.mkdir(parents=True)
    shutil.copyfile(repo / "target/wasm32-unknown-unknown/release/todo.wasm", artifact)
    with (work / "engine.log").open("wb") as log:
        start = now_ns()
        ready, child_output = socket.socketpair()
        try:
            process = subprocess.Popen(sandbox + [str(binaries.parent / "Helpers/playbit-engine.app/Contents/MacOS/playbit-engine")],
                                       env=env, stdout=child_output, stderr=log)
        finally:
            child_output.close()
        try:
            # The engine owns this completion boundary. No periodic endpoint
            # probes; fixed result and one absolute failure deadline.
            with ready:
                expected = b'{"ready":true}\n'
                result = bytearray()
                deadline = start + 10_000_000_000
                while len(result) < len(expected):
                    remaining = (deadline - now_ns()) / 1e9
                    if remaining <= 0:
                        raise RuntimeError("engine startup deadline")
                    ready.settimeout(remaining)
                    part = ready.recv(len(expected) - len(result))
                    if not part:
                        raise RuntimeError("engine exited before ready")
                    result.extend(part)
                if result != expected:
                    raise RuntimeError("invalid engine readiness result")
            evidence["process_start_to_readiness_ms"] = (now_ns() - start) / 1e6
            call(profile, {"op": "status"})
            # Initialize isolated roots through the shared management operation; not pb init qualification.
            roots, sessions, bindings, launches = [], [], [], []
            evidence["fixture_versions"] = []
            for _ in range(args.sessions):
                if args.same_session and roots:
                    root = roots[0]
                else:
                    root = encode_identifier(os.urandom(16))
                    call(profile, {"op": "initialize-root", "volume": root,
                                   "name": "TODO benchmark", "declaration": declaration})
                roots.append(root)
                if not (args.same_session and sessions):
                    (source / "playbit.toml").write_text(manifest.replace('volume = "' + encode_identifier(bytes(16)) + '"', f'volume = "{root}"'))
                    bundle = json.loads(pb("bundle", "--release"))["bundle"]
                    session = call(profile, {"op":"create-session", "bundle":bundle, "root":root})
                sessions.append(session["id"])
                evidence["fixture_versions"].append(session["resolved_build_id"])
                opened = json.loads(pb("session", "join", session["id"], "--new-instance", "--detach"))
                instance = opened["instance"]
                launches.append(dict(session=session["id"], instance=instance))
                deadline = time.monotonic() + 15
                while True:
                    owners = call(profile, {"op": "instance-list", "session": session["id"]})
                    known = [o for o in owners if o["instance"]["id"] == instance]
                    if len(known) != 1:
                        raise RuntimeError("known launch reservation disappeared")
                    if not any(b["instance"] == instance for b in bindings):
                        bindings.append(dict(session=session["id"], instance=instance,
                            generation=known[0]["instance"]["owner_generation"]))
                    if known[0]["process"] is not None:
                        break
                    if time.monotonic() >= deadline:
                        raise RuntimeError("known launch claim deadline; startup was not replayed")
                    # Registry claims do not advance the durable-store change
                    # cursor. Bounded fixture-only read observation; Open is
                    # sent once and no accepted work is replayed.
                    time.sleep(.01)
            evidence["instances"] = bindings
            evidence["gui_pids"] = [known_owner(
                call(profile, {"op": "instance-list", "session": b["session"]}), b)["process"]["pid"]
                for b in bindings]

            def resources():
                return json.loads(subprocess.check_output([str(resource_tool), str(process.pid)]))

            def all_resources():
                return {str(pid): json.loads(subprocess.check_output([str(resource_tool), str(pid)]))
                        for pid in [process.pid, *evidence["gui_pids"]]}

            if gate_path := os.environ.get("PLAYBIT_PERFORMANCE_GATE"):
                gate = Path(gate_path)
                gate.write_text("ready")
                while gate.read_text() != "go":
                    if cancelled.wait(.1):
                        raise RuntimeError("benchmark interrupted while waiting for calm")
            if args.idle_seconds:
                evidence["conditions_before_idle"] = machine_conditions()
                before_all = all_resources()
                if cancelled.wait(args.idle_seconds):
                    raise RuntimeError("benchmark interrupted during idle observation")
                after_all = all_resources()
                evidence["idle_by_pid"] = {pid: idle_observation(before_all[pid], after_all[pid])
                                           for pid in before_all}
                evidence["idle"] = evidence["idle_by_pid"][str(process.pid)]
                for pid, observation in evidence["idle_by_pid"].items():
                    for gate in ("five_minute_cpu_under_0_5_percent", "sampled_rss_at_most_256_mib"):
                        if observation[gate] is False:
                            evidence["errors"].append(f"idle budget failed for {pid}: {gate}")
            evidence["resources_before_work"] = resources()
            evidence["conditions_before_work"] = machine_conditions()
            ready = threading.Barrier(args.sessions + int(args.renames), timeout=30)

            def workload(index, binding):
                target = 100 + index if args.same_session else 100
                try:
                    ready.wait()
                    # One command starts the entire native batch. The driver does
                    # not emit JSON or wait for Python between input and scene.
                    batch = app_exchange(profile, binding,
                        {"op": "benchmark", "target": target, "rows": args.rows,
                         "samples": args.samples, "pause_ms": args.pause_ms},
                        timeout=90 + args.samples * args.pause_ms / 1000)["response"]["Benchmark"]
                    samples, acknowledgments = [], []
                    with (work / f"samples-{index}.jsonl").open("w") as raw:
                        for sample in batch["samples"]:
                            begin, ack, end = (sample[k] for k in ("start_ns", "ack_ns", "end_ns"))
                            if not begin <= ack <= end:
                                raise RuntimeError("invalid native sample clocks")
                            elapsed = (end - begin) / 1e6
                            samples.append(elapsed)
                            acknowledgments.append((ack - begin) / 1e6)
                            raw.write(json.dumps(dict(sample, elapsed_ms=elapsed)) + "\n")
                    if batch["error"]:
                        raise RuntimeError(batch["error"])
                    if len(samples) != args.samples:
                        raise RuntimeError("native batch did not finish its requested inputs")
                    return {"samples_ms": samples, **summary(samples), "input_ack": summary(acknowledgments),
                            "start_ns": batch["samples"][0]["start_ns"],
                            "end_ns": batch["samples"][-1]["end_ns"],
                            "final_checkbox": batch["final_checkbox"]}
                except Exception:
                    cancelled.set()
                    raise

            def rename_workload():
                samples = []
                ready.wait()
                measurement_start = now_ns()
                try:
                    with (work / "rename-samples.jsonl").open("w") as raw:
                        for n in range(args.samples):
                            if args.pause_ms:
                                cancelled.wait(args.pause_ms / 1000)
                            if cancelled.is_set():
                                raise RuntimeError("benchmark interrupted; partial renames retained")
                            prefix = f"Shared TODO {n} "
                            remaining = 256 - len(prefix.encode())
                            label = prefix + "é" * (remaining // 2) + "x" * (remaining % 2)
                            assert len(label.encode()) == 256
                            measured = exchange(profile, {"op": "label-volume", "volume": roots[0], "label": label})
                            begin, end = measured["start_ns"], measured["end_ns"]
                            response = measured["response"]
                            if response.get("name") != label or response.get("id") != roots[0]:
                                raise RuntimeError("rename returned the wrong name or volume")
                            elapsed = (end - begin) / 1e6
                            samples.append(elapsed)
                            raw.write(json.dumps({"start_ns": begin, "end_ns": end,
                                                  "elapsed_ms": elapsed}) + "\n")
                            raw.flush()
                    return {"samples_ms": samples, **summary(samples),
                            "start_ns": measurement_start, "end_ns": end,
                            "label_bytes": len(label.encode())}
                except Exception:
                    cancelled.set()
                    raise

            with concurrent.futures.ThreadPoolExecutor(max_workers=args.sessions + int(args.renames)) as executor:
                inputs = [executor.submit(workload, index, binding) for index, binding in enumerate(bindings)]
                renames = executor.submit(rename_workload) if args.renames else None
                failures = []
                for result in inputs:
                    try:
                        evidence["results"].append(result.result())
                    except Exception as error:
                        failures.append(f"input: {error}")
                if renames:
                    try:
                        evidence["renames"] = renames.result()
                    except Exception as error:
                        failures.append(f"rename: {error}")
                if failures:
                    raise RuntimeError("; ".join(failures))
                if renames:
                    overlap_start = max(result["start_ns"] for result in evidence["results"] + [evidence["renames"]])
                    overlap_end = min(result["end_ns"] for result in evidence["results"] + [evidence["renames"]])
                    if overlap_end <= overlap_start:
                        raise RuntimeError("renames did not overlap every input producer")
                    evidence["all_producers_overlap_ms"] = (overlap_end - overlap_start) / 1e6
            final_boundary = now_ns()
            evidence["resources_after_work"] = resources()
            evidence["all_resources_after_work"] = all_resources()
            evidence["conditions_after_work"] = machine_conditions()
            evidence["fixture_objects"] = []
            for root, result in zip(roots, evidence["results"]):
                objects = json.loads(pb("objects", root, database["id"], encode_identifier(bytes([1] * 16))))["records"]
                evidence["fixture_objects"].append(len(objects))
                if len(objects) != args.rows:
                    raise RuntimeError("fixture row count mismatch")
                final = result["final_checkbox"]
                matching = [o for o in objects if any(field["field_id"] == encode_identifier(bytes([2] * 16))
                            and field["values"] == [{"Text": final["label"]}] for field in o["fields"])]
                if len(matching) != 1 or not any(field["field_id"] == encode_identifier(bytes([3] * 16))
                        and field["values"] == [{"Boolean": final["checked"]}] for field in matching[0]["fields"]):
                    raise RuntimeError("stored TODO state differs from the returned presentation")
        except Exception as error:
            evidence["errors"].append(str(error))
        finally:
            # Each known Quit is attempted once. A lost result cannot be replayed.
            evidence["known_launches"] = locals().get("launches", [])
            quit_failed = len(locals().get("launches", [])) != len(locals().get("bindings", []))
            if quit_failed:
                evidence["errors"].append("launch identity retained without a known generation; engine preserved for inspection")
            for binding in locals().get("bindings", []):
                try:
                    result = app_exchange(profile, binding, {"op": "quit"})["response"]
                    if result.get("Accepted", {}).get("instance") != binding["instance"]:
                        raise RuntimeError("Quit admission did not return its known instance")
                    # Accepted confirms admission, not termination. Observe the
                    # original kernel owner disappearing before stopping services.
                    deadline = time.monotonic() + 15
                    while any(owner["instance"]["id"] == binding["instance"] for owner in
                              call(profile, {"op": "instance-list", "session": binding["session"]})):
                        if time.monotonic() >= deadline:
                            raise RuntimeError("accepted Quit did not retire the original owner")
                        time.sleep(.01)
                except Exception as error:
                    quit_failed = True
                    evidence["errors"].append(f"known Quit failed; inspect retained instance {binding['instance']}: {error}")
            close_all_drivers()
            if quit_failed:
                evidence["retained_engine_pid"] = process.pid
            elif process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    evidence["errors"].append("engine did not stop within cleanup deadline")
                    process.kill()
                    process.wait()
            (work / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
    (work / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps({"evidence": str(work / "evidence.json"), "errors": evidence["errors"],
                      "results": [{k: v for k, v in r.items() if k != "samples_ms"} for r in evidence["results"]]}, indent=2))
    return bool(evidence["errors"])


if __name__ == "__main__":
    raise SystemExit(main())
