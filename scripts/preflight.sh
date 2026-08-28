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
    # Neither require variable is set here, and the two have different reasons.
    #
    # DEEPCODE_REQUIRE_SANDBOX: inert on Windows, so setting it would change
    # nothing either way. It is read at exactly one place — `require_backend_
    # or_skip` in sandbox/mod.rs — which is `cfg(all(test, any(linux, macos)))`
    # and called only from the linux and seatbelt backends, so on a Windows
    # target nothing ever reads it. (The earlier claim here, that an
    # unconditional one "hard-failed on the one platform whose CI never demands
    # it", was never true — it cannot fail what it cannot reach. And release.yml
    # now DOES set it on every matrix leg including windows, for that same
    # reason: harmless today, correct the day a Windows backend exists.)
    #
    # DEEPCODE_REQUIRE_SYMLINKS: ci.yml's windows job DOES set it — this is a
    # deliberate divergence, not a mirror. The hosted runner is elevated and
    # can always create symlinks, so demanding it there turns a runner that
    # lost the privilege into a red build; a local Windows box without
    # Developer Mode genuinely cannot, and the tests are designed to skip.
    # The cost of that choice, stated plainly: on Windows a green preflight
    # does NOT imply a green CI, and the gap is exactly the symlink boundary
    # tests. Set DEEPCODE_REQUIRE_SYMLINKS=1 by hand to close it on a box that
    # has the privilege.
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
