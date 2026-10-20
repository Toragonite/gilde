#!/usr/bin/env bash
# lint-forbidden.sh — enforces the hard bans of docs/DESIGN.md across the Rust tree.
#
#   1. Whole Rust tree: no `pkill`, no `killall`, no `Command::new("sh")`, no `.arg("-c")`.
#      Signals go through supervise::kill (killpg + identity fences), never through a
#      name-matching helper that would hit the user's own terminals, and no argv ever
#      travels through a shell.
#   2. src-tauri/src/pty/: no `from_utf8_lossy`, no `String::from_utf8`.
#      PTY output stays bytes until xterm.js decodes it exactly once (DESIGN §0 rule 4).
#
# Exits 1 on the first category that has a violation (after reporting all of them).
# Lines carrying the marker `gilde-lint-allow: <rule>` are exempt; use sparingly and
# only where the surrounding code proves the rule cannot be violated.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rust_src="${repo_root}/src-tauri/src"
pty_src="${rust_src}/pty"

status=0

# Report matches for one pattern. $1 = human label, $2 = fixed-string pattern,
# $3 = directory to search, $4 = allow-marker rule name.
scan() {
  local label="$1" pattern="$2" dir="$3" rule="$4"
  [ -d "$dir" ] || return 0

  local hits
  hits="$(grep -rnF --include='*.rs' -- "$pattern" "$dir" 2>/dev/null \
          | grep -v "gilde-lint-allow: ${rule}" || true)"

  if [ -n "$hits" ]; then
    echo "FORBIDDEN (${label}): pattern '${pattern}' found under ${dir#"${repo_root}/"}" >&2
    printf '%s\n' "$hits" | sed "s|^${repo_root}/||" | sed 's/^/    /' >&2
    status=1
  fi
}

# --- 1. process-control bans, whole Rust tree ---------------------------------
scan "no process-name killing"  'pkill'              "$rust_src" "pkill"
scan "no process-name killing"  'killall'            "$rust_src" "killall"
scan "no shell spawning"        'Command::new("sh")' "$rust_src" "shell"
scan "no shell spawning"        '.arg("-c")'         "$rust_src" "shell"

# --- 2. byte-fidelity bans, PTY module only -----------------------------------
scan "PTY bytes stay bytes"     'from_utf8_lossy'    "$pty_src"  "pty-bytes"
scan "PTY bytes stay bytes"     'String::from_utf8'  "$pty_src"  "pty-bytes"

if [ "$status" -ne 0 ]; then
  echo "" >&2
  echo "lint-forbidden: violations found (see docs/DESIGN.md §0, §7, §10-2)." >&2
  exit 1
fi

echo "lint-forbidden: clean"
