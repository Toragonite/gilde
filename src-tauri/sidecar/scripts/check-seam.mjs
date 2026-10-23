#!/usr/bin/env node
/**
 * check-seam.mjs — CI gate over the shipped bundle (DESIGN §5-7).
 *
 * The whole reuse strategy rests on one claim: every child process the headless
 * lane creates goes through OUR registered seam, so the host has a durable
 * registry row before the process exists. That claim is only true while the
 * adapters keep taking their `exec` / `lineStream` / `getUsage` seams from us —
 * a version bump could add a spawn site we do not inject into, and nothing in
 * the type system would notice.
 *
 * So this script audits the artifact, not the source. It attributes every
 * `child_process` launch call in `sidecar.cjs` — and every launcher taken as a
 * VALUE (`x.spawn` not called on the spot, e.g. `opts.spawn ?? cp.spawn`, the
 * shape of a spawn seam's default) — to the module it came from (esbuild's
 * per-module `// path` banners) and compares that against a blessed BASELINE:
 *
 *   - a launch call in one of OUR modules is allowed only in the two seam
 *     modules, `src/exec-registered.ts` and `src/duplex.ts`;
 *   - a launch call in a vendor module is allowed only where the baseline says
 *     so: every site must match one blessed SIGNATURE (what it launches, read
 *     off the call's arguments), with exactly the recorded count per signature.
 *     Most sites are vendor seam defaults (`nodeExec`, `nodeLineStream`,
 *     `getLiveUsage`, cursor's keychain reader) that our injected seams replace —
 *     present in the bundle, never reached — and each of those names the source
 *     text in `src/` that injects the replacement, which must still be there;
 *   - a site the sidecar CAN reach without a registry row must be marked
 *     `reachable` with its reason; the clean summary prints every one of them;
 *   - anything else fails the build.
 *
 * When agent-mesh is upgraded this WILL fail. That is the point: a human then
 * re-verifies that `src/fleet.ts` still injects every seam the new version
 * exposes and re-blesses the numbers below.
 */

import { readFileSync, existsSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const sidecarDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const bundlePath = join(sidecarDir, 'sidecar.cjs');

/** child_process members that create a process. */
const LAUNCHERS = ['spawn', 'spawnSync', 'exec', 'execSync', 'execFile', 'execFileSync', 'fork'];

/**
 * The subset safe to look for as a BARE identifier call. `exec` is excluded: it is
 * also the name of the injected seam every adapter calls (`exec(req)`), so matching
 * it bare would flag the very indirection this gate exists to protect.
 */
const BARE_LAUNCHERS = LAUNCHERS.filter((n) => n !== 'exec');

/** Our own modules that may contain a launch call. */
const OUR_SEAM_MODULES = new Set(['src/exec-registered.ts', 'src/duplex.ts']);

/**
 * Blessed vendor launch sites, per module, by signature.
 *
 * `sig` is matched against the call's argument text (the spawn line plus the next
 * one, whitespace removed, from just after `spawn(`). A site that matches no
 * signature fails, and so does a count that differs — so a version that swaps one
 * launch for another of the same count is still caught.
 *
 * `injectedBy` is evidence for "never reached": [file under src/, regex] that must
 * still match, i.e. the seam that replaces this default is still wired. A
 * `reachable` site has no such seam; its `why` is the accepted reason.
 *
 * A `ref: true` entry blesses a launcher taken as a value; its `sig` is matched
 * against that whole source line (whitespace removed). Call entries match calls only.
 *
 * Protocol 2 (2026-10-22, T08): no baseline count changed; cursor's keychain default is
 * now replaced by a seam that refuses (C6) instead of a registered `security` exec.
 *
 * agent-mesh 0.1.4 (re-blessed 2026-10-20 from 0.1.3):
 *   claude  nodeExec + nodeLineStream, getLiveUsage                     (unchanged)
 *   codex   nodeExec + nodeLineStream                                   (unchanged)
 *           `codex app-server` model listing: was a direct, REACHABLE
 *           `spawn(req.cmd, ["app-server"], …)`; 0.1.4 added the `spawn` seam
 *           (CodexAdapterOptions.spawn), so it is now `req.spawn ?? cp.spawn` —
 *           a default replaced by src/fleet.ts's registered spawner, never reached
 *   cursor  nodeExec + nodeLineStream, defaultReadCursorKeychain        (unchanged)
 *   gemini  nodeExec + nodeLineStream                                   (unchanged)
 * No reachable unregistered vendor launch is left.
 */
const SEAM_DEFAULTS = /^req\.cmd,req\.args,\{/;
const BASELINE = new Map([
  [
    'node_modules/@toragonite/agent-mesh-claude/dist/index.js',
    [
      {
        sig: SEAM_DEFAULTS,
        count: 2,
        why: 'nodeExec / nodeLineStream defaults',
        injectedBy: [
          ['src/fleet.ts', /exec: exec\('claude'\),\s*lineStream: lineStream\('claude'\)/],
        ],
      },
      {
        sig: /^claudePath,/,
        count: 1,
        why: 'getLiveUsage default (duplex quota probe)',
        injectedBy: [['src/fleet.ts', /claudeOpts\.getUsage = getUsage/]],
      },
    ],
  ],
  [
    'node_modules/@toragonite/agent-mesh-codex/dist/index.js',
    [
      {
        sig: SEAM_DEFAULTS,
        count: 2,
        why: 'nodeExec / nodeLineStream defaults (lineStream seam since 0.1.3)',
        injectedBy: [['src/fleet.ts', /exec: exec\('codex'\),\s*lineStream: lineStream\('codex'\)/]],
      },
      {
        ref: true,
        sig: /^constspawnFn=req\.spawn\?\?[\w$]+\.spawn;$/,
        count: 1,
        why:
          'listAppServerModels default spawner (`req.spawn ?? spawn`) for the `codex app-server` live ' +
          'model listing behind availableModels() / Fleet.route latencyClass; spawn seam since 0.1.4, ' +
          'injected with the registered spawner (registry row kind probe, own process group)',
        injectedBy: [
          ['src/fleet.ts', /spawn: spawnSeam\('codex'\)/],
          ['src/fleet.ts', /spawn: makeRegisteredSpawn\(cfg\.rpc, cfg\.appInstanceId\)/],
        ],
      },
    ],
  ],
  [
    'node_modules/@toragonite/agent-mesh-cursor/dist/index.js',
    [
      {
        sig: SEAM_DEFAULTS,
        count: 2,
        why: 'nodeExec / nodeLineStream defaults',
        injectedBy: [['src/fleet.ts', /exec: exec\('cursor'\),\s*lineStream: lineStream\('cursor'\)/]],
      },
      {
        sig: /^KEYCHAIN_BINARY,\[\.\.\.KEYCHAIN_ARGS\],\{/,
        count: 1,
        why:
          'defaultReadCursorKeychain (`/usr/bin/security`), new in 0.1.3 — replaced by a seam that ' +
          'never reads the keychain (contract §9.7, C6), so `security` is never spawned',
        injectedBy: [['src/fleet.ts', /readKeychain: refusedKeychain/]],
      },
    ],
  ],
  [
    'node_modules/@toragonite/agent-mesh-gemini/dist/index.js',
    [
      {
        sig: SEAM_DEFAULTS,
        count: 2,
        why: 'nodeExec / nodeLineStream defaults',
        injectedBy: [['src/fleet.ts', /exec: exec\('gemini'\),\s*lineStream: lineStream\('gemini'\)/]],
      },
    ],
  ],
]);

function fail(lines) {
  for (const line of lines) process.stderr.write(`${line}\n`);
  process.stderr.write(
    '\ncheck-seam: the bundle contains process launches this build has not blessed.\n' +
      'Re-read docs/DESIGN.md §5-6/§5-7, confirm src/fleet.ts still injects every seam the\n' +
      'adapters expose, then update BASELINE in scripts/check-seam.mjs deliberately.\n',
  );
  process.exit(1);
}

if (!existsSync(bundlePath)) {
  fail([`check-seam: ${relative(sidecarDir, bundlePath)} not found — run \`npm run build\` first.`]);
}

const source = readFileSync(bundlePath, 'utf8');
const lines = source.split('\n');

// 1. Every local binding of the child_process module, however it was imported.
//    esbuild emits `X = require("child_process")` / `require("node:child_process")`.
const bindings = new Set();
for (const m of source.matchAll(/([A-Za-z_$][\w$]*)\s*=\s*require\(["']node:child_process["']\)/g)) {
  bindings.add(m[1]);
}
for (const m of source.matchAll(/([A-Za-z_$][\w$]*)\s*=\s*require\(["']child_process["']\)/g)) {
  bindings.add(m[1]);
}

if (bindings.size === 0) {
  process.stderr.write(
    'check-seam: no child_process binding found in the bundle. Either nothing spawns\n' +
      '(unexpected) or esbuild changed its output shape — verify this script still works.\n',
  );
  process.exit(1);
}

// 2. Attribute every launch call to its module.
const bindingAlt = [...bindings].map((b) => b.replace(/[$]/g, '\\$')).join('|');
const memberCall = new RegExp(`\\b(?:${bindingAlt})\\s*\\.\\s*(${LAUNCHERS.join('|')})\\b\\s*\\)?\\s*\\(`, 'g');
// A launcher taken as a value (not called right there): `opts.spawn ?? cp.spawn`.
const memberRef = new RegExp(`\\b(?:${bindingAlt})\\s*\\.\\s*(${LAUNCHERS.join('|')})\\b(?!\\s*\\)?\\s*\\()`, 'g');
// Defensive: a bare `spawn(` call, in case a future esbuild inlines the binding.
const bareCall = new RegExp(`(?:^|[^\\w$.])(${BARE_LAUNCHERS.join('|')})\\s*\\(`, 'g');

/** @type {Map<string, Map<string, number>>} module → member → count */
const found = new Map();
/** @type {Map<string, string[]>} module → human-readable sites */
const sites = new Map();
/** @type {Map<string, {member: string, args: string, where: string}[]>} module → each launch */
const launches = new Map();

/** The call's argument text: from just after the launcher's `(` over this line and the next. */
function argumentText(lineIndex, member) {
  const joined = `${lines[lineIndex]}\n${lines[lineIndex + 1] ?? ''}`.replace(/\s+/g, '');
  const at = joined.search(new RegExp(`\\.?${member}\\)?\\(`));
  if (at < 0) return joined;
  const open = joined.indexOf('(', at + member.length);
  return joined.slice(open + 1);
}

let current = '<prelude>';
for (let i = 0; i < lines.length; i += 1) {
  const line = lines[i];
  const banner = /^\/\/ (\S.*)$/.exec(line);
  if (banner) {
    current = banner[1];
    continue;
  }
  // Skip comment-only lines: prose about spawning is not a spawn.
  const code = line.replace(/^\s*\*.*$/, '').replace(/^\s*\/\/.*$/, '');
  if (code.trim().length === 0) continue;

  const hits = [];
  const refs = [];
  for (const m of code.matchAll(memberCall)) hits.push(m[1]);
  for (const m of code.matchAll(memberRef)) refs.push(m[1]);
  if (hits.length === 0 && refs.length === 0) {
    // A bare launcher identifier that is not a property access. `exec(` is
    // deliberately not in this set — it is the injected seam, not a launch.
    for (const m of code.matchAll(bareCall)) hits.push(m[1]);
  }
  if (hits.length === 0 && refs.length === 0) continue;

  if (!found.has(current)) found.set(current, new Map());
  const perModule = found.get(current);
  for (const member of [...hits, ...refs]) perModule.set(member, (perModule.get(member) ?? 0) + 1);
  if (!sites.has(current)) sites.set(current, []);
  sites.get(current).push(`    ${current}:${i + 1}  ${code.trim().slice(0, 100)}`);
  if (!launches.has(current)) launches.set(current, []);
  for (const member of hits) {
    launches.get(current).push({ member, ref: false, args: argumentText(i, member), where: `${current}:${i + 1}` });
  }
  for (const member of refs) {
    launches.get(current).push({ member, ref: true, args: code.replace(/\s+/g, ''), where: `${current}:${i + 1}` });
  }
}

// 3. Judge.
const problems = [];

for (const [module, members] of found) {
  const total = [...members.values()].reduce((a, b) => a + b, 0);

  if (OUR_SEAM_MODULES.has(module)) continue; // our seams may launch freely

  if (module.startsWith('src/')) {
    problems.push(
      `check-seam: ${module} launches a process (${total} site(s)) but is not a seam module.`,
      ...sites.get(module),
    );
    continue;
  }

  const blessed = BASELINE.get(module);
  if (blessed === undefined) {
    problems.push(
      `check-seam: unblessed module launches a process: ${module} (${total} site(s)).`,
      ...sites.get(module),
    );
    continue;
  }

  const perSig = new Map(blessed.map((b) => [b, 0]));
  for (const launch of launches.get(module) ?? []) {
    const match =
      launch.member === 'spawn' ? blessed.filter((b) => (b.ref === true) === launch.ref && b.sig.test(launch.args)) : [];
    if (match.length !== 1) {
      const shown = launch.ref ? `${launch.args.slice(0, 60)}… (launcher taken as a value)` : `${launch.member}(${launch.args.slice(0, 60)}…`;
      problems.push(
        `check-seam: ${launch.where} \`${shown}\` matches ` +
          `${match.length} blessed signature(s), expected exactly 1.`,
      );
      continue;
    }
    perSig.set(match[0], perSig.get(match[0]) + 1);
  }
  for (const [b, count] of perSig) {
    if (count !== b.count) {
      problems.push(
        `check-seam: ${module} has ${count} site(s) of ${b.sig} (${b.why}), baseline says ${b.count}.`,
        ...sites.get(module),
      );
    }
    if (!b.reachable) {
      for (const [file, re] of b.injectedBy ?? []) {
        const path = join(sidecarDir, file);
        const text = existsSync(path) ? readFileSync(path, 'utf8') : '';
        if (!re.test(text)) {
          problems.push(
            `check-seam: ${module} — "${b.why}" is blessed as never reached, but ${file} no longer ` +
              `injects its replacement (${re}).`,
          );
        }
      }
      if ((b.injectedBy ?? []).length === 0) {
        problems.push(`check-seam: ${module} — "${b.why}" is neither reachable nor backed by an injection.`);
      }
    }
  }
}

for (const module of BASELINE.keys()) {
  if (!found.has(module)) {
    problems.push(`check-seam: blessed module ${module} is not in the bundle any more.`);
  }
}

// The seams themselves must still be there — an empty bundle would pass everything else.
for (const module of OUR_SEAM_MODULES) {
  if (!found.has(module)) {
    problems.push(`check-seam: seam module ${module} contains no spawn — the seam is gone.`);
  }
}

if (problems.length > 0) fail(problems);

const ourTotal = [...OUR_SEAM_MODULES].reduce(
  (acc, m) => acc + [...(found.get(m)?.values() ?? [])].reduce((a, b) => a + b, 0),
  0,
);
const vendorTotal = [...BASELINE.keys()].reduce(
  (acc, m) => acc + [...(found.get(m)?.values() ?? [])].reduce((a, b) => a + b, 0),
  0,
);
const reachable = [...BASELINE.entries()].flatMap(([module, blessed]) =>
  blessed.filter((b) => b.reachable).map((b) => ({ module, ...b })),
);
const reachableCount = reachable.reduce((a, b) => a + b.count, 0);
process.stderr.write(
  `check-seam: clean — ${ourTotal} registered launch site(s) in ${OUR_SEAM_MODULES.size} seam module(s), ` +
    `${vendorTotal - reachableCount} blessed vendor default(s) replaced by injected seams, ` +
    `${reachableCount} accepted UNREGISTERED vendor launch(es):\n`,
);
for (const r of reachable) {
  process.stderr.write(`  - ${r.module.replace('node_modules/', '')} ×${r.count}: ${r.why}\n`);
}
