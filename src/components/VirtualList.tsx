/**
 * Fixed-row-height virtual list.
 *
 * The sidebar tree and the process table both go past 200 rows on a busy
 * machine (UI-GUIDELINES §2-2 asks for VS Code density, which means many small
 * rows). Windowing keeps the DOM at the size of the viewport instead of the
 * size of the machine.
 */

import { useRef, useState } from 'react';
import type { CSSProperties, ReactNode } from 'react';

import { useElementSize } from '../hooks/useResizeObserver';

export interface VirtualListProps<T> {
  items: readonly T[];
  /** Every row has the same height; the shell uses 26px rows. */
  rowHeight: number;
  renderRow: (item: T, index: number) => ReactNode;
  keyOf: (item: T, index: number) => string;
  /** Rows rendered above and below the viewport. */
  overscan?: number;
  className?: string;
  style?: CSSProperties;
  /** Rendered instead of the list when `items` is empty. */
  empty?: ReactNode;
}

export function VirtualList<T>({
  items,
  rowHeight,
  renderRow,
  keyOf,
  overscan = 6,
  className,
  style,
  empty,
}: VirtualListProps<T>) {
  const ref = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const { height } = useElementSize(ref);

  const classes = ['vlist', className ?? ''].filter(Boolean).join(' ');

  if (items.length === 0 && empty) {
    return (
      <div ref={ref} className={classes} style={style}>
        {empty}
      </div>
    );
  }

  const first = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan);
  const visible = Math.ceil((height || rowHeight * 20) / rowHeight) + overscan * 2;
  const last = Math.min(items.length, first + Math.max(visible, 1));
  const slice = items.slice(first, last);

  return (
    <div
      ref={ref}
      className={classes}
      style={style}
      onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
    >
      <div style={{ height: items.length * rowHeight, position: 'relative' }}>
        <div style={{ transform: `translateY(${first * rowHeight}px)` }}>
          {slice.map((item, i) => (
            <div key={keyOf(item, first + i)} style={{ height: rowHeight }}>
              {renderRow(item, first + i)}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
