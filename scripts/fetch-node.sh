#!/usr/bin/env bash
#
# fetch-node.sh — vendor the pinned Node runtime the headless lane ships with.
#
# DECISIONS ("M1 세부 기본값"): the sidecar's Node 20 binary is vendored at BUILD
# time by an online fetch (`externalBin`); a system `node` is a development
# convenience and a runtime fallback, never the shipped runtime.
#
# What it does, in order:
#
#   1. If the target is already installed, verified and ad-hoc signed, stop.
#      This is the offline-friendly fast path: a clean `npm run tauri build`
#      after the first fetch never touches the network.
#   2. Download the pinned tarball plus upstream `SHASUMS256.txt`.
#   3. Verify the tarball against **its SHASUMS256.txt entry**, and cross-check
#      that entry against the hash pinned in this file. Either mismatch is fatal:
#      a moved upstream artifact must be re-pinned by a human, never accepted.
#   4. Extract `bin/node`, install it atomically at
#      `src-tauri/binaries/gilde-node-<triple>` (Tauri resolves `binaries/gilde-node`
#      to that name), and ad-hoc codesign it —
#      `codesign -s - --force --timestamp=none` — so the copy inside the bundle
#      keeps a valid signature on Apple silicon.
#   5. Extract the tarball's `LICENSE` to `src-tauri/binaries/node-LICENSE`.
#      Node.js is MIT and that file also carries the notices of everything Node
#      links statically (V8, OpenSSL, ICU, libuv, …); redistributing the binary
#      means shipping it, so `tauri.conf.json > bundle.resources` puts it at
#      `Contents/Resources/licenses/node-LICENSE`. Its sha256 is pinned too.
#   6. Prove it: `--version` must print the pin, `codesign --verify` must pass,
#      the LICENSE must hash to its pin.
#
# Apple silicon only: the one pinned tarball is darwin-arm64, so any other host
# (an Intel Mac, or a Rosetta shell on Apple silicon) stops here with an error
# instead of installing a runtime the bundle could not execute.
#
# Without a network and without an installed binary it fails loudly (see
# `no_network_advice`): the app itself still runs, because
# `SidecarRuntime::resolve` falls back to a login-shell-resolved system `node`
# and `app_diagnostics()` then reports `nodeSource: "system"` — but a *bundle*
# without the vendored runtime is not something this script will fabricate.
#
# Usage:
#   scripts/fetch-node.sh              fetch if needed, verify, sign
#   scripts/fetch-node.sh --check      verify only; never touches the network
#   scripts/fetch-node.sh --force      re-download even if a good copy is there
#   scripts/fetch-node.sh --print-target   print the install path and exit
#
# Environment:
#   GILDE_NODE_TRIPLE=<rust triple>   override the target-triple suffix
#   GILDE_NODE_STUB=1                 offline dev only — see `write_stub`
#
set -euo pipefail

# ---------------------------------------------------------------------------
# the pin
# ---------------------------------------------------------------------------

# Node 20.19.x, per M1-PLAN T28.1. Bumping any of these three lines is a
# deliberate act: the checksum below is the one the release page publishes for
# this exact artifact and was verified on 2026-09-07 (docs/PINNED-VERSIONS.md §7-1).
NODE_VERSION="v20.19.2"
NODE_PLATFORM="darwin-arm64"
NODE_TARBALL_SHA256="5f96c725cd2be1151f08b25c05bf5a21866a2dc7db3803d351a6ea19fcc53665"
# sha256 of `LICENSE` inside that tarball (measured 2026-10-19). Bump with the
# three lines above.
NODE_LICENSE_SHA256="3716cab1c1ca53b1be9481b6d20bd8e00a4b9561d1b77427fd95e17aad6fc17e"

NODE_DIST_BASE="${GILDE_NODE_DIST_BASE:-https://nodejs.org/dist}"
NODE_DIRNAME="node-${NODE_VERSION}-${NODE_PLATFORM}"
NODE_TARBALL="${NODE_DIRNAME}.tar.gz"
NODE_URL="${NODE_DIST_BASE}/${NODE_VERSION}/${NODE_TARBALL}"
SHASUMS_URL="${NODE_DIST_BASE}/${NODE_VERSION}/SHASUMS256.txt"

# ---------------------------------------------------------------------------
# paths
# ---------------------------------------------------------------------------

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binaries_dir="${repo_root}/src-tauri/binaries"

default_triple() {
  case "$(uname -s)/$(uname -m)" in
    Darwin/arm64) echo "aarch64-apple-darwin" ;;
    Darwin/x86_64) echo "x86_64-apple-darwin" ;;
    *) echo "" ;;
  esac
}

TRIPLE="${GILDE_NODE_TRIPLE:-$(default_triple)}"
target="${binaries_dir}/gilde-node-${TRIPLE}"
# Not triple-suffixed: it is a bundle resource, not an externalBin.
license_target="${binaries_dir}/node-LICENSE"

say() { printf '[fetch-node] %s\n' "$*"; }
die() { printf '[fetch-node] error: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# argument handling
# ---------------------------------------------------------------------------

mode="fetch"
for arg in "$@"; do
  case "$arg" in
    --check) mode="check" ;;
    --force) mode="force" ;;
    --print-target) printf '%s\n' "$target"; exit 0 ;;
    -h|--help) sed -n '2,50p' "${BASH_SOURCE[0]}" | sed 's|^# \{0,1\}||'; exit 0 ;;
    *) die "unknown argument: $arg (try --help)" ;;
  esac
done

[ -n "$TRIPLE" ] || die "unsupported host $(uname -s)/$(uname -m); set GILDE_NODE_TRIPLE to override"

if [ "$NODE_PLATFORM" != "darwin-arm64" ] || [ "$TRIPLE" != "aarch64-apple-darwin" ]; then
  die "Gilde builds on Apple silicon only: the pinned runtime is ${NODE_PLATFORM}, this target is ${TRIPLE}.
       On an Apple-silicon Mac, run the build from a native (arm64) shell, not under Rosetta
       (\`uname -m\` must print arm64). Intel Macs are not supported (see BUILDING.md)."
fi

# ---------------------------------------------------------------------------
# verification
# ---------------------------------------------------------------------------

# The three questions an installed runtime has to answer: is it there, is it the
# version we pinned, and does it still carry a signature the loader accepts.
verify_installed() {
  [ -x "$target" ] || return 1
  local reported
  reported="$("$target" --version 2>/dev/null)" || return 1
  [ "$reported" = "$NODE_VERSION" ] || {
    say "installed runtime reports ${reported}, pin is ${NODE_VERSION}"
    return 1
  }
  codesign --verify "$target" >/dev/null 2>&1 || {
    say "installed runtime is not validly signed"
    return 1
  }
  verify_license || return 1
  return 0
}

# The runtime's LICENSE ships with it (bundle.resources); a runtime without it is
# not a shippable install. Checked by hash, so a LICENSE left over from another
# Node version is caught too.
verify_license() {
  [ -f "$license_target" ] || {
    say "the Node LICENSE that ships with the runtime is missing (${license_target})"
    return 1
  }
  local actual
  actual="$(shasum -a 256 "$license_target" | awk '{print $1}')"
  [ "$actual" = "$NODE_LICENSE_SHA256" ] || {
    say "${license_target} hashes to ${actual}, pin is ${NODE_LICENSE_SHA256}"
    return 1
  }
  return 0
}

# A runtime that is the right build but has lost its signature (a stray `strip`,
# an `xattr` dance, a copy through a filesystem that dropped the extended
# attribute) needs a `codesign`, not an 88 MB download.
#
# Signing comes *before* the version check on purpose: on Apple silicon an
# unsigned or badly signed Mach-O is killed by the kernel at exec, so
# `--version` cannot answer until the signature is repaired. If the repaired
# binary then turns out to be the wrong build, the download path overwrites it
# anyway and nothing is lost.
repair_signature() {
  [ -f "$target" ] || return 1
  file -b "$target" | grep -q 'Mach-O' || return 1
  chmod 0755 "$target"
  say "re-signing ${target} (ad-hoc)"
  codesign -s - --force --timestamp=none "$target" >/dev/null 2>&1 || return 1
  verify_installed
}

no_network_advice() {
  cat >&2 <<EOF
[fetch-node] Could not download the pinned Node runtime, and
[fetch-node]   ${target}
[fetch-node] does not exist. This machine appears to be offline, or ${NODE_DIST_BASE}
[fetch-node] is unreachable.
[fetch-node]
[fetch-node] What this blocks: only the *bundle*. \`tauri.conf.json\` declares
[fetch-node]   "externalBin": ["binaries/gilde-node"]
[fetch-node] and tauri-build refuses to build while that file is missing.
[fetch-node]
[fetch-node] What it does not block: the app. With no vendored runtime,
[fetch-node] SidecarRuntime::resolve() falls back to a login-shell-resolved system
[fetch-node] \`node\`, and app_diagnostics() reports  sidecar.nodeSource: "system"
[fetch-node] with the reason in sidecar.warnings.
[fetch-node]
[fetch-node] Ways forward:
[fetch-node]   * get on a network and re-run: scripts/fetch-node.sh
[fetch-node]   * offline dev loop only:  GILDE_NODE_STUB=1 scripts/fetch-node.sh
[fetch-node]     writes a NON-executable placeholder so cargo build proceeds; the
[fetch-node]     app then runs on the system node (nodeSource: "system"). Never
[fetch-node]     ship a bundle built that way.
[fetch-node]   * install by hand: download ${NODE_URL}
[fetch-node]     verify sha256 ${NODE_TARBALL_SHA256}
[fetch-node]     copy ${NODE_DIRNAME}/bin/node to ${target}
[fetch-node]     then: codesign -s - --force --timestamp=none "${target}"
EOF
}

# Offline escape hatch, opt-in only. A 0644 file satisfies tauri-build's
# existence check while `is_executable()` in sidecar/mod.rs rejects it, so the
# runtime resolution lands on the system-node rung it is designed for instead of
# trying to exec a lie.
write_stub() {
  mkdir -p "$binaries_dir"
  cat >"$target" <<EOF
PLACEHOLDER — not a Node binary, and deliberately not executable.

Written by scripts/fetch-node.sh with GILDE_NODE_STUB=1 because the pinned
${NODE_DIRNAME} could not be downloaded. It exists only so tauri-build's
externalBin existence check passes and the offline dev loop keeps working.

At runtime SidecarRuntime::resolve() sees a non-executable file, skips the
vendored rung and uses the system node — app_diagnostics() reports
nodeSource: "system". Delete this file and re-run scripts/fetch-node.sh
before producing anything you intend to ship.
EOF
  chmod 0644 "$target"
  # tauri-build also requires the LICENSE resource to exist; a placeholder that
  # says what it is keeps the offline loop building and fails verify_license.
  if [ ! -f "$license_target" ]; then
    printf 'PLACEHOLDER — the Node.js LICENSE was not downloaded (GILDE_NODE_STUB=1).\nRe-run scripts/fetch-node.sh before building anything you intend to ship.\n' >"$license_target"
  fi
  say "wrote a NON-executable placeholder at ${target} (GILDE_NODE_STUB=1)"
  say "the app will run on the system node; app_diagnostics() will say nodeSource: \"system\""
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

if [ "$mode" = "check" ]; then
  if verify_installed; then
    say "ok: ${target} is ${NODE_VERSION}, ad-hoc signed; its LICENSE matches the pin"
    exit 0
  fi
  die "no verified runtime at ${target} — run scripts/fetch-node.sh"
fi

if [ "$mode" != "force" ]; then
  if verify_installed; then
    say "${target} is already ${NODE_VERSION} and signed; nothing to do"
    exit 0
  fi
  if repair_signature; then
    say "${target} was the right build with a bad signature; re-signed in place"
    exit 0
  fi
fi

for tool in curl shasum tar codesign; do
  command -v "$tool" >/dev/null 2>&1 || die "'${tool}' is required but not on PATH"
done

work="$(mktemp -d "${TMPDIR:-/tmp}/gilde-fetch-node.XXXXXX")"
cleanup() { rm -rf "$work"; }
trap cleanup EXIT

say "fetching ${NODE_URL}"
if ! curl --fail --silent --show-error --location --retry 2 --connect-timeout 15 \
     --output "${work}/${NODE_TARBALL}" "$NODE_URL"; then
  if [ "${GILDE_NODE_STUB:-}" = "1" ]; then
    say "download failed; GILDE_NODE_STUB=1 is set"
    write_stub
    exit 0
  fi
  no_network_advice
  die "download failed"
fi

say "fetching ${SHASUMS_URL}"
if ! curl --fail --silent --show-error --location --retry 2 --connect-timeout 15 \
     --output "${work}/SHASUMS256.txt" "$SHASUMS_URL"; then
  no_network_advice
  die "SHASUMS256.txt download failed — the tarball is not verifiable without it"
fi

# The acceptance is "verified by its SHASUMS entry", so the entry is what we run
# `shasum -c` against; the pinned constant is a second, independent gate that
# catches a re-published artifact.
entry="$(grep -E "[[:space:]]${NODE_TARBALL}\$" "${work}/SHASUMS256.txt" || true)"
[ -n "$entry" ] || die "SHASUMS256.txt has no entry for ${NODE_TARBALL}"
upstream_sha="${entry%% *}"

if [ "$upstream_sha" != "$NODE_TARBALL_SHA256" ]; then
  die "upstream SHASUMS256.txt says ${upstream_sha} for ${NODE_TARBALL}, this script pins ${NODE_TARBALL_SHA256}.
       The artifact changed. Re-pin deliberately after checking the release notes; do not edit this away."
fi

printf '%s\n' "$entry" >"${work}/entry.sha256"
( cd "$work" && shasum -a 256 -c entry.sha256 >/dev/null ) \
  || die "checksum mismatch for ${NODE_TARBALL} — refusing to install it"
say "checksum verified against the SHASUMS256.txt entry (${upstream_sha})"

tar -xzf "${work}/${NODE_TARBALL}" -C "$work" "${NODE_DIRNAME}/bin/node" "${NODE_DIRNAME}/LICENSE" \
  || die "could not extract ${NODE_DIRNAME}/bin/node and LICENSE from the tarball"

license_sha="$(shasum -a 256 "${work}/${NODE_DIRNAME}/LICENSE" | awk '{print $1}')"
[ "$license_sha" = "$NODE_LICENSE_SHA256" ] \
  || die "the tarball's LICENSE hashes to ${license_sha}, this script pins ${NODE_LICENSE_SHA256}"

mkdir -p "$binaries_dir"
staged="${work}/gilde-node"
cp "${work}/${NODE_DIRNAME}/bin/node" "$staged"
chmod 0755 "$staged"

# Ad-hoc signature, exactly as M1-PLAN T28.4 specifies. Signing the staged copy
# before the move keeps the install atomic: the target is never a signed-but-half-
# written file. `--timestamp=none` because an ad-hoc signature has no authority to
# timestamp against and the network call would only make builds slower and flakier.
codesign -s - --force --timestamp=none "$staged" \
  || die "ad-hoc codesign failed for the staged runtime"

mv -f "$staged" "$target"

cp "${work}/${NODE_DIRNAME}/LICENSE" "${work}/node-LICENSE.staged"
chmod 0644 "${work}/node-LICENSE.staged"
mv -f "${work}/node-LICENSE.staged" "$license_target"

verify_installed || die "installed ${target} did not verify (version, signature and/or LICENSE)"
say "installed ${NODE_VERSION} at ${target} (ad-hoc signed, verified)"
say "installed its LICENSE at ${license_target} (sha256 verified)"
