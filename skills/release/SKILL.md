---
name: release
description: How a deep-code version is released and what to check before and after; use when asked whether a release can go out, or to prepare one.
---

# Release

The human action is a single workflow dispatch; everything else is automatic
and fail-closed (details in `RELEASING.md`). Your job is the gate in front of
it and the reading afterwards.

## Before dispatch

1. Confirm `origin/main` holds everything meant for the release: `git status`
   clean, `git log origin/main..HEAD` empty. The release workflow checks out
   the remote branch, so an unpushed commit is silently left out.
2. Confirm the CI run for that head is fully green — every job, not the
   summary badge. The fmt step is the first step of the Linux job; when it is
   red, clippy and tests after it never ran.
3. Run `cargo audit` on the lockfile; a RUSTSEC advisory blocks the release
   until the dependency is bumped (the release workflow has the same gate).
4. Decide the version: patch for fixes and small additions, minor when a tool
   is added or user-visible behaviour changes. Do not edit `CHANGELOG.md` or
   any version number by hand — the workflow drafts the changelog from commit
   subjects and bumps the versions itself.
5. Report in one line whether it can ship and what blocks it if not.

## Dispatch

`Actions → Release Prep → version (without v) → Run`. The workflow drafts the
CHANGELOG section, bumps the versions, commits, tags, builds five targets,
publishes the GitHub Release with `SHA256SUMS`, installs the result on four
platforms, and only then publishes to npm. A version that fails to install
never reaches npm `latest`.

## After

- Read the run: `release` → `verify-install` → `publish` must all be green.
- A red `verify-install` means npm was not touched; fix forward with the next
  patch unless the script itself was at fault (then re-run the failed jobs).
- Check the generated CHANGELOG section for claims the code does not make;
  fix wording in a follow-up commit, never by force-pushing the tag.
