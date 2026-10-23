#!/bin/sh
# hermetic-cargo.sh — run cargo with none of the developer's environment
# (Gilde 2 contract §14.1, A38.1). Every cargo command a gate runs — build,
# clippy, test — goes through it.
#
#   scripts/gates/hermetic-cargo.sh test --test boot
#   CARGO_TARGET_DIR=<lane> CARGO_BUILD_JOBS=3 scripts/gates/hermetic-cargo.sh \
#       clippy --all-targets --features test-fixtures,ts-export,showcase,probe-log,wakeup-count -- -D warnings
#
# cargo "$@" runs under `env -i` with exactly:
#
#   HOME=<fresh mktemp dir>          (removed when the script exits)
#   GILDE_HOME=<that dir>/.gilde
#   PATH=<cargo bin>:/usr/bin:/bin:/usr/sbin:/sbin
#   RUSTUP_HOME, CARGO_HOME          the caller's (default: $HOME/.rustup, $HOME/.cargo)
#   SHELL=/nonexistent               no login shell can read a real profile
#   LANG=en_US.UTF-8
#   AGENT_CLI_CREDENTIAL_STORE=memory
#   DISABLE_AUTOUPDATER=1
#   GIT_CONFIG_NOSYSTEM=1            with the temp HOME: neither ~/.gitconfig nor the system config
#
# plus, when the caller set them, the lane variables CARGO_TARGET_DIR,
# CARGO_INCREMENTAL, CARGO_BUILD_JOBS, CARGO_PROFILE_DEV_DEBUG,
# CARGO_PROFILE_TEST_DEBUG and RUST_BACKTRACE, and the two outputs of
# scripts/gen-ipc.mjs's export run, TS_RS_EXPORT_DIR and
# GILDE_IPC_MANIFEST_OUT. Nothing else passes.
#
# <cargo bin> is $CARGO_HOME/bin when it holds cargo, else the directory of
# the caller's `cargo` on PATH. The script refuses to start (exit 2, nothing
# run) when a vendor CLI — claude, claude.exe, codex, cursor-agent, agent,
# agy, gemini — or tmux is reachable on the PATH it builds. Otherwise it exits
# with cargo's status.

set -eu

die() {
  printf 'hermetic-cargo: %s\n' "$*" >&2
  exit 2
}

# -- the toolchain, from the caller's environment ---------------------------
cargo_home=${CARGO_HOME:-}
rustup_home=${RUSTUP_HOME:-}
if [ -z "$cargo_home" ] || [ -z "$rustup_home" ]; then
  [ -n "${HOME:-}" ] || die "HOME is not set, so CARGO_HOME/RUSTUP_HOME cannot be derived; set them"
fi
[ -n "$cargo_home" ] || cargo_home=$HOME/.cargo
[ -n "$rustup_home" ] || rustup_home=$HOME/.rustup

if [ -x "$cargo_home/bin/cargo" ]; then
  cargo_bin=$cargo_home/bin
else
  found=$(command -v cargo 2>/dev/null || true)
  case $found in
    /*) cargo_bin=$(cd "$(dirname "$found")" && pwd -P) ;;
    *) die "cargo not found (not in $cargo_home/bin, not on PATH)" ;;
  esac
fi
[ -x "$cargo_bin/cargo" ] || die "$cargo_bin/cargo is not executable"

hermetic_path=$cargo_bin:/usr/bin:/bin:/usr/sbin:/sbin

# -- refuse a PATH that reaches a vendor CLI --------------------------------
reachable=
old_ifs=$IFS
IFS=:
for dir in $hermetic_path; do
  for name in claude claude.exe codex cursor-agent agent agy gemini tmux; do
    if [ -e "$dir/$name" ]; then
      reachable="$reachable $dir/$name"
    fi
  done
done
IFS=$old_ifs
[ -z "$reachable" ] || die "a vendor CLI is reachable on the hermetic PATH $hermetic_path:$reachable"

# -- a fresh HOME, removed on exit ------------------------------------------
# Short and under /tmp: Unix socket paths below HOME must stay under 104 bytes.
home=$(mktemp -d /tmp/gh.XXXXXX) || die "mktemp failed"
cleanup() {
  rm -rf "$home"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# -- the command: env -i, the lane variables, then cargo and its arguments --
set -- "$cargo_bin/cargo" "$@"
for var in GILDE_IPC_MANIFEST_OUT TS_RS_EXPORT_DIR RUST_BACKTRACE CARGO_PROFILE_TEST_DEBUG CARGO_PROFILE_DEV_DEBUG CARGO_BUILD_JOBS CARGO_INCREMENTAL CARGO_TARGET_DIR; do
  eval "is_set=\${$var+set}"
  if [ "$is_set" = set ]; then
    eval "value=\$$var"
    set -- "$var=$value" "$@"
  fi
done

set +e
env -i \
  HOME="$home" \
  GILDE_HOME="$home/.gilde" \
  PATH="$hermetic_path" \
  RUSTUP_HOME="$rustup_home" \
  CARGO_HOME="$cargo_home" \
  SHELL=/nonexistent \
  LANG=en_US.UTF-8 \
  AGENT_CLI_CREDENTIAL_STORE=memory \
  DISABLE_AUTOUPDATER=1 \
  GIT_CONFIG_NOSYSTEM=1 \
  "$@"
status=$?
exit "$status"
