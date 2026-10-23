#!/usr/bin/env bash
# run.sh — the G6 probe kit (contract §14.5, A31.3). Runs every verification
# probe — the §14.5 table (SP-*, HP-*) and the lead's own probes (LV-1…LV-6,
# verify_lead) — against the integrated core with fakes only and prints just
# the `PROBE <id>: …` evidence lines plus a pass/fail summary, for the lead's
# VERIFICATION record.
#
#   scripts/verify/run.sh            every probe (HP-5 scale is skipped)
#   scripts/verify/run.sh --scale    also HP-5 (#[ignore]d; builds 37 k files,
#                                    takes minutes — run it at a quiet moment)
#   scripts/verify/run.sh --keep     keep the full logs (path printed at the end)
#
# What it runs (see scripts/verify/README.md for the mapping table):
#   1. the probe tests  src-tauri/tests/verify_*.rs  with --nocapture;
#   2. the front-end probes  src/verify/*.test.tsx  (test:fe filtered);
#   3. the existing tests each probe is mapped to, by exact name — one
#      `PROBE <id>: mapped <binary>::<test> → passed|FAILED` line each.
# A variant whose observed result differs from §14.5 (or from the LV table in
# verify_lead.rs) prints `MISMATCH(...)` inside its PROBE line: a finding for
# the lead, not a failure of the kit.
#
# Environment: every cargo command runs under scripts/gates/hermetic-cargo.sh
# (A38.1: a fresh HOME and GILDE_HOME, a PATH with no vendor CLI,
# AGENT_CLI_CREDENTIAL_STORE=memory, DISABLE_AUTOUPDATER=1), with
# CARGO_INCREMENTAL=0 and the shared build lane (CARGO_TARGET_DIR, default:
# <worktree>/../.target when it exists). No vendor CLI, no real ~/.claude*,
# no keychain, no real tmux, no notification, no app launch. Exit 0 = every
# probe line present and every test passed (mismatches are reported, not
# failed).

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCALE=0
KEEP=0
for a in "$@"; do
  case "$a" in
    --scale) SCALE=1 ;;
    --keep) KEEP=1 ;;
    -h|--help) sed -n '2,29p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "run.sh: unknown argument: $a" >&2; exit 2 ;;
  esac
done

# The PaneHost probes and rigs put Unix sockets under the temp dir; macOS caps
# a socket path at 104 bytes, so a long inherited TMPDIR breaks them.
if [ "${#TMPDIR}" -gt 60 ]; then
  echo "run.sh: TMPDIR is ${#TMPDIR} chars (socket paths would be too long); using /tmp" >&2
  export TMPDIR=/tmp
fi
WORK="$(mktemp -d "${TMPDIR:-/tmp}/gilde-verify.XXXXXX")"
cleanup() {
  if [ "$KEEP" = 1 ]; then echo "logs kept in $WORK"; else rm -rf "$WORK"; fi
}
trap cleanup EXIT
mkdir -p "$WORK/gilde-home"

export AGENT_CLI_CREDENTIAL_STORE=memory
export DISABLE_AUTOUPDATER=1
export GILDE_HOME="$WORK/gilde-home"
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_PROFILE_TEST_DEBUG=line-tables-only
if [ -z "${CARGO_TARGET_DIR:-}" ] && [ -d "$ROOT/../.target" ]; then
  CARGO_TARGET_DIR="$(cd "$ROOT/../.target" && pwd)"
fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/src-tauri/target}"
MANIFEST="$ROOT/src-tauri/Cargo.toml"
# A38.1: cargo never sees the caller's HOME or PATH (the test harness refuses
# to start when a vendor CLI is reachable from them).
cargo() { "$ROOT/scripts/gates/hermetic-cargo.sh" "$@"; }
PROBES="$WORK/probes.txt"
: > "$PROBES"
FAILS=0

note() { echo "run.sh: $*" >&2; }

# The PROBE lines of a log. libtest prints `test <name> ... ` before a test's
# own output (one test thread), so a line is taken from wherever it starts.
probe_lines() { grep -oE 'PROBE (SP|HP|LV)-[0-9]+: .*' "$1" || true; }

# Failed test names of one libtest log ("test <name> ... FAILED").
failed_in() { grep -E '^test .* \.\.\. FAILED$' "$1" | sed -E 's/^test (.*) \.\.\. FAILED$/\1/'; }

# A cargo run that did not even report a result is a failure of its own.
check_ran() {
  local log="$1" what="$2"
  if ! grep -q '^test result:' "$log"; then
    note "$what did not run (build error?) — see the log"
    tail -20 "$log" >&2
    FAILS=$((FAILS + 1))
    return 1
  fi
  return 0
}

# -- 1. the probe tests ---------------------------------------------------------
VERIFY_BINS=(verify_ipc verify_world verify_convo verify_shim verify_privacy verify_discovery verify_lead)
args=()
for b in "${VERIFY_BINS[@]}"; do args+=(--test "$b"); done
note "probe tests: ${VERIFY_BINS[*]}"
cargo test --manifest-path "$MANIFEST" --no-fail-fast "${args[@]}" -- --nocapture --test-threads=1 > "$WORK/verify.log" 2>&1
if check_ran "$WORK/verify.log" "the probe tests"; then
  probe_lines "$WORK/verify.log" >> "$PROBES"
  while IFS= read -r t; do
    [ -n "$t" ] || continue
    note "FAILED probe test: $t"
    FAILS=$((FAILS + 1))
  done < <(failed_in "$WORK/verify.log")
fi

if [ "$SCALE" = 1 ]; then
  note "HP-5 scale probe (ignored test, minutes)"
  cargo test --manifest-path "$MANIFEST" --test verify_scale -- --ignored --nocapture --test-threads=1 > "$WORK/scale.log" 2>&1
  if check_ran "$WORK/scale.log" "the HP-5 scale probe"; then
    probe_lines "$WORK/scale.log" >> "$PROBES"
    while IFS= read -r t; do
      [ -n "$t" ] || continue
      note "FAILED probe test: $t"
      FAILS=$((FAILS + 1))
    done < <(failed_in "$WORK/scale.log")
  fi
else
  echo "PROBE HP-5: skipped (the #[ignore]d scale probe verify_scale; run scripts/verify/run.sh --scale at a quiet moment)" >> "$PROBES"
fi

# -- 2. the front-end probes ----------------------------------------------------
note "front-end probes: src/verify"
( cd "$ROOT" && LANG=ko_KR.UTF-8 node src/test/run.mjs --filter=src/verify ) > "$WORK/fe.log" 2>&1
fe_status=$?
probe_lines "$WORK/fe.log" >> "$PROBES"
if [ "$fe_status" -ne 0 ]; then
  note "front-end probes failed:"
  grep -E 'failed|FAIL|assertion' "$WORK/fe.log" | head -20 >&2
  FAILS=$((FAILS + 1))
fi

# -- 3. the mapped existing tests -----------------------------------------------
# id|binary (an integration test file stem, or "lib")|exact test name
MAPPED='
SP-2|lib|convo::approvals::tests::card_fields_follow_the_table
SP-2|render_sanitize|t_render_sanitize_known_attacks
SP-3|ide_paths|t_ide_paths_relative_absolute_and_dot_dot
SP-3|ide_paths|t_ide_paths_symlinks_out_are_refused_symlinks_in_are_followed
SP-3|ide_paths|t_ide_paths_vendor_roots_are_refused_even_inside_an_allowed_root
SP-3|ide_paths|t_ide_paths_case_variants_and_unicode_forms
SP-3|ide_paths|ipc::link_open_ide_open_and_project_pick_through_the_commands
SP-4|panehost_host|t_shim_auth_refusals_are_counted_never_deviations
SP-4|panehost_sequence|t_shim_auth_a_token_bearing_grandchild_cannot_mutate
SP-5|ipc_contract|t_ipc_1_value_rules_report_field_and_rule
SP-6|ipc_contract|t_contract_non_manifest_commands_are_rejected
SP-7|discovery_privacy|t_priv_never_open_every_listed_path_is_refused_before_open
SP-7|discovery_privacy|t_priv_2_never_write_any_vendor_root_including_a_sibling
SP-7|discovery_vendors|cursor_store_db_is_never_opened
SP-8|follow_tail|t_follow_large
SP-8|convo_l1|oversized_and_deep_lines_are_skipped_and_the_stream_goes_on
HP-1|discovery_registry|t_disc_liveness_each_check_fails_alone
HP-1|discovery_teams|t_team_torn_config_keeps_the_last_good_roster
HP-1|world_derive|t_derive_stale_rows
HP-2|lib|convo::frames::tests::answer_outcomes_follow_c12
HP-2|lib|convo::frames::tests::misfits_and_hidden_button_two
HP-3|panehost_host|t_shim_respawn_input_rules
HP-3|panehost_host|t_shim_16th_pane_is_refused_and_ids_are_never_reused
HP-4|sessions_manager|t_sessions_live_elsewhere
HP-4|sessions_manager|t_sessions_turn_send_idempotent_and_watched_refused
HP-4|sessions_manager|t_sessions_answer_refusals_and_outcomes
HP-5|discovery_registry|t_disc_unknown_format_and_caps
HP-5|discovery_watchscope|session_env_and_ide_are_never_watched
HP-6|notify_policy|hp6_an_8h_jump_produces_no_notification_burst
HP-6|world_derive|t_derive_sleep_does_not_fire_timers
HP-7|discovery_engine|t_disc_permission_denied_keeps_facts
LV-1|lib|discovery::safe_read::tests::symlinks_fifos_and_escapes_are_refused
LV-1|lib|discovery::safe_read::tests::hard_links_are_refused_after_open
LV-1|a29_history|a_relative_path_from_the_index_is_never_opened
LV-2|recovery|a29::one_token_cancels_every_grouped_stop_and_clears_the_window
LV-3|world_message_bodies|wired::t_wiring_a_message_older_than_the_store_is_found
LV-4|ipc_hardening|a45_1_the_confirmation_names_and_applies_the_canonical_path
LV-5|ipc_hardening|g7c_15_agents_rescan_is_rate_limited
LV-5|ipc_hardening|a45_3_twenty_rescans_while_one_runs_are_one_detection
LV-6|sessions_live|g7_02_deny_after_allow_reaches_the_next_prompt
'
bins=$(printf '%s\n' "$MAPPED" | awk -F'|' 'NF==3 {print $2}' | sort -u)
for bin in $bins; do
  names=$(printf '%s\n' "$MAPPED" | awk -F'|' -v b="$bin" 'NF==3 && $2==b {print $3}')
  if [ "$bin" = lib ]; then target=(--lib); else target=(--test "$bin"); fi
  # shellcheck disable=SC2086
  cargo test --manifest-path "$MANIFEST" --no-fail-fast "${target[@]}" -- --exact --test-threads=1 $names > "$WORK/mapped-$bin.log" 2>&1
  check_ran "$WORK/mapped-$bin.log" "mapped tests of $bin" || continue
  while IFS='|' read -r id b name; do
    [ "$b" = "$bin" ] || continue
    if grep -qE "^test ${name} \.\.\. ok$" "$WORK/mapped-$bin.log"; then
      echo "PROBE $id: mapped $bin::$name → passed" >> "$PROBES"
    elif grep -qE "^test ${name} \.\.\. FAILED$" "$WORK/mapped-$bin.log"; then
      echo "PROBE $id: mapped $bin::$name → FAILED" >> "$PROBES"
      FAILS=$((FAILS + 1))
    else
      echo "PROBE $id: mapped $bin::$name → NOT RUN (name not found)" >> "$PROBES"
      FAILS=$((FAILS + 1))
    fi
  done < <(printf '%s\n' "$MAPPED" | awk 'NF')
done
# The front-end tests SP-1 is also mapped to.
( cd "$ROOT" && LANG=ko_KR.UTF-8 node src/test/run.mjs --filter=src/markdown/markdown.test.tsx --grep=T-ui-renderer --grep='links: javascript:' ) > "$WORK/fe-mapped.log" 2>&1
if [ $? -eq 0 ] && grep -q ' passed, 0 failed' "$WORK/fe-mapped.log"; then
  echo "PROBE SP-1: mapped src/markdown/markdown.test.tsx 'T-ui-renderer…' + 'links: javascript:, data:…' → passed" >> "$PROBES"
else
  echo "PROBE SP-1: mapped src/markdown/markdown.test.tsx → FAILED" >> "$PROBES"
  FAILS=$((FAILS + 1))
fi

# -- the evidence and the summary -----------------------------------------------
sort -t: -k1,1 -s "$PROBES"
echo
IDS=(SP-1 SP-2 SP-3 SP-4 SP-5 SP-6 SP-7 SP-8 HP-1 HP-2 HP-3 HP-4 HP-5 HP-6 HP-7 LV-1 LV-2 LV-3 LV-4 LV-5 LV-6)
missing=()
for id in "${IDS[@]}"; do
  if ! grep -qE "^PROBE ${id}: " "$PROBES"; then missing+=("$id"); fi
done
lines=$(wc -l < "$PROBES" | tr -d ' ')
mismatches=$(grep -c 'MISMATCH(' "$PROBES" || true)
mismatch_ids=$(grep 'MISMATCH(' "$PROBES" | sed -E 's/^PROBE ([A-Z]+-[0-9]+):.*/\1/' | sort -u | tr '\n' ' ')
echo "summary: ${lines} PROBE lines; $((${#IDS[@]} - ${#missing[@]}))/${#IDS[@]} probes reported; ${FAILS} failure(s); ${mismatches} line(s) with MISMATCH${mismatch_ids:+ (${mismatch_ids% })}"
if [ "${#missing[@]}" -gt 0 ]; then
  echo "missing: ${missing[*]}"
fi
if [ "$FAILS" -eq 0 ] && [ "${#missing[@]}" -eq 0 ]; then
  echo "result: PASS (mismatches are findings for the VERIFICATION record, not failures)"
  exit 0
fi
echo "result: FAIL"
exit 1
