/**
 * The body of a result card: the streamed text and the tool ticker
 * (DESIGN §9-7) — the headless lane's answer to "how is it going".
 *
 * Behaviour worth naming:
 *
 *  - **Collapsed shows the last ~40 lines, expanded shows everything the store
 *    kept.** The verbatim answer is always in `result.json`, so trimming here
 *    loses nothing that matters.
 *  - **Auto-scroll follows the tail only while the reader is already at the
 *    bottom.** Yanking the view back down while someone is reading three
 *    screens up is the fastest way to make a streaming panel unusable.
 *  - A run with no output yet says so in one line. No spinner, no skeleton
 *    (UI-GUIDELINES §2-2), and the same line for a streaming vendor and a
 *    non-streaming one — codex simply has nothing until its `done` arrives.
 */

import { useEffect, useLayoutEffect, useRef } from 'react';

import { useI18n } from '../i18n/useI18n';
import { STREAM_TAIL_LINES, tailLines } from '../store/dispatchSlice';

export interface RunTimelineProps {
  text: string;
  /** The adapter's current `RunEvent{type:'tool', name}`, when there is one. */
  tool: string | null;
  expanded: boolean;
  /** Nothing will be added to this text any more. */
  finished: boolean;
  /** Compare mode pins every body to the same height. */
  compare: boolean;
  /** Registers the scroll element with the grid, for scroll syncing. */
  registerBody?: (element: HTMLPreElement | null) => void;
  onBodyScroll?: (element: HTMLPreElement) => void;
}

export function RunTimeline({
  text,
  tool,
  expanded,
  finished,
  compare,
  registerBody,
  onBodyScroll,
}: RunTimelineProps) {
  const { t } = useI18n();
  const bodyRef = useRef<HTMLPreElement | null>(null);
  const pinned = useRef(true);

  const visible = expanded ? text : tailLines(text, STREAM_TAIL_LINES);

  useEffect(() => {
    registerBody?.(bodyRef.current);
    return () => registerBody?.(null);
  }, [registerBody]);

  // Layout effect, not effect: scrolling after paint shows one frame of the
  // old position on every chunk.
  useLayoutEffect(() => {
    const element = bodyRef.current;
    if (!element || !pinned.current) return;
    element.scrollTop = element.scrollHeight;
  }, [visible]);

  if (!visible) {
    return (
      <div className="dsp-body dsp-body-empty s12 muted" data-compare={compare ? 'true' : undefined}>
        {finished ? t('dispatch.card.noOutput') : t('dispatch.card.waiting')}
      </div>
    );
  }

  return (
    <>
      <pre
        className="dsp-body mono"
        data-compare={compare ? 'true' : undefined}
        data-expanded={expanded ? 'true' : undefined}
        ref={(element) => {
          bodyRef.current = element;
        }}
        onScroll={(event) => {
          const element = event.currentTarget;
          pinned.current = element.scrollHeight - element.scrollTop - element.clientHeight < 8;
          onBodyScroll?.(element);
        }}
      >
        {visible}
      </pre>
      {tool ? (
        <div className="row gap-6 s11 muted dsp-ticker">
          <span className="dsp-ticker-dot" aria-hidden="true" />
          <span className="ell">{t('dispatch.card.tool', { name: tool })}</span>
        </div>
      ) : null}
    </>
  );
}
