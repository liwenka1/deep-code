#!/usr/bin/env bash
# The local gate, mirroring .github/workflows/ci.yml on this machine's
# platform: fmt, clippy -D warnings, full workspace tests.
#
# Two test invocations on purpose. `--all-targets` covers every compiled
# target but silently EXCLUDES doctests, while CI's plain `cargo test
# --workspace` includes them — so a doctest-only break passes an
# `--all-targets` run locally and lands red on CI (b83ab4a). The `--doc`
# pass closes that gap.
#
# DEEPCODE_REQUIRE_SANDBOX=1 turns "no sandbox backend → skip" into a hard
# failure, so a machine that silently lost its backend cannot report green.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
DEEPCODE_REQUIRE_SANDBOX=1 cargo test --locked --workspace --all-targets
DEEPCODE_REQUIRE_SANDBOX=1 cargo test --locked --workspace --doc
