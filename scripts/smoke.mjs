#!/usr/bin/env node
// T29 — the M1 smoke run (M1-PLAN T29 acceptance 4).
//
//   node scripts/smoke.mjs [options]
//
// What it drives, in one pass:
//
//   1. this repository added as a project, one worktree per vendor;
//   2. an interactive PTY per vendor, each in its own worktree, each classified
//      by the status engine with evidence behind it;
//   3. one prompt dispatched headless to every vendor — one run per vendor,
//      with token counts and a `result.json` on disk;
//   4. every vendor CLI the run spawned present in the process registry, and
//      gone again after the shutdown;
//   5. **no process of ours left on the machine.**
//
// Why the work happens in Rust and the verdict happens here: (5) cannot be
// answered from inside the process that started them, and the whole point of
// the assertion is the view from outside. `cargo test --test boot -- --ignored`
// runs the harness (`src-tauri/tests/boot.rs::full_path_smoke`), which prints
// one `SMOKE {json}` line per step; this script reads those lines, brackets the
// whole thing with a `pgrep` census, and decides.
//
// Options
//   --vendors claude,codex   vendors to exercise (default: claude,codex)
//   --repo <path>            repository to use as the project (default: this one)
//   --prompt <text>          the dispatched prompt
//   --model-<vendor> <id>    pin a model, e.g. --model-codex gpt-5.1-codex.
//                            Pin one for any vendor whose default this machine
//                            cannot reach, or the run fails for a reason that
//                            has nothing to do with the app.
//   --allow-skip             a vendor that is not installed downgrades its
//                            checks to SKIP instead of failing the run
//   --json <path>            also write the machine-readable report here
//   --timeout-pty <s>        patience for a session to settle (default 120)
//   --timeout-run <s>        patience for a run to finish (default 300)
//
// Exit code is 0 only when every check passed.

import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// Every vendor CLI this app can start, plus the sidecar runtime. The exact
// pattern M1-PLAN T29 acceptance 4 names.
const LEFTOVER_PATTERN = 'claude|codex|agy|gilde-node';

// ---------------------------------------------------------------------------
// arguments
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const opts = {
    vendors: ['claude', 'codex'],
    repo: ROOT,
    prompt: 'Reply with exactly the word OK and nothing else.',
    models: {},
    allowSkip: false,
    json: null,
    timeoutPty: 120,
    timeoutRun: 300,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      const value = argv[i + 1];
      if (value === undefined) throw new Error(`${arg} needs a value`);
      i += 1;
      return value;
    };
    if (arg === '--vendors') opts.vendors = next().split(',').map((v) => v.trim()).filter(Boolean);
    else if (arg === '--repo') opts.repo = resolve(next());
    else if (arg === '--prompt') opts.prompt = next();
    else if (arg === '--allow-skip') opts.allowSkip = true;
    else if (arg === '--json') opts.json = resolve(next());
    else if (arg === '--timeout-pty') opts.timeoutPty = Number(next());
    else if (arg === '--timeout-run') opts.timeoutRun = Number(next());
    else if (arg.startsWith('--model-')) opts.models[arg.slice('--model-'.length)] = next();
    else if (arg === '-h' || arg === '--help') opts.help = true;
    else throw new Error(`unknown option ${arg}`);
  }
  return opts;
}

// ---------------------------------------------------------------------------
// the process census
// ---------------------------------------------------------------------------

/** Every pid of ours that matches the vendor pattern, with its command line. */
function census() {
  const uid = String(process.getuid());
  let out = '';
  try {
    out = execFileSync('pgrep', ['-u', uid, '-a', '-f', LEFTOVER_PATTERN], {
      encoding: 'utf8',
    });
  } catch (err) {
    // pgrep exits 1 when nothing matched, which is the good case.
    if (err.status !== 1) throw err;
    out = err.stdout ?? '';
  }
  const rows = new Map();
  for (const line of out.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const space = trimmed.indexOf(' ');
    const pid = Number(space === -1 ? trimmed : trimmed.slice(0, space));
    if (!Number.isFinite(pid) || pid === process.pid) continue;
    rows.set(pid, space === -1 ? '' : trimmed.slice(space + 1));
  }
  return rows;
}

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

// ---------------------------------------------------------------------------
// the harness
// ---------------------------------------------------------------------------

function runHarness(opts) {
  const env = {
    ...process.env,
    GILDE_SMOKE_REPO: opts.repo,
    GILDE_SMOKE_VENDORS: opts.vendors.join(','),
    GILDE_SMOKE_PROMPT: opts.prompt,
    GILDE_SMOKE_PTY_WAIT_S: String(opts.timeoutPty),
    GILDE_SMOKE_RUN_WAIT_S: String(opts.timeoutRun),
    // A smoke run must not inherit a developer's log level and drown the wire.
    RUST_LOG: process.env.RUST_LOG ?? 'warn',
  };
  for (const [vendor, model] of Object.entries(opts.models)) {
    env[`GILDE_SMOKE_MODEL_${vendor.toUpperCase()}`] = model;
  }

  const args = [
    'test',
    '--test',
    'boot',
    '--',
    '--ignored',
    '--nocapture',
    '--test-threads=1',
    'full_path_smoke',
  ];
  const child = spawn('cargo', args, {
    cwd: join(ROOT, 'src-tauri'),
    env,
    stdio: ['ignore', 'pipe', 'inherit'],
  });

  const events = [];
  let buffer = '';
  const onData = (chunk) => {
    buffer += chunk;
    let index = buffer.indexOf('\n');
    while (index !== -1) {
      const line = buffer.slice(0, index);
      buffer = buffer.slice(index + 1);
      // The marker is looked for anywhere in the line, not only at the start:
      // libtest writes `test full_path_smoke ... ` without a newline, so the
      // first event of a run always shares a line with it.
      const marker = line.indexOf('SMOKE {');
      if (marker !== -1) {
        try {
          events.push(JSON.parse(line.slice(marker + 'SMOKE '.length)));
        } catch {
          process.stdout.write(`! unparseable smoke line: ${line}\n`);
        }
      } else if (line.trim()) {
        process.stdout.write(`  cargo | ${line}\n`);
      }
      index = buffer.indexOf('\n');
    }
  };
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', onData);

  return new Promise((done) => {
    child.on('close', (code) => {
      if (buffer.trim()) onData('\n');
      done({ code, events });
    });
  });
}

// ---------------------------------------------------------------------------
// the checks
// ---------------------------------------------------------------------------

class Report {
  constructor(allowSkip) {
    this.allowSkip = allowSkip;
    this.checks = [];
  }

  pass(name, detail = '') {
    this.checks.push({ name, verdict: 'PASS', detail });
  }

  fail(name, detail = '') {
    this.checks.push({ name, verdict: 'FAIL', detail });
  }

  /** A check that could not run. Fatal unless the caller asked for tolerance. */
  skip(name, detail = '') {
    this.checks.push({ name, verdict: this.allowSkip ? 'SKIP' : 'FAIL', detail });
  }

  verdict(name, ok, detail) {
    if (ok) this.pass(name, detail);
    else this.fail(name, detail);
  }

  get failed() {
    return this.checks.filter((c) => c.verdict === 'FAIL');
  }

  print() {
    const width = Math.max(...this.checks.map((c) => c.name.length), 10);
    process.stdout.write('\n');
    for (const check of this.checks) {
      const mark = { PASS: 'PASS', FAIL: 'FAIL', SKIP: 'SKIP' }[check.verdict];
      process.stdout.write(
        `  ${mark}  ${check.name.padEnd(width)}  ${check.detail}\n`,
      );
    }
  }
}

/** `SessionStatus` is an internally tagged enum: `{ kind: "working", ... }`. */
function statusKind(status) {
  if (typeof status === 'string') return status;
  return status?.kind ?? 'unknown';
}

const round = (n) => (typeof n === 'number' ? Math.round(n * 100) / 100 : n);

/** Does this failure come from the provider refusing the model, not from us? */
function isVendorRefusal(text) {
  return /not supported|requires a newer version|invalid_request_error|unauthor|quota|rate limit/i.test(
    String(text ?? ''),
  );
}

/** Total tokens in a run's `usage`, whatever shape the vendor reported it in. */
function tokenTotal(usage) {
  if (!usage || typeof usage !== 'object') return 0;
  const numbers = [];
  const walk = (value) => {
    if (typeof value === 'number' && Number.isFinite(value)) numbers.push(value);
    else if (value && typeof value === 'object') Object.values(value).forEach(walk);
  };
  for (const [key, value] of Object.entries(usage)) {
    if (/token/i.test(key)) walk(value);
  }
  return numbers.reduce((sum, n) => sum + n, 0);
}

function check(opts, events, exitCode, leftovers, report) {
  const of = (step) => events.filter((e) => e.step === step);

  // 0. the harness itself
  report.verdict(
    'harness',
    exitCode === 0 && of('boot').length === 1,
    exitCode === 0 ? '' : `cargo test exited ${exitCode}`,
  );

  // 1. project + one worktree per vendor
  const worktrees = of('worktree').filter((e) => e.ok);
  report.verdict(
    'worktrees',
    worktrees.length === opts.vendors.length,
    `${worktrees.length}/${opts.vendors.length}: ${worktrees.map((w) => w.branch).join(', ')}`,
  );

  // 2. one classified interactive session per vendor
  for (const vendor of opts.vendors) {
    const pty = of('pty').find((e) => e.vendor === vendor);
    const name = `pty:${vendor}`;
    if (!pty) report.fail(name, 'the harness never reached this vendor');
    else if (!pty.ok) report.skip(name, pty.skipped ?? pty.error ?? 'not opened');
    else {
      // `idle` is the goal; `waitingForInput` is what a fresh worktree
      // produces whenever the CLI gates on folder trust before it draws a
      // prompt, and pressing that gate for the user is not a smoke run's job.
      // Anything still `starting` or `working` at the deadline never settled,
      // and that is a failure.
      const kind = statusKind(pty.status);
      const settled = kind === 'idle' || kind === 'waitingForInput';
      const evidence = Array.isArray(pty.evidence) ? pty.evidence.length : 0;
      report.verdict(
        name,
        settled && evidence > 0,
        `${kind} conf=${round(pty.confidence)} evidence=${evidence} in ${pty.ms}ms`,
      );
    }
  }

  // 3. one finished run per vendor, with tokens and a result.json
  for (const vendor of opts.vendors) {
    const run = of('run').find((e) => e.vendor === vendor);
    const name = `run:${vendor}`;
    if (!run) {
      const pty = of('pty').find((e) => e.vendor === vendor);
      report.skip(name, pty && !pty.ok ? (pty.skipped ?? 'vendor unavailable') : 'no run reported');
      continue;
    }
    const tokens = tokenTotal(run.usage);
    const ok = run.status === 'done' && tokens > 0 && run.resultJsonExists === true;
    const why = run.error ?? run.note ?? '';
    const detail = `${run.status} tokens=${tokens} result.json=${run.resultJsonExists}${
      why ? ` — ${why}` : ''
    }`;
    // A provider that refuses the model is this machine's problem, not the
    // app's: the run was dispatched, spawned, streamed and recorded correctly.
    // It is still reported, and still fatal unless `--allow-skip` says the
    // operator knows about it.
    if (!ok && isVendorRefusal(why)) report.skip(name, detail);
    else report.verdict(name, ok, detail);
  }

  // 3b. the dashboard graph — one projection holding every live object
  const graph = of('graph')[0];
  const kinds = graph?.byKind ?? {};
  const hasTree =
    (kinds.project ?? 0) >= 1 && (kinds.worktree ?? 0) >= 2 && (kinds.session ?? 0) >= 1;
  report.verdict(
    'graph',
    Boolean(graph?.ok) && hasTree,
    graph
      ? `${graph.nodes} nodes / ${graph.edges} edges — ${Object.entries(kinds)
          .map(([k, n]) => `${k}:${n}`)
          .join(' ')}`
      : 'the harness never projected the graph',
  );

  // 4. the registry saw every process, and the shutdown emptied it.
  //
  // "some vendor row exists" is not the claim acceptance 4 makes. The claim is
  // that *every* CLI this run spawned is in `proc_registry`, so the verdict is
  // per session pid: each interactive session that actually opened must have a
  // row carrying that pid *and* its vendor. (The Rust harness asserts the same
  // thing at tests/boot.rs; it is repeated here so the printed PASS line means
  // what the acceptance item says, and so a partial registry cannot hide behind
  // one lucky row.)
  const registry = of('registry')[0];
  const rows = registry?.rows ?? [];
  const vendorRows = rows.filter((r) => r.vendor);
  const openedSessions = of('pty').filter((e) => e.ok && typeof e.pid === 'number');
  const missing = openedSessions.filter(
    (s) => !rows.some((r) => r.pid === s.pid && r.vendor === s.vendor),
  );
  const perPid = openedSessions
    .map((s) => `${s.vendor}#${s.pid}${missing.includes(s) ? ' MISSING' : ''}`)
    .join(', ');
  const matched = `${openedSessions.length - missing.length}/${openedSessions.length} session pids matched (${perPid})`;
  if (openedSessions.length === 0) {
    // No vendor was installed, so there is no pid to match and the claim cannot
    // be made either way. `--allow-skip` decides, exactly as it does for the
    // per-vendor `pty:` checks above.
    report.skip('registry', 'the harness opened no session, so there is no pid to match');
  } else {
    report.verdict(
      'registry',
      Boolean(registry?.ok) && vendorRows.length > 0 && missing.length === 0,
      `${vendorRows.length} vendor rows of ${rows.length}; ${matched}`,
    );
  }

  const shutdown = of('shutdown')[0];
  report.verdict(
    'shutdown',
    Boolean(shutdown?.ok),
    shutdown
      ? `killed=${shutdown.killed} survivors=${shutdown.survivors} sidecar=${shutdown.sidecarStopped} instanceClosed=${shutdown.instanceClosed}`
      : 'the harness never reached the shutdown',
  );

  // 5. nothing of ours is left on the machine
  report.verdict(
    'leftovers',
    leftovers.length === 0,
    leftovers.length === 0
      ? `0 new processes matching /${LEFTOVER_PATTERN}/`
      : leftovers.map(([pid, args]) => `${pid} ${args}`).join(' | '),
  );

  // The worktrees are removed. The branches are not: this app never deletes a
  // branch (DESIGN §7-7), so a repeated smoke run leaves one `gilde/smoke-*` per
  // vendor per launch and the operator prunes them.
  const cleanup = of('cleanup');
  report.verdict(
    'cleanup',
    cleanup.length >= worktrees.length && cleanup.length > 0 && cleanup.every((c) => c.ok),
    `${cleanup.filter((c) => c.ok).length}/${cleanup.length} worktrees removed`,
  );
  const kept = of('branches')[0]?.kept ?? [];
  if (kept.length > 0) {
    process.stdout.write(
      `\nsmoke: ${kept.length} branch(es) kept by design — prune with:\n` +
        `  git -C ${opts.repo} branch -d ${kept.join(' ')}\n`,
    );
  }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.help) {
    process.stdout.write(
      'usage: node scripts/smoke.mjs [--vendors claude,codex] [--repo <path>]\n' +
        '                             [--prompt <text>] [--model-<vendor> <id>]\n' +
        '                             [--allow-skip] [--json <path>]\n' +
        '                             [--timeout-pty <s>] [--timeout-run <s>]\n',
    );
    return 0;
  }

  const bundle = join(ROOT, 'src-tauri', 'sidecar', 'sidecar.cjs');
  if (!existsSync(bundle)) {
    process.stdout.write('the sidecar bundle is missing; run `npm run sidecar:build` first\n');
    return 2;
  }

  process.stdout.write(`smoke: repo=${opts.repo} vendors=${opts.vendors.join(',')}\n`);
  const before = census();
  process.stdout.write(`smoke: ${before.size} matching processes already running\n`);

  const started = Date.now();
  const { code, events } = await runHarness(opts);
  const elapsed = Date.now() - started;

  // Give anything that was signalled a moment to actually leave the table: a
  // process that is still being reaped is not a leftover.
  await sleep(2000);
  const after = census();
  const leftovers = [...after.entries()].filter(([pid]) => !before.has(pid));

  const report = new Report(opts.allowSkip);
  check(opts, events, code, leftovers, report);
  report.print();

  const failed = report.failed;
  process.stdout.write(
    `\nsmoke: ${report.checks.length - failed.length}/${report.checks.length} checks passed in ${(
      elapsed / 1000
    ).toFixed(1)}s\n`,
  );

  if (opts.json) {
    mkdirSync(dirname(opts.json), { recursive: true });
    writeFileSync(
      opts.json,
      `${JSON.stringify(
        { at: new Date().toISOString(), opts, elapsedMs: elapsed, checks: report.checks, events },
        null,
        2,
      )}\n`,
    );
    process.stdout.write(`smoke: report written to ${opts.json}\n`);
  }

  return failed.length === 0 ? 0 : 1;
}

main().then(
  (code) => process.exit(code),
  (err) => {
    process.stderr.write(`smoke: ${err?.stack ?? err}\n`);
    process.exit(2);
  },
);
