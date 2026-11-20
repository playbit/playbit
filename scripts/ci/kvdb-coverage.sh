#!/bin/bash
# Shared verification commands for local and GitHub execution.
set -euo pipefail

# Measure line coverage.
cargo llvm-cov -p kvdb --features redb --ignore-filename-regex '(tests|prepared_bench)\.rs$' --text --output-path "$RUNNER_TEMP/coverage.txt"
python3 playbit/kvdb/scripts/line-coverage.py "$RUNNER_TEMP/coverage.txt" 99
