#!/usr/bin/env bash
# Packaging contract checks — the release/install relationships invisible to
# the Rust test suite, checked before a tag is pushed.
#
#   1. ASSET_MAP in packages/deepcode/install.mjs must name exactly the assets
#      release.yml's build matrix produces. Add a platform leg and forget the
#      map, and every install on that platform 404s.
#
#   2. The npm tarball must contain the launcher and the installer, and must
#      never contain a downloaded platform binary. install.mjs's "already
#      installed" fast path trusts any existing file whose hash matches the
#      release's SHA256SUMS — so a stale `deepcode-bin` committed or packed into
#      the tarball would be handed to users as if it were their platform's build.
#
#   3. Build, install gate, and installation rehearsal must cover the same
#      native platforms. A leg on the wrong architecture installs another asset.
#
# Read-only: parses files, packs into a temp dir, asserts. No network, no writes
# outside $TMPDIR. Runs in CI on every PR, and by hand from the repo root.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
info() { printf '%s\n' "$*"; }
bad() { printf '::error::%s\n' "$*"; fail=1; }

node --test packages/deepcode/install.test.mjs

# ── 1. release.yml build matrix ↔ install.mjs ASSET_MAP ──
#
# Compared as key→value PAIRS, not as two sets of names. A set comparison passes
# when two entries' values are swapped — `linux-x64` pointing at the aarch64
# asset, say — which is a plausible copy-paste mistake, breaks two platforms at
# once, and leaves the two collections looking perfectly balanced.
#
# The expected pairs are DERIVED from the matrix rather than hardcoded here: a
# new leg has to be reflected in both files, and hardcoding would make this
# check agree with itself instead of noticing.
#
# awk rather than a `while read` + `case` loop: macOS ships bash 3.2, whose
# parser chokes on a `case` pattern's `*)` inside a command substitution. CI's
# GNU bash 5 accepts it, so that version would only ever fail on a maintainer's
# machine — the worst place to find out.
#
# The `name:` values in release.yml's build matrix are the same strings
# install.mjs uses as ASSET_MAP keys, which is what makes a leg pair-able with
# its asset at all.
TAB="$(printf '\t')"

MATRIX_PAIRS="$(
  # Matrix blocks only. Every leg is `- name:` followed by `os:`/`target:`, and
  # stopping at `steps:` is what keeps step names out of the pairing.
  sed -n '/^[[:space:]]*matrix:/,/^[[:space:]]*steps:/p' .github/workflows/release.yml \
    | awk -v tab="$TAB" '
        /^[[:space:]]*- name:/ {
          n = $0; sub(/^[[:space:]]*- name:[[:space:]]*/, "", n); sub(/[[:space:]]+$/, "", n); next
        }
        /^[[:space:]]*target:/ {
          t = $0; sub(/^[[:space:]]*target:[[:space:]]*/, "", t); sub(/[[:space:]]+$/, "", t)
          if (n != "") printf "%s%sdeep-code-%s%s\n", n, tab, t, (t ~ /windows/ ? ".exe" : "")
          n = ""
        }
      ' \
    | tr -d '\r' \
    | sort -u
)"

MAP_PAIRS="$(
  sed -n '/const ASSET_MAP = {/,/^};/p' packages/deepcode/install.mjs \
    | sed -nE "s/^[[:space:]]*'([^']+)':[[:space:]]*'([^']+)',?[[:space:]]*$/\1\t\2/p" \
    | tr -d '\r' \
    | sort -u
)"

if [ -z "$MATRIX_PAIRS" ]; then
  bad "parsed no name/target pairs out of release.yml's build matrix — this check has stopped reading the file it is supposed to read"
elif [ -z "$MAP_PAIRS" ]; then
  bad "parsed no entries out of install.mjs's ASSET_MAP — same problem, other file"
else
  # Every leg must be paired with its asset under the SAME key.
  while IFS="$TAB" read -r leg asset; do
    [ -n "$leg" ] || continue
    if grep -qxF "${leg}${TAB}${asset}" <<<"$MAP_PAIRS"; then
      info "OK  ${leg} -> ${asset}"
    else
      bad "ASSET_MAP does not pair '${leg}' with '${asset}' (that leg's asset)"
      info "    ASSET_MAP pairs '${leg}' with: $(grep -F "${leg}${TAB}" <<<"$MAP_PAIRS" | cut -f2 | tr '\n' ' ')"
    fi
  done <<<"$MATRIX_PAIRS"

  # Extra ASSET_MAP keys are allowed — win32-arm64 aliases the x64 asset on
  # purpose — but only if they name an asset some leg actually produces. A value
  # nothing builds is a 404 for every user who resolves that key.
  while IFS="$TAB" read -r key asset; do
    [ -n "$key" ] || continue
    if ! grep -qF "${TAB}${asset}" <<<"$MATRIX_PAIRS"; then
      bad "ASSET_MAP entry '${key}' -> '${asset}' names an asset no build leg produces"
    fi
  done <<<"$MAP_PAIRS"
fi

# ── 1b. every built platform must have a native install gate and rehearsal ──
# The workflows use literal include rows. Stop reading at `steps:` and at the
# next job, so names from other matrices or steps cannot fill a missing leg.
platform_matrix() {
  awk -v job="$1" -v field="$2" -v tab="$TAB" '
    /^  [[:alnum:]_-]+:/ {
      if (active) exit
      active = ($0 == "  " job ":")
    }
    !active { next }
    /^[[:space:]]*steps:/ { exit }
    /^[[:space:]]*- name:/ {
      name = $0; sub(/^[[:space:]]*- name:[[:space:]]*/, "", name)
      os = ""; next
    }
    /^[[:space:]]*os:/ {
      os = $0; sub(/^[[:space:]]*os:[[:space:]]*/, "", os); next
    }
    /^[[:space:]]*(target|expect):/ {
      key = $0; sub(/^[[:space:]]*/, "", key); sub(/:.*/, "", key)
      value = $0; sub(/^[[:space:]]*[^:]+:[[:space:]]*/, "", value)
      if (key == field && name != "" && os != "")
        printf "%s%s%s%s%s\n", name, tab, os, tab, (field == "target" ? name : value)
      name = ""
    }
  ' "$3" | tr -d '\r' | sort
}

BUILD_PLATFORMS="$(platform_matrix build target .github/workflows/release.yml)"
INSTALL_PLATFORMS="$(platform_matrix verify-install expect .github/workflows/release.yml)"
CANARY_PLATFORMS="$(platform_matrix verify expect .github/workflows/verify-install-canary.yml)"

if [ -z "$BUILD_PLATFORMS" ]; then
  bad "parsed no native platform rows out of release.yml's build matrix"
elif [ "$BUILD_PLATFORMS" != "$INSTALL_PLATFORMS" ]; then
  bad "release install matrix must match every build platform, OS pin, and expected host"
else
  info "OK  every built platform has a matching native install gate"
fi
if [ -z "$INSTALL_PLATFORMS" ] || [ "$INSTALL_PLATFORMS" != "$CANARY_PLATFORMS" ]; then
  bad "install canary matrix must match the release install gate"
else
  info "OK  install canary covers the same native platforms as the release gate"
fi
# macos-latest is arm64; keeping the name without this native pin would give
# false Intel coverage. Review this guard together with any future runner bump.
if ! grep -qxF "darwin-x64${TAB}macos-15-intel${TAB}darwin-x64" <<<"$BUILD_PLATFORMS"; then
  bad "darwin-x64 must build and verify on the native macos-15-intel runner"
fi

# ── 2. the npm tarball's contents ──
PACK_DIR="$(mktemp -d)"
trap 'rm -rf "$PACK_DIR"' EXIT

# Keep this hermetic. npm writes its cache and debug logs into $HOME by default,
# which fails outright when $HOME is read-only and mutates a user's machine for
# what is supposed to be a read-only check. Both are config keys, so the
# `npm_config_*` spellings redirect them without a flag on every call.
export npm_config_cache="$PACK_DIR/npm-cache"
export npm_config_logs_dir="$PACK_DIR/npm-logs"

( cd packages/deepcode && npm pack --pack-destination "$PACK_DIR" >/dev/null )

# Real pack + list, not `npm pack --dry-run`: this asserts on the artifact that
# would actually be uploaded.
PACKED="$(tar -tzf "$PACK_DIR"/*.tgz | sed -E 's#^package/##' | grep -vE '/$|^$' | tr -d '\r' | sort)"

info "--- tarball contents ---"
printf '%s\n' "$PACKED" | sed 's/^/    /'

REQUIRED='install.mjs
bin/deepcode
package.json'
while IFS= read -r wanted; do
  [ -n "$wanted" ] || continue
  if ! grep -qxF "$wanted" <<<"$PACKED"; then
    bad "tarball is missing required file: $wanted"
  fi
done <<<"$REQUIRED"

FORBIDDEN='^bin/(deepcode-bin|deepcode\.exe)$|\.download$'
if grep -qE "$FORBIDDEN" <<<"$PACKED"; then
  bad "tarball contains a downloaded platform binary — this would be handed to every user"
  grep -E "$FORBIDDEN" <<<"$PACKED" | sed 's/^/    /' || true
fi

# ── 3. the launcher must hand the linked name over ──
# The launcher supplies the public command name to the downloaded binary.
if grep -q 'DEEP_CODE_PROGRAM_NAME' packages/deepcode/bin/deepcode; then
  info "OK  bin/deepcode hands DEEP_CODE_PROGRAM_NAME over to the binary"
else
  bad "bin/deepcode no longer sets DEEP_CODE_PROGRAM_NAME — the binary must report the public command name"
fi

if [ "$fail" -eq 0 ]; then
  info "OK  packaging contract holds (asset map, native verification matrices, tarball contents, launcher handoff)"
fi

exit "$fail"
