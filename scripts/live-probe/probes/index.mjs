/**
 * The single LP list of contract §14.4 as data: every probe with its origin,
 * account, kind, the clause it freezes, and either the kit's script (`run`)
 * or why the kit does not run it (`manual` steps for the owner/developer,
 * `notRun`/`blocked` per OQ1, `conditional` on T20).
 *
 * Kinds: `stream` (pipes, L1), `pty` (a Terminal-mode session), `hosted`
 * (a hosted lead on the pane-host stand-in), `static` / `app` (no vendor CLI
 * turn). `pty` and `hosted` need the owner present on a real run (rule 7).
 */

import { lp10, lp12, lp18, lp38, lp39, lp40 } from './pty.mjs';
import { lp01, lp02, lp03, lp04, lp05, lp06, lp07, lp08, lp09, lp19_26, lp31, lp33, lp34, lp37 } from './stream.mjs';

const P = (id, origin, account, kind, freezes, extra) => ({ id, origin, account, kind, freezes, ...extra });

export const PROBES = [
  P('LP-01', 'B LP-01, A L1/L4', 'g', 'stream', 'P1 → handshakeShape', { run: lp01 }),
  P('LP-01b', 'B LP-01b', 'default', 'stream', 'C8 as decided', { notRun: 'OQ1: the default account is never used' }),
  P('LP-02', 'B LP-02, A L2', 'g', 'stream', 'P4 → allowSession', { run: lp02 }),
  P('LP-03', 'B LP-03, A L3', 'g', 'stream', 'P5 → denyAndStop', { run: lp03 }),
  P('LP-04', 'B LP-04, A L5', 'g', 'stream', 'P6 → resumeRearm; P7 → stdinEofCancel', { run: lp04 }),
  P('LP-05', 'B LP-05, A L4', 'g', 'stream', 'P9 → nowPriority, interruptReceipt, sessionStateEvents', { run: lp05 }),
  P('LP-06', 'B LP-06, A L6', 'g', 'stream', 'P10 → planAnswer, freeTextAnswer', { run: lp06 }),
  P('LP-07', 'B LP-07, A L3/L6', 'g', 'stream', 'P11 → parallelRequests, errorReply, sandboxAsk', { run: lp07 }),
  P('LP-08', 'B LP-08', 'g', 'stream', 'P2 → sessionIdHonoured', { run: lp08 }),
  P('LP-09', 'B LP-09', 'g + scratch', 'stream', 'P12 → getUsageLive; error-patterns.json', { run: lp09 }),
  P('LP-10', 'B LP-10, A L7/L8', 'g', 'hosted', 'P18 → paneHostingVerified', { run: lp10, note: 'stand-in pane host; the Gilde-component variant (probe-log gilde-tmux + PaneHost) runs from the app build' }),
  P('LP-11', 'B LP-11, A L9', 'g', 'hosted', 'P19 → leadPromptsForTeammates', { manual: 'owner-driven: a teammate in default mode runs `touch x`; read the lead screen and `watchAccount` registry/inbox records (`run.mjs --watch`)' }),
  P('LP-12', 'B LP-12, A L13', 'g', 'pty', 'P20 → screenDigitNoCr', { run: lp12 }),
  P('LP-13', 'B LP-13, A L11', 'g', 'hosted', 'P21 (Close-pane wording)', { manual: 'owner-driven: close one pane of a hosted team (SIGHUP to its group), ask the lead to message it, record the lead reaction shape' }),
  P('LP-14', 'B LP-14, A L10', 'g', 'hosted', 'P22 → idlePerTurn', { manual: 'owner-driven: three teammate turns (finished, Esc mid-turn, API error); count idle_notification blocks per case' }),
  P('LP-15', 'B LP-15, A L9', 'g', 'hosted', 'P22 → teammateQuestionLocation', { manual: 'owner-driven: a pane and an in-process teammate each use AskUserQuestion; record where it renders' }),
  P('LP-16', 'B LP-16, A L10', 'g', 'hosted', 'P22 → sendMessageMsgId', { manual: 'project the SendMessage toolUseResult key set from the lead transcript after one lead message' }),
  P('LP-17', 'B LP-17, A L10/L12', 'g', 'hosted', 'P23 → inFlightSignals', { manual: 'owner-driven: a busy pane teammate receives two lead messages; tally its transcript row types' }),
  P('LP-18', 'B LP-18', 'g', 'stream', 'P24 (never resume live)', { run: lp18 }),
  P('LP-19', 'B LP-19', 'g', 'stream', 'P25 (Details copy)', { run: lp19_26, sharedWith: 'LP-26' }),
  P('LP-20', 'B LP-20', 'g', 'hosted', 'P26 → registryThresholds', { manual: 'reuse an LP-11 team; `run.mjs --watch` records the lead registry status sequence' }),
  P('LP-21', 'B LP-21', 'codex', 'stream', 'P27', { notRun: 'outside OQ1 (one Claude worker account)' }),
  P('LP-22', 'B LP-22', 'g', 'pty', 'P28 → bgAttach', { manual: '`claude --bg` starts daemon processes outside the kit process group (rule 5 cannot hold); run by hand with `claude stop`/`claude rm` cleanup' }),
  P('LP-23', 'B LP-23', 'cursor/agy', 'stream', 'P29', { notRun: 'outside OQ1' }),
  P('LP-24', 'B LP-24, A L14', 'none', 'app', 'P30 → vscodeWindowFocus', { manual: 'two editor windows on scratch folders; owner present; `ide/*.lock` never opened' }),
  P('LP-25', 'B LP-25, A L15', 'two accounts', 'stream', 'P31', { blocked: 'OQ1 grants one probe account; the fallback ships' }),
  P('LP-26', 'B LP-26, A L15', 'g', 'stream', 'P32 → transcriptUsage', { run: lp19_26, sharedWith: 'LP-19' }),
  P('LP-27', 'B LP-27', 'g', 'hosted', 'P33', { manual: 'resume a lead that had teammates (`--resume <id> --teammate-mode tmux`); record the new team dir name and member count' }),
  P('LP-28', 'B LP-28', 'none', 'app', 'P34 → notificationCenter', { manual: 'ad-hoc-signed release build: authorize, deliver, click, withdraw a notification' }),
  P('LP-29', 'B LP-29', 'none', 'app', 'P35 → dockMenu', { manual: 'same build: applicationDockMenu hook, right-click the dock icon' }),
  P('LP-30', 'B LP-30', 'none', 'static', 'P36 (claude-compat.json)', { manual: 'per new version: rerun R3b D.2 steps 1–6 and the R5 P-7 byte searches; on a non-empty diff run LP-10' }),
  P('LP-31', 'A-P1 (A L16)', 'g', 'stream', 'P14 → entrypointMarker', { run: lp31 }),
  P('LP-32', 'A-P2 (A L17)', 'g', 'stream', 'P15 → l2SpawnSeam', { conditional: 'only if T20 is triggered (C4)' }),
  P('LP-33', 'A-P3 (A L1)', 'g', 'stream', 'P3 → permissionPromptsHost', { run: lp33 }),
  P('LP-34', 'A-P4 (A L5)', 'g', 'stream', 'P8 → idleStdinExit', { run: lp34 }),
  P('LP-35', 'A-P5 (A L7)', 'none', 'static', 'P16, P17 → teamStoreVerified', { manual: "T21's static literal scan on each installed claude binary (names only)" }),
  P('LP-36', 'A-P6 (A L14)', 'none', 'app', 'P37 → ideGotoWindow', { manual: 'two editor windows; `code --goto <F1>/file:10`; owner present' }),
  P('LP-37', 'merge (A §6.1.10)', 'g', 'stream', 'P13 → ruleAllowMarker', { run: lp37 }),
  P('LP-38', 'review F-19', 'g', 'hosted', 'P39 (§7.4.7)', { run: lp38 }),
  P('LP-39', 'fix verification N-11', 'g', 'hosted', 'P40 (§6.2.1)', { run: lp39 }),
  P('LP-40', 'lead ruling v2 (A6)', 'g', 'pty', 'P41 (§4.2.2 worktree root, §6.2.4)', { run: lp40 }),
];

export const PROBE_IDS = PROBES.map((p) => p.id);

export function probeById(id) {
  return PROBES.find((p) => p.id === id) ?? null;
}

/** What the kit can run (`run` present), deduplicated for shared scripts (LP-19/LP-26). */
export function runnable(ids = null) {
  const seen = new Set();
  const out = [];
  for (const p of PROBES) {
    if (!p.run || (ids && !ids.includes(p.id))) continue;
    if (seen.has(p.run)) continue;
    seen.add(p.run);
    out.push(p);
  }
  return out;
}

export function statusOf(p) {
  if (p.run) return 'kit';
  if (p.manual) return 'manual';
  if (p.notRun) return 'not run';
  if (p.blocked) return 'blocked';
  if (p.conditional) return 'conditional';
  return 'unknown';
}
