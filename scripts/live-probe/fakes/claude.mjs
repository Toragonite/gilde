#!/usr/bin/env node
/**
 * fake claude — the live-probe kit's stand-in for dry runs (contract §14.1
 * "probe runner … a dry-run mode against the fakes", §15.5 T19 "fixtures from
 * the R6 shapes").
 *
 * Every frame shape below is synthesized from the key sets recorded in
 * research/R6-live-probes.md (§3–§6); no recorded probe output is copied and
 * all text is neutral filler. It never contacts a network, never reads a
 * credential, and writes only inside its own CLAUDE_CONFIG_DIR (registry row,
 * transcript rows, its own trust list) and the files a probe asks it to
 * create in the scratch project.
 *
 * Modes:
 *  - `--version` → `<FAKE_CLAUDE_VERSION> (Claude Code)`;
 *  - stream-json (`--input-format stream-json`): the L1 protocol — initialize,
 *    system/init, replay echo, command_lifecycle, session_state_changed,
 *    can_use_tool (Bash, Write, AskUserQuestion, ExitPlanMode,
 *    SandboxNetworkAccess, concurrent subagent asks), the control_response
 *    echo, results with terminal_reason, get_usage, interrupt receipts,
 *    list_permission_rules, rate_limit_event, the EOF rules of R6 LP-04/A-P4,
 *    the logged-out shapes of LP-09(b);
 *  - TUI (anything else, in a PTY): the folder-trust dialog (unnumbered, ↓ +
 *    Enter), a permission prompt and a choice prompt answered by digits
 *    without CR, a hosted lead that drives `tmux` on PATH through the R6 §5
 *    sequence, and the exit dialog with teammates alive (LP-38).
 * Knobs (env, dry runs only): FAKE_CLAUDE_VERSION, FAKE_CLAUDE_UTILIZATION
 * (rate_limit_event five_hour fraction), FAKE_CLAUDE_EOF_DELAY_MS.
 */

import { randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { appendFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SELF = fileURLToPath(import.meta.url);
const argv = process.argv.slice(2);
const VERSION = process.env.FAKE_CLAUDE_VERSION || '2.1.999';

function flag(name) {
  const i = argv.indexOf(name);
  return i >= 0 && i + 1 < argv.length ? argv[i + 1] : null;
}
const has = (name) => argv.includes(name);

if (has('--version')) {
  process.stdout.write(`${VERSION} (Claude Code)\n`);
  process.exit(0);
}

const CFG = process.env.CLAUDE_CONFIG_DIR;
if (!CFG) {
  process.stderr.write('fake claude: CLAUDE_CONFIG_DIR is required\n');
  process.exit(2);
}
const CWD = process.cwd();
const SESSION_ID = flag('--resume') || flag('--session-id') || randomUUID();
const RESUMED = has('--resume');
const MODEL = flag('--model') || 'default';
let permissionMode = flag('--permission-mode') || 'default';
const SIGNED_IN = existsSync(join(CFG, '.claude.json'));
const STREAM = flag('--input-format') === 'stream-json';
const REPLAY = has('--replay-user-messages');
const EMIT_STATE = process.env.CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS === '1';
const AGENT_ID = flag('--agent-id');
const T0 = Date.now();

// ---------------------------------------------------------------------------
// Registry and transcript (R6 §6 A-P1 key set, §3 LP-08 path)
// ---------------------------------------------------------------------------

const regPath = join(CFG, 'sessions', `${process.pid}.json`);
const registry = {
  pid: process.pid,
  sessionId: SESSION_ID,
  cwd: CWD,
  startedAt: T0,
  procStart: new Date(T0).toString().slice(0, 24),
  version: VERSION,
  peerProtocol: 1,
  peerFeatures: [],
  kind: 'interactive',
  entrypoint: process.env.CLAUDE_CODE_ENTRYPOINT || (STREAM ? 'sdk-cli' : 'cli'),
  pidDomain: 'host',
  messagingSocketPath: join(CFG, 'sockets', `${process.pid}.sock`),
  name: 'session',
  nameSource: 'default',
  nameSince: T0,
  status: 'idle',
  updatedAt: T0,
  statusUpdatedAt: T0,
};
function writeRegistry(patch = {}) {
  if (AGENT_ID) return; // teammates have no registry row (R6 §5)
  Object.assign(registry, patch, { updatedAt: Date.now() });
  if (patch.status) registry.statusUpdatedAt = Date.now();
  mkdirSync(dirname(regPath), { recursive: true });
  writeFileSync(regPath, JSON.stringify(registry));
}
function removeRegistry() {
  if (registry.kind !== 'bg') rmSync(regPath, { force: true });
}
const transcript = join(CFG, 'projects', CWD.replace(/[^A-Za-z0-9]/g, '-'), `${SESSION_ID}.jsonl`);
function row(type, extra = {}) {
  mkdirSync(dirname(transcript), { recursive: true });
  appendFileSync(transcript, JSON.stringify({ type, uuid: randomUUID(), timestamp: new Date().toISOString(), sessionId: SESSION_ID, cwd: CWD, ...extra }) + '\n');
}
writeRegistry();
const finish = (code) => {
  removeRegistry();
  process.exit(code);
};
process.on('SIGTERM', () => finish(143));
process.on('SIGHUP', () => finish(129));

// ---------------------------------------------------------------------------
// Permission rules: session rules (AllowSession), project settings (LP-37)
// ---------------------------------------------------------------------------

const sessionRules = [];
function projectAllows(tool, content) {
  for (const f of [join(CWD, '.claude', 'settings.json'), join(CWD, '.claude', 'settings.local.json')]) {
    try {
      const allow = JSON.parse(readFileSync(f, 'utf8'))?.permissions?.allow ?? [];
      if (allow.includes(`${tool}(${content})`) || allow.includes(tool)) return true;
    } catch {
      // no file
    }
  }
  return false;
}
const sessionAllows = (tool, content) => sessionRules.some((r) => r.toolName === tool && (r.ruleContent === undefined || r.ruleContent === content));
const sandboxOn = (() => {
  try {
    return JSON.parse(flag('--settings') ?? '{}')?.sandbox?.enabled === true;
  } catch {
    return false;
  }
})();

if (STREAM) runStream();
else runTui();

// ===========================================================================
// stream-json (L1)
// ===========================================================================

function runStream() {
  const out = (o) => process.stdout.write(JSON.stringify(o) + '\n');
  const state = (s) => {
    if (EMIT_STATE) out({ type: 'system', subtype: 'session_state_changed', state: s, session_id: SESSION_ID, uuid: randomUUID() });
    writeRegistry({ status: s === 'requires_action' ? 'waiting' : s === 'running' ? 'busy' : 'idle', ...(s === 'requires_action' ? { waitingFor: 'permission prompt' } : { waitingFor: undefined }) });
  };
  const util = Number(process.env.FAKE_CLAUDE_UTILIZATION ?? '0.15');
  const rateLimit = () => out({
    type: 'rate_limit_event',
    rate_limit_info: { status: 'allowed', resetsAt: Math.floor(Date.now() / 1000) + 3600, rateLimitType: 'five_hour', overageStatus: 'rejected', isUsingOverage: false, unifiedWindows: { five_hour: { utilization: util, resetsAt: 0 }, seven_day: { utilization: 0.4, resetsAt: 0 } } },
    uuid: randomUUID(), session_id: SESSION_ID,
  });
  const capabilities = ['interrupt_receipt_v1', 'interrupt_cancel_queued_v1', 'interrupt_send_now_v1', 'msg_lifecycle_v1', 'request_marker_lists_v1', 'ui_surface_v1', 'mcp_call_result_not_for_model_v1'];

  let pending = new Map(); // request_id → { tool, input, toolUseId, agentId, resolve }
  let lastError = false;
  let turnActive = false;
  let interruptTurn = null;
  const queued = [];
  let busy = Promise.resolve();

  const init = (userUuids) => out({
    type: 'system', subtype: 'init', cwd: CWD, session_id: SESSION_ID, tools: ['Bash', 'Read', 'Edit', 'Write', 'AskUserQuestion', 'Agent'], mcp_servers: [], model: `claude-${MODEL}-0-0`,
    permissionMode, slash_commands: [], terminal_slash_commands: [], apiKeySource: SIGNED_IN ? 'oauth' : 'none', claude_code_version: VERSION, output_style: 'default',
    agents: [], skills: [], plugins: [], capabilities, analytics_disabled: true, product_feedback_disabled: true, uuid: randomUUID(), memory_paths: [], original_cwd: CWD,
    additional_directories: [], messaging_socket_path: registry.messagingSocketPath, fast_mode_state: 'off', fast_mode_disabled_reason: null, per_turn_effort_active: false, view_mode: 'default',
    ...(userUuids ? { user_message_uuids: userUuids } : {}),
  });

  const result = ({ subtype = 'success', isError = false, terminal = 'completed', stop = 'end_turn', denials = [], uuids = null, apiError = null }) => {
    lastError = isError;
    out({
      type: 'result', subtype, is_error: isError, duration_ms: 900, duration_api_ms: 700, num_turns: 1, result: 'Done.', stop_reason: stop, session_id: SESSION_ID,
      total_cost_usd: 0.001, usage: { input_tokens: 10, output_tokens: 5 }, modelUsage: {}, permission_denials: denials, terminal_reason: terminal, fast_mode_state: 'off',
      fast_mode_disabled_reason: null, origin: { kind: 'human' }, subagent_stats: null, safety_stops: [], api_error_status: apiError, ttft_ms: 300, uuid: randomUUID(),
      ttft_stream_ms: 310, time_to_request_ms: 5, first_content_frame_ms: 320, user_message_uuids: uuids, request_sent_wall_ms: Date.now(), first_request_input_tokens: 10,
      queued_turn_count: queued.length, result_index: 0,
    });
  };

  const usage = { input_tokens: 10, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: 5, service_tier: 'standard' };
  const assistantText = (text) => {
    const id = `msg_${randomUUID().slice(0, 8)}`;
    out({ type: 'assistant', message: { id, role: 'assistant', model: MODEL, content: [{ type: 'text', text }], usage }, parent_tool_use_id: null, session_id: SESSION_ID, uuid: randomUUID() });
    row('assistant', { message: { id, role: 'assistant', content: [{ type: 'text', text: '<text>' }], usage } });
  };
  const toolResult = (toolUseId, content, isError, parent = null) => out({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: toolUseId, content, is_error: isError }] }, parent_tool_use_id: parent, session_id: SESSION_ID, uuid: randomUUID() });

  /** Ask for one tool; resolves with the host's decision. */
  const ask = (tool, input, { suggestions = [], agentId = null, uuidToolId = false, bracket = true, extra = {} } = {}) => new Promise((resolveDecision) => {
    const toolUseId = uuidToolId ? randomUUID() : `toolu_${randomUUID().replace(/-/g, '').slice(0, 20)}`;
    const requestId = randomUUID();
    out({ type: 'stream_event', event: { type: 'content_block_start', index: 0, content_block: { type: 'tool_use', id: toolUseId, name: tool } }, session_id: SESSION_ID, uuid: randomUUID() });
    out({ type: 'stream_event', event: { type: 'content_block_delta', index: 0, delta: { type: 'input_json_delta', partial_json: '{}' } }, session_id: SESSION_ID, uuid: randomUUID() });
    out({ type: 'assistant', message: { id: `msg_${randomUUID().slice(0, 8)}`, role: 'assistant', model: MODEL, content: [{ type: 'tool_use', id: toolUseId, name: tool, input }] }, parent_tool_use_id: null, session_id: SESSION_ID, uuid: randomUUID() });
    out({ type: 'stream_event', event: { type: 'content_block_stop', index: 0 }, session_id: SESSION_ID, uuid: randomUUID() });
    if (bracket && !agentId) state('requires_action');
    pending.set(requestId, { tool, input, toolUseId, agentId, resolve: resolveDecision });
    out({
      type: 'control_request', request_id: requestId,
      request: { subtype: 'can_use_tool', tool_name: tool, display_name: tool, input, description: `Use ${tool}`, permission_suggestions: suggestions, blocked_path: null, tool_use_id: toolUseId, ...(agentId ? { agent_id: agentId, decision_reason_type: 'subcommandResults' } : {}), ...extra },
    });
    out({ type: 'stream_event', event: { type: 'message_delta', delta: { stop_reason: 'tool_use' } }, session_id: SESSION_ID, uuid: randomUUID() });
    out({ type: 'stream_event', event: { type: 'message_stop' }, session_id: SESSION_ID, uuid: randomUUID() });
    rateLimit();
  });

  const bashSuggestions = (command) => [
    { type: 'addRules', rules: [{ toolName: 'Bash', ruleContent: command }], behavior: 'allow', destination: 'localSettings' },
    { type: 'addDirectories', directories: [CWD], destination: 'session' },
    { type: 'setMode', mode: 'acceptEdits', destination: 'session' },
  ];

  /** Apply an allow decision's updatedPermissions (session rules, a mode change). */
  const applyPermissions = (decision) => {
    for (const u of decision?.updatedPermissions ?? []) {
      if (u.destination !== 'session') continue;
      if (u.type === 'addRules') for (const r of u.rules ?? []) sessionRules.push(r);
      if (u.type === 'setMode' && typeof u.mode === 'string') {
        permissionMode = u.mode;
        out({ type: 'system', subtype: 'status', status: null, permissionMode, session_id: SESSION_ID, uuid: randomUUID() });
      }
    }
  };

  /** One Bash call: auto-allowed by a rule, else asked. Returns 'ran' | 'denied' | 'interrupted' | 'errored'. */
  const bash = async (command, denials) => {
    if (projectAllows('Bash', command) || sessionAllows('Bash', command) || permissionMode === 'bypassPermissions') {
      const id = `toolu_${randomUUID().slice(0, 12)}`;
      out({ type: 'assistant', message: { role: 'assistant', content: [{ type: 'tool_use', id, name: 'Bash', input: { command } }] }, parent_tool_use_id: null, session_id: SESSION_ID, uuid: randomUUID() });
      runCommand(command);
      toolResult(id, 'ok', false);
      return 'ran';
    }
    const { decision, toolUseId, kind } = await ask('Bash', { command, description: 'Run a command' }, { suggestions: bashSuggestions(command) });
    if (kind === 'eof') {
      toolResult(toolUseId, 'Tool permission stream closed before response received', true);
      denials.push({ tool_name: 'Bash', tool_use_id: toolUseId });
      return 'denied';
    }
    if (kind === 'error') {
      toolResult(toolUseId, `Tool permission request failed: Error: ${decision}`, true);
      denials.push({ tool_name: 'Bash', tool_use_id: toolUseId });
      return 'errored';
    }
    if (decision.behavior === 'allow') {
      applyPermissions(decision);
      state('running');
      runCommand(command);
      toolResult(toolUseId, 'ok', false);
      return 'ran';
    }
    state('running');
    denials.push({ tool_name: 'Bash', tool_use_id: toolUseId });
    if (decision.interrupt) {
      toolResult(toolUseId, 'The user does not want to proceed with this tool use.', true);
      out({ type: 'user', message: { role: 'user', content: [{ type: 'text', text: '[Request interrupted by user for tool use]' }] }, session_id: SESSION_ID, uuid: randomUUID() });
      return 'interrupted';
    }
    toolResult(toolUseId, typeof decision.message === 'string' ? decision.message : 'denied', true);
    return 'denied';
  };

  const runCommand = (command) => {
    const touch = /^touch\s+([\w.-]+)$/.exec(command.trim());
    if (touch) writeFileSync(join(CWD, touch[1]), '');
  };

  /** One turn for a user frame. */
  const turn = async (frame) => {
    turnActive = true;
    const text = Array.isArray(frame.message?.content) ? frame.message.content.map((b) => b?.text ?? '').join(' ') : String(frame.message?.content ?? '');
    const uuid = typeof frame.uuid === 'string' ? frame.uuid : null;
    if (uuid) {
      out({ type: 'command_lifecycle', command_uuid: uuid, state: 'queued' });
      out({ type: 'command_lifecycle', command_uuid: uuid, state: 'started' });
    }
    state('running');
    init(uuid ? [uuid] : null);
    out({ type: 'system', subtype: 'status', status: 'requesting', session_id: SESSION_ID, uuid: randomUUID() });
    if (REPLAY) out({ type: 'user', message: frame.message, isReplay: true, uuid: uuid ?? randomUUID(), ...(frame.origin ? { origin: frame.origin } : {}), session_id: SESSION_ID });
    out({ type: 'stream_event', event: { type: 'message_start', message: { id: `msg_${randomUUID().slice(0, 8)}` } }, user_message_uuid: uuid, user_message_uuids: uuid ? [uuid] : [], session_id: SESSION_ID, uuid: randomUUID() });
    row('user', { message: { role: 'user', content: '<text>' } });
    const denials = [];
    const end = (opts) => {
      result({ uuids: uuid ? [uuid] : null, denials, ...opts });
      if (uuid) out({ type: 'command_lifecycle', command_uuid: uuid, state: opts.terminal === 'aborted_streaming' ? 'cancelled' : 'completed' });
      state('idle');
      turnActive = false;
    };

    if (!SIGNED_IN) {
      out({ type: 'assistant', error: 'authentication_failed', message: { role: 'assistant', content: [{ type: 'text', text: 'Not logged in' }] }, session_id: SESSION_ID, uuid: randomUUID() });
      end({ isError: true, terminal: 'api_error' });
      return;
    }
    const lower = text.toLowerCase();
    if (lower.includes('subagent')) {
      // Two concurrent subagent asks (R6 LP-07c): distinct request ids and agent ids, no bracket.
      const a = ask('Bash', { command: 'echo a' }, { agentId: `a${randomUUID().replace(/-/g, '').slice(0, 16)}` });
      const b = ask('Bash', { command: 'echo b' }, { agentId: `a${randomUUID().replace(/-/g, '').slice(0, 16)}` });
      for (const r of await Promise.all([a, b])) {
        if (r.kind === 'error') {
          toolResult(r.toolUseId, `Tool permission request failed: Error: ${r.decision}`, true, 'toolu_agent');
          denials.push({ tool_name: 'Bash', tool_use_id: r.toolUseId });
          const retry = await ask('Bash', { command: 'echo retry' }, { agentId: r.agentId });
          toolResult(retry.toolUseId, 'ok', false, 'toolu_agent');
        } else toolResult(r.toolUseId, 'ok', false, 'toolu_agent');
      }
      state('running');
      assistantText('Both subagents finished.');
      end({});
      return;
    }
    if (lower.includes('question')) {
      const questions = [{ question: 'Which colour?', header: 'Colour', options: [{ label: 'red', description: 'Red' }, { label: 'blue', description: 'Blue' }], multiSelect: false }];
      const r = await ask('AskUserQuestion', { questions }, { extra: { requires_user_interaction: true } });
      state('running');
      if (r.kind === 'success' && r.decision.behavior === 'allow') toolResult(r.toolUseId, `answers: ${Object.keys(r.decision.updatedInput?.answers ?? {}).length}`, false);
      else toolResult(r.toolUseId, 'not answered', true);
      assistantText('Noted.');
      end({});
      return;
    }
    if (permissionMode === 'plan') {
      const r = await ask('ExitPlanMode', { plan: 'A short plan.', planFilePath: join(CFG, 'plans', 'plan.md') }, { extra: { requires_user_interaction: true } });
      state('running');
      if (r.kind === 'success' && r.decision.behavior === 'allow') {
        permissionMode = 'default';
        out({ type: 'system', subtype: 'status', status: null, permissionMode, session_id: SESSION_ID, uuid: randomUUID() });
        toolResult(r.toolUseId, 'User has approved your plan.', false);
      } else toolResult(r.toolUseId, r.decision?.message ?? 'keep planning', true);
      assistantText('Plan step done.');
      end({});
      return;
    }
    if (lower.includes('curl') && sandboxOn) {
      const r = await ask('SandboxNetworkAccess', { host: 'example.com' }, { uuidToolId: true, bracket: false, suggestions: [{ type: 'addRules', rules: [{ toolName: 'WebFetch', ruleContent: 'domain:example.com' }], behavior: 'allow', destination: 'localSettings' }] });
      toolResult(r.toolUseId, r.kind === 'success' && r.decision.behavior === 'allow' ? '200' : 'blocked', !(r.kind === 'success' && r.decision.behavior === 'allow'));
      assistantText('Fetched.');
      end({});
      return;
    }
    if (lower.includes('sleep')) {
      out({ type: 'system', subtype: 'task_started', task_id: 't1', task_type: 'local_bash', is_backgrounded: false, session_id: SESSION_ID, uuid: randomUUID() });
      await new Promise((r) => setTimeout(r, 400));
      if (interruptTurn) {
        interruptTurn = null;
        out({ type: 'system', subtype: 'task_notification', task_id: 't1', status: 'stopped', session_id: SESSION_ID, uuid: randomUUID() });
        end({ subtype: 'error_during_execution', isError: true, terminal: 'aborted_tools', stop: 'tool_use' });
        return;
      }
      out({ type: 'system', subtype: 'task_notification', task_id: 't1', status: 'completed', session_id: SESSION_ID, uuid: randomUUID() });
      assistantText('Slept.');
      end({});
      return;
    }
    const commands = [];
    const re = /\b(date|touch\s+[\w.-]+)/gi;
    let m;
    while ((m = re.exec(text)) !== null) commands.push(m[1]);
    if (lower.includes('write')) {
      const r = await ask('Write', { file_path: join(CWD, 'out.txt'), content: '<text>' }, { suggestions: [{ type: 'setMode', mode: 'acceptEdits', destination: 'session' }] });
      state('running');
      toolResult(r.toolUseId, r.kind === 'success' && r.decision.behavior === 'allow' ? 'written' : 'denied', !(r.kind === 'success' && r.decision.behavior === 'allow'));
      assistantText('Write handled.');
      end({});
      return;
    }
    for (const c of commands) {
      const r = await bash(c, denials);
      if (r === 'interrupted') {
        end({ subtype: 'error_during_execution', isError: true, terminal: 'aborted_tools', stop: 'tool_use' });
        return;
      }
    }
    assistantText(commands.length ? 'Commands handled.' : 'Hello.');
    end({});
  };

  // -- stdin ------------------------------------------------------------------
  let buf = '';
  let eof = false;
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', (d) => {
    buf += d;
    let nl;
    while ((nl = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      if (line.trim()) handle(line);
    }
  });
  process.stdin.on('end', () => {
    eof = true;
    if (pending.size > 0) {
      // R6 LP-04(b): EOF does not cancel the prompt; it fails as "stream closed", the turn goes on, exit ~6 s later (shortened here).
      for (const [rid, p] of pending) {
        pending.delete(rid);
        p.resolve({ kind: 'eof', toolUseId: p.toolUseId, agentId: p.agentId });
      }
      busy.then(() => setTimeout(() => finish(lastError ? 1 : 0), Number(process.env.FAKE_CLAUDE_EOF_DELAY_MS ?? '300')));
      return;
    }
    busy.then(() => setTimeout(() => finish(lastError ? 1 : 0), 100));
  });

  function handle(line) {
    let f;
    try {
      f = JSON.parse(line);
    } catch {
      return;
    }
    if (f.type === 'control_request') return control(f);
    if (f.type === 'control_response') {
      const r = f.response ?? {};
      const p = pending.get(r.request_id);
      if (!p) return;
      pending.delete(r.request_id);
      if (r.subtype === 'success') {
        if (REPLAY) out(f);
        p.resolve({ kind: 'success', decision: r.response ?? {}, toolUseId: p.toolUseId, agentId: p.agentId });
      } else p.resolve({ kind: 'error', decision: r.error ?? 'error', toolUseId: p.toolUseId, agentId: p.agentId });
      return;
    }
    if (f.type === 'user') {
      if (turnActive && f.priority !== 'now') {
        queued.push(f);
        if (typeof f.uuid === 'string') out({ type: 'command_lifecycle', command_uuid: f.uuid, state: 'queued' });
        return;
      }
      busy = busy.then(() => turn(f)).then(async () => {
        while (queued.length > 0 && !eof) await turn(queued.shift());
      });
    }
  }

  function control(f) {
    const rid = f.request_id;
    const sub = f.request?.subtype;
    const ok = (response) => out({ type: 'control_response', response: { subtype: 'success', request_id: rid, response, ...(sub === 'initialize' ? { pending_permission_requests: [], pending_user_dialog_requests: [] } : {}) } });
    if (sub === 'initialize') {
      ok({
        commands: [], agents: [], output_style: 'default', available_output_styles: [], user_output_styles_dir: join(CFG, 'output-styles'), models: [],
        account: SIGNED_IN ? { email: 'probe@example.invalid', organization: 'example', subscriptionType: 'test', apiProvider: 'firstParty' } : { tokenSource: 'none', apiProvider: 'firstParty' },
        pid: process.pid, claude_code_version: VERSION, current_permission_mode: permissionMode, feedback_mode: 'off', analytics_disabled: true,
        ide_rc_auto_enable_gate: false, fast_mode_state: 'off', fast_mode_disabled_reason: null, session_state: 'idle', capabilities: ['ui_surface_v1', 'mcp_call_result_not_for_model_v1'],
      });
      return;
    }
    if (sub === 'get_usage') {
      const w = (u) => ({ utilization: u, resets_at: new Date(Date.now() + 3600_000).toISOString(), limit_dollars: null, used_dollars: null, remaining_dollars: null, locked_reason: null });
      ok({ session: { total_cost_usd: 0, total_api_duration_ms: 0, total_duration_ms: 0, total_lines_added: 0, total_lines_removed: 0, model_usage: {} }, subscription_type: 'test', rate_limits_available: true, rate_limits: { five_hour: w(15), seven_day: w(40), seven_day_opus: null }, limits: [], behaviors: { day: [], week: [] } });
      return;
    }
    if (sub === 'interrupt') {
      const cancelled = f.request?.cancel_queued ? queued.splice(0).map((q) => q.uuid).filter(Boolean) : null;
      ok({ still_queued: queued.map((q) => q.uuid).filter(Boolean), ...(cancelled ? { cancelled } : {}) });
      if (cancelled) for (const u of cancelled) out({ type: 'command_lifecycle', command_uuid: u, state: 'cancelled' });
      if (turnActive) interruptTurn = true;
      return;
    }
    if (sub === 'list_permission_rules') {
      ok({ rules: sessionRules.map((r) => ({ behavior: 'allow', source: 'session', rule: `${r.toolName}(${r.ruleContent ?? ''})`, editability: 'session' })) });
      return;
    }
    out({ type: 'control_response', response: { subtype: 'error', request_id: rid, error: `unsupported: ${sub}` } });
  }
}

// ===========================================================================
// TUI (PTY)
// ===========================================================================

function runTui() {
  const w = (s) => process.stdout.write(s);
  if (process.stdin.isTTY) process.stdin.setRawMode(true);
  process.stdin.setEncoding('utf8');
  w('\u001b[?1049h\u001b[?2004h');

  const trustFile = join(CFG, 'fake-trust.json');
  const trusted = () => {
    try {
      const list = JSON.parse(readFileSync(trustFile, 'utf8'));
      return list.some((p) => CWD === p || CWD.startsWith(p + '/'));
    } catch {
      return false;
    }
  };
  const trust = () => {
    let list = [];
    try {
      list = JSON.parse(readFileSync(trustFile, 'utf8'));
    } catch {
      list = [];
    }
    list.push(resolve(CWD));
    mkdirSync(CFG, { recursive: true });
    writeFileSync(trustFile, JSON.stringify(list));
  };
  const screen = (lines) => w(`\u001b[H\u001b[2J${lines.join('\r\n')}`);

  const teammates = [];
  const teamName = `session-${randomUUID().slice(0, 8)}`;
  let mode = 'start';
  let selection = 0;
  let input = '';
  let pasting = false;

  const main = (note = '') => {
    mode = 'main';
    writeRegistry({ status: 'idle', waitingFor: undefined });
    screen([' Claude Code (fake)', '', note, '', '> ' + input, '', permissionMode === 'default' ? ' manual mode on' : ` ${permissionMode} mode on`]);
  };
  const trustDialog = () => {
    mode = 'trust';
    screen([' Do you trust the files in this folder?', '', ` ${selection === 0 ? '❯' : ' '} No, exit`, ` ${selection === 1 ? '❯' : ' '} Yes, I trust this folder`, '', ' Enter to confirm · Esc to cancel']);
  };
  const permissionPrompt = (command) => {
    mode = 'permission';
    pendingCommand = command;
    writeRegistry({ status: 'waiting', waitingFor: 'permission prompt' });
    screen([' Bash command', '', `   ${command}`, '', ' Do you want to proceed?', ' ❯ 1. Yes', '   2. Yes, and always allow access to this folder from', '      this project', '   3. No', '', ' Esc to cancel · Tab to amend']);
  };
  const choicePrompt = () => {
    mode = 'choice';
    writeRegistry({ status: 'waiting', waitingFor: 'input needed' });
    screen([' ☐ Colour', 'Which colour do you prefer?', '❯ 1. Red', '     Choose red', '  2. Blue', '     Choose blue', '  3. Type something.', '─'.repeat(24), '  4. Chat about this', 'Enter to select · ↑/↓ to navigate · Esc to cancel']);
  };
  const exitDialog = () => {
    mode = 'exit';
    screen([' Background work is running', ' The following will stop when you exit:', ...teammates.map((t) => `   ${t.name} teammate`), ' ❯ 1. Exit and stop tasks', '   2. Move to background and exit', '   3. Stay']);
  };
  let pendingCommand = null;

  const tmux = (args) => {
    try {
      return { out: execFileSync('tmux', ['-L', `claude-swarm-${process.pid}`, ...args], { encoding: 'utf8', env: process.env, stdio: ['ignore', 'pipe', 'pipe'] }), code: 0 };
    } catch (e) {
      return { out: String(e.stdout ?? ''), code: e.status ?? 1 };
    }
  };
  const spawnTeammates = (n) => {
    const names = ['alpha', 'beta', 'gamma', 'delta'];
    const colours = ['blue', 'green', 'yellow', 'purple'];
    execFileSync('tmux', ['-V'], { stdio: 'ignore', env: process.env });
    for (let i = 0; i < n; i++) {
      const name = names[i % names.length];
      let pane;
      if (tmux(['has-session', '-t', 'claude-swarm']).code !== 0) {
        pane = tmux(['new-session', '-d', '-s', 'claude-swarm', '-n', 'swarm-view', '-P', '-F', '#{pane_id}', '--', 'cat']).out.trim();
        tmux(['set-option', '-w', '-t', 'claude-swarm:swarm-view', 'pane-border-status', 'top']);
      } else {
        tmux(['list-windows', '-t', 'claude-swarm', '-F', '#{window_name}']);
        pane = tmux(['split-window', '-d', '-t', '%0', i % 2 === 1 ? '-v' : '-h', '-P', '-F', '#{pane_id}', '--', 'cat']).out.trim();
        tmux(['select-layout', '-t', 'claude-swarm:swarm-view', 'tiled']);
      }
      tmux(['list-panes', '-t', 'claude-swarm:swarm-view', '-F', '#{pane_id}']);
      tmux(['select-pane', '-t', pane, '-T', name]);
      tmux(['set-option', '-p', '-t', pane, 'remain-on-exit', 'failed']);
      const settings = flag('--settings');
      const payload = `cd ${CWD} && env CLAUDECODE=1 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 CLAUDE_CONFIG_DIR=${CFG} CLAUDE_CODE_ATTRIBUTION_PROJECT='' ${process.env.FAKE_CLAUDE_SELF || SELF} --agent-id ${name}@${teamName} --agent-name ${name} --team-name ${teamName} --agent-color ${colours[i % colours.length]} --parent-session-id ${SESSION_ID}${settings ? ` --settings '${settings}'` : ''} --model ${MODEL}`;
      tmux(['respawn-pane', '-k', '-t', pane, '--', payload]);
      teammates.push({ name, pane });
    }
    const cfgDir = join(CFG, 'teams', teamName);
    mkdirSync(join(cfgDir, 'inboxes'), { recursive: true });
    writeFileSync(join(cfgDir, 'config.json'), JSON.stringify({ name: teamName, leadSessionId: randomUUID(), members: [{ name: 'team-lead', agentId: `team-lead@${teamName}` }, ...teammates.map((t) => ({ name: t.name, agentId: `${t.name}@${teamName}`, tmuxPaneId: t.pane, backendType: 'tmux', isActive: true }))] }));
  };

  const submit = (text) => {
    input = '';
    const lower = text.toLowerCase();
    if (lower === '/exit') {
      if (teammates.length > 0) return exitDialog();
      w('\u001b[?1049l');
      return finish(0);
    }
    writeRegistry({ status: 'busy' });
    row('user', { message: { role: 'user', content: '<text>' } });
    const spawn = /spawn\s+(\d+)\s+teammates?/.exec(lower);
    if (spawn && has('--teammate-mode') && process.env.CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS === '1') {
      spawnTeammates(Math.min(4, Number(spawn[1])));
      return main(`Spawned ${teammates.length} teammates`);
    }
    if (lower.includes('question')) return choicePrompt();
    const cmd = /\b(touch\s+[\w.-]+|date)\b/.exec(lower);
    if (cmd) return permissionMode === 'bypassPermissions' ? (runCmd(cmd[1]), main('Ran the command')) : permissionPrompt(cmd[1]);
    main('Reply ready');
  };
  const runCmd = (command) => {
    const touch = /^touch\s+([\w.-]+)$/.exec(command);
    if (touch) writeFileSync(join(CWD, touch[1]), '');
  };

  const onKey = (k) => {
    if (mode === 'trust') {
      if (k === '\u001b[B') selection = 1;
      else if (k === '\u001b[A') selection = 0;
      else if (k === '\r') {
        if (selection === 1) {
          trust();
          return main('');
        }
        w('\u001b[?1049l');
        return finish(1);
      } else if (k === '\u001b') {
        w('\u001b[?1049l');
        return finish(1);
      }
      return trustDialog();
    }
    if (mode === 'permission') {
      if (k === '1' || k === '2') {
        runCmd(pendingCommand);
        writeRegistry({ status: 'busy', waitingFor: undefined });
        return main('Ran the command');
      }
      if (k === '3' || k === '\u001b') return main('Interrupted · What should Claude do instead?');
      return;
    }
    if (mode === 'choice') {
      if (/^[1-4]$/.test(k)) return main(`Answered option ${k}`);
      if (k === '\u001b') return main('Interrupted');
      return;
    }
    if (mode === 'exit') {
      if (k === '1') {
        for (const t of teammates) tmux(['kill-pane', '-t', t.pane]);
        w('\u001b[?1049l');
        return finish(0);
      }
      if (k === '2') {
        // LP-38: the lead leaves a background entry with the same session id; teammates keep running.
        registry.kind = 'bg';
        registry.jobId = SESSION_ID.slice(0, 8);
        writeRegistry({ status: 'idle' });
        w('\u001b[?1049l');
        return process.exit(0);
      }
      if (k === '3') return main('');
      return;
    }
    if (mode !== 'main') return;
    if (k.startsWith('\u001b[200~')) pasting = true;
    if (pasting) {
      const end = k.indexOf('\u001b[201~');
      input += k.replace('\u001b[200~', '').replace('\u001b[201~', '');
      if (end >= 0) pasting = false;
      return;
    }
    if (k === '\r') return submit(input.trim());
    if (k === '\u007f') input = input.slice(0, -1);
    else if (!k.startsWith('\u001b')) input += k;
  };

  process.stdin.on('data', (data) => {
    // Split escape sequences and single keys so digits act without CR.
    const keys = data.match(/\u001b\[200~[\s\S]*?(\u001b\[201~|$)|\u001b\[[0-9;]*[A-Za-z~]|\u001b|[\s\S]/g) ?? [];
    for (const k of keys) onKey(k);
  });
  process.stdin.on('end', () => finish(0));

  if (AGENT_ID) {
    // A pane teammate: idle until its pane closes.
    mode = 'teammate';
    screen([` teammate ${AGENT_ID.split('@')[0]} idle`]);
    return;
  }
  if (trusted()) main('');
  else trustDialog();
}
