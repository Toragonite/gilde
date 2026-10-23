#!/usr/bin/env node
// fake codex — `codex exec [resume <id>] --json … -- <prompt>` for the sidecar suites.
// The scenario is a word in the prompt: `scenario:<name>` (default `ok`). Recorded
// JSONL in the `codex exec --json` shape agent-mesh-codex parses. Never contacts
// anything; never reads a credential.
const fs = require('node:fs');
const argv = process.argv.slice(2);
const LOGGED = ['CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'AGY_HOME', 'GILDE_SPAWN_ID', 'GILDE_APP_INSTANCE'];
if (process.env.FAKE_VENDOR_LOG) {
  const env = {};
  for (const k of LOGGED) env[k] = process.env[k] ?? null;
  fs.appendFileSync(process.env.FAKE_VENDOR_LOG, JSON.stringify({ vendor: 'codex', argv, env, pid: process.pid }) + '\n');
}
const out = (o) => process.stdout.write(JSON.stringify(o) + '\n');
if (argv[0] === 'app-server') process.exit(1); // the live model listing: unavailable → static catalog
if (argv[0] !== 'exec') {
  process.stderr.write('fake codex: unknown command\n');
  process.exit(2);
}
const resume = argv[1] === 'resume' ? argv[2] : undefined;
const dash = argv.indexOf('--');
const prompt = dash >= 0 ? argv[dash + 1] ?? '' : '';
const scenario = (/scenario:([a-z-]+)/.exec(prompt) ?? [])[1] ?? 'ok';
const NEW_ID = '0199a1b2-c3d4-7e5f-8a9b-00000000c0d1';
const OTHER_ID = '0199a1b2-c3d4-7e5f-8a9b-00000000c0d2';
switch (scenario) {
  case 'ok':
    out({ type: 'thread.started', thread_id: resume ?? NEW_ID });
    out({ type: 'item.started', item: { id: 'i1', type: 'command_execution', command: 'ls -la' } });
    out({ type: 'item.completed', item: { id: 'i2', type: 'agent_message', text: 'hello from codex' } });
    out({ type: 'turn.completed', usage: { input_tokens: 11, output_tokens: 7, cached_input_tokens: 3 } });
    process.exit(0);
    break;
  case 'new-conversation':
    out({ type: 'thread.started', thread_id: OTHER_ID });
    out({ type: 'item.completed', item: { id: 'i2', type: 'agent_message', text: 'a new chat' } });
    process.exit(0);
    break;
  case 'no-id':
    out({ type: 'item.completed', item: { id: 'i2', type: 'agent_message', text: 'no id here' } });
    process.exit(0);
    break;
  case 'auth':
    out({ type: 'thread.started', thread_id: NEW_ID });
    out({ type: 'error', message: 'You are not logged in. Please run `codex login`.' });
    process.exit(1);
    break;
  case 'quota':
    out({ type: 'thread.started', thread_id: NEW_ID });
    out({ type: 'error', message: "You've hit your usage limit. Rate limit reached; try again later." });
    process.exit(1);
    break;
  case 'exec':
    process.stderr.write('fatal: the fake codex fell over\n');
    process.exit(3);
    break;
  case 'hang':
    out({ type: 'thread.started', thread_id: NEW_ID });
    setInterval(() => {}, 1000);
    break;
  case 'stubborn': {
    // Ignores the first SIGTERM, leaves on the second: the seam repeats SIGTERM
    // and never escalates to SIGKILL (contract §9.8, C5). The handler is in place
    // BEFORE the `ready` line, which is what the test waits for.
    let terms = 0;
    process.on('SIGTERM', () => {
      terms += 1;
      if (process.env.FAKE_VENDOR_LOG) {
        fs.appendFileSync(process.env.FAKE_VENDOR_LOG, JSON.stringify({ vendor: 'codex', signal: 'SIGTERM', n: terms, pid: process.pid }) + '\n');
      }
      if (terms >= 2) process.exit(143);
    });
    if (process.env.FAKE_VENDOR_LOG) {
      fs.appendFileSync(process.env.FAKE_VENDOR_LOG, JSON.stringify({ vendor: 'codex', ready: 'stubborn', pid: process.pid }) + '\n');
    }
    out({ type: 'thread.started', thread_id: NEW_ID });
    setInterval(() => {}, 1000);
    break;
  }
  default:
    process.stderr.write(`fake codex: unknown scenario ${scenario}\n`);
    process.exit(2);
}
