/**
 * The stream-json duplex driver (L1), ported from `probes/scripts/sj.py`.
 *
 * A `StreamSession` starts one claude process through `guardedSpawn` (own
 * process group, guarded env), reads NDJSON frames from stdout, records each
 * frame and each frame it sends as shapes (`record.mjs`), and lets a probe
 * wait for frames by predicate. stderr is never stored: only its byte count,
 * and its lines are offered to the quota monitor. A stdout line over 8 MiB is
 * dropped with a note (the core's cap is §6.1.8's concern; the kit just must
 * not hold it).
 */

import { performance } from 'node:perf_hooks';
import { randomUUID } from 'node:crypto';

import { endGroup, guardedSpawn } from './guard.mjs';

const LINE_CAP = 8 * 1024 * 1024;

export const L1_ARGS = [
  '--input-format', 'stream-json', '--output-format', 'stream-json', '--verbose',
  '--include-partial-messages', '--replay-user-messages', '--permission-prompt-tool', 'stdio',
];

/** The §6.1.1 "new" argv (R6 §1.2), `-p` optional for LP-01(c). */
export function l1Argv({ print = true, sessionId = null, model = 'haiku', extra = [] } = {}) {
  return [...(print ? ['-p'] : []), ...L1_ARGS, ...(sessionId ? ['--session-id', sessionId] : []), ...(model ? ['--model', model] : []), ...extra];
}

export class StreamSession {
  constructor({ name, binary, args, cwd, env, recorder, guard, monitor = null }) {
    Object.assign(this, { name, binary, args, cwd, env, recorder, guard, monitor });
    this.frames = [];
    this.cursor = 0;
    this.exitCode = null;
    this.exitSignal = null;
    this.exitT = null;
    this.stderrBytes = 0;
    this.waiters = new Set();
  }

  t() {
    return Math.round(performance.now() - this.t0) / 1000;
  }

  start() {
    const { child, binary } = guardedSpawn(this.binary, this.args, { env: this.env, cwd: this.cwd, ...this.guard });
    this.child = child;
    this.t0 = performance.now();
    this.exited = new Promise((resolve) => child.once('exit', resolve));
    this.recorder?.spawn(0, { name: this.name, argv: [binary, ...this.args], envKeys: Object.keys(this.env), configKind: this.guard?.configKind ?? null, cwd: this.cwd, pid: child.pid, pgid: child.pid });
    let buf = Buffer.alloc(0);
    let dropping = false;
    child.stdout.on('data', (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      for (;;) {
        const nl = buf.indexOf(10);
        if (nl < 0) {
          if (buf.length > LINE_CAP) {
            dropping = true;
            buf = Buffer.alloc(0);
          }
          break;
        }
        const line = buf.subarray(0, nl).toString('utf8');
        buf = buf.subarray(nl + 1);
        if (dropping) {
          dropping = false;
          this.recorder?.note(this.t(), 'stdout line over 8 MiB dropped');
          continue;
        }
        this.onLine(line);
      }
    });
    child.stderr.on('data', (chunk) => {
      this.stderrBytes += chunk.length;
      this.monitor?.stderr?.(chunk.toString('utf8'));
    });
    child.stdin.on('error', () => this.recorder?.note(this.t(), 'stdin closed by the peer'));
    child.once('exit', (code, signal) => {
      this.exitCode = code;
      this.exitSignal = signal;
      this.exitT = this.t();
      this.recorder?.write({ t: this.exitT, dir: 'exit', code, signal, stderrBytes: this.stderrBytes });
      this.wake();
    });
    return this;
  }

  onLine(line) {
    if (line.trim() === '') return;
    let obj;
    try {
      obj = JSON.parse(line);
    } catch {
      obj = { __unparsed__: true, bytes: line.length };
    }
    const t = this.t();
    this.frames.push({ t, obj });
    this.recorder?.frame(t, 'out', obj);
    this.monitor?.frame?.(obj);
    this.wake();
  }

  wake() {
    for (const w of [...this.waiters]) w();
  }

  send(obj) {
    this.recorder?.frame(this.t(), 'in', obj);
    try {
      this.child.stdin.write(JSON.stringify(obj) + '\n');
    } catch {
      this.recorder?.note(this.t(), 'stdin write failed');
    }
  }

  closeStdin() {
    this.recorder?.write({ t: this.t(), dir: 'stdin_close' });
    try {
      this.child.stdin.end();
    } catch {
      // already closed
    }
  }

  /** The first frame at index ≥ `from` (default: the cursor) matching `pred`, or null on timeout/exit. */
  waitFor(pred, timeoutMs = 120_000, { from = null, advance = true } = {}) {
    let i = from ?? this.cursor;
    return new Promise((resolve) => {
      const finish = (value) => {
        this.waiters.delete(check);
        clearTimeout(timer);
        if (advance) this.cursor = i;
        resolve(value);
      };
      const check = () => {
        while (i < this.frames.length) {
          const f = this.frames[i++];
          let ok = false;
          try {
            ok = pred(f.obj);
          } catch {
            ok = false;
          }
          if (ok) return finish(f.obj);
        }
        if (this.exitCode !== null || this.exitSignal !== null) finish(null);
      };
      const timer = setTimeout(() => finish(null), timeoutMs);
      this.waiters.add(check);
      check();
    });
  }

  /** Frames seen so far matching `pred` (from index `from`). */
  all(pred, from = 0) {
    return this.frames.slice(from).map((f) => f.obj).filter((o) => {
      try {
        return pred(o);
      } catch {
        return false;
      }
    });
  }

  async waitExit(timeoutMs = 30_000) {
    if (this.exitCode !== null || this.exitSignal !== null) return { code: this.exitCode, signal: this.exitSignal, t: this.exitT };
    await Promise.race([this.exited, new Promise((r) => setTimeout(r, timeoutMs))]);
    return { code: this.exitCode, signal: this.exitSignal, t: this.exitT };
  }

  /** SIGTERM to the group now (LP-04a), then wait. Never SIGKILL. */
  async term(waitMs = 10_000) {
    try {
      process.kill(-this.child.pid, 'SIGTERM');
    } catch (e) {
      if (e.code !== 'ESRCH') throw e;
    }
    this.recorder?.write({ t: this.t(), dir: 'signal', signal: 'SIGTERM', target: 'group' });
    return this.waitExit(waitMs);
  }

  /** End the session: close stdin, wait, SIGTERM the group, wait; a survivor is recorded, never killed. */
  async finish() {
    if (this.exitCode === null && this.exitSignal === null) this.closeStdin();
    const r = await endGroup(this.child, this.exited, { graceMs: 3000 });
    if (r.survivor) this.recorder?.note(this.t(), 'process group survived SIGTERM; left running and reported (never SIGKILL)', { pid: this.child.pid });
    return r;
  }
}

// ---------------------------------------------------------------------------
// Frame predicates and protocol helpers (sj.py)
// ---------------------------------------------------------------------------

export const isType = (type, sub = null) => (o) => o.type === type && (sub === null || o.subtype === sub);
export const isCtlReq = (sub) => (o) => o.type === 'control_request' && o.request?.subtype === sub;
export const isCtlResp = (rid) => (o) => o.type === 'control_response' && o.response?.request_id === rid;
export const isResult = (o) => o.type === 'result';
export const isCanUseTool = isCtlReq('can_use_tool');

export async function initialize(s, extra = {}, timeoutMs = 30_000) {
  const rid = randomUUID();
  s.send({ type: 'control_request', request_id: rid, request: { subtype: 'initialize', supportedDialogKinds: [], ...extra } });
  return { response: await s.waitFor(isCtlResp(rid), timeoutMs, { from: 0, advance: false }), rid };
}

export async function control(s, request, timeoutMs = 30_000) {
  const rid = randomUUID();
  s.send({ type: 'control_request', request_id: rid, request });
  return { response: await s.waitFor(isCtlResp(rid), timeoutMs, { from: 0, advance: false }), rid };
}

/** Send a user frame; `full` = the §6.1.3 frame with uuid and origin. Returns the uuid (or null). */
export function user(s, text, { full = true, ...extra } = {}) {
  if (!full) {
    s.send({ type: 'user', message: { role: 'user', content: text }, ...extra });
    return null;
  }
  const uuid = randomUUID();
  s.send({ type: 'user', message: { role: 'user', content: [{ type: 'text', text }] }, parent_tool_use_id: null, uuid, origin: { kind: 'human' }, ...extra });
  return uuid;
}

export function answer(s, req, payload) {
  s.send({ type: 'control_response', response: { subtype: 'success', request_id: req.request_id, response: payload } });
}

export function answerError(s, req, msg = 'probe: host error reply') {
  s.send({ type: 'control_response', response: { subtype: 'error', request_id: req.request_id, error: msg } });
}

export function allowOnce(s, req) {
  const r = req.request;
  answer(s, req, { behavior: 'allow', updatedInput: r.input ?? {}, toolUseID: r.tool_use_id, decisionClassification: 'user_temporary' });
}

/** AllowSession forwarding only `addRules` suggestions, rewritten to `destination: "session"` (§6.1.4, LP-02). */
export function allowSession(s, req) {
  const r = req.request;
  const rules = (Array.isArray(r.permission_suggestions) ? r.permission_suggestions : [])
    .filter((x) => x && x.type === 'addRules')
    .map((x) => ({ ...x, destination: 'session' }));
  answer(s, req, { behavior: 'allow', updatedInput: r.input ?? {}, toolUseID: r.tool_use_id, decisionClassification: 'user_permanent', updatedPermissions: rules });
  return rules.length;
}

export function deny(s, req, { interrupt = false, message = 'The user denied this action.' } = {}) {
  const r = req.request;
  answer(s, req, { behavior: 'deny', message, toolUseID: r.tool_use_id, decisionClassification: 'user_reject', ...(interrupt ? { interrupt: true } : {}) });
}
