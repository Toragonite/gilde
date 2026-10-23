#!/usr/bin/env node
/**
 * mesh-methods.mjs — the protocol-2 lifecycle surface (contract §4.5): `mesh.hello`,
 * `mesh.configure`, `mesh.ping`, `mesh.shutdown`, and what is NOT on the channel.
 *
 * `stub-host.mjs` proves the transport and the supervision seam, `turns.mjs` the
 * turn surface, `probes.mjs` the probes, `lifecycle.mjs` the exits (incl. 78). This
 * file pins the strictness of protocol 2 — "both ends reject unknown fields" — and
 * the method set:
 *
 *   A. mesh.hello      — protocol 1 → -32002 with {expected: 2, received}; an unknown
 *                        field, a missing one, a malformed instance id → -32602 naming
 *                        it; a good hello answers {protocol, buildId, adapters}.
 *   B. mesh.configure  — before hello → -32006; binaries must be absolute canonical
 *                        paths keyed by contract vendors (never agent-mesh's
 *                        "gemini"); accounts carry only an env KEY that is the vendor's
 *                        own variable (C8) and a directory; duplicates and unknown
 *                        fields are refused; reconfiguration works.
 *   C. mesh.ping       — {nonce?} only; the nonce comes back.
 *   D. method set      — every v1 run/introspection method is gone (-32601): mesh.run,
 *                        mesh.stream, mesh.resume, mesh.cancel, mesh.authStatus,
 *                        mesh.availableModels, mesh.quota, mesh.adapters, fleet.route,
 *                        and the unrenamed cco.* names; a host notification for an
 *                        unknown method is ignored, never fatal.
 *   E. orch gating     — `providers.orchestrator` registers `orch.*`; false removes it.
 *   F. mesh.shutdown   — {graceMs ≤ 60000}; out of range → -32602; a good one exits 0.
 *
 * Run:  node test/mesh-methods.mjs   (from src-tauri/sidecar, after `npm run build`)
 */

import { mkdtempSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { accountId, bundleBuildId, harness, installFakeVendors, startSidecar } from './fixtures/host.mjs';

const { check, eq, deepEq, summary } = harness('mesh-methods');
const home = realpathSync(mkdtempSync(join(tmpdir(), 'gilde-mesh-home-')));
const host = startSidecar({ env: { HOME: home } });
const { binaries } = installFakeVendors();

const goodHello = () => ({
  protocol: 2,
  buildId: bundleBuildId(),
  appInstanceId: 'mesh-methods-instance',
  providers: { orchestrator: false },
});

async function main() {
  // ── A. mesh.hello ──────────────────────────────────────────────────────────
  const p1 = await host.callError('mesh.hello', { protocol: 1, hostVersion: '1.0.0', appInstanceId: 'x', bootId: 'y', binaries: {} });
  eq('A1 a protocol-1 hello is -32002', p1?.code, -32002);
  deepEq('A2 … saying what each side speaks', p1?.data, { expected: 2, received: 1 });
  const extra = await host.callError('mesh.hello', { ...goodHello(), hostVersion: '2.0.0' });
  deepEq('A3 an unknown field is refused, named', [extra?.code, extra?.data?.field, extra?.data?.rule], [-32602, 'hostVersion', 'unknown']);
  const { providers, ...noProviders } = goodHello();
  const missing = await host.callError('mesh.hello', noProviders);
  deepEq('A4 a missing field is refused, named', [missing?.data?.field, missing?.data?.rule], ['providers', 'required']);
  const badInstance = await host.callError('mesh.hello', { ...goodHello(), appInstanceId: 'has space' });
  eq('A5 a malformed instance id is refused by value', badInstance?.data?.field, 'appInstanceId');
  const badProviders = await host.callError('mesh.hello', { ...goodHello(), providers: { orchestrator: 'yes' } });
  eq('A6 a non-boolean provider is refused', badProviders?.data?.field, 'providers.orchestrator');
  const tooEarly = await host.callError('mesh.configure', { binaries: {}, accounts: [], providers: { orchestrator: false } });
  eq('B1 mesh.configure before mesh.hello is -32006', tooEarly?.code, -32006);
  const hello = await host.call('mesh.hello', goodHello());
  deepEq('A7 a good hello answers {protocol, buildId, adapters}', [hello.protocol, hello.buildId, hello.adapters.length], [2, bundleBuildId(), 4]);

  // ── B. mesh.configure ──────────────────────────────────────────────────────
  const cfg = (over) => ({ binaries: {}, accounts: [], providers: { orchestrator: false }, ...over });
  const rel = await host.callError('mesh.configure', cfg({ binaries: { codex: 'bin/codex' } }));
  deepEq('B2 a relative binary path is refused', [rel?.data?.field, rel?.data?.rule], ['binaries.codex', 'path']);
  const trailing = await host.callError('mesh.configure', cfg({ binaries: { codex: `${binaries.codex}/` } }));
  eq('B3 a non-canonical path (trailing /) is refused', trailing?.data?.rule, 'path');
  const meshName = await host.callError('mesh.configure', cfg({ binaries: { gemini: binaries.agy } }));
  eq('B4 agent-mesh\'s "gemini" is not a contract vendor', meshName?.data?.field, 'binaries.gemini');
  const wrongKey = await host.callError('mesh.configure', cfg({ accounts: [{ accountId: accountId('c'), vendor: 'claude', envKey: 'CODEX_HOME', configDir: home }] }));
  eq('B5 an env key that is not the vendor\'s own variable is refused', wrongKey?.data?.field, 'accounts[0].envKey');
  const agyKey = await host.callError('mesh.configure', cfg({ accounts: [{ accountId: accountId('a'), vendor: 'agy', envKey: 'AGY_HOME', configDir: home }] }));
  eq('B6 agy can never carry an account variable (AGY_HOME is ignored)', agyKey?.data?.field, 'accounts[0].envKey');
  const keyNoDir = await host.callError('mesh.configure', cfg({ accounts: [{ accountId: accountId('c'), vendor: 'claude', envKey: 'CLAUDE_CONFIG_DIR', configDir: null }] }));
  eq('B7 an env key without a directory is refused', keyNoDir?.data?.field, 'accounts[0].configDir');
  const dupe = await host.callError('mesh.configure', cfg({ accounts: [{ accountId: accountId('c'), vendor: 'claude', envKey: null, configDir: null }, { accountId: accountId('c'), vendor: 'claude', envKey: null, configDir: null }] }));
  eq('B8 a duplicate account id is refused', dupe?.data?.rule, 'unique');
  const accountExtra = await host.callError('mesh.configure', cfg({ accounts: [{ accountId: accountId('c'), vendor: 'claude', envKey: null, configDir: null, token: 'x' }] }));
  deepEq('B9 an account with an unknown field (e.g. a token) is refused', [accountExtra?.data?.field, accountExtra?.data?.rule], ['accounts[0].token', 'unknown']);
  const badAccountId = await host.callError('mesh.configure', cfg({ accounts: [{ accountId: 'acct', vendor: 'claude', envKey: null, configDir: null }] }));
  eq('B10 a malformed account id is refused by value', badAccountId?.data?.rule, 'pattern');
  deepEq('B11 a good configure answers {}', await host.call('mesh.configure', cfg({ binaries: { codex: binaries.codex } })), {});
  deepEq('B12 … and reconfiguring works', await host.call('mesh.configure', cfg({ binaries })), {});

  // ── C. mesh.ping ───────────────────────────────────────────────────────────
  eq('C1 the nonce comes back', (await host.call('mesh.ping', { nonce: 7 })).nonce, 7);
  eq('C2 no params is fine', (await host.call('mesh.ping', undefined)).nonce, null);
  eq('C3 a string nonce is refused', (await host.callError('mesh.ping', { nonce: 'n' }))?.data?.field, 'nonce');
  eq('C4 an unknown ping field is refused', (await host.callError('mesh.ping', { nonce: 1, at: 2 }))?.data?.rule, 'unknown');

  // ── D. the method set ──────────────────────────────────────────────────────
  for (const method of [
    'mesh.run',
    'mesh.stream',
    'mesh.resume',
    'mesh.cancel',
    'mesh.steer',
    'mesh.authStatus',
    'mesh.availableModels',
    'mesh.quota',
    'mesh.adapters',
    'fleet.route',
    'cco.team.list',
    'cco.team.get',
    'cco.workers',
  ]) {
    eq(`D ${method} is not on the protocol-2 channel`, (await host.callError(method, {}))?.code, -32601);
  }
  host.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'host.surprise', params: { x: 1 } })}\n`);
  eq('D a host notification for an unknown method is ignored, never fatal', (await host.call('mesh.ping', { nonce: 9 })).nonce, 9);

  // ── E. orchestrator gating ─────────────────────────────────────────────────
  eq('E1 orchestrator absent: orch.team.list does not exist', (await host.callError('orch.team.list', { roots: [], panePids: [] }))?.code, -32601);
  await host.call('mesh.configure', cfg({ binaries, providers: { orchestrator: true } }));
  const listed = await host.call('orch.team.list', { roots: [], panePids: [] });
  check('E2 orchestrator present: orch.team.list answers', typeof listed === 'object' && listed !== null, JSON.stringify(listed).slice(0, 120));
  check('E3 orch.workers answers too', typeof (await host.call('orch.workers', { roots: [], panePids: [] })) === 'object');
  await host.call('mesh.configure', cfg({ binaries, providers: { orchestrator: false } }));
  eq('E4 orchestrator gone: its methods are removed', (await host.callError('orch.team.get', { teamId: 'tmabcdefghi1' }))?.code, -32601);
  const stats = await host.call('dev.stats', {});
  eq('E5 the observer is disposed', stats.orchestrator, false);

  // ── F. mesh.shutdown ───────────────────────────────────────────────────────
  eq('F1 a grace out of range is refused', (await host.callError('mesh.shutdown', { graceMs: 120_000 }))?.data?.field, 'graceMs');
  deepEq('F2 a good shutdown answers', await host.call('mesh.shutdown', { graceMs: 0 }), { ok: true });
  const exit = await host.waitExit(4_000);
  eq('F3 … and the process exits 0', exit?.code, 0);
  check('Z1 nothing but JSON-RPC reached stdout', host.skipped === 0, String(host.skipped));
}

const guard = setTimeout(() => {
  process.stdout.write('\nmesh-methods: TIMED OUT\n');
  host.child.kill('SIGKILL');
  process.exit(1);
}, 60_000);
guard.unref();

main()
  .catch((err) => check('no unexpected exception', false, err?.stack ?? String(err)))
  .finally(async () => {
    await host.close();
    const failures = summary();
    if (failures > 0) for (const l of host.stderrLines.slice(-20)) process.stdout.write(`  | ${l}\n`);
    process.exit(failures > 0 ? 1 : 0);
  });
