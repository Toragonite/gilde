/**
 * The stream-json (L1) probes of contract §14.4: LP-01, 02, 03, 04, 05, 06,
 * 07, 08, 09, 19/26, 31, 33, 34, 37 — the steps of each block, ported from
 * `probes/scripts/l1_*.py` and `m_lp19_26.py`.
 *
 * Each probe gets a context (`run.mjs` `makeContext`) whose `stream()` starts
 * sessions through the guard and whose recorder writes shapes only, and
 * returns `{ verdict, observations }`: `pass` / `fail` mean the clause's
 * expectation held or not on the version that ran (a `fail` selects the
 * fallback, R6 fold-in); `inconclusive` when a step could not be observed.
 * Observations are key sets, counts, booleans, timings and codes — never text.
 */

import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { keySet, label } from '../lib/record.mjs';
import { statFacts } from '../lib/phase.mjs';
import {
  allowOnce,
  allowSession,
  answer,
  answerError,
  control,
  deny,
  initialize,
  isCanUseTool,
  isCtlResp,
  isResult,
  isType,
  l1Argv,
  user,
} from '../lib/stream.mjs';

const isEcho = (o) => o.type === 'user' && o.isReplay === true;
/**
 * The ordered protocol frames of a turn, up to and including its result:
 * control, system, lifecycle, permission and result frames. Model content
 * (assistant/user blocks, stream events, rate-limit ticks) varies between two
 * runs of the same prompt, and frames after the result race the snapshot, so
 * neither takes part in a "frames identical" comparison.
 */
const protocolLabels = (s, from = 0) => {
  const out = [];
  for (const f of s.frames.slice(from)) {
    const l = label(f.obj);
    if (/^(assistant|user|stream_event|rate_limit_event)/.test(l)) continue;
    out.push(l);
    if (l.startsWith('result/')) break;
  }
  return out;
};
const sameKeys = (a, b) => JSON.stringify(a) === JSON.stringify(b);

/** LP-01 (a/b/c): can_use_tool without initialize; the full handshake; the same without -p. */
export async function lp01(ctx) {
  const proj = ctx.project();
  const obs = {};
  let s = ctx.stream('a', l1Argv({ sessionId: randomUUID() }), { cwd: proj });
  user(s, 'Use the Bash tool to run exactly: date > lp01-a.txt', { full: false });
  const askA = await s.waitFor(isCanUseTool, ctx.timeout);
  obs.a = { canUseToolWithoutInitialize: Boolean(askA), requestKeys: keySet(askA?.request) };
  if (askA) allowOnce(s, askA);
  await s.waitFor(isResult, ctx.timeout);
  await s.finish();
  for (const [part, print] of [['b', true], ['c', false]]) {
    s = ctx.stream(part, l1Argv({ print, sessionId: randomUUID() }), { cwd: proj });
    const { response } = await initialize(s);
    const uuid = user(s, 'Use the Bash tool to run exactly: date > lp01-b.txt');
    const ask = await s.waitFor(isCanUseTool, ctx.timeout);
    if (ask) allowOnce(s, ask);
    const res = await s.waitFor(isResult, ctx.timeout);
    const init = s.all(isType('system', 'init'))[0];
    const echo = s.all(isEcho)[0];
    const answerEcho = ask ? s.all(isCtlResp(ask.request_id))[0] : undefined;
    obs[part] = {
      initializeOk: response?.response?.subtype === 'success',
      pendingIsArray: Array.isArray(response?.response?.pending_permission_requests),
      initializeKeys: keySet(response?.response?.response),
      initKeys: keySet(init),
      canUseToolKeys: keySet(ask?.request),
      echoCarriesOurUuid: echo?.uuid === uuid,
      answerEchoed: Boolean(answerEcho),
      resultKeys: keySet(res),
      lifecycle: s.all(isType('command_lifecycle')).map((o) => o.state),
    };
    await s.finish();
  }
  const b = obs.b;
  const pass = b.initializeOk && b.pendingIsArray && b.initKeys.length > 0 && b.canUseToolKeys.length > 0 && b.echoCarriesOurUuid && obs.c.initializeOk && sameKeys(b.canUseToolKeys, obs.c.canUseToolKeys);
  return { verdict: pass ? 'pass' : 'fail', observations: obs };
}

/** LP-02: AllowSession forwarding only addRules → session; the second identical call is not prompted; no settings file changes. */
export async function lp02(ctx) {
  const proj = ctx.project();
  const files = [join(proj, '.claude', 'settings.local.json'), join(proj, '.claude', 'settings.json'), join(ctx.configDir, 'settings.json')];
  const before = files.map(statFacts);
  const s = ctx.stream('main', l1Argv({ sessionId: randomUUID() }), { cwd: proj });
  await initialize(s);
  user(s, 'Use the Bash tool to run exactly: touch lp02-x.txt');
  const ask1 = await s.waitFor(isCanUseTool, ctx.timeout);
  const suggestions = (ask1?.request?.permission_suggestions ?? []).map((x) => ({ type: x.type, destination: x.destination, mode: typeof x.mode === 'string' ? x.mode : undefined }));
  const forwarded = ask1 ? allowSession(s, ask1) : 0;
  await s.waitFor(isResult, ctx.timeout);
  const turn2From = s.frames.length;
  user(s, 'Use the Bash tool to run exactly: touch lp02-x.txt');
  await s.waitFor(isResult, ctx.timeout, { from: turn2From });
  const promptedAgain = s.all(isCanUseTool, turn2From).length > 0;
  const { response: rules } = await control(s, { subtype: 'list_permission_rules' });
  const turn3From = s.frames.length;
  user(s, 'Use the Write tool to write out.txt');
  const askW = await s.waitFor(isCanUseTool, ctx.timeout, { from: turn3From });
  const writeSuggestions = (askW?.request?.permission_suggestions ?? []).map((x) => ({ type: x.type, destination: x.destination }));
  if (askW) deny(s, askW);
  await s.waitFor(isResult, ctx.timeout, { from: turn3From });
  await s.finish();
  const after = files.map(statFacts);
  const settingsUnchanged = sameKeys(before, after);
  const obs = { bashSuggestions: suggestions, forwardedAddRules: forwarded, promptedAgain, sessionRules: rules?.response?.response?.rules?.length ?? null, writeSuggestions, settingsUnchanged, writeStillPrompted: Boolean(askW) };
  return { verdict: Boolean(ask1) && !promptedAgain && settingsUnchanged ? 'pass' : ask1 ? 'fail' : 'inconclusive', observations: obs };
}

/** LP-03: Deny + interrupt ends the turn; a follow-up starts a new turn. */
export async function lp03(ctx) {
  const proj = ctx.project();
  const s = ctx.stream('main', l1Argv({ sessionId: randomUUID() }), { cwd: proj });
  await initialize(s);
  user(s, 'Use the Bash tool to run exactly: touch lp03.txt');
  const ask = await s.waitFor(isCanUseTool, ctx.timeout);
  if (ask) deny(s, ask, { interrupt: true });
  const r1 = await s.waitFor(isResult, ctx.timeout);
  const from = s.frames.length;
  user(s, 'Reply with one word.');
  const r2 = await s.waitFor(isResult, ctx.timeout, { from });
  await s.finish();
  const obs = { firstResult: { subtype: r1?.subtype ?? null, terminalReason: r1?.terminal_reason ?? null, isError: r1?.is_error ?? null, denials: Array.isArray(r1?.permission_denials) ? r1.permission_denials.length : null }, followUpResult: r2?.subtype ?? null };
  return { verdict: ask && r1 && r2 ? 'pass' : 'inconclusive', observations: obs };
}

/** LP-04: (a) TERM while a prompt is pending, then resume; (b) stdin EOF while a prompt is pending. */
export async function lp04(ctx) {
  const proj = ctx.project();
  const sid = randomUUID();
  let s = ctx.stream('a1', l1Argv({ sessionId: sid }), { cwd: proj });
  await initialize(s);
  user(s, 'Use the Bash tool to run exactly: touch lp04a.txt');
  const askA = await s.waitFor(isCanUseTool, ctx.timeout);
  const termed = askA ? await s.term() : null;
  await s.finish();
  s = ctx.stream('a2', l1Argv({ print: true, sessionId: null, extra: ['--resume', sid] }), { cwd: proj });
  const { response } = await initialize(s);
  const pending = response?.response?.pending_permission_requests;
  user(s, 'Reply with one word.');
  const init = await s.waitFor(isType('system', 'init'), ctx.timeout, { from: 0, advance: false });
  await s.waitFor(isResult, ctx.timeout);
  await s.finish();
  s = ctx.stream('b', l1Argv({ sessionId: randomUUID() }), { cwd: proj });
  await initialize(s);
  user(s, 'Use the Bash tool to run exactly: touch lp04b.txt');
  const askB = await s.waitFor(isCanUseTool, ctx.timeout);
  const eofAt = s.t();
  if (askB) s.closeStdin();
  const exit = await s.waitExit(ctx.timeout);
  const cancel = s.all((o) => o.type === 'control_cancel_request').length;
  await s.finish();
  const obs = {
    a: { termExit: termed ? { code: termed.code, signal: termed.signal } : null, resumedPendingCount: Array.isArray(pending) ? pending.length : null, sessionIdKept: init?.session_id === sid },
    b: { cancelRequests: cancel, exitCode: exit.code, secondsAfterEof: exit.t !== null ? Math.round((exit.t - eofAt) * 100) / 100 : null },
  };
  const rearm = Array.isArray(pending) && pending.length > 0;
  return { verdict: askA && askB ? (rearm && cancel > 0 ? 'pass' : 'fail') : 'inconclusive', observations: obs, note: 'pass = re-delivered + cancel (P6/P7); fail = the fallbacks apply' };
}

/** LP-05: capabilities; requires_action bracketing; queued sends + interrupt{cancel_queued}. */
export async function lp05(ctx) {
  const proj = ctx.project();
  const s = ctx.stream('main', l1Argv({ sessionId: randomUUID() }), { cwd: proj });
  await initialize(s);
  user(s, 'Use the Bash tool to run exactly: date');
  const ask = await s.waitFor(isCanUseTool, ctx.timeout);
  const idx = s.frames.findIndex((f) => f.obj === ask);
  const before = idx > 0 ? s.frames[idx - 1].obj : null;
  const bracketed = before?.type === 'system' && before?.subtype === 'session_state_changed' && before?.state === 'requires_action';
  if (ask) allowOnce(s, ask);
  await s.waitFor(isResult, ctx.timeout);
  const caps = s.all(isType('system', 'init'))[0]?.capabilities ?? [];
  const from = s.frames.length;
  user(s, 'Run sleep 20 with Bash.');
  await s.waitFor(isType('system', 'task_started'), ctx.timeout, { from });
  const q1 = user(s, 'First queued message.');
  const q2 = user(s, 'Second queued message.');
  const { response: receipt } = await control(s, { subtype: 'interrupt', cancel_queued: true });
  const res = await s.waitFor(isResult, ctx.timeout, { from });
  const receiptIdx = s.frames.findIndex((f) => f.obj === receipt);
  const resultIdx = s.frames.findIndex((f) => f.obj === res);
  const cancelled = receipt?.response?.response?.cancelled;
  await s.finish();
  const obs = {
    capabilities: caps,
    requiresActionBeforeAsk: bracketed,
    receiptKeys: keySet(receipt?.response?.response),
    cancelledBoth: Array.isArray(cancelled) && [q1, q2].every((u) => cancelled.includes(u)),
    receiptBeforeResult: receiptIdx >= 0 && (resultIdx < 0 || receiptIdx < resultIdx),
    resultTerminalReason: res?.terminal_reason ?? null,
  };
  const pass = caps.includes('interrupt_receipt_v1') && caps.includes('interrupt_send_now_v1') && bracketed && obs.receiptBeforeResult;
  return { verdict: ask ? (pass ? 'pass' : 'fail') : 'inconclusive', observations: obs };
}

/** LP-06: the ExitPlanMode request shape and Approve; AskUserQuestion answers (single + free text). */
export async function lp06(ctx) {
  const proj = ctx.project();
  let s = ctx.stream('plan', l1Argv({ sessionId: randomUUID(), extra: ['--permission-mode', 'plan'] }), { cwd: proj });
  await initialize(s);
  user(s, 'Plan a one-line change to README.md, then ask to exit plan mode.');
  const plan = await s.waitFor(isCanUseTool, ctx.timeout);
  if (plan) answer(s, plan, { behavior: 'allow', updatedInput: plan.request.input ?? {}, toolUseID: plan.request.tool_use_id });
  const status = await s.waitFor((o) => o.type === 'system' && o.subtype === 'status' && typeof o.permissionMode === 'string', ctx.timeout);
  await s.waitFor(isResult, ctx.timeout, { from: 0 });
  await s.finish();
  s = ctx.stream('ask', l1Argv({ sessionId: randomUUID() }), { cwd: proj });
  await initialize(s);
  user(s, 'Use AskUserQuestion to ask me one question about colours, then stop.');
  const q = await s.waitFor(isCanUseTool, ctx.timeout);
  const questions = q?.request?.input?.questions ?? [];
  if (q) {
    const answers = {};
    questions.forEach((x, i) => {
      answers[x.question] = i === 0 ? (x.options?.[0]?.label ?? 'first') : 'typed by the user';
    });
    answer(s, q, { behavior: 'allow', updatedInput: { questions, answers } });
  }
  const res = await s.waitFor(isResult, ctx.timeout);
  await s.finish();
  const obs = {
    plan: { toolName: plan?.request?.tool_name ?? null, inputKeys: keySet(plan?.request?.input), requestKeys: keySet(plan?.request), modeAfterApprove: status?.permissionMode ?? null },
    question: { toolName: q?.request?.tool_name ?? null, questionKeys: keySet(questions[0]), optionKeys: keySet(questions[0]?.options?.[0]), resultSubtype: res?.subtype ?? null },
  };
  const pass = obs.plan.toolName === 'ExitPlanMode' && obs.question.toolName === 'AskUserQuestion' && res?.subtype === 'success';
  return { verdict: plan && q ? (pass ? 'pass' : 'fail') : 'inconclusive', observations: obs };
}

/** LP-07: concurrent subagent asks answered out of order, an error reply; the sandbox ask shape. */
export async function lp07(ctx) {
  const proj = ctx.project();
  let s = ctx.stream('parallel', l1Argv({ sessionId: randomUUID() }), { cwd: proj });
  await initialize(s);
  user(s, 'Start two subagents in parallel; each runs one Bash echo.');
  const first = await s.waitFor(isCanUseTool, ctx.timeout);
  const second = await s.waitFor(isCanUseTool, ctx.timeout);
  const concurrent = Boolean(first && second);
  if (second) allowOnce(s, second);
  if (first) answerError(s, first);
  const retry = await s.waitFor(isCanUseTool, ctx.timeout);
  if (retry) allowOnce(s, retry);
  await s.waitFor(isResult, ctx.timeout, { from: 0 });
  const errorEchoed = first ? s.all((o) => o.type === 'control_response' && o.response?.request_id === first.request_id && o.response?.subtype === 'error').length > 0 : null;
  await s.finish();
  s = ctx.stream('sandbox', l1Argv({ sessionId: randomUUID(), extra: ['--settings', '{"sandbox":{"enabled":true}}'] }), { cwd: proj });
  await initialize(s, {}, ctx.timeout);
  user(s, 'Use Bash to run: curl -s -o /dev/null -w "%{http_code}" https://example.com');
  const sb = await s.waitFor(isCanUseTool, ctx.timeout);
  const sbIdx = s.frames.findIndex((f) => f.obj === sb);
  const sbPrev = sbIdx > 0 ? s.frames[sbIdx - 1].obj : null;
  if (sb) allowOnce(s, sb);
  await s.waitFor(isResult, ctx.timeout);
  await s.finish();
  const obs = {
    parallel: { concurrent, distinctRequestIds: concurrent && first.request_id !== second.request_id, agentIds: [first, second].filter(Boolean).every((x) => 'agent_id' in (x.request ?? {})), errorReplyEchoed: errorEchoed, retried: Boolean(retry) },
    sandbox: { toolName: sb?.request?.tool_name ?? null, inputKeys: keySet(sb?.request?.input), toolUseIdIsUuid: /^[0-9a-f-]{36}$/.test(sb?.request?.tool_use_id ?? ''), bracketed: sbPrev?.subtype === 'session_state_changed' },
  };
  return { verdict: concurrent && sb ? 'pass' : 'inconclusive', observations: obs };
}

/** LP-08: --session-id = system/init.session_id = transcript file name = registry sessionId. */
export async function lp08(ctx) {
  const proj = ctx.project();
  const sid = randomUUID();
  const s = ctx.stream('main', l1Argv({ sessionId: sid }), { cwd: proj });
  await initialize(s);
  user(s, 'Reply with one word.');
  const init = await s.waitFor(isType('system', 'init'), ctx.timeout, { from: 0, advance: false });
  await s.waitFor(isResult, ctx.timeout);
  const registry = readRegistry(ctx.configDir, s.child.pid);
  await s.finish();
  const transcriptNamed = transcriptExists(ctx.configDir, sid);
  const obs = { initMatches: init?.session_id === sid, transcriptNamedById: transcriptNamed, registryMatches: registry?.sessionId === sid };
  return { verdict: obs.initMatches && obs.transcriptNamedById ? 'pass' : 'fail', observations: obs };
}

/** LP-09: (a) get_usage keys; (b) a fresh, never-signed-in config dir; (c) never forced. */
export async function lp09(ctx) {
  const proj = ctx.project();
  let s = ctx.stream('a', l1Argv({ sessionId: randomUUID() }), { cwd: proj });
  await initialize(s);
  const { response } = await control(s, { subtype: 'get_usage' });
  await s.finish();
  const usage = response?.response?.response ?? null;
  const empty = join(ctx.scratchRoot, 'LP-09', `empty-cfg-${Date.now()}`);
  s = ctx.stream('b', l1Argv({ sessionId: randomUUID() }), { cwd: proj, configDir: empty });
  const { response: init } = await initialize(s);
  user(s, 'Reply with one word.');
  const res = await s.waitFor(isResult, ctx.timeout);
  const assistantError = s.all((o) => o.type === 'assistant' && typeof o.error === 'string')[0]?.error ?? null;
  s.closeStdin();
  const exit = await s.waitExit(ctx.timeout);
  await s.finish();
  const obs = {
    a: { keys: keySet(usage), rateLimitWindows: keySet(usage?.rate_limits), behaviorsDropped: true },
    b: { tokenSource: init?.response?.response?.account?.tokenSource ?? null, assistantError, resultTerminalReason: res?.terminal_reason ?? null, resultIsError: res?.is_error ?? null, exitCode: exit.code },
    c: 'never forced (rule 3)',
  };
  const pass = obs.a.rateLimitWindows.length > 0 && obs.b.tokenSource === 'none';
  return { verdict: pass ? 'pass' : 'fail', observations: obs };
}

/** LP-19 + LP-26: transcript row latency over a few tool calls; usage key names per assistant row. */
export async function lp19_26(ctx) {
  const proj = ctx.project();
  const sid = randomUUID();
  const s = ctx.stream('main', l1Argv({ sessionId: sid }), { cwd: proj });
  await initialize(s);
  const path = transcriptPath(ctx.configDir, sid);
  const arrivals = [];
  let offset = 0;
  const poll = setInterval(() => {
    const p = path() ;
    if (!p) return;
    let text;
    try {
      text = readFileSync(p, 'utf8');
    } catch {
      return;
    }
    const fresh = text.slice(offset);
    const lastNl = fresh.lastIndexOf('\n');
    if (lastNl < 0) return;
    offset += lastNl + 1;
    for (const line of fresh.slice(0, lastNl).split('\n')) {
      try {
        const r = JSON.parse(line);
        const ts = Date.parse(r.timestamp);
        if (Number.isFinite(ts)) arrivals.push({ type: r.type, lagMs: Date.now() - ts, usageKeys: keySet(r.message?.usage), messageId: r.message?.id ?? null });
      } catch {
        // partial row
      }
    }
  }, 10);
  for (let i = 0; i < ctx.calls; i++) {
    const from = s.frames.length;
    user(s, 'Use the Bash tool to run exactly: date');
    const ask = await s.waitFor(isCanUseTool, ctx.timeout, { from });
    if (ask) allowOnce(s, ask);
    await s.waitFor(isResult, ctx.timeout, { from });
  }
  await new Promise((r) => setTimeout(r, 300));
  clearInterval(poll);
  await s.finish();
  const lags = arrivals.map((a) => a.lagMs).sort((a, b) => a - b);
  const q = (p) => (lags.length ? lags[Math.min(lags.length - 1, Math.floor(p * lags.length))] : null);
  const assistant = arrivals.filter((a) => a.type === 'assistant');
  const ids = new Map();
  for (const a of assistant) if (a.messageId) ids.set(a.messageId, (ids.get(a.messageId) ?? 0) + 1);
  const obs = {
    lp19: { rows: arrivals.length, p50Ms: q(0.5), p90Ms: q(0.9), maxMs: lags.length ? lags[lags.length - 1] : null },
    lp26: { assistantRows: assistant.length, usageKeys: assistant[0]?.usageKeys ?? [], rowsPerMessageIdMax: Math.max(0, ...ids.values()) },
  };
  return { verdict: arrivals.length > 0 ? 'pass' : 'inconclusive', observations: obs };
}

/** LP-31: CLAUDE_CODE_ENTRYPOINT=gilde → registry entrypoint "gilde"; frames otherwise identical. */
export async function lp31(ctx) {
  const proj = ctx.project();
  const run = async (name, extra) => {
    const s = ctx.stream(name, l1Argv({ sessionId: randomUUID() }), { cwd: proj, extraEnv: extra });
    await initialize(s);
    user(s, 'Reply with exactly: pong');
    await s.waitFor(isResult, ctx.timeout);
    const reg = readRegistry(ctx.configDir, s.child.pid);
    const seq = protocolLabels(s);
    await s.finish();
    return { entrypoint: reg?.entrypoint ?? null, registryKeys: keySet(reg), sequence: seq };
  };
  const marked = await run('marked', { CLAUDE_CODE_ENTRYPOINT: 'gilde' });
  const plain = await run('plain', {});
  const obs = { marked: marked.entrypoint, plain: plain.entrypoint, registryKeys: marked.registryKeys, framesIdentical: sameKeys(marked.sequence, plain.sequence) };
  return { verdict: marked.entrypoint === 'gilde' && obs.framesIdentical ? 'pass' : marked.entrypoint === null ? 'inconclusive' : 'fail', observations: obs };
}

/** LP-33: LP-01(b) with and without --permission-prompts host. */
export async function lp33(ctx) {
  const proj = ctx.project();
  const run = async (name, extra) => {
    const s = ctx.stream(name, l1Argv({ sessionId: randomUUID(), extra }), { cwd: proj });
    const { response } = await initialize(s);
    user(s, 'Use the Bash tool to run exactly: date');
    const ask = await s.waitFor(isCanUseTool, ctx.timeout);
    if (ask) allowOnce(s, ask);
    await s.waitFor(isResult, ctx.timeout);
    const seq = protocolLabels(s);
    await s.finish();
    return { initKeys: keySet(response?.response?.response), sequence: seq, asked: Boolean(ask) };
  };
  const host = await run('host', ['--permission-prompts', 'host']);
  const plain = await run('plain', []);
  const obs = { initializeIdentical: sameKeys(host.initKeys, plain.initKeys), framesIdentical: sameKeys(host.sequence, plain.sequence), asked: host.asked && plain.asked };
  return { verdict: obs.initializeIdentical && obs.framesIdentical ? 'pass' : 'fail', observations: obs };
}

/** LP-34: closing stdin on an idle session after one turn: exit 0 within 1.5 s. */
export async function lp34(ctx) {
  const proj = ctx.project();
  const s = ctx.stream('main', l1Argv({ sessionId: randomUUID() }), { cwd: proj });
  await initialize(s);
  user(s, 'Reply with one word.');
  await s.waitFor(isResult, ctx.timeout);
  const at = s.t();
  s.closeStdin();
  const exit = await s.waitExit(10_000);
  await s.finish();
  const secs = exit.t !== null ? Math.round((exit.t - at) * 100) / 100 : null;
  const obs = { exitCode: exit.code, secondsAfterEof: secs };
  return { verdict: exit.code === 0 && secs !== null && secs <= 1.5 ? 'pass' : 'fail', observations: obs };
}

/** LP-37: a project rule allowing Bash(date): no can_use_tool; every key around the call. */
export async function lp37(ctx) {
  const proj = ctx.project();
  mkdirSync(join(proj, '.claude'), { recursive: true });
  writeFileSync(join(proj, '.claude', 'settings.json'), JSON.stringify({ permissions: { allow: ['Bash(date)'] } }));
  const s = ctx.stream('main', l1Argv({ sessionId: randomUUID(), extra: ['--permission-mode', 'default'] }), { cwd: proj });
  await initialize(s);
  user(s, 'Use the Bash tool to run exactly: date');
  const res = await s.waitFor(isResult, ctx.timeout);
  const asked = s.all(isCanUseTool).length > 0;
  const toolUse = s.all((o) => o.type === 'assistant' && (o.message?.content ?? []).some((b) => b?.type === 'tool_use'))[0];
  const toolResult = s.all((o) => o.type === 'user' && Array.isArray(o.message?.content) && o.message.content.some((b) => b?.type === 'tool_result'))[0];
  await s.finish();
  const keysOf = (o) => [...new Set(JSON.stringify(o ?? {}).match(/"([A-Za-z_]+)":/g)?.map((k) => k.slice(1, -2)) ?? [])].sort();
  const markerKeys = [...keysOf(toolUse), ...keysOf(toolResult), ...keysOf(res)].filter((k) => /decision|permission|rule|approv/i.test(k));
  const obs = { canUseToolArrived: asked, toolUseKeys: keysOf(toolUse), toolResultKeys: keysOf(toolResult), resultDenials: Array.isArray(res?.permission_denials) ? res.permission_denials.length : null, markerCandidates: [...new Set(markerKeys)] };
  return { verdict: res ? (asked ? 'fail' : 'pass') : 'inconclusive', observations: obs, note: 'P13 ruleAllowMarker: a marker is recorded when markerCandidates names one; else the guard stays flag-only' };
}

// ---------------------------------------------------------------------------
// Account files the probes may look at (metadata only, never content)
// ---------------------------------------------------------------------------

/** The registry row of `pid`: allowlisted fields only (never `.key` files). */
export function readRegistry(configDir, pid) {
  try {
    const r = JSON.parse(readFileSync(join(configDir, 'sessions', `${pid}.json`), 'utf8'));
    return { sessionId: r.sessionId ?? null, entrypoint: r.entrypoint ?? null, kind: r.kind ?? null, status: r.status ?? null, jobId: r.jobId ?? null, keys: Object.keys(r).sort() };
  } catch {
    return null;
  }
}

/** Whether `<cfg>/projects/*\/<sid>.jsonl` exists (directory names only). */
export function transcriptExists(configDir, sid) {
  return transcriptPath(configDir, sid)() !== null;
}

export function transcriptPath(configDir, sid) {
  return () => {
    const root = join(configDir, 'projects');
    if (!existsSync(root)) return null;
    for (const d of readdirSync(root)) {
      const p = join(root, d, `${sid}.jsonl`);
      try {
        if (statSync(p).isFile()) return p;
      } catch {
        // not here
      }
    }
    return null;
  };
}
