#!/usr/bin/env python3
"""Explicit reference-device benchmark; never run on CI. Uses read-only status."""
import argparse
import concurrent.futures
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import threading
import time


class Client:
    def __init__(self, binary, profile):
        self.errors = tempfile.TemporaryFile()
        self.process = subprocess.Popen(
            [str(binary)], env=dict(os.environ, PLAYBIT_PROFILE=str(profile)),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.errors,
            text=True,
        )
        self.sequence = 0

    def status(self):
        self.sequence += 1
        self.process.stdin.write(json.dumps({
            "jsonrpc": "2.0", "id": self.sequence, "method": "tools/call",
            "params": {"name": "playbit", "arguments": {"op": "status"}},
        }) + "\n")
        self.process.stdin.flush()
        response = json.loads(self.process.stdout.readline())
        if response.get("id") != self.sequence or "error" in response:
            raise RuntimeError("Invalid MCP response")
        result = response["result"]
        if result.get("isError"):
            raise RuntimeError(result["content"][0]["text"])
        return json.loads(result["content"][0]["text"])

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
            raise
        finally:
            self.process.stdout.close()
            self.errors.close()
        if self.process.returncode != 0:
            raise RuntimeError("MCP exited unsuccessfully")


def distribution(samples):
    ordered = sorted(samples)
    return {"count": len(ordered), **{
        name: ordered[math.ceil(len(ordered) * percentile) - 1]
        for name, percentile in [("p50_ns", .5), ("p90_ns", .9), ("p99_ns", .99), ("max_ns", 1)]
    }}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("profile", type=Path, help="Isolated fixture profile; service stays running")
    parser.add_argument("--clients", type=int, default=1, choices=range(1, 5))
    parser.add_argument("--samples", type=int, default=1000)
    args = parser.parse_args()
    if not 1 <= args.samples <= 100000:
        parser.error("samples must be within 1..100000")
    args.binary = args.binary.resolve()
    args.profile = args.profile.resolve()
    clients, first = [], []
    try:
        # Initialize sequentially: launch-lock contention is a separate workload.
        # First post-build/process-cold requests never enter warm percentiles.
        for _ in range(args.clients):
            client = Client(args.binary, args.profile)
            clients.append(client)
            started = time.perf_counter_ns()
            fixture = client.status()
            first.append(time.perf_counter_ns() - started)
        barrier = threading.Barrier(args.clients)

        def measure(client):
            barrier.wait(timeout=10)
            samples = []
            for _ in range(args.samples):
                started = time.perf_counter_ns()
                client.status()
                samples.append(time.perf_counter_ns() - started)
            return samples

        with concurrent.futures.ThreadPoolExecutor(max_workers=args.clients) as pool:
            cohorts = list(pool.map(measure, clients))
        print(json.dumps({
            "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
            "host": platform.platform(), "clients": args.clients,
            "boundary": "Python encode/write/flush through complete MCP status decode; warm process",
            "first_request_ns_excluded": first,
            "fixture": {key: len(fixture.get(key, [])) for key in ("volumes", "sessions")},
            "per_client": [distribution(samples) for samples in cohorts],
            "aggregate": distribution([sample for samples in cohorts for sample in samples]),
        }, indent=2))
    finally:
        for client in clients:
            client.close()


if __name__ == "__main__":
    main()
