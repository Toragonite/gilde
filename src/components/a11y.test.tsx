/**
 * axe-core over the foundation components' markup (ux-ia §10, §14.3). The
 * real-browser gate (T19) runs axe on every screen; this catches what the
 * components themselves would break: names, roles, ARIA attribute validity.
 * happy-dom has no layout, so rules that need rendering (colour contrast,
 * target size, scrollable region focus) are switched off here; they belong to
 * the browser gate.
 */

import { render } from '@testing-library/react';
import { useRef } from 'react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { assert, domTest } from '../test/harness';
import { LiveRegion } from './LiveRegion';
import { Popover, popoverTriggerProps } from './Popover';
import { Sheet } from './Sheet';
import { StateGlyph } from './StateGlyph';
import { VirtualList } from './VirtualList';

const LAYOUT_RULES = ['color-contrast', 'target-size', 'scrollable-region-focusable', 'region'];

async function axeViolations(root: Element): Promise<string[]> {
  const axe = (await import('axe-core')).default;
  const result = await axe.run(root, {
    rules: Object.fromEntries(LAYOUT_RULES.map((id) => [id, { enabled: false }])),
    resultTypes: ['violations'],
  });
  return result.violations
    .filter((v) => v.impact === 'serious' || v.impact === 'critical')
    .map((v) => `${v.id}: ${v.nodes.map((n) => n.target.join(' ')).join(', ')}`);
}

function Fixture() {
  const anchor = useRef<HTMLButtonElement>(null);
  return (
    <main>
      <h1>Fixture</h1>
      <button type="button" ref={anchor} {...popoverTriggerProps(true, 'p1', 'menu')}>
        More
      </button>
      <Popover open anchor={anchor} id="p1" role="menu" label="Row menu" onClose={() => {}}>
        <button type="button" role="menuitem">
          Stop session
        </button>
      </Popover>
      <p>
        <StateGlyph state="needsApproval" /> Needs approval
      </p>
      <VirtualList
        role="list"
        aria-label="Sessions"
        count={3}
        getKey={(i) => `s${i}`}
        estimateSize={() => 24}
        viewportHeight={200}
        renderRow={(i) => <div role="listitem">{`session ${i}`}</div>}
      />
      <LiveRegion />
      <Sheet
        open
        title="Stop session"
        destructive
        onCancel={() => {}}
        actions={
          <>
            <button type="button">Cancel</button>
            <button type="button">Stop</button>
          </>
        }
      >
        <label>
          Reason
          <input type="text" />
        </label>
      </Sheet>
    </main>
  );
}

domTest('a11y: axe-core finds no serious or critical violation in the component markup', async () => {
  render(<Fixture />);
  try {
    const violations = await axeViolations(document.body);
    assert(violations.length === 0, `axe violations:\n  ${violations.join('\n  ')}`);
  } finally {
    resetHotkeysForTests();
  }
});

domTest('a11y: the axe check is live — a seeded unnamed button is reported', async () => {
  render(
    <main>
      <h1>Seeded</h1>
      <button type="button">
        <StateGlyph state="error" />
      </button>
    </main>,
  );
  const violations = await axeViolations(document.body);
  assert(
    violations.some((v) => v.startsWith('button-name')),
    `expected button-name, got:\n  ${violations.join('\n  ') || '(none)'}`,
  );
});
