#!/bin/bash
# Retain the complete production-scale namespace histories and assertions.
set -euo pipefail
export CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true
cargo test --locked --release -p playbit --lib --no-default-features -- --exact --test-threads=4 \
    store::volume_index::namespace::source::cut::tests::five_hundred_sequential_births_fit_acceptance_and_durable_receipt \
    store::volume_index::namespace::source::cut::tests::thousand_sequential_edits_fit_acceptance_and_durable_receipt \
    store::volume_index::tests::one_thousand_publications_fit_persisted_causal_source_budgets
