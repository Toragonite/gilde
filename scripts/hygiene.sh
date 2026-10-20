#!/usr/bin/env bash
# hygiene.sh — what did this project's test/dev activity leave running?
#
# Twice in two days an agent-driven test run reported "nothing remains" and was
# wrong: once 15 orphans (gilde-node sidecars spinning at 100 % CPU, zsh load
# loops), once two `cargo test --lib -- --ignored replay_diagnostics` harnesses
# that had been hung for two and a half hours. Both times the evidence was one
# `ps` away. This script is that `ps`, written down, so the check is a command
# and not a habit.
#
#   bash scripts/hygiene.sh              # or --list: what is ours, what is not
#   bash scripts/hygiene.sh --kill       # TERM, wait 3 s, KILL survivors, re-list
#   bash scripts/hygiene.sh --worktrees  # smoke worktrees and branches
#   bash scripts/hygiene.sh --prune-worktrees
#   bash scripts/hygiene.sh --memory     # one screen of memory pressure
#
# Exit codes for --list and --kill: 0 when nothing of ours is running, 1 when
# something is (for --kill, when something *survived* SIGKILL). Anything under
# "NOT ours" never affects the exit code and is never signalled.
#
# Design constraints, deliberate:
#
#   * Zero dependencies. `ps`, `lsof`, `kill`, `awk`, `sed`, `git` — all in the
#     macOS base system. bash 3.2 (`/bin/bash`), so no associative arrays, no
#     `mapfile`, no `${x^^}`.
#   * **No `pkill`, no `killall`.** Same rule as `docs/DESIGN.md` §0/§7 puts on
#     the Rust tree, for the same reason: a name matcher hits the developer's
#     own `claude`, the VS Code extension's `codex`, and the terminal this is
#     typed into. Every signal here goes to a pid this script printed first.
#   * Identity is *cwd*, not name. Seven `claude`/`codex` processes belong to
#     VS Code on this machine at any time; what separates ours from theirs is
#     that ours sit in `~/.gilde/worktrees/` or a test temp dir. They are
#     printed under "NOT ours (skipped)" so the reader can see the script
#     considered and rejected them.
#   * The caller's own process and every ancestor of it are protected, and so
#     are their process groups: `--kill` run from a session whose cwd is this
#     repo must not kill that session.

set -uo pipefail

# Both spellings of the repo. A checkout reached through a symlink has two
# paths, so a process started through one spelling has argv and a cwd that the
# other one does not match.
repo_root_logical="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd "$repo_root_logical" && pwd -P)"
repo_roots="$repo_root"
if [ "$repo_root_logical" != "$repo_root" ]; then
  repo_roots="${repo_roots}
${repo_root_logical}"
fi
# `~` is resolved from the password database when HOME is unset.
home="${HOME:-$(cd ~ 2>/dev/null && pwd)}"
# `GILDE_HYGIENE_WORKTREE_ROOT` exists so `--prune-worktrees` can be exercised
# against a throwaway root instead of the developer's real one.
worktree_root="${GILDE_HYGIENE_WORKTREE_ROOT:-${home}/.gilde/worktrees}"
snapshot_dir="${home}/.claude/shell-snapshots"

# lsof is not on the default PATH of a launchd-spawned shell.
LSOF=""
for candidate in /usr/sbin/lsof "$(command -v lsof 2>/dev/null || true)"; do
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then LSOF="$candidate"; break; fi
done

# Roots that make an agent CLI *ours*. Deliberately not `$repo_root` itself: a
# developer's own `claude` session is very often sitting in the repo, and this
# script must never be the reason one dies. Ours run in a worktree or a temp
# dir; nothing else does.
tmp_roots="${worktree_root}"
[ -n "${TMPDIR:-}" ] && tmp_roots="${tmp_roots}
${TMPDIR%/}"
tmp_roots="${tmp_roots}
/private/tmp/claude-
/tmp/claude-"

# Busy-loop threshold for snapshot shells: older than this with no live child.
# Overridable only so the rule can be exercised without waiting half an hour.
STALE_SHELL_SECS="${GILDE_HYGIENE_STALE_SECS:-1800}"
MAX_ARGV=220

# ---------------------------------------------------------------------------
# small helpers
# ---------------------------------------------------------------------------

die() { echo "hygiene: $*" >&2; exit 2; }

# `~` for `$HOME`, so a line fits a terminal.
tildify() {
  case "$1" in
    "$home"/*) printf '~%s' "${1#"$home"}" ;;
    *)         printf '%s' "$1" ;;
  esac
}

# Redact anything that looks like a credential, then truncate. argv of an agent
# CLI can carry `--api-key`, and this output is meant to be pasted into a task
# report as evidence.
mask() {
  local s
  s="$(printf '%s' "$1" | sed -E \
    -e 's/(sk-[A-Za-z0-9]{4})[A-Za-z0-9_-]{8,}/\1…REDACTED/g' \
    -e 's/(-{1,2}[A-Za-z0-9-]*([Tt]oken|[Kk]ey|[Ss]ecret|[Pp]ass(word|wd)?)[A-Za-z0-9-]*[= ])[^ ]+/\1REDACTED/g' \
    -e 's/([A-Za-z_][A-Za-z0-9_]*(TOKEN|KEY|SECRET|PASSWORD|PASSWD|AUTH)[A-Za-z0-9_]*=)[^ ]+/\1REDACTED/g')"
  s="$(tildify "$s")"
  if [ "${#s}" -gt "$MAX_ARGV" ]; then
    printf '%s…(+%d chars)' "${s:0:$MAX_ARGV}" "$(( ${#s} - MAX_ARGV ))"
  else
    printf '%s' "$s"
  fi
}

# `ps -o etime` → seconds. Formats: SS, MM:SS, HH:MM:SS, D-HH:MM:SS.
etime_secs() {
  local e="$1" days=0 rest h=0 m=0 s=0
  case "$e" in
    *-*) days="${e%%-*}"; rest="${e#*-}" ;;
    *)   rest="$e" ;;
  esac
  local IFS=:
  # shellcheck disable=SC2086
  set -- $rest
  case $# in
    3) h="$1"; m="$2"; s="$3" ;;
    2) m="$1"; s="$2" ;;
    1) s="$1" ;;
  esac
  # Strip leading zeros so bash does not read them as octal.
  days=$((10#${days:-0})); h=$((10#${h:-0})); m=$((10#${m:-0})); s=$((10#${s:-0}))
  echo $(( days * 86400 + h * 3600 + m * 60 + s ))
}

# Is $1 (a path) inside any of the newline-separated prefixes in $2?
under_any() {
  local path="$1" prefixes="$2" p
  [ -n "$path" ] || return 1
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    case "$path" in
      "$p"|"$p"/*) return 0 ;;
      "$p"*)
        # `/private/tmp/claude-` is a *prefix*, not a directory.
        case "$p" in */) return 0 ;; *-) return 0 ;; esac
        ;;
    esac
  done <<EOF
$prefixes
EOF
  return 1
}

is_in_list() {
  local needle="$1" hay="$2" item
  for item in $hay; do [ "$item" = "$needle" ] && return 0; done
  return 1
}

# Does this argv name a path inside the repo (either spelling)?
argv_in_repo() {
  local argv="$1" r
  while IFS= read -r r; do
    [ -n "$r" ] || continue
    case "$argv" in *"$r"/*) return 0 ;; esac
  done <<EOF
$repo_roots
EOF
  return 1
}

# ---------------------------------------------------------------------------
# the snapshot
# ---------------------------------------------------------------------------

TMPWORK=""
cleanup() { [ -n "$TMPWORK" ] && rm -rf "$TMPWORK"; }
trap cleanup EXIT INT TERM

# Fills:
#   $SNAP      pid \t ppid \t pgid \t etime \t %cpu \t rss \t argv
#   $CWDMAP    pid \t cwd
#   PROTECTED_PIDS / PROTECTED_PGIDS  the caller and its ancestors
#   OURS_ROWS / NOT_OURS_ROWS         group \t pid \t ... \t cwd \t argv
#   OURS_PIDS
collect() {
  [ -n "$TMPWORK" ] || TMPWORK="$(mktemp -d "${TMPDIR:-/tmp}/hygiene.XXXXXX")" || die "mktemp failed"
  SNAP="$TMPWORK/ps"
  CWDMAP="$TMPWORK/cwd"

  ps -Ao pid=,ppid=,pgid=,etime=,pcpu=,rss=,args= 2>/dev/null \
    | tr '\t' ' ' \
    | awk '{ pid=$1; ppid=$2; pgid=$3; et=$4; cpu=$5; rss=$6;
             argv=""; for (i = 7; i <= NF; i++) argv = argv (i > 7 ? " " : "") $i;
             printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n", pid, ppid, pgid, et, cpu, rss, argv }' \
    > "$SNAP"
  [ -s "$SNAP" ] || die "ps produced nothing"

  # --- protected: this process and every ancestor, plus their groups --------
  PROTECTED_PIDS=""
  PROTECTED_PGIDS=""
  local walk="$$" guard=0 line ppid pgid
  while [ -n "$walk" ] && [ "$walk" != "0" ] && [ "$walk" != "1" ] && [ "$guard" -lt 64 ]; do
    line="$(awk -F'\t' -v p="$walk" '$1 == p { print; exit }' "$SNAP")"
    [ -n "$line" ] || break
    ppid="$(printf '%s' "$line" | cut -f2)"
    pgid="$(printf '%s' "$line" | cut -f3)"
    PROTECTED_PIDS="$PROTECTED_PIDS $walk"
    PROTECTED_PGIDS="$PROTECTED_PGIDS $pgid"
    walk="$ppid"
    guard=$((guard + 1))
  done
  # `$$` is the script's shell; `ps` also has the pipeline's own children.
  PROTECTED_PIDS="$PROTECTED_PIDS $$ $PPID"

  # --- pass 1: which pids are worth an lsof --------------------------------
  local candidates="" pid etime cpu rss argv first base
  while IFS="$(printf '\t')" read -r pid ppid pgid etime cpu rss argv; do
    [ -n "$pid" ] || continue
    if prescreen "$argv"; then candidates="$candidates,$pid"; fi
  done < "$SNAP"
  candidates="${candidates#,}"

  # --- pass 2: cwd of the candidates, in one lsof --------------------------
  : > "$CWDMAP"
  if [ -n "$candidates" ] && [ -n "$LSOF" ]; then
    "$LSOF" -w -a -p "$candidates" -d cwd -Fpn 2>/dev/null \
      | awk '/^p/ { pid = substr($0, 2); next }
             /^n/ { if (pid != "") { printf "%s\t%s\n", pid, substr($0, 2); pid = "" } }' \
      > "$CWDMAP"
  fi

  # --- pass 3: classify ----------------------------------------------------
  OURS_ROWS=""
  NOT_OURS_ROWS=""
  OURS_PIDS=""
  local cwd
  while IFS="$(printf '\t')" read -r pid ppid pgid etime cpu rss argv; do
    [ -n "$pid" ] || continue
    prescreen "$argv" || continue
    cwd="$(awk -F'\t' -v p="$pid" '$1 == p { print $2; exit }' "$CWDMAP")"
    classify "$pid" "$argv" "$cwd"      # sets G_GROUP, G_OURS
    [ -n "$G_GROUP" ] || continue
    local row
    row="$(printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s' \
           "$G_GROUP" "$pid" "$ppid" "$pgid" "$etime" "$cpu" "$rss" \
           "${cwd:-?}")"
    row="$row$(printf '\t%s' "$argv")"
    if [ "$G_OURS" = "1" ]; then
      OURS_ROWS="${OURS_ROWS}${row}
"
      OURS_PIDS="$OURS_PIDS $pid"
    else
      NOT_OURS_ROWS="${NOT_OURS_ROWS}${row}
"
    fi
  done < "$SNAP"
}

# Cheap argv-only test: is this pid even worth an lsof? Keep it wide — the real
# decision is `classify`, which sees the cwd.
prescreen() {
  local argv="$1" first base
  first="${argv%% *}"
  base="${first##*/}"
  case "$argv" in
    *"src-tauri/binaries/"*|*"Gilde Code.app/Contents/MacOS/gilde-node"*|*"/ao-node"*) return 0 ;;
    *"Gilde Code.app/Contents/MacOS/gilde"|*"Gilde Code.app/Contents/MacOS/gilde "*) return 0 ;;
    */target/release/gilde|*/target/release/gilde\ *) return 0 ;;
    */target/debug/gilde|*/target/debug/gilde\ *) return 0 ;;
    */target/debug/deps/*) return 0 ;;
    */fake_agent|*/fake_agent\ *) return 0 ;;
    *"$snapshot_dir"*) return 0 ;;
  esac
  case "$base" in
    claude|codex|agy|fake_agent) return 0 ;;
    cargo) case "$argv" in *" test"*) return 0 ;; esac ;;
    python|python3|python3.*) case "$argv" in *scratchpad*|*"/claude-"*) return 0 ;; esac ;;
  esac
  return 1
}

# Sets G_GROUP ("" when it is not ours and not even interesting) and G_OURS.
classify() {
  local pid="$1" argv="$2" cwd="$3" first base
  G_GROUP=""; G_OURS=0
  first="${argv%% *}"
  base="${first##*/}"

  # Never classify the caller or an ancestor as a leftover.
  if is_in_list "$pid" "$PROTECTED_PIDS"; then return 0; fi

  case "$argv" in
    *"src-tauri/binaries/"*|*"Gilde Code.app/Contents/MacOS/gilde-node"*|*"/ao-node"*)
      G_GROUP="a-node-runtime"; G_OURS=1; return 0 ;;
  esac
  case "$argv" in
    *"Gilde Code.app/Contents/MacOS/gilde"|*"Gilde Code.app/Contents/MacOS/gilde "*|\
    */target/release/gilde|*/target/release/gilde\ *|\
    */target/debug/gilde|*/target/debug/gilde\ *)
      G_GROUP="b-app"; G_OURS=1; return 0 ;;
  esac
  case "$argv" in
    */target/debug/deps/gilde_code_lib-*)
      G_GROUP="c-cargo-test"; G_OURS=1; return 0 ;;
    */target/debug/deps/*-????????????????|*/target/debug/deps/*-????????????????\ *)
      # An integration-test binary. The hash suffix alone is not proof it is
      # ours — `cargo` names every crate's test binaries this way — so the argv
      # or the cwd has to point back at this repo.
      if argv_in_repo "$argv" || under_any "$cwd" "$repo_roots"; then
        G_GROUP="c-cargo-test"; G_OURS=1; return 0
      fi
      ;;
  esac
  if [ "$base" = "cargo" ]; then
    case "$argv" in
      *" test"*)
        if under_any "$cwd" "$repo_roots"; then G_GROUP="c-cargo-test"; G_OURS=1; return 0; fi
        ;;
    esac
  fi
  case "$argv" in
    */fake_agent|*/fake_agent\ *) G_GROUP="d-fake-agent"; G_OURS=1; return 0 ;;
  esac
  if [ "$base" = "fake_agent" ]; then G_GROUP="d-fake-agent"; G_OURS=1; return 0; fi

  case "$base" in
    claude|codex|agy)
      G_GROUP="e-agent-cli"
      if under_any "$cwd" "$tmp_roots"; then G_OURS=1; else G_OURS=0; fi
      return 0 ;;
  esac

  case "$base" in
    python|python3|python3.*)
      case "$argv$cwd" in
        *scratchpad*) G_GROUP="f-python-pty"; G_OURS=1; return 0 ;;
      esac
      ;;
  esac

  case "$argv" in
    *"$snapshot_dir"*)
      # A snapshot shell is normal — every Bash tool call is one. Only a stale
      # one with no live child is a leftover: that is the shape of the `zsh`
      # load loops that survived the last two runs.
      local age kids
      age="$(etime_secs "$(awk -F'\t' -v p="$pid" '$1 == p { print $4; exit }' "$SNAP")")"
      kids="$(awk -F'\t' -v p="$pid" '$2 == p { n++ } END { print n + 0 }' "$SNAP")"
      if [ "$age" -gt "$STALE_SHELL_SECS" ] && [ "$kids" -eq 0 ]; then
        G_GROUP="g-stale-shell"; G_OURS=1
      fi
      return 0 ;;
  esac
  return 0
}

group_title() {
  if [ "${2:-ours}" = "not" ]; then
    case "$1" in
      e-agent-cli) echo "(e) agent CLIs elsewhere — VS Code's, the developer's own"; return 0 ;;
    esac
  fi
  case "$1" in
    a-node-runtime) echo "(a) gilde-node / ao-node sidecar runtimes" ;;
    b-app)          echo "(b) the app itself" ;;
    c-cargo-test)   echo "(c) cargo test harnesses and test binaries" ;;
    d-fake-agent)   echo "(d) fake_agent fixtures" ;;
    e-agent-cli)    echo "(e) agent CLIs in a worktree / test temp dir" ;;
    f-python-pty)   echo "(f) python pty harnesses under the scratchpad" ;;
    g-stale-shell)  echo "(g) stale snapshot shells (>$((STALE_SHELL_SECS / 60)) min, no live child)" ;;
    *)              echo "$1" ;;
  esac
}

print_rows() {
  local rows="$1" side="${2:-ours}" g rg pid ppid pgid etime cpu rss cwd argv
  local order="a-node-runtime b-app c-cargo-test d-fake-agent e-agent-cli f-python-pty g-stale-shell"
  for g in $order; do
    local any=0
    while IFS="$(printf '\t')" read -r rg pid ppid pgid etime cpu rss cwd argv; do
      [ -n "$rg" ] || continue
      [ "$rg" = "$g" ] || continue
      if [ "$any" = "0" ]; then
        printf '  %s\n' "$(group_title "$g" "$side")"
        printf '    %-7s %-7s %-7s %-13s %6s %9s\n' PID PPID PGID ETIME %CPU RSS
        any=1
      fi
      printf '    %-7s %-7s %-7s %-13s %6s %8sM\n' \
        "$pid" "$ppid" "$pgid" "$etime" "$cpu" "$(( (rss + 512) / 1024 ))"
      printf '        cwd  %s\n' "$(tildify "${cwd:-?}")"
      printf '        argv %s\n' "$(mask "$argv")"
    done <<EOF
$rows
EOF
    [ "$any" = "1" ] && echo ""
  done
  return 0
}

count_rows() { printf '%s\n' "$1" | awk 'NF { n++ } END { print n + 0 }'; }

# ---------------------------------------------------------------------------
# --list
# ---------------------------------------------------------------------------

do_list() {
  collect
  local n_ours n_not
  n_ours="$(count_rows "$OURS_ROWS")"
  n_not="$(count_rows "$NOT_OURS_ROWS")"

  echo "hygiene — $(date '+%Y-%m-%d %H:%M:%S')"
  echo "  repo        $(printf '%s' "$repo_roots" | tr '\n' ' ' | sed "s|$home|~|g")"
  echo "  worktrees   $(tildify "$worktree_root")"
  echo "  ours-if-cwd $(printf '%s' "$tmp_roots" | tr '\n' ' ' | sed "s|$home|~|g")"
  echo "  protected   $(printf '%s\n' $PROTECTED_PIDS | sort -un | tr '\n' ' ')(this shell and its ancestors)"
  echo ""

  echo "OURS — ${n_ours} process(es)"
  if [ "$n_ours" -eq 0 ]; then
    echo "  (none)"
    echo ""
  else
    echo ""
    print_rows "$OURS_ROWS"
  fi

  echo "NOT ours (skipped) — ${n_not} process(es)"
  if [ "$n_not" -eq 0 ]; then
    echo "  (none)"
  else
    echo "  matched by name, rejected by cwd. Never signalled."
    echo ""
    print_rows "$NOT_OURS_ROWS" not
  fi
  echo ""

  if [ "$n_ours" -eq 0 ]; then
    echo "hygiene: clean"
    return 0
  fi
  echo "hygiene: ${n_ours} process(es) belong to this project — run: bash scripts/hygiene.sh --kill"
  return 1
}

# ---------------------------------------------------------------------------
# --kill
# ---------------------------------------------------------------------------

# One signal to one pid, or to its group when the pid leads one. Never a name.
signal_one() {
  local sig="$1" pid="$2" pgid="$3"
  if is_in_list "$pid" "$PROTECTED_PIDS"; then
    echo "    skip  $pid (protected: this shell or an ancestor)"
    return 0
  fi
  if [ "$pid" = "$pgid" ] && ! is_in_list "$pgid" "$PROTECTED_PGIDS"; then
    if kill -"$sig" -- "-$pgid" 2>/dev/null; then
      echo "    $sig   group $pgid (leader $pid)"
    else
      echo "    $sig   group $pgid failed; falling back to pid $pid"
      kill -"$sig" "$pid" 2>/dev/null && echo "    $sig   pid $pid"
    fi
    return 0
  fi
  if [ "$pid" = "$pgid" ]; then
    echo "    note  pgid $pgid holds a protected process; signalling pid $pid only"
  fi
  kill -"$sig" "$pid" 2>/dev/null && echo "    $sig   pid $pid"
  return 0
}

do_kill() {
  collect
  local n_ours
  n_ours="$(count_rows "$OURS_ROWS")"
  if [ "$n_ours" -eq 0 ]; then
    echo "hygiene: nothing of ours is running; nothing to kill"
    echo ""
    do_list
    return $?
  fi

  echo "hygiene --kill — about to signal ${n_ours} process(es):"
  echo ""
  print_rows "$OURS_ROWS"

  echo "  SIGTERM:"
  local rg pid ppid pgid etime cpu rss cwd argv
  while IFS="$(printf '\t')" read -r rg pid ppid pgid etime cpu rss cwd argv; do
    [ -n "$pid" ] || continue
    signal_one TERM "$pid" "$pgid"
  done <<EOF
$OURS_ROWS
EOF

  echo "  waiting 3 s…"
  sleep 3

  echo "  SIGKILL for survivors:"
  local survivors=0
  while IFS="$(printf '\t')" read -r rg pid ppid pgid etime cpu rss cwd argv; do
    [ -n "$pid" ] || continue
    if kill -0 "$pid" 2>/dev/null; then
      survivors=$((survivors + 1))
      signal_one KILL "$pid" "$pgid"
    fi
  done <<EOF
$OURS_ROWS
EOF
  [ "$survivors" -eq 0 ] && echo "    (none: SIGTERM was enough)"

  echo "  waiting 1 s…"
  sleep 1
  echo ""
  echo "--- re-list ---"
  echo ""
  do_list
  return $?
}

# ---------------------------------------------------------------------------
# --worktrees / --prune-worktrees
# ---------------------------------------------------------------------------

# The prefixes a smoke/gui test creates. Anything else under
# `~/.gilde/worktrees/` was made by a human or by the app in normal use and is
# never touched.
smoke_prefixes="smoke- gui-smoke- nits-check- m15-check- readme-date- packaging-"

is_smoke_name() {
  local name="$1" p
  for p in $smoke_prefixes; do
    case "$name" in "$p"*) return 0 ;; esac
  done
  return 1
}

# The main repo that owns a linked worktree, via its `.git` pointer file.
owner_repo_of() {
  local wt="$1" common
  common="$(git -C "$wt" rev-parse --git-common-dir 2>/dev/null)" || return 1
  [ -n "$common" ] || return 1
  case "$common" in
    /*) : ;;
    *)  common="$(cd "$wt" && cd "$common" 2>/dev/null && pwd)" || return 1 ;;
  esac
  ( cd "$common/.." 2>/dev/null && pwd ) || return 1
}

do_worktrees() {
  local prune="${1:-0}" found=0 wt name owner dirty branch

  echo "worktrees under $(tildify "$worktree_root")"
  echo ""
  if [ ! -d "$worktree_root" ]; then
    echo "  (no $(tildify "$worktree_root"))"
  else
    for wt in "$worktree_root"/*/*; do
      [ -d "$wt" ] || continue
      found=$((found + 1))
      name="$(basename "$wt")"
      owner="$(owner_repo_of "$wt" 2>/dev/null || true)"
      branch="$(git -C "$wt" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')"
      dirty="$(git -C "$wt" status --porcelain 2>/dev/null | head -20)"
      if is_smoke_name "$name"; then
        printf '  [smoke] %s\n' "$(tildify "$wt")"
      else
        printf '  [keep ] %s\n' "$(tildify "$wt")"
      fi
      printf '          branch %s   owner %s\n' "$branch" "$(tildify "${owner:-?}")"
      if [ -n "$dirty" ]; then
        printf '          DIRTY:\n'
        printf '%s\n' "$dirty" | sed 's/^/            /'
      fi
      if [ "$prune" = "1" ] && is_smoke_name "$name"; then
        if [ -n "$dirty" ]; then
          echo "          SKIPPED by --prune-worktrees: uncommitted work above would be lost."
          echo "          Remove it by hand once you have decided what to do with it."
        elif [ -z "$owner" ]; then
          echo "          SKIPPED: cannot find the repo that owns this worktree."
        else
          echo "          removing…"
          if git -C "$owner" worktree remove "$wt" 2>&1 | sed 's/^/            /'; then
            echo "            removed"
          fi
          if [ "$branch" != "?" ] && [ "$branch" != "HEAD" ] && is_smoke_name "${branch##*/}"; then
            git -C "$owner" branch -D "$branch" 2>&1 | sed 's/^/            /'
          fi
          git -C "$owner" worktree prune 2>/dev/null
        fi
      fi
      echo ""
    done
    [ "$found" -eq 0 ] && echo "  (none)"
  fi

  echo ""
  echo "git worktree list — $(tildify "$repo_root")"
  git -C "$repo_root" worktree list 2>/dev/null | sed 's/^/  /' || echo "  (not a git repo)"

  echo ""
  echo "smoke-shaped branches in $(tildify "$repo_root")"
  local branches p pat=""
  for p in $smoke_prefixes; do pat="${pat}${pat:+|}^(gilde/)?${p}"; done
  branches="$(git -C "$repo_root" for-each-ref --format='%(refname:short)' refs/heads 2>/dev/null \
              | grep -E "$pat" || true)"
  if [ -z "$branches" ]; then
    echo "  (none)"
  else
    printf '%s\n' "$branches" | sed 's/^/  /'
    if [ "$prune" = "1" ]; then
      echo "  deleting:"
      printf '%s\n' "$branches" | while IFS= read -r b; do
        [ -n "$b" ] || continue
        git -C "$repo_root" branch -D "$b" 2>&1 | sed 's/^/    /'
      done
    fi
  fi
  echo ""
  if [ "$prune" != "1" ]; then
    echo "hygiene: nothing was removed. Run --prune-worktrees to remove the [smoke] entries."
  fi
  return 0
}

# ---------------------------------------------------------------------------
# --memory
# ---------------------------------------------------------------------------

do_memory() {
  local page_size
  page_size="$(vm_stat 2>/dev/null | sed -n '1s/.*page size of \([0-9]*\) bytes.*/\1/p')"
  [ -n "$page_size" ] || page_size=4096

  echo "memory — $(date '+%Y-%m-%d %H:%M:%S')"
  echo ""
  vm_stat 2>/dev/null | awk -v ps="$page_size" '
    /Pages free/                 { free = $3 }
    /Pages active/               { active = $3 }
    /Pages inactive/             { inactive = $3 }
    /Pages wired down/           { wired = $4 }
    /Pages occupied by compressor/ { comp = $5 }
    END {
      gsub(/\./, "", free); gsub(/\./, "", active); gsub(/\./, "", inactive)
      gsub(/\./, "", wired); gsub(/\./, "", comp)
      mb = ps / 1048576
      printf "  vm_stat     free %7.0f MB   active %7.0f MB   inactive %7.0f MB\n", free*mb, active*mb, inactive*mb
      printf "              wired %6.0f MB   compressed %5.0f MB\n", wired*mb, comp*mb
    }'
  echo "  swap        $(sysctl -n vm.swapusage 2>/dev/null)"
  echo "  pressure    $(memory_pressure 2>/dev/null | tail -1)"
  echo ""

  echo "  top 10 by RSS"
  printf '    %9s  %-7s %s\n' RSS PID COMMAND
  ps -Ao rss=,pid=,comm= 2>/dev/null | sort -rn | head -10 \
    | while read -r rss pid comm; do
        printf '    %8sM  %-7s %s\n' "$(( (rss + 512) / 1024 ))" "$pid" "$(tildify "$comm")"
      done
  echo ""

  collect
  local total=0 rg pid ppid pgid etime cpu rss cwd argv
  while IFS="$(printf '\t')" read -r rg pid ppid pgid etime cpu rss cwd argv; do
    [ -n "$rss" ] || continue
    total=$((total + rss))
  done <<EOF
$OURS_ROWS
EOF
  echo "  this project's groups: $(count_rows "$OURS_ROWS") process(es), $(( (total + 512) / 1024 )) MB RSS"
  if [ "$(count_rows "$OURS_ROWS")" -gt 0 ]; then
    echo "  → bash scripts/hygiene.sh --list   (then --kill)"
  fi
  return 0
}

# ---------------------------------------------------------------------------

usage() {
  # The header comment above is the manual; print it rather than repeat it.
  awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"
}

case "${1:---list}" in
  --list|-l|"")       do_list ;;
  --kill|-k)          do_kill ;;
  --worktrees)        do_worktrees 0 ;;
  --prune-worktrees)  do_worktrees 1 ;;
  --memory|-m)        do_memory ;;
  --help|-h)          usage ;;
  *)                  echo "hygiene: unknown option $1" >&2; usage >&2; exit 2 ;;
esac
