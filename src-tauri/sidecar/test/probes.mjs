#!/usr/bin/env node
/**
 * probes.mjs — `probe.authStatus` / `probe.models` / `probe.quota` (contract §4.5)
 * against the real bundle, the real adapters and FAKE vendor CLIs. No real vendor
 * CLI runs and no vendor endpoint is ever contacted.
 *
 *   A. before `mesh.configure` every probe is `-32006` (not ready)
 *   B. sign-in: a probe that would need a credential file answers `unknown`, never
 *      `signedOut` — and the file is NEVER OPENED: the credential files of this
 *      test's HOME are FIFOs, so an open would block the probe forever (C6)
 *   C. models: by value, junk ids dropped, the contract shape
 *   D. quota: claude through the registered duplex probe (kind `probe`), agy
 *      through `agy -p /usage`; codex / cursor need a token Gilde never reads →
 *      `null`; percentages and reset times converted; `behaviors` never crosses
 *   E. refusals: unknown fields, malformed ids, an account of another vendor, a
 *      vendor that is not installed, a spawn the host refuses (→ `unknown`)
 *   F. every probe child is registered as `probe`, with no turn, and settles
 *
 * Run:  node test/probes.mjs   (from src-tauri/sidecar, after `npm run build`)
 */

import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { accountId, harness, installFakeVendors, startSidecar } from './fixtures/host.mjs';

const { check, eq, deepEq, summary } = harness('probes');

const home = realpathSync(mkdtempSync(join(tmpdir(), 'gilde-probes-home-')));
// Credential files as FIFOs: `stat` sees them, `open(O_RDONLY)` would block until a
// writer appears — which never happens. A probe that returns at all did not open them.
mkdirSync(join(home, '.claude'));
const codexHome = join(home, '.codex-work');
mkdirSync(codexHome);
for (const fifo of [join(home, '.claude.json'), join(home, '.claude', '.credentials.json'), join(codexHome, 'auth.json'), join(home, '.codex-auth-token')]) {
  execFileSync('mkfifo', [fifo]);
}

const { binaries } = installFakeVendors();
const A = {
  claude: accountId('claude-default'),
  codex: accountId('codex-work'),
  cursor: accountId('cursor-default'),
  agy: accountId('agy-default'),
};
const accounts = [
  { accountId: A.claude, vendor: 'claude', envKey: null, configDir: null },
  { accountId: A.codex, vendor: 'codex', envKey: 'CODEX_HOME', configDir: codexHome },
  { accountId: A.cursor, vendor: 'cursor', envKey: null, configDir: null },
  { accountId: A.agy, vendor: 'agy', envKey: null, configDir: null },
];

const host = startSidecar({ env: { HOME: home } });

async function main() {
  await host.hello();

  // ── A. not ready ───────────────────────────────────────────────────────────
  for (const method of ['probe.authStatus', 'probe.models', 'probe.quota']) {
    const err = await host.callError(method, { vendor: 'claude', accountId: A.claude });
    eq(`A ${method} before configure is not ready`, err?.code, -32006);
  }
  await host.configure({ binaries, accounts });

  // ── B. sign-in ─────────────────────────────────────────────────────────────
  const claudeAuth = await host.call('probe.authStatus', { vendor: 'claude', accountId: A.claude }, 10_000);
  deepEq('B1 claude: the login lives in .claude.json, which is never opened → unknown', claudeAuth, { signIn: 'unknown' });
  const codexAuth = await host.call('probe.authStatus', { vendor: 'codex', accountId: A.codex }, 10_000);
  deepEq('B2 codex: auth.json is never opened → unknown', codexAuth, { signIn: 'unknown' });
  const cursorAuth = await host.call('probe.authStatus', { vendor: 'cursor', accountId: A.cursor }, 15_000);
  deepEq('B3 cursor: `cursor-agent status` says logged in → signedIn', cursorAuth, { signIn: 'signedIn' });
  const agyAuth = await host.call('probe.authStatus', { vendor: 'agy', accountId: A.agy }, 15_000);
  deepEq('B4 agy: `agy models` lists models → signedIn', agyAuth, { signIn: 'signedIn' });
  const stats = await host.call('dev.stats', {});
  check('B5 the never-open filter refused the credential reads', stats.neverOpenRefused >= 2, JSON.stringify(stats));

  // ── C. models ──────────────────────────────────────────────────────────────
  const claudeModels = await host.call('probe.models', { vendor: 'claude', accountId: A.claude });
  check('C1 claude: a non-empty static list of {id, label, isDefault}', Array.isArray(claudeModels) && claudeModels.length > 0 && claudeModels.every((m) => Object.keys(m).sort().join(',') === 'id,isDefault,label'), JSON.stringify(claudeModels).slice(0, 200));
  check('C2 claude: at most one default', claudeModels.filter((m) => m.isDefault).length <= 1);
  const cursorModels = await host.call('probe.models', { vendor: 'cursor', accountId: A.cursor }, 15_000);
  deepEq('C3 cursor: the live list, junk ids dropped', cursorModels.map((m) => m.id), ['auto', 'composer-2.5']);
  const agyModels = await host.call('probe.models', { vendor: 'agy', accountId: A.agy });
  check('C4 agy: model ids pass the id rule', agyModels.every((m) => /^[A-Za-z0-9._:[\]/-]{1,128}$/.test(m.id)), JSON.stringify(agyModels).slice(0, 200));
  const codexModels = await host.call('probe.models', { vendor: 'codex', accountId: A.codex }, 30_000);
  check('C5 codex: an array (the live listing failed → the catalog, gated by an unknown login)', Array.isArray(codexModels), JSON.stringify(codexModels));
  const listing = host.journal.find((e) => e.method === 'host.registerSpawn' && e.params.vendor === 'codex' && e.params.args?.[0] === 'app-server')?.params;
  check('C6 the codex app-server listing is registered as a probe', listing?.kind === 'probe' && listing?.turnId === null, JSON.stringify(listing));

  // ── D. quota ───────────────────────────────────────────────────────────────
  const beforeD = host.journal.length;
  const claudeQuota = await host.call('probe.quota', { vendor: 'claude', accountId: A.claude }, 40_000);
  check('D1 claude quota has the contract shape', claudeQuota !== null && Object.keys(claudeQuota).sort().join(',') === 'fetchedAt,stale,unavailable,windows', JSON.stringify(claudeQuota));
  deepEq(
    'D2 claude windows: kinds, used %, reset times',
    (claudeQuota?.windows ?? []).map((w) => [w.kind, w.usedPct, w.resetsAt]),
    [
      ['fiveHour', 42, Date.parse('2026-10-22T18:00:00Z')],
      ['week', 87, Date.parse('2026-10-27T00:00:00Z')],
    ],
  );
  eq('D3 a fresh reading is not stale', claudeQuota?.stale, false);
  check('D4 the usage advice (`behaviors`) never crosses the channel', !JSON.stringify(claudeQuota).includes('other-project'));
  const duplex = host.journal.slice(beforeD).find((e) => e.method === 'host.registerSpawn')?.params;
  check('D5 the claude probe child is a registered probe with no turn', duplex?.kind === 'probe' && duplex?.turnId === null && duplex?.vendor === 'claude' && duplex?.cmd === binaries.claude, JSON.stringify(duplex));
  const duplexExit = await host.waitFor(() => host.journal.some((e) => e.method === 'host.exited' && e.params.spawnId === duplex?.spawnId), 10_000);
  const duplexExited = host.journal.find((e) => e.method === 'host.exited' && e.params.spawnId === duplex?.spawnId)?.params;
  check('D6 … and it settles without SIGKILL', duplexExit && duplexExited?.signal !== 'SIGKILL', JSON.stringify(duplexExited));
  const agyQuota = await host.call('probe.quota', { vendor: 'agy', accountId: A.agy }, 30_000);
  deepEq(
    'D7 agy windows from `agy -p /usage` (remaining fraction → used %)',
    (agyQuota?.windows ?? []).map((w) => [w.kind, w.usedPct, w.label]),
    [
      ['fiveHour', 25, 'gemini-5h'],
      ['week', 60, 'gemini-weekly'],
    ],
  );
  eq('D8 codex quota needs a token Gilde never reads → null', await host.call('probe.quota', { vendor: 'codex', accountId: A.codex }, 10_000), null);
  eq('D9 cursor quota → null (no keychain, no login file, no network)', await host.call('probe.quota', { vendor: 'cursor', accountId: A.cursor }, 10_000), null);

  // ── E. refusals ────────────────────────────────────────────────────────────
  const unknownField = await host.callError('probe.quota', { vendor: 'claude', accountId: A.claude, extra: true });
  eq('E1 an unknown field is rejected', unknownField?.data?.rule, 'unknown');
  const badId = await host.callError('probe.quota', { vendor: 'claude', accountId: 'acct-1' });
  eq('E2 a malformed account id is rejected by value', badId?.data?.rule, 'pattern');
  const badVendor = await host.callError('probe.models', { vendor: 'gemini', accountId: A.agy });
  eq('E3 agent-mesh\'s own vendor name is not a contract vendor', badVendor?.data?.field, 'vendor');
  const otherVendor = await host.callError('probe.quota', { vendor: 'agy', accountId: A.claude });
  eq('E4 an account of another vendor is rejected', otherVendor?.data?.field, 'accountId');
  await host.configure({ binaries: { claude: binaries.claude }, accounts });
  const notInstalled = await host.callError('probe.models', { vendor: 'cursor', accountId: A.cursor });
  eq('E5 a vendor without a binary is UNSUPPORTED{vendorNotInstalled}', `${notInstalled?.code}:${notInstalled?.data?.cap}`, '-32004:vendorNotInstalled');
  await host.configure({ binaries, accounts });
  host.allowed.delete(binaries.cursor);
  const refusedAuth = await host.call('probe.authStatus', { vendor: 'cursor', accountId: A.cursor }, 15_000);
  host.allowed.add(binaries.cursor);
  deepEq('E6 a spawn the host refuses reads as unknown, never signedOut', refusedAuth, { signIn: 'unknown' });

  // ── F. every probe child settled ───────────────────────────────────────────
  await new Promise((r) => setTimeout(r, 500));
  const registered = host.journal.filter((e) => e.method === 'host.spawned').map((e) => e.params.spawnId);
  const exited = new Set(host.journal.filter((e) => e.method === 'host.exited').map((e) => e.params.spawnId));
  check('F1 every spawned probe child reported its exit', registered.every((id) => exited.has(id)), `${registered.length} spawned, ${exited.size} exited`);
  check(
    'F2 every registration of this suite was a probe with no turn',
    host.journal.filter((e) => e.method === 'host.registerSpawn').every((e) => e.params.kind === 'probe' && e.params.turnId === null),
  );
  check('Z1 nothing but JSON-RPC reached stdout', host.skipped === 0, String(host.skipped));
}

const guard = setTimeout(() => {
  process.stdout.write('\nprobes: TIMED OUT (a credential FIFO was opened?)\n');
  host.child.kill('SIGKILL');
  process.exit(1);
}, 150_000);
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
