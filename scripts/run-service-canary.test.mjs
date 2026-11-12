import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./run-service-canary.sh', import.meta.url));
const cases = [
  ['sse', 'client::tests::real_deepseek_streams_text_and_done', '90s'],
  ['web', 'web_tools::tests::tools_api_real_network', '120s'],
  ['subagent', 'subagent::tests::integration::real_deepseek_child_returns_structured_report', '150s'],
  ['lsp', 'lsp::manager::tests::real_rust_analyzer_reports_diagnostics_for_broken_file', '90s'],
];

function fixture(t, { listed, exitCode = '0' }) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'deep-code-canary-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const log = path.join(root, 'calls');
  fs.writeFileSync(path.join(root, 'cargo'), `#!/bin/sh
printf 'cargo %s\\n' "$*" >> "$CANARY_STUB_LOG"
case " $* " in
  *' --list '*)
    if [ -n "$CANARY_STUB_LISTED" ]; then
      printf '%s: test\\n' "$CANARY_STUB_LISTED"
    fi
    exit 0
    ;;
esac
exit "$CANARY_STUB_EXIT"
`, { mode: 0o755 });
  fs.writeFileSync(path.join(root, 'timeout'), `#!/bin/sh
printf 'timeout %s\\n' "$*" >> "$CANARY_STUB_LOG"
shift 3
exec "$@"
`, { mode: 0o755 });
  return {
    run: (kind) => spawnSync('bash', [script, kind], {
      encoding: 'utf8',
      env: {
        ...process.env,
        PATH: `${root}${path.delimiter}${process.env.PATH}`,
        CANARY_STUB_LOG: log,
        CANARY_STUB_LISTED: listed,
        CANARY_STUB_EXIT: exitCode,
      },
    }),
    calls: () => fs.existsSync(log) ? fs.readFileSync(log, 'utf8') : '',
  };
}

for (const [kind, name, wall] of cases) {
  test(`${kind} runs exactly the ignored smoke behind its process deadline`, (t) => {
    const stub = fixture(t, { listed: name });
    const result = stub.run(kind);
    assert.equal(result.status, 0, result.stderr);
    const calls = stub.calls().trim().split('\n');
    assert.equal(calls.length, 3);
    assert.ok(calls[0].includes(`${name} -- --ignored --exact --list`));
    assert.ok(calls[1].startsWith(`timeout --signal=TERM --kill-after=5s ${wall} cargo test `));
    assert.ok(calls[2].includes(`${name} -- --ignored --exact --nocapture --test-threads=1`));
    assert.ok(calls.every((line) => line.includes('--locked --offline -p deep-code-agent --lib')));
  });
}

test('a renamed or missing ignored smoke fails instead of reporting a zero-test pass', (t) => {
  const stub = fixture(t, { listed: 'some::different_test' });
  const result = stub.run('sse');
  assert.equal(result.status, 1);
  assert.match(result.stderr, /refusing a zero-test pass/);
  assert.equal(stub.calls().trim().split('\n').length, 1);
});

test('a real test failure or timeout remains a non-zero canary outcome', (t) => {
  for (const exitCode of ['1', '124']) {
    const stub = fixture(t, { listed: cases[0][1], exitCode });
    const result = stub.run('sse');
    assert.equal(result.status, Number(exitCode), result.stderr);
  }
});

test('an unknown canary name is a usage error without running cargo', (t) => {
  const stub = fixture(t, { listed: cases[0][1] });
  const result = stub.run('unknown');
  assert.equal(result.status, 2);
  assert.match(result.stderr, /usage:/);
  assert.equal(stub.calls(), '');
});
