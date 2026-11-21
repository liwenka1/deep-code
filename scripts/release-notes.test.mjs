import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const script = fileURLToPath(new URL("./release-notes.mjs", import.meta.url));
const reviewed = "- A reviewed breaking change.\n- Install with the documented npm command.";
const draft = "- Improve terminal editing.";

function workspace(t, pendingNotes = "") {
  const directory = mkdtempSync(join(tmpdir(), "deepcode-release-notes-"));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  writeFileSync(join(directory, "CHANGELOG.md"), [
    "# Changelog",
    "",
    "## [Unreleased]",
    "",
    pendingNotes,
    "",
    "## [0.5.1] - 2026-11-17",
    "",
    "- Previously published notes.",
    "",
  ].join("\n"));
  return directory;
}

function run(directory, ...args) {
  return spawnSync(process.execPath, [script, ...args], { cwd: directory, encoding: "utf8" });
}

function prepare(directory, version, base, supplement) {
  const draftPath = join(directory, "draft.md");
  writeFileSync(draftPath, supplement);
  return run(directory, "prepare", version, base, draftPath);
}

test("reviewed notes survive an incomplete draft and supply the release body", (t) => {
  const directory = workspace(t, reviewed);
  const prepared = prepare(directory, "0.5.2", "v0.5.1", draft);
  assert.equal(prepared.status, 0, prepared.stderr);
  assert.ok(prepared.stdout.includes(reviewed));
  assert.ok(prepared.stdout.includes(draft));

  const changelog = readFileSync(join(directory, "CHANGELOG.md"), "utf8");
  assert.match(changelog, /^## \[Unreleased\]\n\n## \[0\.5\.2\]/m);
  assert.match(changelog, /^- Previously published notes\.$/m);
  assert.ok(changelog.includes("compare/v0.5.1...v0.5.2"));
  const rendered = run(directory, "render", "0.5.2");
  assert.equal(rendered.status, 0, rendered.stderr);
  assert.equal(rendered.stdout, prepared.stdout);
});

test("reviewed notes survive failed tags and stop carrying forward once published", (t) => {
  const directory = workspace(t, reviewed);
  const firstAttempt = prepare(directory, "0.5.2", "v0.5.1", draft);
  assert.equal(firstAttempt.status, 0, firstAttempt.stderr);

  const changelogPath = join(directory, "CHANGELOG.md");
  const changelog = readFileSync(changelogPath, "utf8");
  writeFileSync(changelogPath, changelog.replace(
    "## [Unreleased]\n\n", "## [Unreleased]\n\n- A reviewed follow-up.\n\n",
  ));
  const retry = prepare(directory, "0.5.3", "v0.5.1", "");
  assert.equal(retry.status, 0, retry.stderr);
  assert.ok(retry.stdout.includes(reviewed));
  assert.ok(retry.stdout.includes("- A reviewed follow-up."));

  const thirdAttempt = prepare(directory, "0.5.4", "v0.5.1", draft);
  assert.equal(thirdAttempt.status, 0, thirdAttempt.stderr);
  assert.equal(thirdAttempt.stdout.split(reviewed).length, 2);
  assert.equal(thirdAttempt.stdout.split("- A reviewed follow-up.").length, 2);

  const partialPublish = prepare(directory, "0.5.5", "v0.5.2", draft);
  assert.equal(partialPublish.status, 0, partialPublish.stderr);
  assert.ok(!partialPublish.stdout.includes(reviewed));
  assert.ok(partialPublish.stdout.includes("- A reviewed follow-up."));

  const nextRelease = prepare(directory, "0.5.6", "v0.5.5", draft);
  assert.equal(nextRelease.status, 0, nextRelease.stderr);
  assert.equal(nextRelease.stdout, `${draft}\n`);
});

test("an invalid or empty release leaves the changelog unchanged", (t) => {
  const directory = workspace(t);
  const path = join(directory, "CHANGELOG.md");
  const original = readFileSync(path, "utf8");
  for (const supplement of ["", "## Unexpected heading\n- A draft entry."]) {
    const result = prepare(directory, "0.5.2", "v0.5.1", supplement);
    assert.equal(result.status, 1, result.stderr);
    assert.equal(readFileSync(path, "utf8"), original);
  }
  const missing = run(directory, "render", "0.5.2");
  assert.equal(missing.status, 1, missing.stderr);
  assert.match(missing.stderr, /one nonempty section for 0\.5\.2/);
});

test("the tag workflow rejects reviewed notes left in Unreleased", (t) => {
  const directory = workspace(t, reviewed);
  const rendered = run(directory, "render", "0.5.1");
  assert.equal(rendered.status, 1, rendered.stderr);
  assert.match(rendered.stderr, /archive the Unreleased notes/);
  assert.equal(rendered.stdout, "");
});
