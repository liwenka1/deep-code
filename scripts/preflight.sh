#!/usr/bin/env bash
# The local gate, mirroring .github/workflows/ci.yml on this machine's
# platform: fmt, clippy -D warnings, full workspace tests.
#
# Two test invocations on purpose. `--all-targets` covers every compiled
# target but silently EXCLUDES doctests, while CI's plain `cargo test
# --workspace` includes them — so a doctest-only break passes an
# `--all-targets` run locally and lands red on CI (b83ab4a). The `--doc`
# pass closes that gap.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    # Mirror ci.yml's windows job, which sets NEITHER require variable: an
    # unconditional DEEPCODE_REQUIRE_SANDBOX here hard-failed on the one
    # platform whose CI never demands it. DEEPCODE_REQUIRE_SYMLINKS is CI's
    # business too — the hosted runner is elevated, while a local Windows box
    # without Developer Mode genuinely cannot create symlinks, and the
    # symlink tests are designed to skip there.
    cargo test --locked --workspace --all-targets
    cargo test --locked --workspace --doc
    ;;
  *)
    # DEEPCODE_REQUIRE_SANDBOX=1 turns "no sandbox backend → skip" into a
    # hard failure, so a machine that silently lost its backend cannot report
    # green. (No symlink counterpart needed on unix: the test helper panics
    # on any symlink-creation failure there instead of skipping.)
    DEEPCODE_REQUIRE_SANDBOX=1 cargo test --locked --workspace --all-targets
    DEEPCODE_REQUIRE_SANDBOX=1 cargo test --locked --workspace --doc
    ;;
esac
