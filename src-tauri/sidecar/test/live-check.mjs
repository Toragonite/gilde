#!/usr/bin/env node
/**
 * live-check.mjs — the protocol-2 channel driven against REAL vendor CLIs.
 *
 * `turns.mjs` / `probes.mjs` prove protocol 2 with fake CLIs; nothing there can tell
 * you whether the argv the adapters build is still the argv today's `codex` accepts,
 * or whether the claude control-protocol quota probe still gets a `control_response`.
 *
 * THIS SCRIPT RUNS REAL VENDOR CLIs AND SPENDS QUOTA. It is never part of `npm test`
 * or any build gate, a build worker never runs it, and it refuses to start unless
 * BOTH are set (the same discipline as the lead's probe kit, `scripts/live-probe/`,
 * under OQ1):
 *
 *   GILDE_LIVE_CHECK=1
 *   GILDE_LIVE_CLAUDE_CONFIG_DIR=<an absolute probe-account config dir>   (never the default ~/.claude)
 *
 * Run:  GILDE_LIVE_CHECK=1 GILDE_LIVE_CLAUDE_CONFIG_DIR=… node test/live-check.mjs
 */

import { execFileSync } from 'node:child_process';
import { mkdtempSync, realpathSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { isAbsolute, join } from 'node:path';

import { accountId, harness, sessionUid, startSidecar } from './fixtures/host.mjs';

const configDir = process.env.GILDE_LIVE_CLAUDE_CONFIG_DIR ?? '';
if (process.env.GILDE_LIVE_CHECK !== '1' || !isAbsolute(configDir) || configDir === join(homedir(), '.claude')) {
  process.stdout.write(
    'live-check: refused — it runs real vendor CLIs. Set GILDE_LIVE_CHECK=1 and ' +
      'GILDE_LIVE_CLAUDE_CONFIG_DIR to a probe account (never ~/.claude). Not part of npm test.\n',
  );
  process.exit(2);
}

const { check, summary } = harness('live-check');
const which = (name) => {
  try {
    return realpathSync(execFileSync('/usr/bin/which', [name], { encoding: 'utf8' }).trim());
  } catch {
    return undefined;
  }
};
const binaries = Object.fromEntries(
  [
    ['claude', which('claude')],
    ['codex', which('codex')],
  ].filter(([, p]) => p !== undefined),
);
const work = mkdtempSync(join(tmpdir(), 'gilde-live-check-'));
execFileSync('git', ['init', '-q'], { cwd: work, stdio: 'ignore' });
const A = { claude: accountId('live-claude'), codex: accountId('live-codex') };
const host = startSidecar({ selftest: false });

async function main() {
  await host.hello();
  await host.configure({
    binaries,
    accounts: [
      { accountId: A.claude, vendor: 'claude', envKey: 'CLAUDE_CONFIG_DIR', configDir: realpathSync(configDir) },
      { accountId: A.codex, vendor: 'codex', envKey: null, configDir: null },
    ],
  });
  if (binaries.claude !== undefined) {
    const quota = await host.call('probe.quota', { vendor: 'claude', accountId: A.claude }, 120_000);
    check('live-1 probe.quota(claude) returned windows', Array.isArray(quota?.windows) && quota.windows.length > 0, JSON.stringify(quota));
    const models = await host.call('probe.models', { vendor: 'claude', accountId: A.claude });
    check('live-2 probe.models(claude) is non-empty', Array.isArray(models) && models.length > 0, JSON.stringify(models));
  }
  if (binaries.codex !== undefined) {
    const turnId = 'live-codex';
    await host.call('turn.start', {
      turnId,
      sessionUid: sessionUid(1),
      vendor: 'codex',
      accountId: A.codex,
      cwd: realpathSync(work),
      model: null,
      permissionMode: 'plan',
      prompt: 'Reply with the single word: ok',
      resumeId: null,
      timeoutMs: 180_000,
    });
    await host.waitFor(() => host.turnEvents(turnId).some((e) => e.event.kind === 'done' || e.event.kind === 'error'), 200_000);
    const last = host.turnEvents(turnId).at(-1)?.event;
    check('live-3 a real codex turn ends done{ok}', last?.kind === 'done' && last.status === 'ok', JSON.stringify(last));
  }
}

main()
  .catch((err) => check('no unexpected exception', false, err?.stack ?? String(err)))
  .finally(async () => {
    await host.close();
    process.exit(summary() > 0 ? 1 : 0);
  });
