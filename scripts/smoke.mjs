#!/usr/bin/env node
// The hermetic smoke run of Gilde 2 (contract §14.1 hard rule: no real vendor
// CLI in any test; A38.4).
//
//   node scripts/smoke.mjs --fake-bin <dir> [--test boot] [--filter full_path_smoke]
//                          [--json <path>] [--check-only]
//
// `--fake-bin` is a directory holding only fake vendor binaries: the
// `test-fixtures` [[bin]]s (`fake_claude`, `fake_agent`, … from
// `cargo build --features test-fixtures`) linked or copied under the vendor
// names, or scripts written there.
// The run:
//
//   1. builds the hermetic environment of `scripts/gates/hermetic-cargo.sh`
//      (A38.1) — PATH = <cargo bin>:/usr/bin:/bin:/usr/sbin:/sbin, HOME and
//      GILDE_HOME in a fresh temp dir, SHELL=/nonexistent,
//      AGENT_CLI_CREDENTIAL_STORE=memory, DISABLE_AUTOUPDATER=1,
//      GIT_CONFIG_NOSYSTEM=1, the caller's toolchain and lane variables, and
//      GILDE_SMOKE_FAKE_BIN=<fake-bin> (the smoke test pins detection to it) —
//      and refuses to start if a vendor CLI is reachable on that PATH or if
//      an entry of <fake-bin> named like a vendor CLI resolves outside it to
//      anything but a `fake_*` fixture binary. cargo gets exactly that
//      environment: the PATH it checked is the PATH it hands over;
//   2. brackets the Rust smoke test (`cargo test --test <test> -- --ignored
//      <filter>`, `boot.rs::full_path_smoke` by default, which prints one
//      `SMOKE {json}` line per step) with a census of our processes matching
//      the vendor and fake names;
//   3. decides: cargo passed and ran at least one test (a filter that matches
//      nothing fails), at least one step was reported and every step is ok,
//      every session pid it opened had a process-registry row, the shutdown
//      left no survivors, and no process of ours is left on the machine.
//
// The M1 checks of the dispatch lane and the graph are gone with those lanes
// (contract §2.2). `--check-only` validates the environment and exits.
// Exit code 0 only when every check passed; 2 when the run could not start.

import { execFileSync, spawn } from 'node:child_process';
import { accessSync, constants, existsSync, mkdirSync, mkdtempSync, realpathSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const VENDOR_NAMES = ['claude', 'claude.exe', 'codex', 'cursor-agent', 'agent', 'agy', 'gemini', 'tmux'];
const SYSTEM_FLOOR = ['/usr/bin', '/bin', '/usr/sbin', '/sbin'];
const LANE_VARS = ['CARGO_TARGET_DIR', 'CARGO_INCREMENTAL', 'CARGO_BUILD_JOBS', 'CARGO_PROFILE_DEV_DEBUG', 'CARGO_PROFILE_TEST_DEBUG', 'RUST_BACKTRACE'];
const LEFTOVER_PATTERN = 'claude|codex|cursor-agent|agy|gilde-node|fake_claude|fake_agent|fake_real_tmux|gilde-tmux';

export function parseArgs(argv) {
  const o = { fakeBin: null, test: 'boot', filter: 'full_path_smoke', json: null, checkOnly: false, help: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      if (i + 1 >= argv.length) throw new Error(`${a} needs a value`);
      return argv[++i];
    };
    if (a === '--fake-bin') o.fakeBin = resolve(next());
    else if (a === '--test') o.test = next();
    else if (a === '--filter') o.filter = next();
    else if (a === '--json') o.json = resolve(next());
    else if (a === '--check-only') o.checkOnly = true;
    else if (a === '-h' || a === '--help') o.help = true;
    else throw new Error(`unknown option ${a}`);
  }
  if (!o.help && !o.fakeBin) throw new Error('--fake-bin <dir> is required: the smoke run never starts a real vendor CLI');
  if (o.test && !/^[\w-]+$/.test(o.test)) throw new Error('--test takes a test target name');
  return o;
}

const isFile = (p) => {
  try {
    return statSync(p).isFile();
  } catch {
    return false;
  }
};

const isExecutable = (p) => {
  try {
    accessSync(p, constants.X_OK);
    return isFile(p);
  } catch {
    return false;
  }
};

/** The cargo bin dir and toolchain homes, derived from the caller's environment as hermetic-cargo.sh does. */
export function toolchain(callerEnv = process.env) {
  const home = callerEnv.HOME;
  const cargoHome = callerEnv.CARGO_HOME || (home ? join(home, '.cargo') : null);
  const rustupHome = callerEnv.RUSTUP_HOME || (home ? join(home, '.rustup') : null);
  if (!cargoHome || !rustupHome) throw new Error('HOME is not set, so CARGO_HOME/RUSTUP_HOME cannot be derived; set them');
  let cargoBin = join(cargoHome, 'bin');
  if (!isExecutable(join(cargoBin, 'cargo'))) {
    const onPath = (callerEnv.PATH ?? '').split(':').filter((d) => d.startsWith('/')).find((d) => isExecutable(join(d, 'cargo')));
    if (!onPath) throw new Error(`cargo not found (not in ${cargoHome}/bin, not on PATH)`);
    cargoBin = realpathSync(onPath);
  }
  return { cargoBin, cargoHome, rustupHome };
}

/** Every vendor-named file on a PATH value (stat only). */
export function vendorsOnPath(pathValue) {
  const found = [];
  for (const dir of pathValue.split(':')) {
    for (const name of VENDOR_NAMES) {
      const p = join(dir || '.', name);
      if (isFile(p)) found.push(p);
    }
  }
  return found;
}

/**
 * The vendor-named entries of the fake bin that may be a real CLI: an entry is
 * a fake when its real file lies inside the fake bin (a copy or a script
 * written there) or is a test-fixtures binary (`fake_*`, what the fixtures
 * are linked from); anything else it resolves to is refused.
 */
export function nonFixturesIn(fakeBin) {
  const fake = realpathSync(fakeBin);
  const bad = [];
  for (const name of VENDOR_NAMES) {
    const p = join(fake, name);
    if (!existsSync(p)) continue;
    const real = realpathSync(p);
    if (!real.startsWith(fake + '/') && !basename(real).startsWith('fake_')) bad.push(`a real ${name} is reachable through ${p} (→ ${real})`);
  }
  return bad;
}

/**
 * The hermetic environment (A38.1/A38.4) and the proof that nothing real is
 * reachable. Throws on a real CLI. `env.PATH` is the PATH that was checked and
 * the one cargo gets.
 */
export function hermeticEnv(fakeBin, base, callerEnv = process.env) {
  if (!existsSync(fakeBin)) throw new Error(`--fake-bin ${fakeBin} does not exist`);
  const fake = realpathSync(fakeBin);
  const { cargoBin, cargoHome, rustupHome } = toolchain(callerEnv);
  const path = [cargoBin, ...SYSTEM_FLOOR].join(':');
  const onPath = vendorsOnPath(path);
  if (onPath.length) throw new Error(`a vendor CLI is reachable on the smoke PATH ${path}: ${onPath.join(', ')}`);
  const notFixtures = nonFixturesIn(fake);
  if (notFixtures.length) throw new Error(`--fake-bin may hold only fakes (files inside it, or links to fake_* fixture binaries): ${notFixtures.join('; ')}`);
  const home = join(base, 'home');
  mkdirSync(home, { recursive: true });
  const env = {
    PATH: path,
    HOME: home,
    GILDE_HOME: join(home, '.gilde'),
    TMPDIR: base,
    SHELL: '/nonexistent',
    LANG: 'en_US.UTF-8',
    AGENT_CLI_CREDENTIAL_STORE: 'memory',
    DISABLE_AUTOUPDATER: '1',
    GIT_CONFIG_NOSYSTEM: '1',
    RUSTUP_HOME: rustupHome,
    CARGO_HOME: cargoHome,
    RUST_LOG: 'warn',
    GILDE_SMOKE_FAKE_BIN: fake,
  };
  for (const v of LANE_VARS) if (callerEnv[v] !== undefined) env[v] = callerEnv[v];
  const reachable = Object.fromEntries(VENDOR_NAMES.map((name) => [name, existsSync(join(fake, name))]));
  return { env, cargo: join(cargoBin, 'cargo'), reachable };
}

/** How many tests cargo's libtest output says ran (passed + failed, over every `test result:` line). */
export function countTestsRun(text) {
  let ran = 0;
  for (const m of text.matchAll(/^test result: \w+\. (\d+) passed; (\d+) failed;/gm)) ran += Number(m[1]) + Number(m[2]);
  return ran;
}

/** `ps` rows of this user's processes: pid → { ppid, args } (read-only). */
export function parsePs(text) {
  const rows = new Map();
  for (const line of text.split('\n')) {
    const m = /^\s*(\d+)\s+(\d+)\s?(.*)$/.exec(line);
    if (m) rows.set(Number(m[1]), { ppid: Number(m[2]), args: m[3] });
  }
  return rows;
}

function census() {
  // `ps`, not `pgrep -a`: on macOS `-a` means "include ancestors" and prints no arguments.
  const out = execFileSync('/bin/ps', ['-U', String(process.getuid()), '-o', 'pid=,ppid=,command='], { encoding: 'utf8', maxBuffer: 64 << 20 });
  return parsePs(out);
}

/**
 * The processes this run left behind: new since `before`, matching the
 * pattern, and either orphaned (ppid 1 — what a survivor of the test binary
 * becomes) or still below this script. Another program's fresh processes
 * (a shell of another session whose arguments name one of the patterns) are
 * not ours and are not counted.
 */
export function leftoversOf(before, after, selfPid, pattern = LEFTOVER_PATTERN) {
  const re = new RegExp(pattern);
  const below = (pid) => {
    for (let p = pid, hops = 0; p > 1 && hops < 64; hops++) {
      if (p === selfPid) return true;
      p = after.get(p)?.ppid ?? 0;
    }
    return false;
  };
  return [...after.entries()]
    .filter(([pid, row]) => pid !== selfPid && !before.has(pid) && re.test(row.args))
    .filter(([pid, row]) => row.ppid === 1 || below(pid))
    .map(([pid, row]) => [pid, row.args]);
}

function runHarness(o, env, cargo) {
  const args = ['test', '--test', o.test, '--', '--ignored', '--nocapture', '--test-threads=1', o.filter];
  // Exactly the environment hermeticEnv checked: no PATH added here (A38.4).
  const child = spawn(cargo, args, { cwd: join(ROOT, 'src-tauri'), env, stdio: ['ignore', 'pipe', 'inherit'] });
  const events = [];
  let output = '';
  let buffer = '';
  const onData = (chunk) => {
    buffer += chunk;
    output += chunk;
    let i;
    while ((i = buffer.indexOf('\n')) !== -1) {
      const line = buffer.slice(0, i);
      buffer = buffer.slice(i + 1);
      const at = line.indexOf('SMOKE {');
      if (at !== -1) {
        try {
          events.push(JSON.parse(line.slice(at + 'SMOKE '.length)));
        } catch {
          process.stdout.write('! unparseable smoke line\n');
        }
      } else if (line.trim()) process.stdout.write(`  cargo | ${line}\n`);
    }
  };
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', onData);
  return new Promise((done) => child.on('close', (code) => {
    if (buffer.trim()) onData('\n');
    done({ code, events, testsRun: countTestsRun(output) });
  }));
}

export function checks(events, code, leftovers, testsRun) {
  const out = [];
  const add = (name, ok, detail) => out.push({ name, verdict: ok ? 'PASS' : 'FAIL', detail });
  add('harness', code === 0, code === 0 ? `${events.length} step(s) reported` : `cargo test exited ${code}`);
  // A step that runs 0 tests is a failure, never a pass (A38.4).
  add('tests', testsRun > 0, testsRun > 0 ? `${testsRun} test(s) ran` : 'cargo ran 0 tests: the --test/--filter pair matches no test');
  const failedSteps = events.filter((e) => e.ok === false);
  add(
    'steps',
    events.length > 0 && failedSteps.length === 0,
    events.length === 0
      ? 'no SMOKE step was reported'
      : failedSteps.length
        ? failedSteps.map((e) => `${e.step}: ${e.error ?? e.skipped ?? 'failed'}`).join(' | ')
        : 'every reported step ok',
  );
  const opened = events.filter((e) => e.step === 'pty' && e.ok && typeof e.pid === 'number');
  const rows = events.find((e) => e.step === 'registry')?.rows ?? [];
  if (opened.length) {
    const missing = opened.filter((s) => !rows.some((r) => r.pid === s.pid));
    add('registry', missing.length === 0, `${opened.length - missing.length}/${opened.length} session pids have a registry row`);
  }
  const shutdown = events.find((e) => e.step === 'shutdown');
  if (shutdown) add('shutdown', Boolean(shutdown.ok) && !shutdown.survivors, `survivors=${shutdown.survivors ?? 0}`);
  add('leftovers', leftovers.length === 0, leftovers.length ? leftovers.map(([pid, args]) => `${pid} ${args.slice(0, 80)}`).join(' | ') : `0 new processes matching /${LEFTOVER_PATTERN}/`);
  return out;
}

async function main() {
  let o;
  try {
    o = parseArgs(process.argv.slice(2));
  } catch (e) {
    process.stderr.write(`smoke: ${e.message}\n`);
    return 2;
  }
  if (o.help) {
    process.stdout.write('usage: node scripts/smoke.mjs --fake-bin <dir> [--test boot] [--filter full_path_smoke] [--json <path>] [--check-only]\n');
    return 0;
  }
  // Short and under /tmp: Unix socket paths below HOME must stay under 104 bytes.
  const base = realpathSync(mkdtempSync('/tmp/gs.'));
  try {
    let env;
    let cargo;
    let reachable;
    try {
      ({ env, cargo, reachable } = hermeticEnv(o.fakeBin, base));
    } catch (e) {
      process.stderr.write(`smoke: ${e.message}\n`);
      return 2;
    }
    process.stdout.write(`smoke: hermetic env ok (fakes reachable: ${Object.entries(reachable).filter(([, v]) => v).map(([k]) => k).join(', ') || 'none'})\n`);
    if (o.checkOnly) return 0;
    const before = census();
    const started = Date.now();
    const { code, events, testsRun } = await runHarness(o, env, cargo);
    await new Promise((r) => setTimeout(r, 2000));
    const leftovers = leftoversOf(before, census(), process.pid);
    const report = checks(events, code, leftovers, testsRun);
    for (const c of report) process.stdout.write(`  ${c.verdict}  ${c.name.padEnd(10)}  ${c.detail}\n`);
    const failed = report.filter((c) => c.verdict === 'FAIL').length;
    process.stdout.write(`smoke: ${report.length - failed}/${report.length} checks passed in ${((Date.now() - started) / 1000).toFixed(1)}s\n`);
    if (o.json) {
      mkdirSync(dirname(o.json), { recursive: true });
      writeFileSync(o.json, JSON.stringify({ at: new Date().toISOString(), test: o.test, filter: o.filter, testsRun, checks: report, events }, null, 2) + '\n');
    }
    return failed ? 1 : 0;
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then((code) => process.exit(code), (err) => {
    process.stderr.write(`smoke: ${err?.stack ?? err}\n`);
    process.exit(2);
  });
}
