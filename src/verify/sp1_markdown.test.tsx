/**
 * G6 probe SP-1 (contract §14.5, A31.3): agent markdown with `<img onerror>`,
 * `<script>`, `javascript:` links and `data:` images renders as text or
 * inert elements, and nothing navigates. Each variant goes through the real
 * renderer (`src/markdown`) into the shared DOM; the probe prints one line
 * `PROBE SP-1: …`. Owner: T33.
 */

import { fireEvent, render } from '@testing-library/react';

import { installShowcaseBackend } from '../lib/commands';
import type { CommandName } from '../lib/commands';
import { setShowcaseModeForTests } from '../lib/mode';
import { Markdown } from '../markdown';
import { assert, assertEqual, domTest } from '../test/harness';

const ACTIVE_TAGS = 'img, script, svg, iframe, object, embed, video, audio, source, link, meta, base, form, input, style';

interface Variant {
  name: string;
  text: string;
  /** Literal text that must stay visible (the raw HTML shown as text). */
  visible?: string;
}

const VARIANTS: Variant[] = [
  { name: '<img onerror> block', text: '<img src="x" onerror="alert(1)">', visible: 'onerror' },
  { name: '<img onerror> inline', text: 'before <img src=x onerror=alert(1)> after', visible: 'onerror' },
  { name: '<script> block', text: '<script>alert(1)</script>', visible: '<script>' },
  { name: '<script> inline', text: 'a <script>alert(2)</script> b', visible: '<script>' },
  { name: 'javascript: link', text: '[click](javascript:alert(1))' },
  { name: 'javascript: reference link', text: '[ref][1]\n\n[1]: javascript:alert(3)' },
  { name: 'javascript: autolink', text: '<javascript:alert(4)>', visible: 'javascript:alert(4)' },
  { name: 'data:image/png image', text: '![png](data:image/png;base64,iVBORw0KGgo=)' },
  { name: 'data:image/svg+xml image', text: '![svg](data:image/svg+xml,<svg onload=alert(5)>)' },
  { name: 'javascript: image', text: '![js](javascript:alert(6))' },
];

/** What the rendered DOM holds that it must not. */
function hazards(container: HTMLElement): string[] {
  const out: string[] = [];
  for (const el of Array.from(container.querySelectorAll(ACTIVE_TAGS))) out.push(`<${el.tagName.toLowerCase()}>`);
  for (const el of Array.from(container.querySelectorAll('*'))) {
    for (const attr of Array.from(el.attributes)) {
      const n = attr.name.toLowerCase();
      if (n.startsWith('on') || n === 'href' || n === 'src' || n === 'srcset' || n === 'style' || n === 'action') {
        out.push(`<${el.tagName.toLowerCase()} ${n}>`);
      }
    }
  }
  return out;
}

domTest('SP-1 probe: hostile markdown renders inert and never navigates', async () => {
  const calls: { name: CommandName; req: unknown }[] = [];
  setShowcaseModeForTests(true);
  installShowcaseBackend(async (name, req) => {
    calls.push({ name, req });
    return { opened: true };
  });
  const where = window.location.href;
  const parts: string[] = [];
  try {
    for (const v of VARIANTS) {
      const { container, unmount } = render(<Markdown text={v.text} />);
      const found = hazards(container);
      assertEqual(found.join(' '), '', `${v.name}: no active markup`);
      const activatable = container.querySelectorAll('[role="link"]').length;
      assertEqual(activatable, 0, `${v.name}: nothing activatable`);
      const inert = Array.from(container.querySelectorAll<HTMLElement>('.md-link-inert'));
      for (const el of inert) {
        fireEvent.click(el);
        fireEvent.keyDown(el, { key: 'Enter' });
      }
      if (v.visible) {
        assert((container.textContent ?? '').includes(v.visible), `${v.name}: shown as text`);
      }
      const shown = v.visible ? 'text' : inert.length > 0 ? `${inert.length} inert link(s)` : 'plain text';
      parts.push(`${v.name} → ${shown}, 0 active elements/handlers/URLs`);
      unmount();
    }
    await Promise.resolve();
    assertEqual(calls.length, 0, 'no link_open (or any command) was sent');
    assertEqual(window.location.href, where, 'no navigation');
    parts.push('clicks/Enter on every inert link → 0 commands, location unchanged');
  } finally {
    installShowcaseBackend(null);
    setShowcaseModeForTests(null);
    console.log(`PROBE SP-1: ${parts.join('; ')}`);
  }
});
