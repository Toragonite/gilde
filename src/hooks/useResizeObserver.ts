import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { RefObject } from 'react';

/**
 * `ResizeObserver` with an optional trailing debounce.
 *
 * The terminal host uses this at 60 ms (DESIGN §9-4): fit, then resize the PTY
 * only when the cell grid actually changed. Panels use it undebounced.
 */
export function useResizeObserver<T extends Element>(
  ref: RefObject<T | null>,
  onResize: (entry: DOMRectReadOnly) => void,
  debounceMs = 0,
): void {
  const handler = useRef(onResize);
  handler.current = onResize;

  useEffect(() => {
    const element = ref.current;
    if (!element) return;

    let timer: ReturnType<typeof setTimeout> | null = null;
    const observer = new ResizeObserver((entries) => {
      const rect = entries[entries.length - 1]?.contentRect;
      if (!rect) return;
      if (debounceMs <= 0) {
        handler.current(rect);
        return;
      }
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = null;
        handler.current(rect);
      }, debounceMs);
    });

    observer.observe(element);
    return () => {
      if (timer) clearTimeout(timer);
      observer.disconnect();
    };
  }, [ref, debounceMs]);
}

export interface Size {
  width: number;
  height: number;
}

/** Current size of an element, for virtualized lists and canvases. */
export function useElementSize<T extends Element>(ref: RefObject<T | null>): Size {
  const [size, setSize] = useState<Size>({ width: 0, height: 0 });

  useLayoutEffect(() => {
    const element = ref.current;
    if (element) {
      const rect = element.getBoundingClientRect();
      setSize({ width: rect.width, height: rect.height });
    }
  }, [ref]);

  useResizeObserver(ref, (rect) => {
    setSize((prev) =>
      prev.width === rect.width && prev.height === rect.height
        ? prev
        : { width: rect.width, height: rect.height },
    );
  });

  return size;
}
