#!/usr/bin/env node
// fake agy — `agy … --output-format stream-json … -p <prompt>`, `agy models`,
// `agy -p /usage --output-format json`. Scenario: `scenario:<name>` in the prompt.
const fs = require('node:fs');
const argv = process.argv.slice(2);
const LOGGED = ['CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'AGY_HOME', 'GILDE_SPAWN_ID', 'GILDE_APP_INSTANCE'];
if (process.env.FAKE_VENDOR_LOG) {
  const env = {};
  for (const k of LOGGED) env[k] = process.env[k] ?? null;
  fs.appendFileSync(process.env.FAKE_VENDOR_LOG, JSON.stringify({ vendor: 'agy', argv, env, pid: process.pid }) + '\n');
}
const out = (o) => process.stdout.write(JSON.stringify(o) + '\n');
if (argv[0] === 'models') {
  process.stdout.write('gemini-3.6-flash-high\tGemini 3.6 Flash (High)\ngemini-3.1-pro-high\tGemini 3.1 Pro (High)\n');
  process.exit(0);
}
if (argv.includes('/usage')) {
  out({
    status: 'SUCCESS',
    command: {
      name: 'usage',
      data: {
        groups: [
          {
            name: 'gemini',
            buckets: [
              { id: 'gemini-5h', window: '5h', remaining_fraction: 0.75, reset_time: '2026-10-22T18:00:00Z' },
              { id: 'gemini-weekly', window: 'weekly', remaining_fraction: 0.4, reset_time: '2026-10-27T00:00:00Z' },
            ],
          },
        ],
      },
    },
  });
  process.exit(0);
}
const prompt = argv[argv.length - 1] ?? '';
const scenario = (/scenario:([a-z-]+)/.exec(prompt) ?? [])[1] ?? 'ok';
const CID = 'a9e0c1d2-0000-4000-8000-0000000a9e01';
const step = (text) => out({ event: 'step_update', step_update: { step_type: 'agent_response', text_delta: text } });
switch (scenario) {
  case 'ok':
    out({ event: 'init', conversation_id: CID });
    step('hi ');
    step('there');
    out({ event: 'result', result: { status: 'SUCCESS', response: 'hi there', conversation_id: CID, usage: { input_tokens: 2, output_tokens: 3 } } });
    process.exit(0);
    break;
  case 'empty-id':
    step('anonymous');
    out({ event: 'result', result: { status: 'SUCCESS', response: 'anonymous' } });
    process.exit(0);
    break;
  case 'agent-error':
    out({ event: 'init', conversation_id: CID });
    out({ event: 'result', result: { status: 'ERROR', error: 'The model produced an invalid tool call.', conversation_id: CID } });
    process.exit(0);
    break;
  default:
    process.stderr.write(`fake agy: unknown scenario ${scenario}\n`);
    process.exit(2);
}
