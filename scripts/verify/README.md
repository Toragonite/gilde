# The G6 probe kit (contract §14.5, A31.3)

The lead runs every §14.5 verification probe (SP-1…SP-8, HP-1…HP-7) and the
lead's own probes (LV-1…LV-6, beyond the §14.5 table) against the integrated
core with fakes only, and records one observed line per probe in the
VERIFICATION record. One command does it:

```sh
scripts/verify/run.sh            # every probe; HP-5 (scale) is skipped
scripts/verify/run.sh --scale    # also HP-5 — minutes, at a quiet moment
scripts/verify/run.sh --keep     # keep the full logs (path printed)
```

It prints only `PROBE <id>: <observed>` lines and a summary. Every cargo
command runs under `scripts/gates/hermetic-cargo.sh` (A38.1: a fresh `HOME`
and `GILDE_HOME`, a `PATH` with no vendor CLI,
`AGENT_CLI_CREDENTIAL_STORE=memory`, `DISABLE_AUTOUPDATER=1`),
`CARGO_INCREMENTAL=0`, on the shared build lane (`CARGO_TARGET_DIR`, default
`<worktree>/../.target`). Nothing runs a vendor CLI, reads the real
`~/.claude*`, the keychain or a real tmux, posts a notification or launches
the app.

## How to read a PROBE line

- Each probe test prints exactly one line: its variants in order,
  `variant → observed`, separated by `;`. A failing test still prints its line
  (`FAILED after: …`).
- `MISMATCH(§14.5 expects …)` marks a variant whose observed result differs
  from the §14.5 "Expected" column (for LV-*, from the LV table below). The kit records it and does **not** assert
  it: it is a finding for the lead (no product code was changed for it). The
  other variants of that probe are asserted.
- `PROBE <id>: mapped <binary>::<test> → passed` lines come from the existing
  tests the probe is mapped to (run by exact name). They are supporting
  evidence; the probe test is the integrated, multi-variant check.
- SP-8 has two probe lines: the stream half (`verify_convo`) and the
  transcript half (`verify_world`).

## Where each probe lives

| Probe test file | Probes | Drives |
|---|---|---|
| `src/verify/sp1_markdown.test.tsx` | SP-1 | the real markdown renderer in the shared DOM; the showcase command backend records every command |
| `src/verify/hp7_health_lines.test.tsx` | HP-7 (UI half; a second `PROBE HP-7:` line) | the real Health panel over a store holding World's denial notices (A38.8) |
| `src-tauri/tests/verify_convo.rs` | SP-2, HP-2, SP-8 (stream) | the real Conversation driver against `fake_claude` over pipes; cards teed into a real World; `interaction_detail` |
| `src-tauri/tests/verify_ipc.rs` | SP-3, SP-5, HP-4 (and the A38.9 `continue_all{uids: []}` test) | the real IPC (Tauri mock runtime, generated context = the shipped ACL, real registry); SP-5/HP-4 over the whole composition root (`lifecycle::wiring`), SP-3 over a real World with a recording launcher |
| `src-tauri/tests/verify_shim.rs` | SP-4, HP-3 | one real PaneHost with two live leads; the real `gilde-tmux` shim over the real socket, and `PaneHost::handle` where argv cannot carry the input |
| `src-tauri/tests/verify_privacy.rs` | SP-7 (+ G7-02 hard links), HP-7 | the composition root over a hermetic vendor tree (chmod 000 files, mid-run chmod 000 dirs, hard links to never-open files); an Engine reconcile on the test thread for the read counters |
| `src-tauri/tests/verify_discovery.rs` | HP-1 | the composition root over torn and lying vendor files with a fake process table |
| `src-tauri/tests/verify_scale.rs` | HP-5 (`#[ignore]`) | 2,000 registry files, 10 k transcripts, 25 k `session-env` dirs in a temp root |
| `src-tauri/tests/verify_world.rs` | SP-8 (transcript), HP-6 | a real World with manual clocks, the real ContentHub/Follower, the real NotifyService with fake sinks; a wake through the composition root |
| `src-tauri/tests/verify_lead.rs` | LV-1…LV-6 | the composition root and its real IPC for every probe (LV-4: the real settings commands with a recording confirmation); fake processes (`/bin/sh` signal loggers registered with the supervisor), a fake claude TUI in a real PTY, a counting fake claude for detection |
| `src-tauri/tests/fixtures/verify/mod.rs` | — | the shared harness: `Probe`, the IPC helpers, `with_wired` (`Ctx::call_raw` for a streaming command, `Ctx::another_app` for a second app — its own rate limiter — over the same core), the process-wide log capture |

## The mapping table

Each row: the §14.5 input and expectation, the probe test that drives it, and
the existing tests that already cover part of it (with the assertion that
proves it). "Gap closed" says what the probe adds over the existing tests.

| # | Input → expected (§14.5) | Probe test | Existing tests (assertion) | Gap closed by the probe |
|---|---|---|---|---|
| SP-1 | markdown with `<img onerror>`, `<script>`, a `javascript:` link, a `data:` image → rendered as text/inert; no navigation | `src/verify/sp1_markdown.test.tsx` "SP-1 probe: hostile markdown renders inert and never navigates" | `src/markdown/markdown.test.tsx` "T-ui-renderer: random markdown and hostile HTML…" (L201 `assert(problems.length === 0, …)`), "links: javascript:, data:, file: and userinfo destinations are plain text" (L275 `querySelectorAll('[role="link"]').length, 0`) | deterministic `<img onerror>` and `<script>` (block and inline), `data:image/png` and `data:image/svg+xml` images, a `javascript:` reference link; clicks/Enter on every inert link send no command and do not navigate |
| SP-2 | a `can_use_tool` with ANSI and bidi in `decision_reason` and `mcp_server.name` → stripped before display | `verify_convo::probe_sp2_hostile_card_text_is_stripped_before_display` | lib `convo::approvals::tests::card_fields_follow_the_table` (`src-tauri/src/convo/approvals.rs:904` `…detail_text("matched ask rule")), "ANSI stripped"`); `render_sanitize::t_render_sanitize_known_attacks` (`tests/render_sanitize.rs:179` RLO case) | bidi in `decision_reason`; ANSI and bidi in `mcp_server.name`; end to end through `fake_claude`, World's copy and `interaction_detail` |
| SP-3 | `ide_open` with `../../etc/passwd`, a symlink out of the root, a path under `~/.claude`, a case-variant path → `REFUSED{outsideRoots}` | `verify_ipc::probe_sp3_ide_open_stays_inside_the_roots` | `ide_paths::t_ide_paths_relative_absolute_and_dot_dot` (L253–254 `refused(…)`, helper L233), `::t_ide_paths_symlinks_out_are_refused_symlinks_in_are_followed` (L272), `::t_ide_paths_vendor_roots_are_refused_even_inside_an_allowed_root` (L309), `::t_ide_paths_case_variants_and_unicode_forms` (L286, L289; case-insensitive volumes only), `::ipc::link_open_ide_open_and_project_pick_through_the_commands` (L722, `/etc/hosts` over IPC) | the literal probe inputs over the real IPC with a real World; File and Diff (two operations); the launcher is never called; an escape is `REFUSED` whether or not it exists, an existing and a missing outside file answer the same, an inside missing name (also a literal `~`) is `NOT_FOUND{file}` (A33.1) |
| SP-4 | a shim request with a foreign token, another lead's pid, a non-lead pid → `not authorized`, counted, logged; nothing spawned | `verify_shim::probe_sp4_shim_auth_refusals` | `panehost_host::t_shim_auth_refusals_are_counted_never_deviations` (L63 `refusals(Refusal::Token), 1`, L84 `refusals(Refusal::NotParent), 1`); `panehost_sequence::t_shim_auth_a_token_bearing_grandchild_cannot_mutate` (L159 `open_count(), 0, "nothing spawned"`) | two real live leads; every variant over the real socket; the `panehost::security` log line asserted; `open_count` after every refusal; a malformed token on the raw socket and an over-long line are counted with the auth refusals and logged (A33.7) |
| SP-5 | `settings_set` with `agents.claude.binary`; `settings_set_executable` with `envPassthrough=["TMUX"]` → `INVALID_ARGUMENT` / rejected | `verify_ipc::probe_sp5_settings_classes` | `ipc_contract::t_ipc_1_value_rules_report_field_and_rule` (case L422, assertion L434 `assert_ipc_error(&r, "INVALID_ARGUMENT", …)`) | `settings_set` with the executable key (flat and nested) through the composition root; a second reserved name in position 1; the config file unchanged |
| SP-6 | a WebView `invoke` of a deleted command (`session_open`, `dispatch_create`, `graph_snapshot`) → not invocable | — (covered) | `ipc_contract::t_contract_non_manifest_commands_are_rejected` (L197 the list incl. `session_open`, `dispatch_create`, `graph_snapshot`, `no_such_command`; L199 `expect_err("… must not be invocable")`; L200–203 "rejected by the ACL before any command ran") | — |
| SP-7 | a fixture tree with `*.key`, `ide/*.lock`, `.credentials.json`, cursor `store.db`, `history.jsonl` (all chmod 000) → never opened (no `EACCES` in logs) | `verify_privacy::probe_sp7_never_open_files_are_never_opened` | `discovery_privacy::t_priv_never_open_every_listed_path_is_refused_before_open` (L60 `s.eacces, 0`), `::t_priv_2_never_write_any_vendor_root_including_a_sibling` (L119 `safe_read::stats().eacces, 0`); `discovery_vendors::cursor_store_db_is_never_opened` (L132) | all five kinds together in one integrated pass (default and sibling config dir), the discovery thread's own counters, the log capture, the follow opener |
| SP-8 | a 2 MiB transcript line and a 70 MiB stream frame → skipped with `RowTooLarge`; UI alive | `verify_convo::probe_sp8_oversized_stream_frames_are_skipped_and_the_stream_goes_on`; `verify_world::probe_sp8_transcript_rows_over_the_cap` | `follow_tail::t_follow_large` (L182 `Line::TooLarge { start: big_at }`, 4.5 MiB line); `convo_l1::oversized_and_deep_lines_are_skipped_and_the_stream_goes_on` (L795 `SystemNote::RowTooLarge`; 3 MiB against a 1 MiB test cap) | the real stream cap (64 MiB) with 70 MiB, exactly-at-cap and cap+1; transcript lines of 2 MiB, =cap, cap+1, 6 MiB through the ContentHub; reading continues after each row (the next row arrives in the same read); exactly 4 MiB is parsed, 4 MiB + 1 is `RowTooLarge` (A33.2: §14.5's 2 MiB is an erratum) |
| HP-1 | torn and lying vendor files → hidden/stale/counted; state from the next source; no crash | `verify_discovery::probe_hp1_torn_and_lying_vendor_files` (≈ 20 s since T35) | `discovery_registry::t_disc_liveness_each_check_fails_alone` (`StaleWhy::PidMismatch` with either pid alive), `::t_disc_pid_mismatch_is_stale_whatever_runs` (A33.5(a): `registryStale`, `hiddenStale`, no session id); `world::derive::tests::a33_5b_old_busy_without_newer_rows_is_no_progress` (A33.5(b)); `discovery_inbox::*` and `world::teams::claude::tests::a33_5d_a_10k_inbox_is_bounded_per_cycle` (A33.5(d) counters); `discovery_teams::t_team_torn_config_keeps_the_last_good_roster`; `world_derive::t_derive_stale_rows` | every variant through the composition root and World (hidden in the store, counted); `procStart` both directions; the 2-day-old `busy` with and without transcript rows; a 10 k-entry inbox (and one over the 4 MiB cap) paged over IPC; a `deleted` task file for an unknown id; A33.5(a)(b)(d) asserted with structural counters (findings 6, 7, 9 fixed by T35); (c) `team_messages_page` reads World, fixed by T34 |
| HP-2 | protocol misuse by `fake_claude` → dedupe; ignore; button 2 hidden; `Unknown` mapped; line dropped with a notice | `verify_convo::probe_hp2_protocol_misuse_by_fake_claude` | lib `convo::frames::tests::answer_outcomes_follow_c12` (`src-tauri/src/convo/frames.rs:2398` `pending_cards().len(), 1`; L2423 `ghost` cancel), `convo::frames::tests::misfits_and_hidden_button_two` (L2446 `allowSessionHidden`) | all five variants end to end through `fake_claude`; the 300 Mi-char line at the real cap (streamed); absent and empty suggestions; an unknown `permissionMode` makes the driver's mode unknown (`None`), never the previous one (A33.3); the over-cap `System{RowTooLarge}` block is the notice (A33.3) |
| HP-3 | shim grammar abuse → exit 1 with the exact stderr; nothing spawned | `verify_shim::probe_hp3_shim_grammar_abuse` | `panehost_host::t_shim_respawn_input_rules` (L151 NUL → `"gilde-tmux: bad request\n"`, L155 `"can't find pane: %999999999\n"`, L156 `open_count(), 0`); `::t_shim_16th_pane_is_refused_and_ids_are_never_reused` (L111 `texts::NO_SPACE`) | `split-window`/`respawn-pane`/`kill-pane` on another live lead's pane; NUL in two positions and as raw socket JSON; the 16th pane by `split-window` and `new-window`; no spawn after each |
| HP-4 | two-writer attempts → `SESSION_LIVE_ELSEWHERE` / `UNSUPPORTED` / `REFUSED{watching}` | `verify_ipc::probe_hp4_two_writers_are_refused` | `sessions_manager::t_sessions_live_elsewhere` (L1363 `ErrorCode::SessionLiveElsewhere`), `::t_sessions_turn_send_idempotent_and_watched_refused` (L1680 `Unsupported, Some(("cap", "send"))`), `::t_sessions_answer_refusals_and_outcomes` (L1776 `Refused, Some(("reason", "watching"))`) | a real VS Code session (registry `claude-vscode` + a live process) through the composition root and the real IPC; two `turn_send` priorities; `pty_write`; allow and deny on the synthesized watched card; nothing spawned |
| HP-5 | 2,000 registry files, 10 k transcripts, 25 k `session-env` dirs → first rows ≤ 2 s; idle ≤ 1 %; caps reported | `verify_scale::probe_hp5_resource_exhaustion` (`#[ignore]`, `run.sh --scale`) | `discovery_registry::t_disc_unknown_format_and_caps` (L134 `files.len(), SESSIONS_ENTRIES_CAP`); `discovery_watchscope::session_env_and_ide_are_never_watched` (L30 no recursive watch over `session-env`) | the three sizes together in one tree; the History first page over IPC and Home's live rows timed; idle CPU of the process and of the engine; the caps exceeded on purpose (4,200 registry files, 20,001 transcripts) and reported as `truncated`; **finding 10** |
| HP-6 | advance the fake uptime/wall clocks 8 h between ticks → no No-progress storm; no notification burst; reconcile ran | `verify_world::probe_hp6_clock_jumps_and_sleep` | `notify_policy::hp6_an_8h_jump_produces_no_notification_burst` (L677 `now.len(), 1`); `world_derive::t_derive_sleep_does_not_fire_timers` (L597 `ActivityState::Working`) | World → the real NotifyService with one clock: a sleep (wall only) and a jump of both clocks; the wake through the composition root runs a reconcile (counted against a control) |
| HP-7 | `chmod 000` a project dir and a config dir mid-run → source `PermissionDenied`; sessions keep their last state with "likely"; one notice; no crash | `verify_privacy::probe_hp7_tcc_denial_mid_run_keeps_sessions`; `src/verify/hp7_health_lines.test.tsx` (UI) | `discovery_engine::t_disc_permission_denied_keeps_facts` (`<cfg>/sessions`); `discovery_denied::*` (config root with a team, project dir, transcript file, first sight while denied, recovery); `world::graph::tests::a33_4_*` (one notice + category lines from `Sources`) | a transcript file, a project transcripts dir, `<cfg>/sessions` and the whole config dir, each mid-run through World, all asserted (A33.4): `PermissionDenied`, the session's state kept with `Likely` + `evidence.cantRead`, exactly one `notice.cantReadFolders` and a `health.folderDenied{category}` line, no notification, everything cleared after the restore (finding 4 fixed by T35) |
| SP-7 (G7-02) | a hard link with an innocent name to `.credentials.json` / `*.key` → refused, its bytes never returned (A34.3); prints a second `PROBE SP-7:` line | `verify_privacy::probe_g7_02_hard_links_to_never_open_files_are_refused` | lib `discovery::safe_read::tests::hard_links_are_refused_after_open` | both reviewer variants against `safe_read`, and through the integrated core (a linked registry record and a linked team config never read, the canary in no log line) |

## The lead's own probes (LV-1…LV-6)

Contract-violating inputs beyond the §14.5 table, designed by the lead
(T45). Same rules: one `PROBE LV-n:` line each, every variant asserted,
a differing variant recorded as `MISMATCH(...)`.

| # | Input → expected | Probe test | Existing tests (assertion) | Gap closed by the probe |
|---|---|---|---|---|
| LV-1 | `history_row` and `convo_subscribe`'s history fallback (A29.2) for a uid whose indexed transcript was swapped for a symlink pointing out of the vendor root; variants: a hard link to a never-open file, the project dir swapped for a symlink out → refused or unavailable; nothing outside the root is opened | `verify_lead::probe_lv1_a_swapped_transcript_never_leads_a_read_out_of_the_root` | lib `discovery::safe_read::tests::symlinks_fifos_and_escapes_are_refused`, `::hard_links_are_refused_after_open` (`safe_read` alone); `a29_history::a_relative_path_from_the_index_is_never_opened` (the resolver only) | the swap **after** the stat pass indexed the file (the TOCTOU case), over the real IPC; History's Read goes through the hub's follower, which does not open through `safe_read`; every target carries a canary in transcript-shaped rows; `safe_read`'s open counter on the test thread, the follower's followed list, a second view on the same hub, World's store and the log capture; a re-index after the swap. Findings LV-1(b)/(c) on `aa718ea`, ruled A50.1 and fixed by T46: now asserted (`UNSUPPORTED{noReadableConversation}`, nothing followed, no canary) |
| LV-2 | `undo_cancel` with the A29.3 recovery group token (a) after the window expired, (b) twice; variant: a token of another group → `NOT_FOUND{token}`; no signal is sent | `verify_lead::probe_lv2_a_stale_or_foreign_undo_token_is_not_found_and_signals_nothing` | `recovery::a29::one_token_cancels_every_grouped_stop_and_clears_the_window` (one token cancels the whole group; rusty-fork) | over the real IPC with real `StopGroup`s on the wired supervisor; the members are supervisor-registered `/bin/sh` fakes that log every signal they get; a late undo after the stop fired, a second undo, another group's token (fired, and cancelled) while a third group is pending, a forged well-formed token; the pending group's token is untouched (it fires on its own window); the cancelled group's member never gets a signal |
| LV-3 | `team_message_body` with a MessageKey belonging to another team, and with a forged well-formed key → `NOT_FOUND`; no transcript is read for the other team | `verify_lead::probe_lv3_a_body_of_a_team_world_does_not_hold_is_never_read` | `world_message_bodies::wired::t_wiring_a_message_older_than_the_store_is_found` (`m-9999` → `NOT_FOUND`) | team A held by World; team B's transcripts on disk (a canary in every body) but not held; B's keys for B's own id and for an id A uses, a random `g_<16 hex>`, an unknown id in A's team; a counting reader in front of the real one proves no lookup runs; the canary is in no answer, store row or log line; A's own key is the control. The request carries no team field: a key names its team, so a key of any team World holds is answered by design (not a refusal case) |
| LV-4 | `settings_set_executable` with a binary in a world-writable directory (1777, `/tmp`-like), under a world-writable ancestor without the sticky bit (A50.2), a world-writable binary file (A50.4), and a symlink into a vendor dir → rejected before any confirmation | `verify_lead::probe_lv4_executables_from_writable_or_vendor_dirs_are_refused_before_any_dialog` | none for `worldWritableDir` / `insideVendorDir`; `ipc_hardening::a45_1_the_confirmation_names_and_applies_the_canonical_path` (the canonical path is what is shown and applied) | 1777 and 0777 dirs, a clean symlink to one, `sidecar.nodePath` and `paths.override`; symlinks into `~/.claude` (file and dir), into a sibling `.claude-*` root, and a case variant `~/.CLAUDE/…`; a binary and a `pathsOverride` dir in owner-only dirs under a 0777 dir (A50.2); a 0777 binary file in a clean dir (A50.4, `worldWritableFile`); a recording confirmation counts the dialogs (0 for every refusal); the user's own dir under a 1777 dir and a clean dir are the controls. Finding LV-4(nested) on `aa718ea`, ruled A50.2 and fixed by T46: now asserted |
| LV-5 | an `agents_rescan` burst → `BUSY` by its probe class; only one detection runs (the A45.3 coalescing) | `verify_lead::probe_lv5_an_agents_rescan_burst_is_one_detection` | `ipc_hardening::g7c_15_agents_rescan_is_rate_limited` (1 scheduled, 19 `BUSY`), `::a45_3_twenty_rescans_while_one_runs_are_one_detection` (direct `spawn_rescan_as`) | both halves in one run over the real IPC: the burst is cut by the limiter (`retryAfterMs` ≈ 60 s), and calls through two fresh limiters (`Ctx::another_app`, the same core) while the detection is held in flight are scheduled and join it; passes are counted from the rerun loop's own log line (`agent detection rerun finished`), not from the fake's runs (the canary's binary resolution is cached, so runs per pass vary) |
| LV-6 | `screen_prompt_key` with a stale `promptRev` after the screen changed to a different prompt (another command in the header), and after a resize rewrap → `STALE_PROMPT`; no key is written to the PTY (A34.1) | `verify_lead::probe_lv6_a_stale_prompt_rev_types_nothing` | `sessions_live::g7_02_deny_after_allow_reaches_the_next_prompt` (sessions manager over a fake World); `pty_prompts::g7_01_a_key_for_prompt_a_never_approves_prompt_b`, `::a_resize_that_rewraps_the_header_makes_the_old_key_stale` (PtyManager only, not run by the kit) | the whole composition root: detection of a fake claude TUI at the pinned path, `session_start` (Terminal) over IPC, `pty_resize` over IPC, `screen_prompt_key` over IPC for a key and a deny-with-reply; the TUI logs every byte it reads, so "nothing typed" is observed at the PTY's far end; the current card's rev is the control |

## Findings of the lead's probes (T45) and their rulings (A50)

Found by T45 on `aa718ea` (recorded then as `MISMATCH(...)`), ruled in
amendment A50 and fixed by T46; the probes now assert the ruled behaviour.

| Finding | Observed on `aa718ea` | Where | Ruling / status |
|---|---|---|---|
| LV-1(b) | The indexed transcript replaced by a **hard link** (same name) to `<cfg>/.credentials.json`: `convo_subscribe` (History's Read) answers `OK{source: transcript}` and the never-open file's bytes reach the view. `history_row` is safe (`safe_read` refuses `nlink > 1`, A34.3). | `follow::tail::open_followed` — the one opener of the follower (`TranscriptTail::open`) and of the hub's initial load and older reads — checks the name and `O_NOFOLLOW` only: no `nlink > 1` refusal and no never-open check on the opened file (A34.3 applies both in `safe_read::open_in` only). | A50.1 — fixed (T46): `open_followed` applies `safe_read`'s checks (`nlink == 1`, never-open on the opened file, `O_NONBLOCK`) and holds the file to its vendor root (`follow::tail::followed_root`); regressions in `follow_tail::a50::*`, `follow_hub::a50::*` |
| LV-1(c) | The **project dir** replaced by a symlink to a dir outside the vendor root: `convo_subscribe` follows `<cfg>/projects/<link>/<sid>.jsonl` and shows the outside file. `history_row` is safe (`F_GETPATH` is outside the root). | the same opener: `O_NOFOLLOW` guards the last path component only, and nothing compares the opened file's real path with the vendor root. The hub's team-message body reader rooted `safe_read` at the file's own directory, which the symlink moves too. | A50.1 — fixed (T46): the opened path must be the vendor root's realpath plus the rest of the path (no symlinked directory below `<cfg>/projects` or `<home>/sessions`); the body reader roots `safe_read` at the vendor root with the same check |
| LV-4(nested) | `~/open/sub/claude` — `sub` 0755 and the user's, `open` 0777 **without** the sticky bit — is applied after one dialog. | `commands::settings::check_executable` checks the binary's own directory only: another local user can rename `sub` away and put a binary at the pinned path. | A50.2 — fixed (T46): any ancestor world-writable without the sticky bit refuses (`worldWritableDir`), for binaries and `pathsOverride`; under a sticky ancestor, an entry not owned by the user or root refuses; regression `commands::settings::tests::a50_2_no_world_writable_ancestor` |

## Findings and their rulings (A33)

The T33 run found these; amendment A33 ruled on each. The probes assert the
ruled behaviour; a variant that still differs prints `MISMATCH(...)`.

| Finding | Ruling | Status |
|---|---|---|
| SP-3: an escape to a missing path answered `NOT_FOUND{file}` (an existence oracle) | A33.1: lexical roots check first | fixed (T34): `ide::paths::check_lexical` before `resolve` |
| SP-8: a 2 MiB transcript line is under the cap | A33.2: §14.5 erratum, the cap is 4 MiB | probe asserts =4 MiB parsed, 4 MiB + 1 `RowTooLarge`, reading continues |
| HP-2: an unknown `permissionMode` kept the previous mode | A33.3: unknown → `None`; composer "Unknown mode" | fixed (T34): the driver reports it through `ConvoHooks::permission_mode` → `World::set_permission_mode` (A35.1), so the session row is `None`; the composer shows "Unknown mode" |
| HP-2: the dropped over-cap line shows as a `System{RowTooLarge}` block | A33.3: that block is the notice | asserted |
| SP-4: a malformed token is refused by the shim client; on the raw socket it is a bad frame | A33.7: fine; the server counts and logs every refused request | asserted (T34 also counts and logs the over-long line and the lead-ended-mid-call races) |
| HP-1(c): `team_messages_page` paged the store's newest 500 | A33.5(c): page World | fixed (T34) |
| HP-1(a)(b)(d): a lying registry pid with a live process, a 2-day-old `busy` without rows, the 10 k-inbox discovery stall | A33.5(a)(b)(d) | fixed (T35): (a) `StaleWhy::PidMismatch` (hidden, `hiddenStale`; the process is an unregistered claude); (b) the `statusUpdatedAt` uptime stamp drives R8's No progress, also without a transcript; (d) the newest 5,000 entries per inbox, read only when the file changes, the observer on its own thread (`gilde-teams`), ≤ 5,000 entries applied per cycle in one World mutation, trimmed messages never re-created — asserted by counters |
| HP-7: no notice; a denied project dir / config root not reported | A33.4 | fixed (T35): a denied config root, project dir or transcript is `PermissionDenied` with its last facts kept and only its sessions marked; World derives one `notice.cantReadFolders` + one `health.folderDenied{category}` per category from `Sources` (A38.8: the Health panel's lines) |
| G7-02: a hard link with an innocent name to a never-open file was opened | A34.3 | fixed (T35): `open_in` re-applies the filter to the opened path and refuses a file with a second link (the kernel reports the innocent name); a second `PROBE SP-7:` line |
| HP-5: first rows > 2 s on a loaded debug build | A33.6: a quiet release-build run by the lead | `run.sh --scale` |

## Known limits of the kit

- Cargo runs only through `scripts/gates/hermetic-cargo.sh` (T45). Before,
  `run.sh` called `cargo test` with the caller's `HOME` and `PATH`, and the
  test harness's guard (A38.2) refused every test that builds a `Hermetic`
  tree on a machine with a vendor CLI installed — HP-2, SP-2, SP-8, SP-7's
  integrated step, HP-6's wake step and the mapped `convo_l1`, `follow_tail`,
  `sessions_manager` and `ipc_contract` tests failed for that reason alone
  and pass under the wrapper.
- The mapped HP-5 test `discovery_watchscope::session_env_and_ide_are_never_watched`
  depended on FSEvents timing (a loaded machine delivered the creation events
  of the agy dir and its `log/`, made before the stream started, after the
  start). A50.3 (T46): it now synchronizes on marker events in a watched
  root — one after the setup, one after the writes it asserts on — and
  passed 5 of 5 runs at load average 34–48.

- Rate classes (A45.3): every command passes the central limiter
  (`src-tauri/src/ipc_rate.rs`, §4.1 rule 6) once its request is valid, so a
  probe that sends one command more often than its class allows in a burst is
  answered `BUSY{retryAfterMs}`. Such a probe waits `retryAfterMs` and sends
  the call once more (SP-3: `ide_open`, class `default`, 10/s per command);
  the class is never relaxed for a probe. SP-3 also asserts that a burst over
  the class *is* cut with `BUSY`, so the probe proves the limiter too.

- `with_wired` builds the fake process table inside the composition root, so a
  probe adds its fake processes after discovery's first pass and rewrites the
  registry file to make discovery read it again (HP-1, HP-4, HP-5). HP-5's Home
  timing is therefore measured from that rewrite, not from launch.
- "No `EACCES` in logs" (SP-7) is weak on its own (the product does not log
  `EACCES`); the probe's evidence is the discovery thread's `eacces` counter,
  which HP-7 shows rising on a real denial.
