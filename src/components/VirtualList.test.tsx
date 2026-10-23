/**
 * VirtualList: windowing, measured variable heights, anchoring on prepend,
 * follow-the-tail and the 5,000-row budget (T-ui-virtual-5k, PM-08).
 *
 * happy-dom has no layout, so rows are measured through the injected
 * `measure` (each row reports its own height in a data attribute) and the
 * viewport height is given. Scroll is driven by setting `scrollTop` and
 * firing `scroll`, which is what a browser does.
 */

import { act, fireEvent, render } from '@testing-library/react';
import { createRef } from 'react';

import { assert, assertEqual, domTest, test } from '../test/harness';
import { VirtualList, computeOffsets, rowAt, visibleRange } from './VirtualList';
import type { VirtualListHandle } from './VirtualList';

const measure = (el: HTMLElement) => Number(el.firstElementChild?.getAttribute('data-h') ?? 0);

function heightOf(key: string): number {
  // Deterministic variable heights: 20–79 px.
  let h = 0;
  for (let i = 0; i < key.length; i++) h = (h * 31 + key.charCodeAt(i)) % 60;
  return 20 + h;
}

function rows(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll<HTMLElement>('.g-vlist-row'));
}

/**
 * CPU milliseconds of this process (user + system) when the runtime reports
 * them (Node), else wall-clock milliseconds. CPU time is what the list costs;
 * wall time on a shared build machine also counts every other process.
 */
function cpuNow(): number {
  const proc = (globalThis as { process?: { cpuUsage?: () => { user: number; system: number } } }).process;
  if (proc?.cpuUsage) {
    const u = proc.cpuUsage();
    return (u.user + u.system) / 1000;
  }
  return performance.now();
}

function scrollTo(view: { container: HTMLElement }, top: number) {
  const el = view.container.querySelector('.g-vlist') as HTMLElement;
  el.scrollTop = top;
  fireEvent.scroll(el);
}

test('virtual list (core): offsets, rowAt and the visible window', () => {
  const sizes = new Map<string, number>([['b', 50]]);
  const o = computeOffsets(4, (i) => 'abcd'[i] ?? '', () => 10, sizes);
  assertEqual(Array.from(o).join(','), '0,10,60,70,80', 'measured size overrides the estimate');
  assertEqual(rowAt(o, 0), 0, 'top');
  assertEqual(rowAt(o, 59), 1, 'inside b');
  assertEqual(rowAt(o, 60), 2, 'start of c');
  assertEqual(rowAt(o, 999), 3, 'past the end clamps');
  const r = visibleRange(o, 15, 20, 1);
  assertEqual(`${r.first} ${r.start} ${r.end}`, '1 0 3', 'window with overscan');
});

domTest('T-ui-virtual-5k: 5,000 variable rows render only the window, within budget', () => {
  const keys = Array.from({ length: 5000 }, (_, i) => `block-${i}`);
  const viewport = 600;
  const overscan = 6;
  let rowRenders = 0;
  const start = cpuNow();
  const view = render(
    <VirtualList
      role="log"
      aria-label="conversation"
      aria-live="off"
      count={keys.length}
      getKey={(i) => keys[i] as string}
      estimateSize={() => 40}
      measure={measure}
      viewportHeight={viewport}
      overscan={overscan}
      renderRow={(i) => {
        rowRenders++;
        return (
          <article data-h={heightOf(keys[i] as string)} aria-label={`claude, ${i}`}>
            {`block ${i}`}
          </article>
        );
      }}
    />,
  );
  const mountMs = cpuNow() - start;
  const list = view.container.querySelector('.g-vlist') as HTMLElement;
  assertEqual(list.getAttribute('role'), 'log', 'ARIA passes through');
  assertEqual(list.getAttribute('aria-live'), 'off', 'streaming text is not live');
  // Smallest row is 20 px: at most 600/20 + 1 visible, plus overscan both sides.
  const maxRows = Math.ceil(viewport / 20) + 1 + overscan * 2;
  assert(rows(view.container).length <= maxRows, `rendered ${rows(view.container).length} ≤ ${maxRows} of 5,000`);

  const times: number[] = [];
  for (const top of [1_000, 50_000, 120_000, 200_000, 10_000]) {
    rowRenders = 0;
    const t0 = cpuNow();
    act(() => scrollTo(view, top));
    times.push(cpuNow() - t0);
    assert(rows(view.container).length <= maxRows, `bounded at scrollTop ${top}`);
    // The work per jump is bounded: a few passes (scroll, measure, re-anchor) over the window only.
    assert(rowRenders <= maxRows * 6, `${rowRenders} row renders for one jump (≤ ${maxRows * 6})`);
  }
  const first = Number(rows(view.container)[0]?.getAttribute('data-index'));
  assert(first > 0, 'the window moved with the scroll');
  times.sort((a, b) => a - b);
  const median = times[Math.floor(times.length / 2)] as number;
  // Time budget in CPU time, for happy-dom (a real WebView is far faster):
  // mount ≤ 1 s and the median scroll update ≤ 50 ms. The median, not the
  // mean, because one GC pause makes a single update spike without the list
  // doing more work; the work bound above is the deterministic part of this gate.
  assert(mountMs <= 1000, `mount took ${mountMs.toFixed(1)} ms CPU (budget 1000)`);
  assert(median <= 50, `median scroll update ${median.toFixed(1)} ms CPU (budget 50)`);
});

domTest('virtual list: measured heights replace estimates', () => {
  const view = render(
    <VirtualList
      count={3}
      getKey={(i) => `k${i}`}
      estimateSize={() => 10}
      measure={measure}
      viewportHeight={500}
      renderRow={(i) => <div data-h={30 + i * 10}>{`row ${i}`}</div>}
    />,
  );
  const spacer = view.container.querySelector('.g-vlist-spacer') as HTMLElement;
  assertEqual(spacer.style.height, '120px', '30 + 40 + 50');
});

domTest('virtual list: prepending rows keeps the first visible row in place (load older)', () => {
  let keys = Array.from({ length: 200 }, (_, i) => `n${i + 100}`);
  const make = () => (
    <VirtualList
      count={keys.length}
      getKey={(i) => keys[i] as string}
      estimateSize={() => 30}
      measure={measure}
      viewportHeight={300}
      renderRow={(i) => <div data-h={30}>{keys[i]}</div>}
    />
  );
  const view = render(make());
  act(() => scrollTo(view, 30 * 50 + 7)); // row n150, 7 px into it
  const list = view.container.querySelector('.g-vlist') as HTMLElement;
  assertEqual(list.getAttribute('data-first-visible'), '50', 'n150 is the first visible row');
  keys = [...Array.from({ length: 100 }, (_, i) => `n${i}`), ...keys];
  view.rerender(make());
  assertEqual(list.scrollTop, 30 * 150 + 7, 'scroll moved by the prepended height');
  assertEqual(list.getAttribute('data-first-visible'), '150', 'n150 still first visible, at index 150');
  const firstRow = rows(view.container).find((r) => r.textContent === 'n150');
  assert(firstRow !== undefined, 'n150 rendered');
});

domTest('virtual list: followTail pins the bottom while at the bottom and reports leaving it', () => {
  let count = 50;
  const changes: boolean[] = [];
  const ref = createRef<VirtualListHandle>();
  const make = () => (
    <VirtualList
      ref={ref}
      count={count}
      getKey={(i) => `b${i}`}
      estimateSize={() => 20}
      measure={measure}
      viewportHeight={200}
      followTail
      onAtBottomChange={(b) => changes.push(b)}
      renderRow={(i) => <div data-h={20}>{`b${i}`}</div>}
    />
  );
  const view = render(make());
  const list = view.container.querySelector('.g-vlist') as HTMLElement;
  assertEqual(list.scrollTop, 50 * 20 - 200, 'opens at the bottom');
  assertEqual(changes[changes.length - 1], true, 'at bottom');
  count = 60;
  view.rerender(make());
  assertEqual(list.scrollTop, 60 * 20 - 200, 'new rows keep the bottom pinned');
  act(() => scrollTo(view, 100));
  assertEqual(changes[changes.length - 1], false, 'scrolling up releases the tail (the pill shows)');
  count = 70;
  view.rerender(make());
  assertEqual(list.scrollTop, 100, 'not pinned: the view stays where the user is');
  act(() => ref.current?.scrollToBottom());
  assertEqual(list.scrollTop, 70 * 20 - 200, 'scrollToBottom re-pins');
  assert(ref.current?.isAtBottom() === true, 'at bottom again');
  act(() => ref.current?.scrollToIndex(10));
  assertEqual(list.scrollTop, 200, 'scrollToIndex start');
});

domTest('virtual list: the empty state renders when there are no rows', () => {
  const view = render(
    <VirtualList count={0} getKey={() => ''} estimateSize={() => 20} renderRow={() => null} empty={<p>nothing</p>} />,
  );
  assert(view.queryByText('nothing') !== null, 'empty content');
});
