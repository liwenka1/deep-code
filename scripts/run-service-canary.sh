#!/usr/bin/env bash
# Run exactly one ignored real-service smoke test. CI builds the test binary
# before giving this script a credential. Keep the name check: libtest exits 0
# for a misspelled --exact filter, which would otherwise look like a live pass.
set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-}" in
  sse)
    test_name='client::tests::real_deepseek_streams_text_and_done'
    wall_seconds=90
    ;;
  web)
    test_name='web_tools::tests::tools_api_real_network'
    wall_seconds=120
    ;;
  subagent)
    test_name='subagent::tests::integration::real_deepseek_child_returns_structured_report'
    wall_seconds=150
    ;;
  lsp)
    test_name='lsp::manager::tests::real_rust_analyzer_reports_diagnostics_for_broken_file'
    wall_seconds=90
    ;;
  *)
    echo 'usage: scripts/run-service-canary.sh sse|web|subagent|lsp' >&2
    exit 2
    ;;
esac

if ! command -v timeout >/dev/null 2>&1; then
  echo 'GNU timeout is required (the canary runs on Ubuntu).' >&2
  exit 1
fi

listing="$(cargo test --locked --offline -p deep-code-agent --lib "$test_name" -- --ignored --exact --list)"
if ! printf '%s\n' "$listing" | grep -Fqx "$test_name: test"; then
  echo "::error::ignored canary test '$test_name' was not found; refusing a zero-test pass" >&2
  exit 1
fi

exec timeout --signal=TERM --kill-after=5s "${wall_seconds}s" \
  cargo test --locked --offline -p deep-code-agent --lib "$test_name" \
  -- --ignored --exact --nocapture --test-threads=1
