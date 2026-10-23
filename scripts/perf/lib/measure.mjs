/**
 * Measurements of the G5 performance gates (contract §11.1, §14.3).
 *
 *  - Idle CPU: `top -l <n+1> -s 1 -stats pid,cpu -pid …` on the given pids
 *    (core, WebContent, sidecar); the first sample is dropped (top has no
 *    delta for it); the budget applies to the 60 s average of the summed CPU
 *    (≤ 1 % focused, ≤ 0.5 % unfocused).
 *  - Wakeups [F-34]: an in-process counter around the async runtime's
 *    park/unpark (core feature `wakeup-count`), read from the counter file the
 *    core writes when `GILDE_WAKEUP_COUNT_FILE` is set:
 *    `{"v":1,"wakeups":<cumulative>,"monotonicMs":<ms>}` (atomic rename);
 *    two readings 60 s apart give wakeups per 60 s (≤ 40 focused, ≤ 12
 *    unfocused). The once-per-release cross-check parses a saved
 *    `sudo powermetrics --samplers tasks -i 60000 -n 1` output — the harness
 *    never runs sudo.
 */

import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';

export const BUDGETS = {
  cpuPct: { focused: 1.0, unfocused: 0.5 },
  wakeupsPer60s: { focused: 40, unfocused: 12 },
};

/** Parse `top -l N -stats pid,cpu` output into samples: `[{pid: cpu%}]`. */
export function parseTop(text) {
  const samples = [];
  let current = null;
  for (const line of text.split('\n')) {
    if (/^Processes:/.test(line)) {
      current = {};
      samples.push(current);
      continue;
    }
    const m = /^\s*(\d+)\s+(\d+(?:\.\d+)?)\s*$/.exec(line);
    if (m && current) current[Number(m[1])] = Number(m[2]);
  }
  return samples;
}

/** Average of the summed CPU of `pids` over `samples`, the first sample dropped. */
export function summarizeCpu(samples, pids) {
  const used = samples.slice(1);
  const perPid = {};
  for (const pid of pids) {
    const values = used.map((s) => s[pid]).filter((v) => typeof v === 'number');
    perPid[pid] = values.length ? values.reduce((a, b) => a + b, 0) / values.length : null;
  }
  const missing = pids.filter((p) => perPid[p] === null);
  const total = used.length ? used.map((s) => pids.reduce((a, p) => a + (s[p] ?? 0), 0)).reduce((a, b) => a + b, 0) / used.length : null;
  return { samples: used.length, totalPct: total === null ? null : Math.round(total * 1000) / 1000, perPid, missing };
}

/** Run top for `seconds` samples on `pids` (top is read-only: it signals nothing). */
export function sampleTop(pids, seconds) {
  return new Promise((resolve, reject) => {
    const args = ['-l', String(seconds + 1), '-s', '1', '-stats', 'pid,cpu', ...pids.flatMap((p) => ['-pid', String(p)])];
    const child = spawn('/usr/bin/top', args, { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    child.stdout.on('data', (d) => {
      out += d;
    });
    child.stderr.on('data', () => {});
    child.once('error', reject);
    child.once('exit', (code) => (code === 0 ? resolve(parseTop(out)) : reject(new Error(`top exited ${code}`))));
  });
}

/** One wakeup-counter reading, or null when the file is absent or malformed. */
export function readCounter(path) {
  try {
    const d = JSON.parse(readFileSync(path, 'utf8'));
    if (d?.v !== 1 || !Number.isFinite(d.wakeups) || !Number.isFinite(d.monotonicMs)) return null;
    return { wakeups: d.wakeups, monotonicMs: d.monotonicMs };
  } catch {
    return null;
  }
}

/** Wakeups per 60 s between two readings (normalized by the core's own monotonic clock). */
export function wakeupRate(a, b) {
  if (!a || !b || b.monotonicMs <= a.monotonicMs || b.wakeups < a.wakeups) return null;
  return Math.round(((b.wakeups - a.wakeups) * 60_000) / (b.monotonicMs - a.monotonicMs) * 10) / 10;
}

/** Read the counter (waiting up to `firstWaitMs` for a first value), wait `seconds`, read again. */
export async function measureWakeups(path, seconds, { firstWaitMs = 10_000 } = {}) {
  let a = readCounter(path);
  for (let waited = 0; !a && waited < firstWaitMs; waited += 100) {
    await new Promise((r) => setTimeout(r, 100));
    a = readCounter(path);
  }
  await new Promise((r) => setTimeout(r, seconds * 1000));
  const b = readCounter(path);
  return { first: a, last: b, per60s: wakeupRate(a, b) };
}

/**
 * The row of `pid` in a saved `powermetrics --samplers tasks` output:
 * `Name ID CPU ms/s User% Deadlines(<2 ms, 2-5 ms) Wakeups(Intr, Pkg idle) …`.
 * Returns interrupt and package-idle wakeups per second, and per 60 s.
 */
export function parsePowermetrics(text, pid) {
  for (const line of text.split('\n')) {
    const tokens = line.trim().split(/\s+/);
    const at = tokens.findIndex((t, i) => i > 0 && t === String(pid));
    if (at < 0) continue;
    const nums = tokens.slice(at + 1).map(Number);
    if (nums.length < 6 || nums.slice(0, 6).some((n) => !Number.isFinite(n))) continue;
    const [cpuMsPerS, userPct, , , intrPerS, idlePerS] = nums;
    return { name: tokens.slice(0, at).join(' '), cpuMsPerS, userPct, intrPerS, idlePerS, per60s: Math.round(intrPerS * 60 * 10) / 10 };
  }
  return null;
}

/** Verdicts against the §11.1 budgets for `focus` (`focused` | `unfocused`). */
export function judge({ cpuPct = null, wakeupsPer60s = null }, focus) {
  const out = [];
  if (cpuPct !== null) out.push({ gate: 'idle CPU', value: cpuPct, budget: BUDGETS.cpuPct[focus], ok: cpuPct <= BUDGETS.cpuPct[focus] });
  if (wakeupsPer60s !== null) out.push({ gate: 'wakeups per 60 s', value: wakeupsPer60s, budget: BUDGETS.wakeupsPer60s[focus], ok: wakeupsPer60s <= BUDGETS.wakeupsPer60s[focus] });
  return out;
}
