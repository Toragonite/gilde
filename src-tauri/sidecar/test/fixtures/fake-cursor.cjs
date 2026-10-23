#!/usr/bin/env node
// fake cursor-agent — `cursor-agent -p --output-format stream-json … [--resume <id>] … -- <prompt>`,
// `cursor-agent models`, `cursor-agent status`. Scenario: `scenario:<name>` in the prompt.
const fs = require('node:fs');
const argv = process.argv.slice(2);
const LOGGED = ['CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'AGY_HOME', 'GILDE_SPAWN_ID', 'GILDE_APP_INSTANCE'];
if (process.env.FAKE_VENDOR_LOG) {
  const env = {};
  for (const k of LOGGED) env[k] = process.env[k] ?? null;
  fs.appendFileSync(process.env.FAKE_VENDOR_LOG, JSON.stringify({ vendor: 'cursor', argv, env, pid: process.pid }) + '\n');
}
const out = (o) => process.stdout.write(JSON.stringify(o) + '\n');
if (argv[0] === 'models') {
  process.stdout.write('Available models\n\nauto - Auto\ncomposer-2.5 - Composer 2.5\nbad id - Not a model\n');
  process.exit(0);
}
if (argv[0] === 'status') {
  if (process.env.FAKE_CURSOR_STATUS === 'loggedout') {
    process.stdout.write('Not logged in\n');
    process.exit(1);
  }
  process.stdout.write('Logged in as fake-user\n');
  process.exit(0);
}
const at = argv.indexOf('--resume');
const resume = at >= 0 ? argv[at + 1] : undefined;
const dash = argv.indexOf('--');
const prompt = dash >= 0 ? argv[dash + 1] ?? '' : '';
const scenario = (/scenario:([a-z-]+)/.exec(prompt) ?? [])[1] ?? 'ok';
const SID = resume ?? '7c1d2e3f-0000-4000-8000-0000000c0501';
const delta = (text) => out({ type: 'assistant', timestamp_ms: Date.now(), message: { content: [{ type: 'text', text }] } });
const result = (session, text) =>
  out({ type: 'result', subtype: 'success', is_error: false, result: text, session_id: session, usage: { input_tokens: 5, output_tokens: 9 } });
switch (scenario) {
  case 'ok':
    out({ type: 'system', subtype: 'init', session_id: SID });
    delta('hello ');
    delta('from cursor');
    result(SID, 'hello from cursor');
    process.exit(0);
    break;
  case 'new-chat': {
    // An id cursor does not know silently starts a NEW chat (R2 C7).
    const fresh = '7c1d2e3f-0000-4000-8000-0000000c0599';
    out({ type: 'system', subtype: 'init', session_id: fresh });
    delta('fresh chat');
    result(fresh, 'fresh chat');
    process.exit(0);
    break;
  }
  case 'deltas': {
    out({ type: 'system', subtype: 'init', session_id: SID });
    let all = '';
    for (let i = 0; i < 400; i += 1) {
      const t = `d${i};`;
      all += t;
      delta(t);
    }
    result(SID, all);
    process.exit(0);
    break;
  }
  case 'big': {
    out({ type: 'system', subtype: 'init', session_id: SID });
    // 40 KiB in one delta, with a 4-byte character straddling the 16 KiB cut.
    const big = 'a'.repeat(16 * 1024 - 2) + '😀' + 'b'.repeat(24 * 1024);
    delta(big);
    result(SID, big);
    process.exit(0);
    break;
  }
  case 'no-result':
    out({ type: 'system', subtype: 'init', session_id: SID });
    delta('half an answer');
    process.exit(0);
    break;
  case 'hang':
    out({ type: 'system', subtype: 'init', session_id: SID });
    delta('thinking…');
    setInterval(() => {}, 1000);
    break;
  default:
    process.stderr.write(`fake cursor: unknown scenario ${scenario}\n`);
    process.exit(2);
}
