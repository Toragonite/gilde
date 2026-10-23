/**
 * Variable-height virtual list (contract PM-08, §11, T-ui-virtual-5k).
 *
 * A conversation can hold up to 2,000 blocks in memory and lists (History,
 * the sidebar on a busy machine) run into the thousands, so only the rows in
 * the viewport plus `overscan` are in the DOM.
 *
 * - Rows have any height. Each rendered row is measured after layout
 *   (`getBoundingClientRect`, or the injected `measure` — the test DOM has no
 *   layout) and the size is cached by the row's key, so a row keeps its size
 *   when rows are inserted before it.
 * - The first visible row is the anchor: when rows are prepended (load older)
 *   or a row above changes size, the scroll position is corrected so the anchor
 *   stays exactly where it was on screen.
 * - `followTail` keeps the bottom pinned while the list is at the bottom
 *   (streaming output); scrolling up releases it and `onAtBottomChange(false)`
 *   lets the screen show its "follow the tail" pill. `scrollToBottom()` re-pins.
 * - ARIA attributes pass through to the scroll container (`role="log"`
 *   `aria-live="off"` for the conversation, ux-ia §10).
 */

import { useCallback, useImperativeHandle, useLayoutEffect, useRef, useState } from 'react';
import type { HTMLAttributes, ReactNode, Ref, UIEvent } from 'react';

import { useElementSize } from '../hooks/useResizeObserver';
import './components.css';

export interface VirtualListHandle {
  scrollToIndex: (index: number, align?: 'start' | 'center' | 'end') => void;
  /** Scroll to the end and pin it again when `followTail` is on. */
  scrollToBottom: () => void;
  isAtBottom: () => boolean;
  /** The scroll container. */
  element: () => HTMLDivElement | null;
}

export interface VirtualListProps extends Omit<HTMLAttributes<HTMLDivElement>, 'children' | 'onScroll'> {
  count: number;
  /** A stable key per row (sizes are cached by key). */
  getKey: (index: number) => string;
  /** Height used until the row has been measured, px. */
  estimateSize: (index: number) => number;
  renderRow: (index: number) => ReactNode;
  /** Rows rendered above and below the viewport. Default 6. */
  overscan?: number;
  /** Keep the bottom pinned while at the bottom. */
  followTail?: boolean;
  onAtBottomChange?: (atBottom: boolean) => void;
  /** Rendered range changes (first rendered index, one past the last). */
  onRangeChange?: (first: number, end: number) => void;
  /** How a row element is measured; defaults to its layout height. Tests inject one. */
  measure?: (element: HTMLElement, index: number) => number;
  /** Viewport height override, px (when the container cannot be measured). */
  viewportHeight?: number;
  /** Rendered instead of rows when `count` is 0. */
  empty?: ReactNode;
  ref?: Ref<VirtualListHandle>;
}

/** Used when the container has no measurable height (no layout yet). */
export const FALLBACK_VIEWPORT = 600;
/** Within this many px of the end counts as "at the bottom". */
export const BOTTOM_SLOP = 4;

/** Prefix sums of row heights: `offsets[i]` is the top of row i, `offsets[count]` the total. */
export function computeOffsets(
  count: number,
  getKey: (index: number) => string,
  estimateSize: (index: number) => number,
  sizes: ReadonlyMap<string, number>,
): Float64Array {
  const offsets = new Float64Array(count + 1);
  for (let i = 0; i < count; i++) {
    const size = sizes.get(getKey(i)) ?? estimateSize(i);
    offsets[i + 1] = (offsets[i] as number) + (Number.isFinite(size) && size > 0 ? size : 0);
  }
  return offsets;
}

/** The row containing pixel `y` (the last row whose top is ≤ y). */
export function rowAt(offsets: Float64Array, y: number): number {
  const count = offsets.length - 1;
  if (count <= 0) return 0;
  let lo = 0;
  let hi = count - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if ((offsets[mid] as number) <= y) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

/** The rendered window [start, end) for a scroll position. */
export function visibleRange(
  offsets: Float64Array,
  scrollTop: number,
  viewport: number,
  overscan: number,
): { first: number; start: number; end: number } {
  const count = offsets.length - 1;
  if (count <= 0) return { first: 0, start: 0, end: 0 };
  const first = rowAt(offsets, scrollTop);
  const last = rowAt(offsets, scrollTop + Math.max(viewport, 1) - 1);
  return { first, start: Math.max(0, first - overscan), end: Math.min(count, last + 1 + overscan) };
}

interface Anchor {
  key: string;
  index: number;
  /** scrollTop − top of the anchor row. */
  delta: number;
}

export function VirtualList({
  count,
  getKey,
  estimateSize,
  renderRow,
  overscan = 6,
  followTail = false,
  onAtBottomChange,
  onRangeChange,
  measure,
  viewportHeight,
  empty,
  ref,
  className,
  style,
  ...rest
}: VirtualListProps) {
  const scroller = useRef<HTMLDivElement>(null);
  const sizes = useRef(new Map<string, number>());
  const rowEls = useRef(new Map<number, HTMLElement>());
  const [scrollTop, setScrollTop] = useState(0);
  const [, setVersion] = useState(0);
  const measured = useElementSize(scroller);
  const viewport = viewportHeight ?? (measured.height > 0 ? measured.height : FALLBACK_VIEWPORT);

  const offsets = computeOffsets(count, getKey, estimateSize, sizes.current);
  const total = offsets[count] as number;
  const { first, start, end } = visibleRange(offsets, scrollTop, viewport, overscan);

  const latest = useRef({ offsets, count, viewport, total, getKey });
  latest.current = { offsets, count, viewport, total, getKey };
  const atBottom = useRef<boolean>(followTail);
  const anchor = useRef<Anchor | null>(null);
  const reportedBottom = useRef<boolean | null>(null);
  const reportedRange = useRef<string>('');
  const pinned = useRef<boolean>(followTail);

  const anchorFrom = useCallback((top: number): Anchor | null => {
    const { offsets: o, count: n, getKey: key } = latest.current;
    if (n === 0) return null;
    const i = rowAt(o, top);
    return { key: key(i), index: i, delta: top - (o[i] as number) };
  }, []);

  const applyScroll = useCallback((top: number) => {
    const el = scroller.current;
    const clamped = Math.max(0, top);
    if (el) el.scrollTop = clamped;
    const actual = el ? el.scrollTop : clamped;
    setScrollTop(actual);
    return actual;
  }, []);

  const onScroll = useCallback(
    (event: UIEvent<HTMLDivElement>) => {
      const top = event.currentTarget.scrollTop;
      const { viewport: vh, total: t } = latest.current;
      anchor.current = anchorFrom(top);
      pinned.current = top + vh >= t - BOTTOM_SLOP;
      setScrollTop(top);
    },
    [anchorFrom],
  );

  useLayoutEffect(() => {
    // 1. Measure the rendered rows; a changed size re-renders with new offsets.
    let changed = false;
    for (const [index, el] of rowEls.current) {
      if (index >= count) continue;
      const h = measure ? measure(el, index) : el.getBoundingClientRect().height;
      if (!(h > 0)) continue;
      const key = getKey(index);
      if (sizes.current.get(key) !== h) {
        sizes.current.set(key, h);
        changed = true;
      }
    }
    if (changed) {
      setVersion((v) => v + 1);
      return;
    }

    let top = scroller.current ? scroller.current.scrollTop : scrollTop;
    const maxTop = Math.max(0, total - viewport);
    if (followTail && pinned.current) {
      // 2a. Pinned to the tail: stay at the end as content grows.
      if (Math.abs(top - maxTop) > 0.5) top = applyScroll(maxTop);
    } else if (anchor.current) {
      // 2b. Keep the anchor row where it was (prepends, size changes above).
      const a = anchor.current;
      let index = -1;
      if (a.index < count && getKey(a.index) === a.key) index = a.index;
      else {
        for (let i = 0; i < count; i++) {
          if (getKey(i) === a.key) {
            index = i;
            break;
          }
        }
      }
      if (index >= 0) {
        const desired = Math.min((offsets[index] as number) + a.delta, maxTop);
        if (Math.abs(desired - top) > 0.5) top = applyScroll(desired);
        anchor.current = { key: a.key, index, delta: a.delta };
      } else {
        anchor.current = anchorFrom(top);
      }
    } else {
      anchor.current = anchorFrom(top);
    }

    // 3. Report "at the bottom" and the rendered range when they change.
    const bottom = top + viewport >= total - BOTTOM_SLOP;
    atBottom.current = bottom;
    if (!followTail) pinned.current = bottom;
    if (reportedBottom.current !== bottom) {
      reportedBottom.current = bottom;
      onAtBottomChange?.(bottom);
    }
    const range = `${start}:${end}`;
    if (reportedRange.current !== range) {
      reportedRange.current = range;
      onRangeChange?.(start, end);
    }
  });

  useImperativeHandle(
    ref,
    (): VirtualListHandle => ({
      scrollToIndex: (index, align = 'start') => {
        const { offsets: o, count: n, viewport: vh } = latest.current;
        if (n === 0) return;
        const i = Math.min(Math.max(index, 0), n - 1);
        const rowTop = o[i] as number;
        const rowBottom = o[i + 1] as number;
        const target = align === 'start' ? rowTop : align === 'end' ? rowBottom - vh : rowTop - (vh - (rowBottom - rowTop)) / 2;
        pinned.current = false;
        const actual = applyScroll(target);
        anchor.current = anchorFrom(actual);
      },
      scrollToBottom: () => {
        const { total: t, viewport: vh } = latest.current;
        pinned.current = true;
        const actual = applyScroll(Math.max(0, t - vh));
        anchor.current = anchorFrom(actual);
      },
      isAtBottom: () => atBottom.current,
      element: () => scroller.current,
    }),
    [anchorFrom, applyScroll],
  );

  const classes = ['g-vlist', className ?? ''].filter(Boolean).join(' ');

  if (count === 0 && empty !== undefined) {
    return (
      <div {...rest} ref={scroller} className={classes} style={style}>
        {empty}
      </div>
    );
  }

  // The element of every rendered row, rebuilt on each render so an index
  // never points at a row element that now shows another index.
  const els = new Map<number, HTMLElement>();
  rowEls.current = els;
  const rows: ReactNode[] = [];
  for (let i = start; i < end; i++) {
    rows.push(
      <div
        key={getKey(i)}
        className="g-vlist-row"
        data-index={i}
        ref={(el) => {
          if (el) els.set(i, el);
        }}
      >
        {renderRow(i)}
      </div>,
    );
  }

  return (
    <div {...rest} ref={scroller} className={classes} style={style} onScroll={onScroll} data-first-visible={first}>
      <div className="g-vlist-spacer" style={{ height: total }}>
        <div className="g-vlist-window" style={{ transform: `translateY(${offsets[start] as number}px)` }}>
          {rows}
        </div>
      </div>
    </div>
  );
}
