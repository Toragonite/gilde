/**
 * The one-line tile strip (ux-ia §3.7 below 900 px, §3.9 "more tiles than
 * fit"): one button per tile — swatch, state glyph, name — that brings the
 * tile into view and focuses it. It is the visible control for `⌘1…⌘6`.
 */

import { StateGlyph } from '../components/StateGlyph';
import { useI18n } from '../i18n/useI18n';
import type { ActivityState } from '../lib/gen/ActivityState';
import type { IdentityColour } from '../lib/gen/IdentityColour';

export interface StripItem {
  key: string;
  name: string;
  state: ActivityState['kind'] | null;
  swatch: IdentityColour | null;
  /** On screen right now. */
  shown: boolean;
}

export function TileStrip({ items, onPick, prefix }: { items: readonly StripItem[]; onPick: (key: string) => void; prefix?: string }) {
  const { t } = useI18n();
  if (items.length === 0) return null;
  return (
    <div className="g-tstrip" role="toolbar" aria-label={t('tile.strip')}>
      {prefix && (
        <span className="g-tstrip-more" aria-hidden="true">
          {prefix}
        </span>
      )}
      {items.map((item) => (
        <button key={item.key} type="button" className="g-tstrip-item" aria-pressed={item.shown} onClick={() => onPick(item.key)}>
          {item.swatch && <span className="g-swatch" data-colour={item.swatch} aria-hidden="true" />}
          {item.state && <StateGlyph state={item.state} />}
          <span className="g-tstrip-name">{item.name}</span>
        </button>
      ))}
    </div>
  );
}
