---
name: preflight
description: Run this repository's full local gate (fmt, clippy -D warnings, all tests with the sandbox required, doctests) before a commit, and read a failure the way CI would.
---

# Preflight

Run before every commit, and once more after the last edit of a batch. Every
commit must pass it on its own.

1. From the repository root run `scripts/preflight.sh`. It is the local mirror
   of `.github/workflows/ci.yml` for this platform, in this order:
   `cargo fmt --all -- --check` → `cargo clippy --locked --workspace --all-targets -- -D warnings`
   → `DEEPCODE_REQUIRE_SANDBOX=1 cargo test --locked --workspace --all-targets`
   → `DEEPCODE_REQUIRE_SANDBOX=1 cargo test --locked --workspace --doc`.
   It stops at the first failing stage.
2. The script runs for several minutes; run it as a background job and read the
   log when it exits. Do not run two of them at once (they share the target dir).
3. Read a red result at the stage that failed, not at the summary:
   - fmt: run `cargo fmt --all` and re-run.
   - clippy: fix the lint; do not add `allow` attributes. "Too many arguments
     (8/7)" means bundle into a struct or use a builder method.
   - tests: find the panic line (`panicked at <file>:<line>`), read the test's
     doc comment first — it states which claim the test pins.
   - doctests: a `///` code block was compiled as Rust; fence non-Rust examples
     with ```text.
4. Green here is not green on CI for two things the script cannot check:
   the MSRV job (`RUSTUP_TOOLCHAIN=1.88.0 cargo check --locked --workspace`) and
   anything behind `cfg(windows)` / `cfg(target_os = "linux")`. Say so when
   reporting, rather than claiming CI will pass.
5. Report the numbers: tests passed/failed per stage and the exit code. Never
   summarize a run you did not read to the end.
