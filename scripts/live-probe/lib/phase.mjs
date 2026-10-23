/**
 * Phase-level rules of the live-probe kit (contract §14.4 rules 2–4):
 *  - pre-flight (rule 2): `stat <cfg>/.claude.json` (existence only — never
 *    opened), the binary's `--version`, realpath and sha-256 fingerprint,
 *    re-checked before every probe; a changed binary is recorded and each
 *    verdict is attributed to the version that ran;
 *  - quota pressure stop (rule 3): before each probe, a fresh quota reading
 *    (the dev build's `quota_refresh` output saved as JSON) must show every
 *    window below 60 %; during a probe, a `rate_limit_event` at ≥ 0.6, a
 *    rejected rate-limit status, an `api_retry` with 429, or usage-limit text
 *    on stderr stops the phase;
 *  - scratch projects (rule 4): `<scratch root>/<LP-id>/proj` with `git init`
 *    and one file; removed after the phase, and only inside the scratch root.
 */

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, realpathSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { GuardError, checkConfigDir, checkScratchRoot, guardedSpawn, realOrResolved } from './guard.mjs';

export class PhaseStop extends Error {
  constructor(reason) {
    super(`phase stopped: ${reason}`);
    this.name = 'PhaseStop';
    this.reason = reason;
  }
}

// ---------------------------------------------------------------------------
// Pre-flight
// ---------------------------------------------------------------------------

/** sha-256 of a file, hex. */
export function fingerprint(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

/** `<binary> --version` through the guard; resolves to the first x.y.z or null. */
export function binaryVersion(binary, { env, cwd, guard }) {
  return new Promise((resolve) => {
    let out = '';
    let child;
    try {
      ({ child } = guardedSpawn(binary, ['--version'], { env, cwd, ...guard }));
    } catch (e) {
      resolve({ version: null, error: e.message });
      return;
    }
    child.stdout.on('data', (d) => {
      out = (out + d.toString('utf8')).slice(0, 4096);
    });
    child.stderr.on('data', () => {});
    child.stdin.end();
    const timer = setTimeout(() => {
      try {
        process.kill(-child.pid, 'SIGTERM');
      } catch {
        // gone
      }
    }, 15_000);
    child.once('exit', (code) => {
      clearTimeout(timer);
      const m = /\b(\d+\.\d+\.\d+)\b/.exec(out);
      resolve({ version: m ? m[1] : null, code });
    });
  });
}

/**
 * Pre-flight facts of a binary and a config dir. The OQ1 rule runs first, so
 * nothing under a refused dir is ever stat'ed; `.claude.json` is never opened.
 */
export async function preflight({ binary, configDir, env, cwd, guard }) {
  if (!env || env.CLAUDE_CONFIG_DIR !== configDir) throw new GuardError('preflight', 'pre-flight config dir and the process env disagree');
  checkConfigDir(configDir, { home: env.HOME, scratchRoots: guard?.scratchRoots });
  const claudeJson = join(configDir, '.claude.json');
  let configured = false;
  try {
    configured = statSync(claudeJson).isFile();
  } catch {
    configured = false;
  }
  const real = realpathSync(binary);
  const { version } = await binaryVersion(binary, { env, cwd, guard });
  return { binaryRealpath: real, fingerprint: fingerprint(real), version, claudeJsonExists: configured };
}

/** Re-check before a probe: the same realpath and fingerprint, else the change is reported. */
export async function recheck(first, { binary, configDir, env, cwd, guard }) {
  const now = await preflight({ binary, configDir, env, cwd, guard });
  const changed = now.binaryRealpath !== first.binaryRealpath || now.fingerprint !== first.fingerprint || now.version !== first.version;
  return { ...now, changed };
}

// ---------------------------------------------------------------------------
// Quota pressure
// ---------------------------------------------------------------------------

export const PRESSURE_PCT = 60;
const STDERR_PRESSURE = /(usage limit|rate limit|rate_limit|too many requests|\b429\b|limit reached)/i;

/**
 * A saved quota reading: `{windows:[{kind, usedPct}]}` (the `Quota` shape of
 * `quota_refresh`) or a `get_usage` response (`rate_limits.<w>.utilization`
 * in percent). Throws PhaseStop at ≥ 60 % in any window; a missing,
 * unreadable or stale (older than `maxAgeMs`) reading also stops.
 */
export function checkQuotaReading(path, { now = Date.now(), maxAgeMs = 15 * 60_000 } = {}) {
  if (!path || !existsSync(path)) throw new PhaseStop('no quota reading (--quota-file): rule 3 requires one before each probe');
  let data;
  try {
    data = JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    throw new PhaseStop(`quota reading ${path} is not JSON`);
  }
  const quota = data?.quota ?? data;
  const windows = [];
  if (Array.isArray(quota?.windows)) for (const w of quota.windows) windows.push({ name: String(w.kind), pct: w.usedPct });
  const rl = quota?.rate_limits ?? quota?.response?.rate_limits;
  if (rl && typeof rl === 'object') {
    for (const [name, w] of Object.entries(rl)) if (w && typeof w === 'object' && typeof w.utilization === 'number') windows.push({ name, pct: w.utilization });
  }
  if (windows.length === 0) throw new PhaseStop(`quota reading ${path} has no windows`);
  const fetched = typeof quota?.fetchedAt === 'number' ? quota.fetchedAt : statSync(path).mtimeMs;
  if (now - fetched > maxAgeMs) throw new PhaseStop(`quota reading is older than ${Math.round(maxAgeMs / 60_000)} min`);
  for (const w of windows) {
    if (typeof w.pct !== 'number') throw new PhaseStop(`quota window ${w.name} has no usage figure`);
    if (w.pct >= PRESSURE_PCT) throw new PhaseStop(`quota window ${w.name} at ${w.pct} % (≥ ${PRESSURE_PCT} %)`);
  }
  return windows;
}

/**
 * A frame/stderr monitor that calls `onStop(reason)` at the first sign of
 * pressure. Unknown rate-limit statuses count as pressure (the safe side).
 */
export function pressureMonitor(onStop) {
  let stopped = false;
  const stop = (reason) => {
    if (!stopped) {
      stopped = true;
      onStop(reason);
    }
  };
  return {
    frame(o) {
      if (!o || typeof o !== 'object') return;
      if (o.type === 'rate_limit_event') {
        const info = o.rate_limit_info ?? {};
        if (info.status !== undefined && info.status !== 'allowed') stop(`rate_limit_event status ${String(info.status)}`);
        for (const [name, w] of Object.entries(info.unifiedWindows ?? {})) {
          if (w && typeof w.utilization === 'number' && w.utilization >= PRESSURE_PCT / 100) stop(`rate limit window ${name} at ${Math.round(w.utilization * 100)} %`);
        }
      }
      if (o.type === 'system' && o.subtype === 'api_retry' && (o.error_status === 429 || o.status === 429)) stop('api_retry 429');
      if (o.type === 'assistant' && typeof o.error === 'string' && /rate|usage|limit/i.test(o.error)) stop(`assistant error ${o.error}`);
      if (o.type === 'result' && o.api_error_status === 429) stop('result api_error_status 429');
    },
    stderr(text) {
      if (STDERR_PRESSURE.test(text)) stop('usage or rate-limit text on stderr');
    },
    get stopped() {
      return stopped;
    },
  };
}

// ---------------------------------------------------------------------------
// Scratch projects
// ---------------------------------------------------------------------------

function git(cwd, args) {
  execFileSync('git', ['-c', 'user.name=probe', '-c', 'user.email=probe@localhost', '-c', 'commit.gpgsign=false', ...args], { cwd, stdio: 'ignore' });
}

/** `<root>/<lp>/proj` with git init and one committed file. */
export function makeScratchProject(root, lp, { name = 'proj' } = {}) {
  const rootReal = checkScratchRoot(root);
  if (!/^LP-\d{2}[a-z]?$/.test(lp)) throw new GuardError('scratch', `bad probe id ${lp}`);
  const dir = join(rootReal, lp, name);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'README.md'), `# ${lp} scratch project\n`);
  git(dir, ['init', '-q']);
  git(dir, ['add', 'README.md']);
  git(dir, ['commit', '-q', '-m', 'init']);
  return realpathSync(dir);
}

/** Remove `<root>/<lp>` (never anything outside the scratch root). */
export function removeScratch(root, lp) {
  const rootReal = checkScratchRoot(root);
  const dir = realOrResolved(join(rootReal, lp));
  if (!dir.startsWith(rootReal + '/')) throw new GuardError('scratch', `${dir} is outside the scratch root`);
  rmSync(dir, { recursive: true, force: true });
}

/** stat facts of a file (size, mtime) without opening it; null when absent. */
export function statFacts(path) {
  try {
    const st = statSync(path);
    return { size: st.size, mtimeMs: Math.round(st.mtimeMs) };
  } catch {
    return null;
  }
}
