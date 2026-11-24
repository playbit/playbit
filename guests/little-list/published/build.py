#!/usr/bin/env python3
"""Build an unsigned distribution input without recording the builder's source paths."""
import json
import os
from pathlib import Path
import subprocess
import tomllib

root = Path(__file__).resolve().parents[3]
guest = root / "guests/little-list"
sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
cargo = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")).resolve()
with (root / ".cargo/config.toml").open("rb") as config:
    flags = tomllib.load(config)["target"]["wasm32-unknown-unknown"]["rustflags"]
for source, canonical in [(sysroot, "/rust/toolchain"), (cargo, "/rust/cargo"), (root, "/playbit")]:
    flags.append(f"--remap-path-prefix={source}={canonical}")
env = dict(os.environ, CARGO_ENCODED_RUSTFLAGS="\x1f".join(flags))
result = subprocess.check_output(["pb", "build", "--release", "--sdk=system", "--template"],
                                 cwd=guest, env=env, text=True)
package = Path(json.loads(result)["bundle"])
data = package.read_bytes()
if any(os.fsencode(path) in data for path in (Path.home(), sysroot, cargo, root)):
    raise SystemExit("Little List contains a local build path")
print(package)
