#!/usr/bin/env node
/**
 * cco-vendor.mjs — the copy test of the vendored Claude Code Orchestrator files
 * (team contract v2 §8; host-protocol.ts "list_workers' structured answer" RULES:
 * "Vendored copies … record the commit they were copied from and carry a check that
 * they are byte-identical to it").
 *
 *   1. Always: each file under src/vendored/cco/ hashes to what VENDORED.json pins —
 *      a local edit of a vendored copy fails here, not in a live run.
 *   2. Drift: each copy is byte-identical to
 *      `git -C <orchestrator> show <VENDORED.json commit>:<from>`. The orchestrator
 *      checkout is CCO_ORCHESTRATOR_REPO, else the first of `claude-code-orchestrator`
 *      and `fable-orchestrator` checked out next to this repository.
 *      When that repository (or the recorded commit in it) is absent this prints SKIP with
 *      the reason; it never fails for that.
 *   3. The copies import nothing but Node built-ins and each other (`./host-protocol`, and
 *      host-protocol's type-only `../uiTypes`, which src/vendored/uiTypes.ts re-exports from
 *      ./cco/uiTypes — checked to be exactly that one re-export).
 *   4. P38 (contract §1.3, §7.3.1, pixel-office Q-F5) — the static check of what the
 *      orchestrator feed exposes, read off the vendored types themselves:
 *        - board tasks carry NO dependency / blocked-by ids (`BoardTask` has only
 *          `state: 'blocked'` and a free-text `note`) → the Board shows `blockedBy` only
 *          when declared, i.e. never for orchestrator tasks in this version;
 *        - members DO carry their undelivered-message count (`MemberState.pending`,
 *          "Undelivered messages for it") → an in-flight count is available (the observer
 *          forwards it; Rust's `CcoMemberState.pending`).
 *      A re-vendored copy that changes either fact fails here, so the P38 mapping is
 *      re-decided by a person instead of drifting.
 *
 * Read-only: `git show` / `git cat-file` only, never a command that changes a repository.
 *
 * Run:  node test/cco-vendor.mjs   [CCO_ORCHESTRATOR_REPO=/path/to/orchestrator]
 */

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const vendoredDir = join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'vendored');
const dir = join(vendoredDir, 'cco');
const manifest = JSON.parse(readFileSync(join(dir, 'VENDORED.json'), 'utf8'));
let failures = 0;
let skips = 0;
const check = (name, ok, detail) => {
  process.stdout.write(`  ${ok ? 'ok  ' : 'FAIL'} ${name}${ok || detail === undefined ? '' : ` — ${detail}`}\n`);
  if (!ok) failures += 1;
};
const skip = (name, why) => {
  skips += 1;
  process.stdout.write(`  SKIP ${name} — ${why}\n`);
};

check('VENDORED.json records a full commit id', /^[0-9a-f]{40}$/.test(manifest.commit ?? ''), String(manifest.commit));
check(
  'VENDORED.json lists host-protocol.ts, host-client.ts and uiTypes.ts',
  ['host-protocol.ts', 'host-client.ts', 'uiTypes.ts'].every((f) => manifest.files?.[f] !== undefined),
  Object.keys(manifest.files ?? {}).join(', '),
);

const ALLOWED_IMPORTS = ['crypto', 'path', 'net', 'fs', 'os', './host-protocol', '../uiTypes'];
for (const [file, pin] of Object.entries(manifest.files)) {
  const bytes = readFileSync(join(dir, file));
  const sha = createHash('sha256').update(bytes).digest('hex');
  check(`${file} hashes to its pin`, sha === pin.sha256, `${sha} ≠ ${pin.sha256}`);
  const imports = [...bytes.toString('utf8').matchAll(/^import\s.*?from\s+'([^']+)'/gm)].map((m) => m[1]);
  const foreign = imports.filter((m) => !ALLOWED_IMPORTS.includes(m));
  check(`${file} imports only Node built-ins and its vendored siblings`, foreign.length === 0, foreign.join(', '));
}

// The shim host-protocol's '../uiTypes' lands on: one type-only re-export, nothing else.
{
  const shim = readFileSync(join(vendoredDir, 'uiTypes.ts'), 'utf8');
  const code = shim.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/.*$/gm, '').trim();
  check("src/vendored/uiTypes.ts is exactly `export type * from './cco/uiTypes';`", code === "export type * from './cco/uiTypes';", code);
}

// ── P38: what the orchestrator feed exposes (static, from the vendored types) ──
{
  const proto = readFileSync(join(dir, 'host-protocol.ts'), 'utf8');
  const body = (name) => {
    const m = new RegExp(`export interface ${name} \\{([\\s\\S]*?)\\n\\}`).exec(proto);
    return m === null ? undefined : m[1].replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/.*$/gm, '');
  };
  const boardTask = body('BoardTask');
  const memberState = body('MemberState');
  check('P38 the vendored BoardTask and MemberState interfaces are present', boardTask !== undefined && memberState !== undefined);
  const depField = /\b(blockedBy|blocked_by|dependsOn|depends_on|dependencies|deps|blocks|waitingOn)\??\s*:/;
  const blockedBy = boardTask !== undefined && depField.test(boardTask);
  check('P38 BoardTask declares no dependency / blocked-by ids (blockedBy only when declared → none)', boardTask !== undefined && !blockedBy, 'a dependency field appeared: re-decide P38 (§7.3.1)');
  const undelivered = memberState !== undefined && /\bpending\s*:\s*number\s*;/.test(memberState) && /Undelivered messages for it/.test(proto);
  check('P38 MemberState.pending (undelivered messages) is exposed → in-flight count available', undelivered, 'MemberState.pending changed: re-decide P38 (§7.3.1)');
  const observer = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'methods', 'cco.ts'), 'utf8');
  check('P38 the observer forwards MemberState.pending', /pending:\s*count\(raw\.pending\)/.test(observer));
  process.stdout.write(
    `  P38 report: blockedBy ids ${blockedBy ? 'EXPOSED' : 'not exposed (board tasks: state + note only)'}; ` +
      `undelivered messages ${undelivered ? 'EXPOSED (MemberState.pending)' : 'not exposed'} — vendored ${manifest.commit.slice(0, 7)}\n`,
  );
}

// Drift: byte-identical to the recorded commit.
// test/ → sidecar/ → src-tauri/ → the repository; its sibling is the default checkout.
const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const upstream =
  process.env.CCO_ORCHESTRATOR_REPO ||
  ['claude-code-orchestrator', 'fable-orchestrator'].map((d) => join(repoRoot, '..', d)).find((d) => existsSync(join(d, '.git'))) ||
  join(repoRoot, '..', 'claude-code-orchestrator');
const git = (args) => execFileSync('git', ['-C', upstream, ...args], { stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 64 * 1024 * 1024 });
let haveCommit = false;
if (!existsSync(join(upstream, '.git'))) {
  skip('drift against the recorded commit', `no orchestrator repository at ${upstream}`);
} else {
  try {
    git(['cat-file', '-e', `${manifest.commit}^{commit}`]);
    haveCommit = true;
  } catch {
    skip('drift against the recorded commit', `commit ${manifest.commit} is not in ${upstream}`);
  }
}
if (haveCommit) {
  for (const [file, pin] of Object.entries(manifest.files)) {
    let theirs;
    try {
      theirs = git(['show', `${manifest.commit}:${pin.from}`]);
    } catch (err) {
      check(`${file} equals ${pin.from} at ${manifest.commit.slice(0, 7)}`, false, `git show failed: ${String(err.message).split('\n')[0]}`);
      continue;
    }
    const ours = readFileSync(join(dir, file));
    check(`${file} is byte-identical to ${pin.from} at ${manifest.commit.slice(0, 7)}`, theirs.equals(ours), `${ours.length} vs ${theirs.length} bytes`);
  }
}

process.stdout.write(`\ncco-vendor: ${failures === 0 ? 'clean' : `${failures} failure(s)`}${skips > 0 ? ` (${skips} skipped)` : ''}\n`);
process.exit(failures === 0 ? 0 : 1);
