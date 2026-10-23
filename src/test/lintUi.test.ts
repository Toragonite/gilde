/**
 * Locks `scripts/lint-ui.sh` (contract §9.1 "lint no-raw-html, test-locked",
 * the dependency allowlist revision of §14.2 T-ui-* / R1 §7.4): each case seeds
 * a scratch tree, runs the real script on it and checks what it reports.
 *
 * The banned identifiers are assembled from pieces here, so this file itself
 * never contains them.
 */

import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';

import { assert, assertEqual, test } from './harness';

function repoRoot(): string {
  let dir = process.cwd();
  for (let i = 0; i < 6; i++) {
    if (existsSync(join(dir, 'scripts', 'lint-ui.sh'))) return dir;
    dir = resolve(dir, '..');
  }
  throw new Error('scripts/lint-ui.sh not found above the working directory');
}

interface LintRun {
  status: number | null;
  out: string;
}

function lint(files: Record<string, string>): LintRun {
  const root = mkdtempSync(join(tmpdir(), 'gilde-lint-ui-'));
  try {
    mkdirSync(join(root, 'src'), { recursive: true });
    for (const [path, content] of Object.entries(files)) {
      const abs = join(root, path);
      mkdirSync(dirname(abs), { recursive: true });
      writeFileSync(abs, content);
    }
    const run = spawnSync('bash', [join(repoRoot(), 'scripts', 'lint-ui.sh'), root, '-q'], { encoding: 'utf8' });
    return { status: run.status, out: `${run.stdout}${run.stderr}` };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

const RAW = [
  'dangerously' + 'SetInnerHTML',
  'inner' + 'HTML',
  'outer' + 'HTML',
  'insertAdjacent' + 'HTML',
  'document.' + 'write',
  'document.' + 'writeln',
  'createContextual' + 'Fragment',
  'set' + 'HTMLUnsafe',
];

test('lint-ui: a clean tree passes', () => {
  const run = lint({ 'src/ok.tsx': 'export const A = () => <p>text</p>;\n' });
  assertEqual(run.status, 0, `clean tree exit 0\n${run.out}`);
});

test('lint-ui ui/no-raw-html: every raw HTML insertion API is reported, in code, tests and comments', () => {
  const files: Record<string, string> = {};
  RAW.forEach((api, i) => {
    files[`src/f${i}.tsx`] = `// mention: ${api}\nexport const x${i} = 1;\n`;
  });
  files['src/deep/a.test.ts'] = `el.${'inner' + 'HTML'} = s;\n`;
  files['src/page.html'] = `<script>document.${'write'}('x')</script>\n`;
  const run = lint(files);
  assertEqual(run.status, 1, 'violations fail the lint');
  for (const api of RAW) {
    const name = api.replace(/^document\./, 'document.');
    assert(run.out.includes(`ui/no-raw-html: ${name}`), `${name} reported\n${run.out}`);
  }
  assert(run.out.includes('src/deep/a.test.ts:1: ui/no-raw-html'), 'test files are covered');
  assert(run.out.includes('src/page.html:1: ui/no-raw-html'), 'html files are covered');
});

test('lint-ui ui/no-raw-html: the allow marker does not exempt it', () => {
  const run = lint({ 'src/x.ts': `el.${'inner' + 'HTML'} = s; // lint-ui: allow\n` });
  assertEqual(run.status, 1, 'still a violation');
  assert(run.out.includes('ui/no-raw-html'), 'reported');
});

test('lint-ui ui/no-raw-html: only an unchanged hash-pinned legacy test is skipped', () => {
  const body = `// ${'outer' + 'HTML'} in a legacy comment\n`;
  // sha256 of `body`, computed the same way the runner pins legacy files.
  const sha = spawnSync('shasum', ['-a', '256'], { input: body, encoding: 'utf8' }).stdout.slice(0, 64);
  const quarantine = `export const QUARANTINE = [\n  { path: 'src/old/legacy.test.tsx', owner: 'T14', sha256: '${sha}' },\n];\n`;
  const pinned = lint({ 'src/test/quarantine.mjs': quarantine, 'src/old/legacy.test.tsx': body });
  assertEqual(pinned.status, 0, `the pinned, unchanged file is skipped\n${pinned.out}`);
  const changed = lint({ 'src/test/quarantine.mjs': quarantine, 'src/old/legacy.test.tsx': body + '// edited\n' });
  assertEqual(changed.status, 1, 'a rewritten legacy file is checked like any other');
});

test('lint-ui ui/no-new-deps: the revised allowlist', () => {
  const allowed = {
    dependencies: {
      react: '1', 'react-dom': '1', zustand: '1', '@tauri-apps/api': '1', micromark: '1',
      'micromark-extension-gfm': '1', 'mdast-util-from-markdown': '1', 'mdast-util-gfm': '1',
      '@xterm/xterm': '1', '@xterm/addon-fit': '1', '@xterm/addon-web-links': '1', '@xterm/addon-webgl': '1',
    },
    devDependencies: {
      'happy-dom': '1', '@testing-library/react': '1', '@testing-library/dom': '1', 'axe-core': '1',
      typescript: '1', vite: '1', '@vitejs/plugin-react': '1', '@types/react': '1', '@types/react-dom': '1', '@tauri-apps/cli': '1',
    },
  };
  const ok = lint({ 'package.json': JSON.stringify(allowed, null, 2) });
  assertEqual(ok.status, 0, `the pinned set passes\n${ok.out}`);
  for (const removed of ['@xyflow/react', '@dagrejs/dagre', '@tauri-apps/plugin-dialog', '@tauri-apps/plugin-notification', '@tauri-apps/plugin-opener', 'left-pad']) {
    const run = lint({ 'package.json': JSON.stringify({ dependencies: { [removed]: '1' } }) });
    assertEqual(run.status, 1, `${removed} is not allowed`);
    assert(run.out.includes(`ui/no-new-deps: ${removed}`), `${removed} reported`);
  }
});

test('lint-ui: generated contract types are exempt from the colour and copy rules only', () => {
  // Assembled so this file does not itself trip the two rules.
  const issue = '#' + '105';
  const bang = '`' + String.fromCharCode(33) + '`';
  const gen = lint({ 'src/lib/gen/X.ts': `/** see ${issue} and ${bang} */\nexport type X = 1;\n` });
  assertEqual(gen.status, 0, `generated doc comments pass\n${gen.out}`);
  const ui = lint({ 'src/ui/X.tsx': `export const c = '${issue}';\n` });
  assertEqual(ui.status, 1, 'the same text in UI code is a colour violation');
  const genRaw = lint({ 'src/lib/gen/Y.ts': `// ${'inner' + 'HTML'}\n` });
  assertEqual(genRaw.status, 1, 'generated files are still covered by no-raw-html');
});
