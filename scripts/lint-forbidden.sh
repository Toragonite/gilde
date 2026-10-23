#!/usr/bin/env bash
# lint-forbidden.sh — the hard bans across the Rust tree (docs/DESIGN.md §0, §7,
# §10-2; Gilde 2 contract §9.3 enforcement 2, §9.7; owner T11).
#
#   1. Whole Rust tree: no `pkill`, no `killall`, no shell spawning — neither
#      `Command::new("sh")`/`"/bin/sh"`/`"bash"`/`"zsh"` (or the PTY
#      `CommandBuilder` spellings), nor `.arg("-c")`, nor a `"/bin/sh", "-c"`
#      argv. Signals go through supervise::kill (killpg + identity fences),
#      never through a name-matching helper that would hit the user's own
#      terminals, and no argv ever travels through a shell.
#      ONE named exemption (§9.3, A §9.2): src-tauri/src/panehost/panes.rs,
#      where the per-lead token check sits beside the `/bin/sh -c` of a
#      teammate pane.
#   2. src-tauri/src/pty/: no `from_utf8_lossy`, no `String::from_utf8`.
#      PTY output stays bytes until xterm.js decodes it exactly once (DESIGN §0 rule 4).
#   3. Production code (outside `#[cfg(test)]`): no raw argv formatted into a log
#      line or a string (`argv = ?…`, `?argv`, `{argv}`, `"{:?}", argv`)
#      outside the one redaction helper src-tauri/src/supervise/redact.rs
#      (§9.7, F-03): store, log or show only `redact_argv(…)`.
#
# Exits 1 when a category has a violation (after reporting all of them).
# Lines carrying the marker `gilde-lint-allow: <rule>` are exempt; use sparingly and
# only where the surrounding code proves the rule cannot be violated.
#
#   bash scripts/lint-forbidden.sh [--root <repo>]
#   bash scripts/lint-forbidden.sh --self-test     (each rule fails on a seeded violation)

set -uo pipefail

self="${BASH_SOURCE[0]}"
repo_root="$(cd "$(dirname "$self")/.." && pwd)"

if [ "${1:-}" = "--self-test" ]; then
  failed=0
  run_case() { # $1 = name, $2 = expected exit (0|1), $3 = rel path, $4 = file body
    local name="$1" want="$2" rel="$3" body="$4" tmp got
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/lint-forbidden.XXXXXX")"
    mkdir -p "$tmp/src-tauri/src/$(dirname "$rel")"
    printf '%s\n' "$body" > "$tmp/src-tauri/src/$rel"
    bash "$self" --root "$tmp" >/dev/null 2>&1
    got=$?
    rm -rf "$tmp"
    if [ "$got" = "$want" ]; then
      echo "ok   $name"
    else
      echo "FAIL $name (exit $got, wanted $want)"
      failed=$((failed + 1))
    fi
  }
  run_case "a clean file passes"                          0 "world/a.rs"         'fn f() { let x = 1; }'
  run_case "pkill is rejected"                            1 "supervise/x.rs"     'fn f() { run("pkill"); }'
  run_case "killall is rejected"                          1 "supervise/x.rs"     'fn f() { run("killall"); }'
  run_case "Command::new(\"sh\") is rejected"            1 "world/b.rs"         'fn f() { std::process::Command::new("sh"); }'
  run_case "Command::new(\"/bin/sh\") is rejected"       1 "world/b.rs"         'fn f() { std::process::Command::new("/bin/sh"); }'
  run_case "Command::new(\"bash\") is rejected"          1 "world/b.rs"         'fn f() { std::process::Command::new("bash"); }'
  run_case ".arg(\"-c\") is rejected"                     1 "world/b.rs"         'fn f(c: &mut Command) { c.arg("-c"); }'
  run_case "a /bin/sh -c argv is rejected"               1 "world/b.rs"         'fn f() -> Vec<&'"'"'static str> { vec!["/bin/sh", "-c", "x"] }'
  run_case "the PTY CommandBuilder shell is rejected"   1 "pty/x.rs"           'fn f() { CommandBuilder::new("/bin/sh"); }'
  run_case "panes.rs is the one shell exemption"         0 "panehost/panes.rs"  'fn f(cmd: &str) -> Vec<String> { vec!["/bin/sh".into(), "-c".into(), cmd.into()] } fn g() { let a = ["/bin/sh", "-c"]; }'
  run_case "panes.rs is not exempt from pkill"           1 "panehost/panes.rs"  'fn f() { run("pkill"); }'
  run_case "PTY text decoding is rejected"               1 "pty/y.rs"           'fn f(b: &[u8]) { String::from_utf8_lossy(b); }'
  run_case "a raw argv log field is rejected"            1 "sessions/z.rs"      'fn f(argv: &[String]) { tracing::info!(argv = ?argv, "spawn"); }'
  run_case "a raw argv shorthand field is rejected"      1 "sessions/z.rs"      'fn f(plan: &P) { tracing::warn!(?plan.argv, "spawn"); }'
  run_case "a raw argv format capture is rejected"       1 "sessions/z.rs"      'fn f(argv: &[String]) -> String { format!("ran {argv:?}") }'
  run_case "a raw argv debug argument is rejected"       1 "sessions/z.rs"      'fn f(argv: &[String]) -> String { format!("ran {:?}", argv) }'
  run_case "the redaction helper may format argv"        0 "supervise/redact.rs" 'fn f(argv: &[String]) -> String { format!("{argv:?}") }'
  run_case "argv formatting in a test module passes"     0 "sessions/t.rs"      '#[cfg(test)]
mod tests { fn t(argv: &[String]) { assert!(argv.is_empty(), "{argv:?}"); } }'
  run_case "the allow marker exempts one line"           0 "world/c.rs"         'fn f(c: &mut Command) { c.arg("-c"); } // gilde-lint-allow: shell'
  if [ "$failed" -ne 0 ]; then
    echo "lint-forbidden self-test: $failed case(s) failed"
    exit 1
  fi
  echo "lint-forbidden self-test: all cases pass"
  exit 0
fi

if [ "${1:-}" = "--root" ]; then
  if [ -z "${2:-}" ] || [ ! -d "$2" ]; then
    echo "usage: lint-forbidden.sh [--root <repo>] | --self-test" >&2
    exit 2
  fi
  repo_root="$(cd "$2" && pwd)"
fi

rust_src="${repo_root}/src-tauri/src"
pty_src="${rust_src}/pty"
shell_exempt="src-tauri/src/panehost/panes.rs"
argv_helper="src-tauri/src/supervise/redact.rs"

status=0

# Report matches for one fixed-string pattern. $1 = human label, $2 = pattern,
# $3 = directory to search, $4 = allow-marker rule name, $5 = exempt file (optional).
scan() {
  local label="$1" pattern="$2" dir="$3" rule="$4" exempt="${5:-}"
  [ -d "$dir" ] || return 0

  local hits
  hits="$(grep -rnF --include='*.rs' -- "$pattern" "$dir" 2>/dev/null \
          | grep -v "gilde-lint-allow: ${rule}" || true)"
  if [ -n "$exempt" ] && [ -n "$hits" ]; then
    hits="$(printf '%s\n' "$hits" | grep -vF -- "${repo_root}/${exempt}:" || true)"
  fi

  if [ -n "$hits" ]; then
    echo "FORBIDDEN (${label}): pattern '${pattern}' found under ${dir#"${repo_root}/"}" >&2
    printf '%s\n' "$hits" | sed "s|^${repo_root}/||" | sed 's/^/    /' >&2
    status=1
  fi
}

# Production lines of every Rust file (`file:line:text`), stopping at the
# first top-level `#[cfg(test)]` (test modules sit at the end of a file);
# `tests.rs` files and `tests/` dirs are test modules.
production_lines() {
  local f
  find "$rust_src" -name '*.rs' ! -name 'tests.rs' ! -path '*/tests/*' 2>/dev/null | sort | while read -r f; do
    awk -v F="${f#"${repo_root}/"}" '/^[[:space:]]*#\[cfg\(test\)\]/{exit} {print F ":" NR ":" $0}' "$f"
  done
}

# Report production-code matches of one extended regex. $1 = label, $2 = ERE,
# $3 = allow-marker rule, $4 = exempt file (optional).
scan_prod() {
  local label="$1" ere="$2" rule="$3" exempt="${4:-}"
  [ -d "$rust_src" ] || return 0
  local hits
  hits="$(production_lines | grep -E -- "$ere" | grep -v "gilde-lint-allow: ${rule}" \
          | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' || true)"
  if [ -n "$exempt" ] && [ -n "$hits" ]; then
    hits="$(printf '%s\n' "$hits" | grep -vF -- "${exempt}:" || true)"
  fi
  if [ -n "$hits" ]; then
    echo "FORBIDDEN (${label}):" >&2
    printf '%s\n' "$hits" | sed 's/^/    /' >&2
    status=1
  fi
}

# --- 1. process-control bans, whole Rust tree ---------------------------------
scan "no process-name killing"  'pkill'                     "$rust_src" "pkill"
scan "no process-name killing"  'killall'                   "$rust_src" "killall"
scan "no shell spawning"        'Command::new("sh")'        "$rust_src" "shell" "$shell_exempt"
scan "no shell spawning"        'Command::new("/bin/sh")'   "$rust_src" "shell" "$shell_exempt"
scan "no shell spawning"        'Command::new("bash")'      "$rust_src" "shell" "$shell_exempt"
scan "no shell spawning"        'Command::new("/bin/bash")' "$rust_src" "shell" "$shell_exempt"
scan "no shell spawning"        'Command::new("zsh")'       "$rust_src" "shell" "$shell_exempt"
scan "no shell spawning"        'Command::new("/bin/zsh")'  "$rust_src" "shell" "$shell_exempt"
scan "no shell spawning"        'CommandBuilder::new("sh")' "$rust_src" "shell" "$shell_exempt"
scan "no shell spawning"        'CommandBuilder::new("/bin/sh")' "$rust_src" "shell" "$shell_exempt"
scan "no shell spawning"        '.arg("-c")'                "$rust_src" "shell" "$shell_exempt"
scan_prod "no shell spawning: a /bin/sh -c argv" '"/bin/(sh|bash|zsh|dash)"[[:space:]]*(\.(into|to_string|to_owned)\(\))?[[:space:]]*,[[:space:]]*"-c"' "shell" "$shell_exempt"

# --- 2. byte-fidelity bans, PTY module only -----------------------------------
scan "PTY bytes stay bytes"     'from_utf8_lossy'    "$pty_src"  "pty-bytes"
scan "PTY bytes stay bytes"     'String::from_utf8'  "$pty_src"  "pty-bytes"

# --- 3. raw argv is formatted only by the redaction helper (§9.7) -------------
scan_prod "raw argv formatted outside supervise/redact.rs (use redact_argv)" \
  '(argv|cmdline|command_line)[[:space:]]*=[[:space:]]*[?%]|[?%]((self|plan|req|spec|row|args)\.)?(argv|cmdline)\b|\{(argv|cmdline)(:[^}]*)?\}|"[^"]*\{:\??\}[^"]*"[^;]*,[[:space:]]*&?([a-z_]+\.)*(argv|cmdline)\b' \
  "argv" "$argv_helper"

if [ "$status" -ne 0 ]; then
  echo "" >&2
  echo "lint-forbidden: violations found (see docs/DESIGN.md §0, §7, §10-2; contract §9.3, §9.7)." >&2
  exit 1
fi

echo "lint-forbidden: clean"
