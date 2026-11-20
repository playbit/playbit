#!/bin/bash
# Shared verification commands for local and GitHub execution.
set -euo pipefail

# Check first-party Rust formatting.
cargo fmt -p playbit -p playbit-api -p playbit-trace -p kvdb -p todo -p shared-note -p ui-gallery -p micro-kart --check

# Check component fuzz target formatting.
cargo fmt --manifest-path playbit/fuzz/Cargo.toml -p playbit-fuzz --check

# Test the style report.
python3 -m unittest discover -s scripts -p 'test_style_report.py'

# Report Tiger Style and check tier A regressions.
python3 scripts/style-report.py --check | tee -a "$GITHUB_STEP_SUMMARY"
