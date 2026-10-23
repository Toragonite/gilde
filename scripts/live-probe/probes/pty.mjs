/**
 * Terminal-mode and hosted-team probes of contract §14.4: LP-12, LP-18, LP-40
 * (A6) in a PTY, and LP-10, LP-38, LP-39 against the kit's pane-host stand-in
 * (`lib/panehost.mjs`, the port of `host.py` + the logging shim).
 *
 * PTY screens stay in memory; the record holds key names, named patterns and
 * whether they matched, file existence checks and shim-call shapes. These
 * probes need the owner present on a real run (rule 7); `run.mjs` asks for
 * `--owner-present`.
 */

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { join } from 'node:path';

import { isResult, initialize, l1Argv, user } from '../lib/stream.mjs';
import { readRegistry, transcriptPath } from './stream.mjs';

const TRUST = /Do you trust the files in this folder|Yes, I trust this folder/;
const MAIN = /manual mode on|auto mode on|mode on/;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Wait for the main screen, answering a folder-trust dialog with ↓ + Enter when it shows (R6 §4). */
async function reachMain(p, ctx, { trust = true } = {}) {
  const first = await Promise.race([
    p.waitText('trust-dialog', TRUST, ctx.timeout).then((hit) => (hit ? 'trust' : null)),
    p.waitText('main-screen', MAIN, ctx.timeout).then((hit) => (hit ? 'main' : null)),
  ]);
  if (first === 'trust') {
    if (!trust) return 'trust';
    p.write('\u001b[B');
    await sleep(200);
    p.write('\r');
    return (await p.waitText('main-after-trust', MAIN, ctx.timeout)) ? 'trusted' : null;
  }
  return first;
}

function git(cwd, args) {
  execFileSync('git', ['-c', 'user.name=probe', '-c', 'user.email=probe@localhost', ...args], { cwd, stdio: 'ignore' });
}

/** LP-12: digits select without CR on a permission prompt and a choice prompt; Esc interrupts. */
export async function lp12(ctx) {
  const proj = ctx.project();
  const p = ctx.pty('solo', ['--session-id', randomUUID(), '--model', 'haiku', '--permission-mode', 'default'], { cwd: proj });
  const reached = await reachMain(p, ctx);
  const obs = { reached };
  if (reached) {
    let m = p.mark();
    p.paste('Use the Bash tool to run exactly: touch lp12-a.txt');
    obs.permissionPrompt = await p.waitText('permission-prompt', /Do you want to proceed\?/, ctx.timeout, { from: m });
    if (obs.permissionPrompt) {
      p.write('1');
      for (let i = 0; i < 40 && !existsSync(join(proj, 'lp12-a.txt')); i++) await sleep(100);
      obs.digitSelectedWithoutCr = existsSync(join(proj, 'lp12-a.txt'));
    }
    m = p.mark();
    p.paste('Use the Bash tool to run exactly: touch lp12-b.txt');
    if (await p.waitText('permission-prompt-2', /Do you want to proceed\?/, ctx.timeout, { from: m })) {
      const m2 = p.mark();
      p.write('\u001b');
      obs.escInterrupted = await p.waitText('interrupted', /Interrupted/, ctx.timeout, { from: m2 });
      obs.escRanNothing = !existsSync(join(proj, 'lp12-b.txt'));
    }
    m = p.mark();
    p.paste('Ask me one question with AskUserQuestion about colours.');
    obs.choicePrompt = await p.waitText('choice-prompt', /Enter to select/, ctx.timeout, { from: m });
    if (obs.choicePrompt) {
      const m2 = p.mark();
      p.write('2');
      obs.choiceAnswered = await p.waitText('choice-answered', /Answered|answered|→/, ctx.timeout, { from: m2 });
    }
  }
  await p.finish();
  return { verdict: obs.digitSelectedWithoutCr ? 'pass' : reached ? 'fail' : 'inconclusive', observations: obs };
}

/** LP-18: session A and a --resume of A both append to one transcript (sizes and row counts only). */
export async function lp18(ctx) {
  const proj = ctx.project();
  const sid = randomUUID();
  const count = () => {
    const p = transcriptPath(ctx.configDir, sid)();
    if (!p) return { rows: 0, bytes: 0 };
    return { rows: readFileSync(p, 'utf8').split('\n').filter(Boolean).length, bytes: statSync(p).size };
  };
  const a = ctx.stream('a', l1Argv({ sessionId: sid }), { cwd: proj });
  await initialize(a);
  user(a, 'Reply with one word.');
  await a.waitFor(isResult, ctx.timeout);
  const afterA1 = count();
  const b = ctx.stream('b', l1Argv({ sessionId: null, extra: ['--resume', sid] }), { cwd: proj });
  await initialize(b);
  user(b, 'Reply with one word.');
  await b.waitFor(isResult, ctx.timeout);
  const afterB = count();
  const from = a.frames.length;
  user(a, 'Reply with one more word.');
  await a.waitFor(isResult, ctx.timeout, { from });
  const afterA2 = count();
  await b.finish();
  await a.finish();
  const obs = { afterA1, afterB, afterA2, bothAppendToOneFile: afterB.rows > afterA1.rows && afterA2.rows > afterB.rows };
  return { verdict: obs.bothAppendToOneFile ? 'pass' : 'inconclusive', observations: obs, note: 'pass confirms the never-resume-live rule (P24)' };
}

/** LP-40 (A6): a worktree inside a trusted repository skips the trust dialog; one under ~/.gilde does not. */
export async function lp40(ctx) {
  const repo = ctx.project('repo');
  mkdirSync(join(repo, '.git', 'info'), { recursive: true });
  writeFileSync(join(repo, '.git', 'info', 'exclude'), '.gilde/\n', { flag: 'a' });
  const obs = {};
  // 1. Trust the repository root once at its dialog.
  let p = ctx.pty('root', ['--permission-mode', 'default'], { cwd: repo });
  obs.root = await reachMain(p, ctx);
  await p.finish();
  // 2. Two worktrees: inside the repo, and under $HOME/.gilde (both with git worktree add).
  const inRepo = join(repo, '.gilde', 'worktrees', 'lp40');
  const homeBase = join(ctx.home, '.gilde', 'worktrees');
  const inHome = join(homeBase, 'lp40');
  const createdHomeBase = !existsSync(homeBase);
  git(repo, ['worktree', 'add', '-q', '-b', 'lp40-in', inRepo]);
  if (existsSync(inHome)) throw new Error(`${inHome} exists already; LP-40 never reuses a directory it did not create`);
  git(repo, ['worktree', 'add', '-q', '-b', 'lp40-home', inHome]);
  try {
    for (const [name, dir] of [['inRepoWorktree', inRepo], ['homeWorktree', inHome]]) {
      p = ctx.pty(name, ['--permission-mode', 'default'], { cwd: dir });
      const seen = await reachMain(p, ctx, { trust: false });
      obs[name] = { dialogShown: seen === 'trust' };
      if (seen === 'trust') {
        // Leave without trusting: the highlighted default is "No, exit".
        p.write('\r');
        await p.finish({ exitCommand: false });
      } else await p.finish();
    }
  } finally {
    // 3. Clean up exactly what this probe created.
    for (const dir of [inRepo, inHome]) {
      try {
        git(repo, ['worktree', 'remove', '--force', dir]);
      } catch {
        rmSync(dir, { recursive: true, force: true });
      }
    }
    if (createdHomeBase) {
      for (const d of [homeBase, join(ctx.home, '.gilde')]) {
        try {
          if (existsSync(d) && readdirSync(d).length === 0) rmSync(d, { recursive: false, force: true });
        } catch {
          // keep a non-empty dir
        }
      }
    }
  }
  const pass = obs.inRepoWorktree && !obs.inRepoWorktree.dialogShown;
  return { verdict: obs.root ? (pass ? 'pass' : 'fail') : 'inconclusive', observations: obs, note: 'pass freezes P41 (worktree root <repo>/.gilde/worktrees/); fail keeps the current root and the trust card' };
}

// ---------------------------------------------------------------------------
// Hosted teams (pane-host stand-in)
// ---------------------------------------------------------------------------

const LEAD_SETTINGS = '{"permissions":{"defaultMode":"default"}}';

/** The §7.4.4 subcommands a hosted lead may send (rows 11a–16). */
export const ALLOWED_SUBCOMMANDS = new Set(['has-session', 'new-session', 'list-panes', 'list-windows', 'new-window', 'split-window', 'set-option', 'select-pane', 'select-layout', 'resize-pane', 'respawn-pane', 'kill-pane']);

function shimCalls(log) {
  try {
    return readFileSync(log, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));
  } catch {
    return [];
  }
}

async function waitCalls(log, pred, timeoutMs) {
  const until = Date.now() + timeoutMs;
  for (;;) {
    const calls = shimCalls(log);
    if (pred(calls)) return calls;
    if (Date.now() > until) return calls;
    await sleep(100);
  }
}

const respawns = (calls) => calls.filter((c) => c.argv?.[2] === 'respawn-pane').length;

/** LP-10: the hosted-lead argv spawns teammates through the shim with the §7.4.4 surface only; a -p lead makes no shim calls. */
export async function lp10(ctx) {
  const proj = ctx.project();
  const team = await ctx.hosted();
  try {
    const lead = ctx.pty('lead', ['--teammate-mode', 'tmux', '--session-id', randomUUID(), '--model', 'haiku', '--permission-mode', 'default', '--settings', LEAD_SETTINGS], { cwd: proj, env: team.env });
    const reached = await reachMain(lead, ctx);
    const obs = { reached };
    if (reached) {
      lead.paste('Spawn 3 teammates named alpha, beta and gamma with model haiku; each replies ok.');
      const calls = await waitCalls(team.log, (c) => respawns(c) >= 3, ctx.timeout);
      obs.respawnPanes = respawns(calls);
      const m = lead.mark();
      lead.write('/exit\r');
      if (await lead.waitText('exit-dialog', /Exit and stop tasks/, ctx.timeout, { from: m })) lead.write('1');
      await Promise.race([lead.exited, sleep(3000)]);
    }
    await lead.finish({ exitCommand: false });
    const calls = shimCalls(team.log);
    const subs = calls.filter((c) => c.route === 'host').map((c) => c.argv?.[2]);
    obs.calls = calls.length;
    obs.routes = calls.reduce((m, c) => ({ ...m, [c.route]: (m[c.route] ?? 0) + 1 }), {});
    obs.subcommands = [...new Set(subs)].sort();
    obs.outsideSurface = obs.subcommands.filter((s) => !ALLOWED_SUBCOMMANDS.has(s));
    obs.versionProbes = calls.filter((c) => c.route === 'version').length;
    obs.tmuxEnvAbsent = calls.every((c) => !c.envPresent?.TMUX && !c.envPresent?.TMUX_PANE);
    obs.killPanes = subs.filter((s) => s === 'kill-pane').length;
    // The -p lead (Conversation mode with --teammate-mode tmux): no shim calls.
    const before = calls.length;
    const s = ctx.stream('p-lead', l1Argv({ sessionId: randomUUID(), extra: ['--teammate-mode', 'tmux'] }), { cwd: proj, env: { ...team.env, CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS: '1' } });
    await initialize(s);
    user(s, 'Spawn one teammate named delta with model haiku.');
    await s.waitFor(isResult, ctx.timeout);
    await s.finish();
    obs.pLeadShimCalls = shimCalls(team.log).length - before;
    const stop = await team.stop();
    obs.deviations = stop.deviations;
    obs.paneSurvivors = stop.survivors.length;
    const pass = obs.respawnPanes >= 3 && obs.outsideSurface.length === 0 && obs.deviations === 0 && obs.pLeadShimCalls === 0;
    return { verdict: reached ? (pass ? 'pass' : 'fail') : 'inconclusive', observations: obs };
  } finally {
    await team.stop();
  }
}

/** LP-38 (F-19): "Move to background and exit" with live pane teammates. */
export async function lp38(ctx) {
  const proj = ctx.project();
  const team = await ctx.hosted();
  const sid = randomUUID();
  try {
    const lead = ctx.pty('lead', ['--teammate-mode', 'tmux', '--session-id', sid, '--model', 'haiku', '--permission-mode', 'default', '--settings', LEAD_SETTINGS], { cwd: proj, env: team.env });
    const obs = { reached: await reachMain(lead, ctx) };
    if (obs.reached) {
      lead.paste('Spawn 2 teammates named alpha and beta with model haiku; each replies ok.');
      obs.respawnPanes = respawns(await waitCalls(team.log, (c) => respawns(c) >= 2, ctx.timeout));
      const m = lead.mark();
      lead.write('/exit\r');
      if (await lead.waitText('exit-dialog', /Move to background and exit/, ctx.timeout, { from: m })) {
        const callsBefore = shimCalls(team.log).length;
        const at = Date.now();
        lead.write('2');
        const exit = await Promise.race([lead.exited.then((code) => code), sleep(ctx.timeout).then(() => null)]);
        obs.leadExitCode = exit;
        let bg = null;
        for (let i = 0; i < 30 && !bg; i++) {
          bg = findBackgroundEntry(ctx.configDir, sid);
          if (!bg) await sleep(100);
        }
        obs.bgEntrySeconds = bg ? Math.round((Date.now() - at) / 100) / 10 : null;
        obs.bgEntry = Boolean(bg);
        await sleep(1000);
        obs.teammatesAlive = [...team.host.panes.values()].filter((p) => p.exitCode === null).length;
        obs.laterShimCalls = shimCalls(team.log).length - callsBefore;
      }
    }
    await lead.finish({ exitCommand: false });
    const pass = obs.leadExitCode === 0 && obs.bgEntry && obs.teammatesAlive >= 1;
    return { verdict: obs.reached ? (pass ? 'pass' : 'fail') : 'inconclusive', observations: obs, note: 'a backgrounded lead keeps running outside the kit; stop it with `claude stop <jobId>` (recorded jobId)' };
  } finally {
    await team.stop();
  }
}

/** LP-39 (N-11): a bypass-mode hosted lead; record the teammate pane command's flags. */
export async function lp39(ctx) {
  const proj = ctx.project();
  const team = await ctx.hosted();
  try {
    const lead = ctx.pty('lead', ['--teammate-mode', 'tmux', '--session-id', randomUUID(), '--model', 'haiku', '--permission-mode', 'bypassPermissions', '--allow-dangerously-skip-permissions', '--settings', '{"permissions":{"defaultMode":"bypassPermissions"}}'], { cwd: proj, env: team.env });
    const obs = { reached: await reachMain(lead, ctx) };
    if (obs.reached) {
      lead.paste('Spawn 1 teammate named alpha with model haiku and ask it to run: touch x');
      await waitCalls(team.log, (c) => respawns(c) >= 1, ctx.timeout);
      const m = lead.mark();
      obs.leadPrompted = await lead.waitText('lead-prompt', /Do you want to proceed\?/, Math.min(ctx.timeout, 15_000), { from: m });
      const pane = team.paneCommands()[0] ?? '';
      obs.paneFlags = ['--permission-mode', '--settings', '--dangerously-skip-permissions', '--allow-dangerously-skip-permissions'].filter((f) => pane.includes(f));
      lead.write('/exit\r');
      if (await lead.waitText('exit-dialog', /Exit and stop tasks/, ctx.timeout)) lead.write('1');
    }
    await lead.finish({ exitCommand: false });
    return { verdict: obs.reached ? 'pass' : 'inconclusive', observations: obs, note: 'records behaviour (P40); the verdict is the recorded flags and prompt location' };
  } finally {
    await team.stop();
  }
}

/** A `kind:"bg"` registry entry with this session id (allowlisted fields only). */
function findBackgroundEntry(configDir, sid) {
  let names = [];
  try {
    names = readdirSync(join(configDir, 'sessions')).filter((n) => /^\d+\.json$/.test(n));
  } catch {
    return null;
  }
  for (const n of names) {
    const r = readRegistry(configDir, Number(n.slice(0, -5)));
    if (r && r.kind === 'bg' && r.sessionId === sid) return r;
  }
  return null;
}
