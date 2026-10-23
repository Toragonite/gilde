#!/usr/bin/env node
/**
 * turns.mjs — `turn.start` / `turn.cancel` / `turn.event` (contract §4.5, §6.3)
 * against the real bundle, the real agent-mesh adapters and FAKE vendor CLIs
 * (`fixtures/fake-*.cjs`: recorded `codex exec --json`, `cursor-agent … stream-json`
 * and `agy … stream-json` shapes). No real vendor CLI runs.
 *
 *   A. a codex turn end to end: session → tool → text → usage → done, `seq`
 *      strictly increasing, exactly one terminal event, the registry row of kind
 *      `turn`, the contract's RegisterSpawnParams keys
 *   B. the `normalizeTask` fix: model, permission mode (mapped to the vendor's own
 *      vocabulary), account and `resumeId` all reach the CLI's argv / env
 *   C. the account variable rule (C8): the default login runs with the variable
 *      UNSET; a configured account gets exactly its directory; agy never AGY_HOME
 *   D. typed errors: auth / quota / exec / unknown
 *   E. new conversations and empty ids (R2 C7): a resumed turn whose session id
 *      differs from `resumeId`; an id cursor does not know; no id at all
 *   F. text coalescing: 50 ms / ≤ 16 KiB, no character split
 *   G. cancel: `done{stopped}` promptly, the child gets SIGTERM — and a child that
 *      ignores one gets a second SIGTERM, never SIGKILL (C5)
 *   H. refusals: the host's allowlist, a vendor that is not installed, malformed
 *      params (unknown field, bad id, empty prompt, duplicate turn id)
 *   I. the pump's own guarantees with a scripted source (`dev.turnScript`)
 *   J. a wall clock: the timeout SIGTERM ends the turn `incomplete`
 *
 * Run:  node test/turns.mjs   (from src-tauri/sidecar, after `npm run build`)
 */

import { mkdirSync, mkdtempSync, readFileSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { accountId, alive, harness, installFakeVendors, sessionUid, startSidecar } from './fixtures/host.mjs';

const { check, eq, deepEq, summary } = harness('turns');

const work = realpathSync(mkdtempSync(join(tmpdir(), 'gilde-turns-')));
const vendorLog = join(work, 'vendor.log');
const cwd = join(work, 'project');
mkdirSync(cwd);
const codexWorkHome = join(work, 'codex-work');
mkdirSync(codexWorkHome);
const { binaries } = installFakeVendors();

const A = {
  codexDefault: accountId('codex-default'),
  codexWork: accountId('codex-work'),
  cursor: accountId('cursor-default'),
  agy: accountId('agy-default'),
};
const accounts = [
  { accountId: A.codexDefault, vendor: 'codex', envKey: null, configDir: null },
  { accountId: A.codexWork, vendor: 'codex', envKey: 'CODEX_HOME', configDir: codexWorkHome },
  { accountId: A.cursor, vendor: 'cursor', envKey: null, configDir: null },
  { accountId: A.agy, vendor: 'agy', envKey: null, configDir: null },
];

const host = startSidecar({ env: { FAKE_VENDOR_LOG: vendorLog } });

function vendorCalls() {
  try {
    return readFileSync(vendorLog, 'utf8')
      .split('\n')
      .filter((l) => l.trim().length > 0)
      .map((l) => JSON.parse(l));
  } catch {
    return [];
  }
}

let n = 0;
function startParams(overrides) {
  n += 1;
  return {
    turnId: `t${n}`,
    sessionUid: sessionUid(n),
    vendor: 'codex',
    accountId: A.codexWork,
    cwd,
    model: null,
    permissionMode: null,
    prompt: 'scenario:ok please',
    resumeId: null,
    timeoutMs: 60_000,
    ...overrides,
  };
}

/** Start a turn and wait for its terminal event; returns `{params, events}`. */
async function turn(overrides, waitMs = 20_000) {
  const params = startParams(overrides);
  const res = await host.call('turn.start', params);
  eq(`${params.turnId} accepted`, res?.accepted, true);
  await host.waitFor(() => host.turnEvents(params.turnId).some((e) => e.event.kind === 'done' || e.event.kind === 'error'), waitMs);
  await new Promise((r) => setTimeout(r, 100)); // anything after the terminal would land now
  return { params, events: host.turnEvents(params.turnId) };
}

function terminalOf(events) {
  return events.filter((e) => e.event.kind === 'done' || e.event.kind === 'error');
}

function seqOk(events) {
  return events.every((e, i) => e.seq === i + 1);
}

async function main() {
  await host.hello();
  await host.configure({ binaries: { codex: binaries.codex, cursor: binaries.cursor, agy: binaries.agy }, accounts });

  // ── A. a codex turn end to end ─────────────────────────────────────────────
  const beforeA = host.journal.length;
  const a = await turn({ model: 'gpt-6.1-sol', permissionMode: 'plan' });
  const kinds = a.events.map((e) => e.event.kind);
  deepEq('A1 event order', kinds, ['session', 'tool', 'text', 'usage', 'done']);
  check('A2 seq is 1..n, strictly increasing', seqOk(a.events), JSON.stringify(a.events.map((e) => e.seq)));
  eq('A3 exactly one terminal, and it is last', terminalOf(a.events).length, 1);
  const aDone = a.events.at(-1)?.event;
  eq('A4 done.status ok', aDone?.status, 'ok');
  eq('A5 done.conversationId = the thread id', aDone?.conversationId, '0199a1b2-c3d4-7e5f-8a9b-00000000c0d1');
  deepEq('A6 done.usage', aDone?.usage, { inputTokens: 11, outputTokens: 7, cachedInputTokens: 3 });
  eq('A7 done.costUsd is null (unknown, never 0)', aDone?.costUsd, null);
  const tool = a.events.find((e) => e.event.kind === 'tool')?.event;
  eq('A8 tool name', tool?.name, 'command_execution');
  check('A9 tool detailPreview carries the command', typeof tool?.detailPreview === 'string' && tool.detailPreview.includes('ls -la'), tool?.detailPreview);
  const reg = host.journal.slice(beforeA).find((e) => e.method === 'host.registerSpawn')?.params;
  deepEq(
    'A10 registerSpawn carries exactly the contract fields',
    Object.keys(reg ?? {}).sort(),
    ['args', 'cmd', 'cwd', 'envKeys', 'kind', 'spawnId', 'turnId', 'vendor'],
  );
  eq('A11 registry kind turn', reg?.kind, 'turn');
  eq('A12 attributed to the turn', reg?.turnId, a.params.turnId);
  eq('A13 vendor in contract naming', reg?.vendor, 'codex');
  eq('A14 argv[0] is the configured binary', reg?.cmd, binaries.codex);
  const aCall = vendorCalls().find((c) => c.vendor === 'codex' && c.argv?.includes('scenario:ok please'));
  check('A15 the child saw GILDE_SPAWN_ID = the host marker', aCall?.env?.GILDE_SPAWN_ID === host.markers.get(reg?.spawnId), JSON.stringify(aCall?.env));

  // ── B. every field forwarded (normalizeTask fix) ───────────────────────────
  const argv = aCall?.argv ?? [];
  check('B1 the model reached argv', argv.join(' ').includes('--model gpt-6.1-sol'), argv.join(' '));
  check('B2 plan → codex --sandbox read-only (never a Claude mode verbatim)', argv.join(' ').includes('--sandbox read-only'), argv.join(' '));
  eq('B3 the prompt is the argv after --', argv[argv.indexOf('--') + 1], 'scenario:ok please');
  const resumed = await turn({ resumeId: aDone?.conversationId, model: 'gpt-6.1-sol', permissionMode: 'acceptEdits', prompt: 'scenario:ok again' });
  const rCall = vendorCalls().filter((c) => c.vendor === 'codex' && c.argv.includes('scenario:ok again')).at(-1);
  deepEq('B4 resumeId reached argv (exec resume <id>)', rCall?.argv.slice(0, 3), ['exec', 'resume', aDone?.conversationId]);
  check('B5 a resumed codex turn restates the sandbox with -c', (rCall?.argv ?? []).join(' ').includes('sandbox_mode="workspace-write"'), (rCall?.argv ?? []).join(' '));
  eq('B6 the resumed turn keeps its conversation', resumed.events.at(-1)?.event.conversationId, aDone?.conversationId);
  const noModel = await host.callError('turn.start', startParams({ resumeId: aDone?.conversationId, model: null }));
  eq('B7 a codex resume without a model is refused before any spawn', noModel?.code, -32602);
  eq('B8 … naming the model field', noModel?.data?.field, 'model');
  const cmdUnmapped = await turn({ vendor: 'cursor', accountId: A.cursor, permissionMode: 'acceptEdits', prompt: 'scenario:ok unmapped' });
  const cuCall = vendorCalls().filter((c) => c.vendor === 'cursor' && c.argv.includes('scenario:ok unmapped')).at(-1);
  check('B9 an unmapped mode passes no flag (cursor default sandbox)', !(cuCall?.argv ?? []).includes('--sandbox'), (cuCall?.argv ?? []).join(' '));
  eq('B10 … and the turn still ran', cmdUnmapped.events.at(-1)?.event.kind, 'done');

  // ── C. the account variable rule ───────────────────────────────────────────
  check('C1 a configured codex account gets CODEX_HOME = its dir', aCall?.env?.CODEX_HOME === codexWorkHome, JSON.stringify(aCall?.env));
  await turn({ accountId: A.codexDefault, prompt: 'scenario:ok default-login' });
  const dCall = vendorCalls().filter((c) => c.argv?.includes('scenario:ok default-login')).at(-1);
  eq('C2 the default login runs with CODEX_HOME unset (C8)', dCall?.env?.CODEX_HOME, null);
  await turn({ vendor: 'agy', accountId: A.agy, permissionMode: 'plan', prompt: 'scenario:ok agy-turn' });
  const agyCall = vendorCalls().filter((c) => c.vendor === 'agy' && c.argv.includes('scenario:ok agy-turn')).at(-1);
  eq('C3 agy never gets AGY_HOME', agyCall?.env?.AGY_HOME, null);
  check('C4 agy gets no --mode for a Claude mode', !(agyCall?.argv ?? []).includes('--mode'), (agyCall?.argv ?? []).join(' '));
  const wrongVendor = await host.callError('turn.start', startParams({ vendor: 'cursor', accountId: A.codexWork }));
  eq('C5 an account of another vendor is refused', wrongVendor?.data?.field, 'accountId');

  // ── D. typed errors ────────────────────────────────────────────────────────
  for (const [scenario, cls] of [
    ['auth', 'auth'],
    ['quota', 'quota'],
    ['exec', 'exec'],
  ]) {
    const t = await turn({ prompt: `scenario:${scenario}` });
    const last = t.events.at(-1)?.event;
    eq(`D ${scenario}: the turn ends with one error`, terminalOf(t.events).length === 1 && last?.kind, 'error');
    eq(`D ${scenario}: class ${cls}`, last?.class, cls);
  }
  const agyErr = await turn({ vendor: 'agy', accountId: A.agy, prompt: 'scenario:agent-error' });
  eq('D agent-error: an unclassified failure is unknown', agyErr.events.at(-1)?.event.class, 'unknown');

  // ── E. new conversations and empty ids ─────────────────────────────────────
  const nc = await turn({ resumeId: aDone?.conversationId, model: 'gpt-6.1-sol', prompt: 'scenario:new-conversation' });
  const ncSession = nc.events.find((e) => e.event.kind === 'session')?.event;
  check('E1 codex: a session id ≠ resumeId is reported as the vendor said it', ncSession !== undefined && ncSession.conversationId !== aDone?.conversationId, JSON.stringify(ncSession));
  eq('E2 … and done carries the new id', nc.events.at(-1)?.event.conversationId, '0199a1b2-c3d4-7e5f-8a9b-00000000c0d2');
  const cursorNew = await turn({ vendor: 'cursor', accountId: A.cursor, resumeId: 'deadbeef-unknown', prompt: 'scenario:new-chat' });
  const cnSession = cursorNew.events.find((e) => e.event.kind === 'session')?.event;
  check('E3 cursor: an unknown id silently starts a new chat — visible as a different session id', cnSession?.conversationId === '7c1d2e3f-0000-4000-8000-0000000c0599', JSON.stringify(cnSession));
  const noId = await turn({ prompt: 'scenario:no-id' });
  eq('E4 codex: no thread id → done.conversationId "" (not resumable)', noId.events.at(-1)?.event.conversationId, '');
  check('E5 … and no session event', !noId.events.some((e) => e.event.kind === 'session'));
  const agyNoId = await turn({ vendor: 'agy', accountId: A.agy, prompt: 'scenario:empty-id' });
  eq('E6 agy: no conversation id → ""', agyNoId.events.at(-1)?.event.conversationId, '');

  // ── F. text coalescing ─────────────────────────────────────────────────────
  const deltas = await turn({ vendor: 'cursor', accountId: A.cursor, prompt: 'scenario:deltas' });
  const texts = deltas.events.filter((e) => e.event.kind === 'text').map((e) => e.event.delta);
  let expected = '';
  for (let i = 0; i < 400; i += 1) expected += `d${i};`;
  eq('F1 every byte of text arrives, in order', texts.join(''), expected);
  check('F2 400 deltas are coalesced into far fewer events', texts.length > 0 && texts.length < 100, `${texts.length} text events`);
  const big = await turn({ vendor: 'cursor', accountId: A.cursor, prompt: 'scenario:big' });
  const bigTexts = big.events.filter((e) => e.event.kind === 'text').map((e) => e.event.delta);
  check('F3 no text event exceeds 16 KiB', bigTexts.every((t) => Buffer.byteLength(t, 'utf8') <= 16 * 1024), bigTexts.map((t) => Buffer.byteLength(t)).join(','));
  check('F4 the 40 KiB delta was split, the emoji kept whole', bigTexts.length >= 3 && bigTexts.join('').includes('😀') && !bigTexts.some((t) => /[\ud800-\udbff]$/.test(t)), `${bigTexts.length} events`);
  check('F5 seq stays strictly increasing across merges', seqOk(big.events));

  // ── G. cancel: SIGTERM, never SIGKILL ──────────────────────────────────────
  const hang = startParams({ prompt: 'scenario:hang' });
  await host.call('turn.start', hang);
  await host.waitFor(() => host.journal.some((e) => e.method === 'host.spawned' && host.journal.some((r) => r.method === 'host.registerSpawn' && r.params.turnId === hang.turnId && r.params.spawnId === e.params.spawnId)), 10_000);
  const hangReg = host.journal.find((e) => e.method === 'host.registerSpawn' && e.params.turnId === hang.turnId)?.params;
  const hangPid = host.journal.find((e) => e.method === 'host.spawned' && e.params.spawnId === hangReg?.spawnId)?.params?.pid;
  check('G1 the hanging child runs', typeof hangPid === 'number' && alive(hangPid), String(hangPid));
  const t0 = Date.now();
  const cancelled = await host.call('turn.cancel', { turnId: hang.turnId });
  eq('G2 turn.cancel signalled the group', cancelled?.signalled, true);
  await host.waitFor(() => terminalOf(host.turnEvents(hang.turnId)).length > 0, 5_000);
  const hangLast = host.turnEvents(hang.turnId).at(-1)?.event;
  eq('G3 a cancelled turn ends done{stopped}', `${hangLast?.kind}:${hangLast?.status}`, 'done:stopped');
  check('G4 … promptly', Date.now() - t0 < 3_000, `${Date.now() - t0} ms`);
  await host.waitFor(() => !alive(hangPid), 5_000);
  check('G5 the child is gone', !alive(hangPid));
  const hangExit = await host.waitFor(() => host.journal.some((e) => e.method === 'host.exited' && e.params.spawnId === hangReg?.spawnId), 5_000);
  const hangExited = host.journal.find((e) => e.method === 'host.exited' && e.params.spawnId === hangReg?.spawnId)?.params;
  check('G6 host.exited reports SIGTERM, not SIGKILL', hangExit && hangExited?.signal === 'SIGTERM', JSON.stringify(hangExited));
  eq('G7 cancelling an unknown turn signals nothing', (await host.call('turn.cancel', { turnId: 'nope' }))?.signalled, false);

  const stubborn = startParams({ prompt: 'scenario:stubborn' });
  await host.call('turn.start', stubborn);
  // Wait until its SIGTERM handler is in place (the fake logs `ready` after it).
  await host.waitFor(() => vendorCalls().some((c) => c.ready === 'stubborn'), 15_000);
  const sCall = vendorCalls().find((c) => c.argv?.includes('scenario:stubborn'));
  await host.call('turn.cancel', { turnId: stubborn.turnId });
  await host.waitFor(() => !alive(sCall?.pid ?? 0), 12_000);
  const terms = vendorCalls().filter((c) => c.signal === 'SIGTERM' && c.pid === sCall?.pid).length;
  check('G8 a child that ignores SIGTERM gets a second SIGTERM (and leaves)', terms >= 2 && !alive(sCall?.pid ?? 0), `${terms} SIGTERMs, alive=${alive(sCall?.pid ?? 0)}`);
  const sReg = host.journal.find((e) => e.method === 'host.registerSpawn' && e.params.turnId === stubborn.turnId)?.params;
  await host.waitFor(() => host.journal.some((e) => e.method === 'host.exited' && e.params.spawnId === sReg?.spawnId), 5_000);
  const sExited = host.journal.find((e) => e.method === 'host.exited' && e.params.spawnId === sReg?.spawnId)?.params;
  check('G9 it was never SIGKILLed (exit 143, no signal)', sExited?.signal !== 'SIGKILL' && sExited?.exitCode === 143, JSON.stringify(sExited));

  // ── H. refusals ────────────────────────────────────────────────────────────
  host.allowed.delete(binaries.codex);
  const beforeH = host.journal.length;
  const refused = await turn({ prompt: 'scenario:ok refused' });
  host.allowed.add(binaries.codex);
  eq('H1 a spawn the host refuses ends the turn with an exec error', `${refused.events.at(-1)?.event.kind}:${refused.events.at(-1)?.event.class}`, 'error:exec');
  check('H2 … and nothing was spawned', !host.journal.slice(beforeH).some((e) => e.method === 'host.spawned'));
  check('H3 … and the fake never ran', !vendorCalls().some((c) => c.argv?.includes('scenario:ok refused')));
  await host.configure({ binaries: { codex: binaries.codex }, accounts });
  const notInstalled = await host.callError('turn.start', startParams({ vendor: 'cursor', accountId: A.cursor }));
  eq('H4 a vendor without a configured binary is UNSUPPORTED', notInstalled?.code, -32004);
  eq('H5 … cap vendorNotInstalled', notInstalled?.data?.cap, 'vendorNotInstalled');
  await host.configure({ binaries: { codex: binaries.codex, cursor: binaries.cursor, agy: binaries.agy }, accounts });
  const unknownField = await host.callError('turn.start', { ...startParams({}), extra: 1 });
  eq('H6 an unknown field is rejected', `${unknownField?.code}:${unknownField?.data?.rule}`, '-32602:unknown');
  const badUid = await host.callError('turn.start', startParams({ sessionUid: 's_nope' }));
  eq('H7 a malformed session uid is rejected by value', badUid?.data?.rule, 'pattern');
  const emptyPrompt = await host.callError('turn.start', startParams({ prompt: '' }));
  eq('H8 an empty prompt is rejected', emptyPrompt?.data?.field, 'prompt');
  const missing = await host.callError('turn.start', (({ resumeId, ...rest }) => rest)(startParams({})));
  eq('H9 a missing field is rejected', missing?.data?.rule, 'required');
  const relCwd = await host.callError('turn.start', startParams({ cwd: 'relative/dir' }));
  eq('H10 a relative cwd is rejected', relCwd?.data?.field, 'cwd');
  const flagId = await host.callError('turn.start', startParams({ resumeId: '--help', model: 'm' }));
  eq('H11 a flag-shaped resume id is rejected', flagId?.data?.field, 'resumeId');
  const dupe = startParams({ prompt: 'scenario:hang' });
  await host.call('turn.start', dupe);
  const dupeErr = await host.callError('turn.start', { ...dupe });
  eq('H12 a duplicate live turnId is rejected', dupeErr?.data?.rule, 'unique');
  await host.call('turn.cancel', { turnId: dupe.turnId });
  await host.waitFor(() => terminalOf(host.turnEvents(dupe.turnId)).length > 0, 5_000);

  // ── I. the pump with a scripted source ─────────────────────────────────────
  await host.call('dev.turnScript', { turnId: 'script-throw', events: [{ type: 'text', text: 'partial' }], end: 'throw', throwMessage: 'Not logged in · Please run /login' });
  await host.waitFor(() => terminalOf(host.turnEvents('script-throw')).length > 0);
  const st = host.turnEvents('script-throw');
  deepEq('I1 a throwing source: text flushed, then one auth error', st.map((e) => `${e.event.kind}${e.event.class ? `:${e.event.class}` : ''}`), ['text', 'error:auth']);
  await host.call('dev.turnScript', { turnId: 'script-natural', events: [{ type: 'text', text: 'x' }], end: 'natural' });
  await host.waitFor(() => terminalOf(host.turnEvents('script-natural')).length > 0);
  eq('I2 a source that just ends gets one synthesized exec error', host.turnEvents('script-natural').at(-1)?.event.class, 'exec');
  await host.call('dev.turnScript', {
    turnId: 'script-twice',
    events: [
      { type: 'done', result: { vendor: 'codex', conversationId: 'c1', text: 'one', status: 'ok' } },
      { type: 'error', message: 'late' },
      { type: 'text', text: 'after' },
    ],
  });
  await host.waitFor(() => terminalOf(host.turnEvents('script-twice')).length > 0);
  await new Promise((r) => setTimeout(r, 150));
  deepEq('I3 nothing follows the first terminal', host.turnEvents('script-twice').map((e) => e.event.kind), ['done']);
  await host.call('dev.turnScript', { turnId: 'script-status-error', events: [{ type: 'done', result: { vendor: 'codex', conversationId: 'c2', text: '', status: 'error', note: 'usage limit reached' } }] });
  await host.waitFor(() => terminalOf(host.turnEvents('script-status-error')).length > 0);
  eq('I4 a result with status error is an error event (class from the note)', host.turnEvents('script-status-error').at(-1)?.event.class, 'quota');
  await host.call('dev.turnScript', { turnId: 'script-hang', events: [{ type: 'session', conversationId: 'conv-9' }], end: 'hang' });
  await host.waitFor(() => host.turnEvents('script-hang').length > 0);
  await host.call('turn.cancel', { turnId: 'script-hang' });
  await host.waitFor(() => terminalOf(host.turnEvents('script-hang')).length > 0, 3_000);
  const shLast = host.turnEvents('script-hang').at(-1)?.event;
  eq('I5 a cancel during a silent source: done{stopped} with the last session id', `${shLast?.status}:${shLast?.conversationId}`, 'stopped:conv-9');

  // ── J. the wall clock ──────────────────────────────────────────────────────
  const timed = await turn({ prompt: 'scenario:hang', timeoutMs: 1_200 }, 15_000);
  eq('J1 a turn past its wall clock ends incomplete', `${timed.events.at(-1)?.event.kind}:${timed.events.at(-1)?.event.status}`, 'done:incomplete');
  const tReg = host.journal.find((e) => e.method === 'host.registerSpawn' && e.params.turnId === timed.params.turnId)?.params;
  const tExit = host.journal.find((e) => e.method === 'host.exited' && e.params.spawnId === tReg?.spawnId)?.params;
  check('J2 … through SIGTERM, reported as timedOut', tExit?.timedOut === true && tExit?.signal === 'SIGTERM', JSON.stringify(tExit));

  check('Z1 nothing but JSON-RPC reached stdout', host.skipped === 0, String(host.skipped));
}

const guard = setTimeout(() => {
  process.stdout.write('\nturns: TIMED OUT\n');
  host.child.kill('SIGKILL');
  process.exit(1);
}, 180_000);
guard.unref();

main()
  .catch((err) => {
    check('no unexpected exception', false, err?.stack ?? String(err));
  })
  .finally(async () => {
    await host.close();
    const failures = summary();
    if (failures > 0) for (const l of host.stderrLines.slice(-20)) process.stdout.write(`  | ${l}\n`);
    process.exit(failures > 0 ? 1 : 0);
  });
