#!/usr/bin/env node
// fake claude — only the live-usage control protocol (`-p --input-format stream-json
// --output-format stream-json --verbose`, then `control_request get_usage` on stdin),
// which the claude quota probe (usage.ts) speaks. FAKE_CLAUDE_USAGE=none answers with
// no plan limits. It exits on stdin end or SIGTERM, never needing SIGKILL.
const fs = require('node:fs');
const argv = process.argv.slice(2);
const LOGGED = ['CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'AGY_HOME', 'GILDE_SPAWN_ID', 'GILDE_APP_INSTANCE'];
if (process.env.FAKE_VENDOR_LOG) {
  const env = {};
  for (const k of LOGGED) env[k] = process.env[k] ?? null;
  fs.appendFileSync(process.env.FAKE_VENDOR_LOG, JSON.stringify({ vendor: 'claude', argv, env, pid: process.pid }) + '\n');
}
const out = (o) => process.stdout.write(JSON.stringify(o) + '\n');
out({ type: 'system', subtype: 'init', session_id: 'fake' });
let buf = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (d) => {
  buf += d;
  for (;;) {
    const nl = buf.indexOf('\n');
    if (nl < 0) break;
    const line = buf.slice(0, nl);
    buf = buf.slice(nl + 1);
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.type === 'control_request' && msg.request?.subtype === 'get_usage') {
      const response =
        process.env.FAKE_CLAUDE_USAGE === 'none'
          ? { rate_limits_available: false }
          : {
              rate_limits_available: true,
              subscription_type: 'max',
              rate_limits: {
                limits: [
                  { kind: 'session', percent: 42, severity: 'normal', resets_at: '2026-10-22T18:00:00Z' },
                  { kind: 'weekly_all', percent: 87, severity: 'warning', resets_at: '2026-10-27T00:00:00Z' },
                ],
              },
              behaviors: { advice: 'see /Users/someone/other-project/secret.ts' },
            };
      out({ type: 'control_response', response: { subtype: 'success', request_id: msg.request_id, response } });
    }
  }
});
process.stdin.on('end', () => process.exit(0));
setInterval(() => {}, 1000);
