/**
 * Shape-only recording (contract §14.4 rule 6: "keys, types, ordering, counts,
 * exit codes, timings — never message text").
 *
 * A frame is written as its label (`sj.py`'s `label`), its key skeleton
 * (`shape`: types only, 3 levels) and a few allowlisted scalar fields that are
 * identifiers or codes, never prose (`fields`). Argv goes through the §9.7
 * redaction; paths under the home dir are written as `~/…`. Frames live in
 * memory only for the probe logic and are dropped with the process.
 */

import { closeSync, mkdirSync, openSync, writeSync } from 'node:fs';
import { dirname } from 'node:path';

import { redactArgv } from './redact.mjs';

/** Key skeleton: objects → {k: shape} (3 levels), arrays → ['list(n)', shape(first)], scalars → type. */
export function shape(value, depth = 0) {
  if (Array.isArray(value)) return [`list(${value.length})`, ...(value.length ? [shape(value[0], depth + 1)] : [])];
  if (value !== null && typeof value === 'object') {
    if (depth >= 3) return `{${Object.keys(value).length} keys}`;
    const out = {};
    for (const [k, v] of Object.entries(value)) out[k] = shape(v, depth + 1);
    return out;
  }
  return value === null ? 'null' : typeof value;
}

/** A short label per frame kind (port of sj.py `label`). */
export function label(o) {
  if (!o || typeof o !== 'object') return typeof o;
  const t = o.type;
  if (t === 'system') return `system/${o.subtype}${o.subtype === 'session_state_changed' ? `(${o.state})` : ''}`;
  if (t === 'stream_event') {
    const ev = o.event ?? {};
    const d = ev.delta && typeof ev.delta === 'object' ? ev.delta.type : null;
    const cb = ev.content_block && typeof ev.content_block === 'object' ? ev.content_block.type : null;
    return `stream_event/${ev.type}${d ? '/' + d : ''}${cb ? '/' + cb : ''}`;
  }
  if (t === 'control_request') {
    const r = o.request ?? {};
    return `control_request/${r.subtype}${r.tool_name ? '/' + r.tool_name : ''}`;
  }
  if (t === 'control_response') return `control_response/${o.response?.subtype}`;
  if (t === 'assistant') {
    const c = Array.isArray(o.message?.content) ? o.message.content : [];
    return `assistant/${c.filter((b) => b && typeof b === 'object').map((b) => b.type).join(',')}`;
  }
  if (t === 'user') {
    const c = o.message?.content;
    const kinds = Array.isArray(c) ? c.filter((b) => b && typeof b === 'object').map((b) => b.type).join(',') : 'str';
    return `user/${kinds}${o.isReplay ? '/isReplay' : ''}`;
  }
  if (t === 'result') return `result/${o.subtype}`;
  return String(t);
}

const IDENT = /^[\w.:/@+-]{0,80}$/;

/** Allowlisted scalar fields of a frame: codes and identifiers only. */
const FIELD_PATHS = [
  ['type'], ['subtype'], ['state'], ['status'], ['permissionMode'], ['model'], ['claude_code_version'],
  ['request', 'subtype'], ['request', 'tool_name'], ['response', 'subtype'],
  ['terminal_reason'], ['is_error'], ['stop_reason'], ['num_turns'], ['duration_ms'], ['api_error_status'],
  ['error'], ['isReplay'], ['event', 'type'], ['event', 'delta', 'type'], ['event', 'content_block', 'type'],
  ['rate_limit_info', 'status'], ['rate_limit_info', 'unifiedWindows', 'five_hour', 'utilization'],
  ['rate_limit_info', 'unifiedWindows', 'seven_day', 'utilization'], ['queued_turn_count'],
];
const PRESENCE = ['uuid', 'session_id', 'request_id', 'parent_tool_use_id', 'user_message_uuids'];

function at(o, path) {
  let cur = o;
  for (const k of path) {
    if (cur === null || typeof cur !== 'object') return undefined;
    cur = cur[k];
  }
  return cur;
}

export function fields(o) {
  const out = {};
  if (!o || typeof o !== 'object') return out;
  for (const path of FIELD_PATHS) {
    const v = at(o, path);
    if (typeof v === 'number' || typeof v === 'boolean') out[path.join('.')] = v;
    else if (typeof v === 'string' && IDENT.test(v)) out[path.join('.')] = v;
  }
  const caps = o.capabilities;
  if (Array.isArray(caps) && caps.every((c) => typeof c === 'string' && IDENT.test(c))) out.capabilities = caps;
  const has = {};
  for (const k of PRESENCE) if (k in o || (o.request && k in o.request) || (o.response && k in o.response)) has[k] = true;
  if (o.request && 'agent_id' in o.request) has.agent_id = true;
  if (Object.keys(has).length) out.has = has;
  return out;
}

/** `/Users/x/…` → `~/…` for the given home. */
export function homeRelative(path, home) {
  if (typeof path !== 'string') return path;
  return home && (path === home || path.startsWith(home + '/')) ? '~' + path.slice(home.length) : path;
}

/** One JSONL file of shape records. */
export class Recorder {
  constructor(path, { home } = {}) {
    mkdirSync(dirname(path), { recursive: true });
    this.path = path;
    this.home = home;
    this.fd = openSync(path, 'a');
    this.count = 0;
  }

  write(record) {
    writeSync(this.fd, JSON.stringify(record) + '\n');
    this.count++;
  }

  frame(t, dir, obj) {
    this.write({ t, dir, label: label(obj), keys: shape(obj), fields: fields(obj) });
  }

  spawn(t, { name, argv, envKeys, configKind, cwd, pid, pgid }) {
    const shown = redactArgv(argv);
    if (shown.length) shown[0] = homeRelative(shown[0], this.home);
    this.write({ t, dir: 'spawn', name, argv: shown, envKeys: [...envKeys].sort(), configKind, cwd: homeRelative(cwd, this.home), pid, pgid });
  }

  /** A note written by the kit itself (never vendor text). */
  note(t, text, extra = {}) {
    this.write({ t, dir: 'note', note: text, ...extra });
  }

  close() {
    if (this.fd !== null) closeSync(this.fd);
    this.fd = null;
  }
}

/** The label sequence of a frame list, stream events collapsed (sj.py `sequence`). */
export function sequence(frames) {
  const out = [];
  let last = null;
  for (const { t, obj } of frames) {
    const lb = label(obj);
    if (last && lb.startsWith('stream_event') && last.lb === lb) {
      last.n++;
      continue;
    }
    last = { t, lb, n: 1 };
    out.push(last);
  }
  return out.map((x) => `${x.t.toFixed(2)} ${x.lb}${x.n > 1 ? ` x${x.n}` : ''}`);
}

/** Sorted key set of an object (for summaries). */
export function keySet(o) {
  return o && typeof o === 'object' && !Array.isArray(o) ? Object.keys(o).sort() : [];
}
