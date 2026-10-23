/**
 * Materialize the idle fixture (`../fixtures/idle-20.json`) in a temp root:
 * a HOME with a Claude config dir, GILDE_HOME, scratch projects, and one idle
 * stand-in process per watched session, each with a registry row
 * (`<cfg>/sessions/<pid>.json`, the R6 A-P1 key set, `procStart` from `ps`)
 * and a short transcript (R2 row shapes, neutral text). Sessions marked
 * `hostileArgv` start with that F-03 vector as argv and the env-value canary
 * in their environment, so discovery and the redaction helper meet them.
 *
 * Everything lives under the temp root; every process is started here, in its
 * own process group, and ended with SIGTERM to that group — never SIGKILL.
 */

import { spawn, execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { loadCanaries } from '../../gates/lib/canary.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
export const SCENARIO = join(HERE, '..', 'fixtures', 'idle-20.json');
const IDLER = join(HERE, '..', 'fakes', 'idler.mjs');

export function loadScenario(path = SCENARIO) {
  return JSON.parse(readFileSync(path, 'utf8'));
}

/**
 * `ps -o lstart=` of a pid ("Thu Oct 22 02:25:01 2026"), the registry's
 * procStart. ps follows the user's locale (e.g. "목 10/22 …"), so it runs
 * with LC_ALL=C to produce the registry grammar.
 */
function procStart(pid) {
  try {
    return execFileSync('/bin/ps', ['-o', 'lstart=', '-p', String(pid)], { encoding: 'utf8', env: { PATH: '/usr/bin:/bin', LC_ALL: 'C', LANG: 'C' } }).trim().replace(/\s+/g, ' ');
  } catch {
    return null;
  }
}

/**
 * Create the fixture under `root`. Returns `{ home, gildeHome, configDir,
 * sessions: [{ name, pid, sessionId, hostile }], stop() }`.
 */
export async function materialize(root, scenario = loadScenario()) {
  const home = join(root, 'home');
  const configDir = join(home, '.claude');
  const gildeHome = join(root, 'gilde-home');
  mkdirSync(join(configDir, 'sessions'), { recursive: true });
  mkdirSync(gildeHome, { recursive: true });
  const { argv: vectors, env: canaryEnv } = loadCanaries();
  const sessions = [];
  const children = [];
  for (const w of scenario.watched) {
    const proj = join(root, 'code', w.project);
    mkdirSync(proj, { recursive: true });
    const hostile = Number.isInteger(w.hostileArgv) ? vectors[w.hostileArgv] : null;
    // The stand-in runs the idler; a hostile session passes the F-03 vector as its argv.
    const args = [IDLER, '--name', w.name, ...(hostile ? ['--', ...hostile.argv] : [])];
    const env = { PATH: '/usr/bin:/bin', HOME: home, ...(hostile ? canaryEnv : {}) };
    const child = spawn(process.execPath, args, { cwd: proj, env, detached: true, stdio: ['ignore', 'ignore', 'ignore'] });
    children.push(child);
    await new Promise((r) => setTimeout(r, 30));
    const sessionId = randomUUID();
    const now = Date.now();
    const row = {
      pid: child.pid, sessionId, cwd: proj, startedAt: now, procStart: procStart(child.pid), version: '2.1.307', peerProtocol: 1, peerFeatures: [],
      kind: w.origin === 'background' ? 'bg' : 'interactive', entrypoint: w.origin === 'vsCode' ? 'claude-vscode' : 'cli', pidDomain: 'host',
      messagingSocketPath: join(configDir, 'sockets', `${child.pid}.sock`), name: w.name, nameSource: 'default', nameSince: now, status: 'idle', updatedAt: now, statusUpdatedAt: now,
      ...(w.origin === 'background' ? { jobId: sessionId.slice(0, 8) } : {}),
    };
    writeFileSync(join(configDir, 'sessions', `${child.pid}.json`), JSON.stringify(row));
    const transcript = join(configDir, 'projects', proj.replace(/[^A-Za-z0-9]/g, '-'), `${sessionId}.jsonl`);
    mkdirSync(dirname(transcript), { recursive: true });
    for (const [type, text] of [['user', 'Tidy up the README.'], ['assistant', 'Done.']]) {
      appendFileSync(transcript, JSON.stringify({ type, uuid: randomUUID(), timestamp: new Date(now).toISOString(), sessionId, cwd: proj, message: { role: type, content: [{ type: 'text', text }] } }) + '\n');
    }
    sessions.push({ name: w.name, pid: child.pid, sessionId, hostile: Boolean(hostile) });
  }
  const stop = async () => {
    for (const c of children) {
      try {
        process.kill(-c.pid, 'SIGTERM');
      } catch {
        // already gone
      }
    }
    await Promise.all(children.map((c) => (c.exitCode !== null || c.signalCode !== null ? null : new Promise((r) => {
      const t = setTimeout(r, 5000);
      c.once('exit', () => {
        clearTimeout(t);
        r();
      });
    }))));
    return children.filter((c) => c.exitCode === null && c.signalCode === null).map((c) => c.pid);
  };
  return { home, gildeHome, configDir, sessions, stop };
}
