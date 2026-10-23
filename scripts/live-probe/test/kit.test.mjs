// The probe kit's privacy and phase rules: §9.7 redaction against the F-03
// fixture, shape-only records (rule 6), quota pressure (rule 3), scratch
// confinement (rule 4), and the guard on an intermediate dangling symlink.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, utimesSync, writeFileSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { loadCanaries, needles } from '../../gates/lib/canary.mjs';
import { checkConfigDir } from '../lib/guard.mjs';
import { PhaseStop, checkQuotaReading, makeScratchProject, pressureMonitor, removeScratch } from '../lib/phase.mjs';
import { Recorder, fields, label, shape } from '../lib/record.mjs';
import { redactArgv, redactCommand, shellWords } from '../lib/redact.mjs';
import { describeKeys, stripAnsi } from '../lib/pty.mjs';

const { canaries, argv: vectors } = loadCanaries();
const leaks = (text) => needles(canaries).filter(({ needle }) => text.includes(needle)).map(({ name }) => name);

test('redaction: no F-03 canary survives any hostile argv vector (§9.7 rules 1–5)', () => {
  for (const { name, argv } of vectors) {
    const out = JSON.stringify(redactArgv(argv));
    assert.deepEqual(leaks(out), [], `${name}: ${out}`);
  }
});

test('redaction: env assignments keep their names, sh -c strings are redacted word by word', () => {
  const cmd = redactCommand(`cd /tmp/p && env CLAUDECODE=1 AWS_SECRET_ACCESS_KEY=abc /usr/local/bin/claude --agent-id a@t --settings '{"x":1}' --model haiku`);
  assert.match(cmd, /CLAUDECODE=\*\*\*/);
  assert.match(cmd, /AWS_SECRET_ACCESS_KEY=\*\*\*/);
  assert.match(cmd, /--settings \*\*\*/);
  assert.match(cmd, /--agent-id a@t/);
  assert.match(cmd, /--model haiku/);
  assert.equal(redactCommand(`echo 'unterminated`), '<command redacted>');
  assert.deepEqual(shellWords(`a 'b c' "d \\"e\\"" f\\ g`), ['a', 'b c', 'd "e"', 'f g']);
  assert.equal(shellWords(`"open`), null);
});

test('redaction: prompts become <prompt>, ids and paths stay, credentials go', () => {
  assert.deepEqual(redactArgv(['/x/claude', '-p', 'secret plan', '--session-id', '0192f0aa-0000-4000-8000-000000000001']), ['/x/claude', '-p', '<prompt>', '--session-id', '0192f0aa-0000-4000-8000-000000000001']);
  assert.deepEqual(redactArgv(['/x/claude', '-p', '--input-format', 'stream-json']), ['/x/claude', '-p', '--input-format', 'stream-json']);
  assert.deepEqual(redactArgv(['/x/claude', '--', 'do the thing']), ['/x/claude', '--', '<prompt>']);
  assert.deepEqual(redactArgv(['/x/claude', '--resume', 'abc', 'free text']), ['/x/claude', '--resume', 'abc', '<prompt>']);
  assert.deepEqual(redactArgv(['/usr/bin/curl', '-H', 'Bearer', 'QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo0NTY3ODkw'], { claude: false }), ['/usr/bin/curl', '-H', '***', '***']);
  assert.deepEqual(redactArgv(['/x/claude', '--add-dir', '/private/var/folders/abcdefghijklmnopqrstuvwxyz0123456789/T'], {}), ['/x/claude', '--add-dir', '/private/var/folders/abcdefghijklmnopqrstuvwxyz0123456789/T'], 'paths are not credentials');
});

test('records: shapes and allowlisted codes only — never message text', () => {
  const frame = {
    type: 'assistant',
    error: 'authentication_failed',
    message: { role: 'assistant', content: [{ type: 'text', text: canaries.PROMPT }] },
    result: canaries.PROMPT,
    session_id: '0192f0aa-0000-4000-8000-000000000001',
  };
  const dir = mkdtempSync(join(tmpdir(), 'lp-rec-'));
  try {
    const r = new Recorder(join(dir, 'x.jsonl'), { home: '/Users/someone' });
    r.frame(0.1, 'out', frame);
    r.frame(0.2, 'out', { type: 'result', subtype: 'success', result: canaries.PROMPT, terminal_reason: 'completed', is_error: false, permission_denials: [{ tool_input: { command: canaries.SK_ANT } }] });
    r.spawn(0, { name: 's', argv: ['/Users/someone/.npm/claude', '--settings', '{"apiKeyHelper":"x"}', '-p', canaries.PROMPT], envKeys: ['B', 'A'], configKind: 'accountG', cwd: '/Users/someone/p', pid: 1, pgid: 1 });
    r.close();
    const text = readFileSync(join(dir, 'x.jsonl'), 'utf8');
    assert.deepEqual(leaks(text), [], text);
    const lines = text.trim().split('\n').map((l) => JSON.parse(l));
    assert.equal(lines[0].label, 'assistant/text');
    assert.equal(lines[0].fields.error, 'authentication_failed');
    assert.deepEqual(lines[0].keys.message.content, ['list(1)', '{2 keys}'], 'content is a count and a key count, never text');
    assert.equal(lines[0].keys.result, 'string');
    assert.equal(lines[1].fields.terminal_reason, 'completed');
    assert.equal(lines[2].argv[0], '~/.npm/claude', 'home-relative binary');
    assert.equal(lines[2].cwd, '~/p');
    assert.deepEqual(lines[2].envKeys, ['A', 'B'], 'env keys only, sorted');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  assert.deepEqual(fields({ type: 'system', subtype: 'init', model: 'claude-haiku', capabilities: ['interrupt_receipt_v1'], cwd: '/Users/x/secret project' }), { type: 'system', subtype: 'init', model: 'claude-haiku', capabilities: ['interrupt_receipt_v1'] });
  assert.equal(label({ type: 'control_request', request: { subtype: 'can_use_tool', tool_name: 'Bash' } }), 'control_request/can_use_tool/Bash');
  assert.deepEqual(shape({ a: [1, 2], b: { c: { d: { e: 1 } } } }), { a: ['list(2)', 'number'], b: { c: { d: '{1 keys}' } } });
});

test('PTY helpers: keys are recorded by name; ANSI is stripped for matching', () => {
  assert.equal(describeKeys('1'), 'digit:1');
  assert.equal(describeKeys('\u001b[B'), 'Down');
  assert.equal(describeKeys('\r'), 'Enter');
  assert.equal(describeKeys('\u001b[200~secret\u001b[201~'), 'paste(18)');
  assert.equal(describeKeys('hello world'), 'text(11)');
  assert.match(stripAnsi('\u001b[?1049h\u001b[H\u001b[2J Do you trust\u001b[1m this folder\u001b[0m?\u001b]0;title\u0007'), /Do you trust\s+this folder/);
});

test('quota rule 3: a fresh reading below 60 % passes; missing, stale, ≥ 60 % or windowless readings stop', () => {
  const dir = mkdtempSync(join(tmpdir(), 'lp-quota-'));
  try {
    const q = (name, data) => {
      const p = join(dir, name);
      writeFileSync(p, JSON.stringify(data));
      return p;
    };
    assert.equal(checkQuotaReading(q('ok.json', { windows: [{ kind: 'fiveHour', usedPct: 15 }, { kind: 'week', usedPct: 59 }], fetchedAt: Date.now() })).length, 2);
    assert.ok(checkQuotaReading(q('usage.json', { rate_limits: { five_hour: { utilization: 15 }, seven_day: { utilization: 44 } } })));
    assert.throws(() => checkQuotaReading(join(dir, 'missing.json')), PhaseStop);
    assert.throws(() => checkQuotaReading(q('high.json', { windows: [{ kind: 'week', usedPct: 60 }], fetchedAt: Date.now() })), /≥ 60/);
    assert.throws(() => checkQuotaReading(q('stale.json', { windows: [{ kind: 'week', usedPct: 1 }], fetchedAt: Date.now() - 3600_000 })), /older/);
    assert.throws(() => checkQuotaReading(q('none.json', { windows: [] })), /no windows/);
    assert.throws(() => checkQuotaReading(q('null.json', { windows: [{ kind: 'week', usedPct: null }], fetchedAt: Date.now() })), /no usage figure/);
    const old = q('mtime.json', { windows: [{ kind: 'week', usedPct: 1 }] });
    utimesSync(old, new Date(Date.now() - 3600_000), new Date(Date.now() - 3600_000));
    assert.throws(() => checkQuotaReading(old), /older/, 'without fetchedAt the file time counts');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('quota rule 3: the monitor stops at the first sign of pressure', () => {
  const cases = [
    [{ type: 'rate_limit_event', rate_limit_info: { status: 'allowed', unifiedWindows: { five_hour: { utilization: 0.6 } } } }, /five_hour/],
    [{ type: 'rate_limit_event', rate_limit_info: { status: 'rejected' } }, /status rejected/],
    [{ type: 'rate_limit_event', rate_limit_info: { status: 'allowed_warning' } }, /allowed_warning/],
    [{ type: 'system', subtype: 'api_retry', error_status: 429 }, /429/],
    [{ type: 'assistant', error: 'rate_limit' }, /rate_limit/],
    [{ type: 'result', api_error_status: 429 }, /429/],
  ];
  for (const [frame, re] of cases) {
    let reason = null;
    pressureMonitor((r) => {
      reason = r;
    }).frame(frame);
    assert.match(String(reason), re, JSON.stringify(frame));
  }
  let calm = null;
  const m = pressureMonitor((r) => {
    calm = r;
  });
  m.frame({ type: 'rate_limit_event', rate_limit_info: { status: 'allowed', unifiedWindows: { five_hour: { utilization: 0.15 } } } });
  m.frame({ type: 'assistant', error: 'authentication_failed' });
  assert.equal(calm, null, 'no pressure');
  m.stderr('Claude usage limit reached');
  assert.match(String(calm), /stderr/);
});

test('scratch rule 4: projects live under the scratch root and removal never leaves it', () => {
  const root = mkdtempSync(join(tmpdir(), 'lp-scratch-'));
  try {
    const p = makeScratchProject(root, 'LP-12');
    assert.ok(p.endsWith('/LP-12/proj'));
    assert.throws(() => makeScratchProject(root, '../escape'), (e) => e.rule === 'scratch');
    removeScratch(root, 'LP-12');
    assert.throws(() => removeScratch(root, '..'), (e) => e.rule === 'scratch');
    assert.throws(() => makeScratchProject('/usr/local', 'LP-12'), (e) => e.rule === 'scratchRoot');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('guard: a config dir below an intermediate (dangling) symlink is refused', () => {
  const root = mkdtempSync(join(tmpdir(), 'lp-link-'));
  try {
    const scratch = join(root, 'lp');
    mkdirSync(scratch);
    symlinkSync(join(root, 'missing-target'), join(scratch, 'LP-09'));
    assert.throws(() => checkConfigDir(join(scratch, 'LP-09', 'cfg'), { home: join(root, 'h'), scratchRoots: [scratch] }), (e) => e.rule === 'symlink');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
