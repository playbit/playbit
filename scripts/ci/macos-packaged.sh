#!/bin/bash
# Exercise the packaged client after retaining available products.
set -euo pipefail
python3 scripts/test-packaged-init.py
# Keep cold oracle compilation outside the smoke test's operation deadline.
cargo build --locked --release -p playbit --no-default-features --features presentation --bin check-presentation
RUSTC_WRAPPER= python3 scripts/test-packaged-workflow.py --native
