#!/usr/bin/env bash
# Verify that a deepcode version actually installs and runs, end to end.
#
# Two callers, deliberately sharing one implementation:
#   * release.yml `verify-install` — the gate that runs BEFORE `npm publish`, so
#     a version that cannot be installed never reaches npm's `latest` tag and no
#     human has to promote or roll anything back.
#   * verify-install-canary.yml — the same check against an already-published
#     version. Used to rehearse the gate before a release, and to diagnose a
#     user's install failure after one.
# If those two ever fork into separate copies, the rehearsal stops proving
# anything about the real gate. That is why this is one script.
#
# What this covers that nothing else in the pipeline does: all of `install.mjs`'s
# download path — the ASSET_MAP lookup, the GitHub Release asset URL, the
# SHA256SUMS fetch and hash comparison, chmod, the rename into place — plus the
# `bin/deepcode` launcher and its DEEP_CODE_PROGRAM_NAME handoff. Before this
# existed, that path first executed on a real user's machine, where a mismatch
# between ASSET_MAP and the uploaded asset names breaks every platform at once.
#
# It installs from a LOCAL tarball rather than from the registry, because the
# release gate has to run before the version is on npm. `npm pack` produces the
# same tarball `npm publish` would upload: `files` is the same field, and there
# is no `prepack`/`prepublishOnly` hook. If one is ever added, that equivalence —
# and this gate's coverage of the published artifact — is void.
#
# Usage: scripts/verify-install.sh <version-without-leading-v> [package-dir]
#
# The package directory defaults to this script's own sibling, which is what
# release.yml wants: there, the checker and the released tree are the same
# checkout, so the gate runs the script that shipped in the tag.
#
# The second argument exists for the canary, which must verify an ALREADY
# published version whose tag predates this script — v0.4.8 has no
# scripts/verify-install.sh in it at all, so checking that tag out and running
# the script from it fails for a reason that has nothing to do with the release.
# There the caller checks out two trees on purpose: the released tree is the
# subject, the current script is the checker.
set -euo pipefail

EXPECTED="${1:-}"
PKG_DIR="${2:-}"

if [ -z "$EXPECTED" ]; then
  printf 'usage: %s <version-without-leading-v> [package-dir]\n' "$0" >&2
  exit 2
fi

if [ -z "$PKG_DIR" ]; then
  PKG_DIR="$(dirname "$0")/../packages/deepcode"
fi

cd "$PKG_DIR"

# Deliberately a path inside the tree, handed to npm as a RELATIVE path.
#
# Not `mktemp -d`: this script runs on a Windows runner too, where `npm` is a
# native process and an absolute POSIX `/tmp/...` argument is subject to MSYS
# path translation that may or may not fire. A path relative to the working
# directory means the same thing to bash and to npm on every platform, and that
# is a property worth more here than a tidier temp directory. Removed on exit so
# it never becomes an untracked file somebody has to notice.
PACK_DIR=".verify-install-tmp"
rm -rf "$PACK_DIR"
mkdir -p "$PACK_DIR"
trap 'rm -rf "$PACK_DIR"' EXIT

# npm's cache and debug logs default to $HOME, which fails on a read-only $HOME
# and is nobody's business during an install check. Both are config keys, so the
# `npm_config_*` spellings cover `npm pack` and `npm i -g` alike.
export npm_config_cache="$PWD/$PACK_DIR/npm-cache"
export npm_config_logs_dir="$PWD/$PACK_DIR/npm-logs"

printf '=== packing @liwenkai/deepcode@%s ===\n' "$EXPECTED"
npm pack --pack-destination "$PACK_DIR"

TARBALL="$(ls -1 "$PACK_DIR"/*.tgz | head -n1)"
printf '=== installing from %s (postinstall downloads the release asset) ===\n' "$TARBALL"
npm i -g "$TARBALL"

# `npm i -g` just put a new executable in a directory bash has already searched.
# Without this, bash can hand back its cached (failed) lookup and report the
# launcher as "command not found" on a perfectly good install.
hash -r

# `cli.rs` prints "<program name> <CARGO_PKG_VERSION>" then exits 0. There are
# TWO assertions here, and the second one depends on the subject.
#
# 1. The VERSION is always asserted. Every release must report its own version,
#    and that is what ties the artifact that ran to the release being verified.
#
# 2. The PROGRAM NAME is asserted only when this release's launcher hands it
#    over. `bin/deepcode` sets DEEP_CODE_PROGRAM_NAME to the name npm actually
#    linked; without that handoff the binary falls back to argv[0], which on unix
#    is `deepcode-bin` — the deliberately distinct download name, on nobody's
#    PATH. That is f514302 ("npm 装出来的命令自报 deepcode-bin"), and it first
#    shipped AFTER v0.4.8. Windows is unaffected either way: the binary there is
#    `deepcode.exe`, whose stem is already `deepcode` — which is exactly why a
#    rehearsal against v0.4.8 reports `deepcode-bin 0.4.8` on unix and passes on
#    Windows.
#
#    This check has to work against ANY published version, so the SUBJECT decides
#    which assertion applies. Asserting the current tree's behaviour against an
#    older release fails for a reason that has nothing to do with that release,
#    and that is what the first rehearsal run did.
#
#    The lenient branch is NOT a silent skip — it announces itself with
#    ::warning::, and check-packaging.sh independently requires the handoff to be
#    present in the current tree. So this leniency cannot hide a regression in the
#    tree we are about to ship; it only declines to assert something about a
#    release that never had it.
#
# `tr -d '\r'` is not paranoia: on Windows the value arrives through a pipe, and
# a stray CR would fail an exact comparison for a reason that has nothing to do
# with the release.
VERSION_OUT="$(deepcode --version | tr -d '\r')"
printf 'deepcode --version  ->  %s\n' "$VERSION_OUT"

if [[ "$VERSION_OUT" != *" ${EXPECTED}" ]]; then
  printf '::error::"deepcode --version" printed "%s", which does not end in " %s"\n' \
    "$VERSION_OUT" "$EXPECTED"
  exit 1
fi

if grep -q 'DEEP_CODE_PROGRAM_NAME' bin/deepcode; then
  if [ "$VERSION_OUT" != "deepcode ${EXPECTED}" ]; then
    printf '::error::"deepcode --version" printed "%s", expected exactly "deepcode %s" — this launcher DOES hand the linked name over, so the program name is part of the assertion\n' \
      "$VERSION_OUT" "$EXPECTED"
    exit 1
  fi
  printf 'OK   program name is "deepcode" — the handoff is present in this launcher\n'
else
  printf '::warning::bin/deepcode in this release has no DEEP_CODE_PROGRAM_NAME handoff (f514302, first shipped after v0.4.8) — the version is asserted, the PROGRAM NAME is not. On unix this release reports "deepcode-bin".\n'
fi

# `--help` is the other early-exit path a new user hits first, and it carries its
# own regression: it used to fall through to the unknown-argument branch and
# print "Unknown arguments: --help" to stderr with exit 2. Only the exit status
# is asserted: the first line of the help text is not the program name (it is a
# heading), so asserting on the text here would be asserting the wrong thing.
printf '=== deepcode --help ===\n'
deepcode --help >/dev/null

printf 'OK: @liwenkai/deepcode@%s installs and runs\n' "$EXPECTED"
