/**
 * The Split (ux-ia §3.7, J4; contract S9): 2–6 sessions side by side, from any
 * project, one persistent set per window (`ui.split`, persisted as `splitSet`).
 *
 *   header   Split · n sessions · key hint                     [Add session]
 *   (choice) Up to 6 sessions side by side. Pick the tile … replaces. [Replace]
 *   tiles    1 → one · 2 → side by side · 3 → 1 + 2 · 4 → 2×2 · 5–6 → 3×2
 *            (two columns at 900–1199 px; one tile and a strip below 900 px)
 *
 * Keys (scope T): `⌘1…⌘6` focus tile n, `⌥⌘` arrows move focus spatially,
 * `⇧⌘⏎` zooms the focused tile and back; `⌘W` (the shell's) removes the
 * focused tile and never stops its session. Focus moves only on these keys,
 * a click, or a strip pick — never because a tile or a card arrived.
 * Every change of the set, the focus or the zoom goes through the UI store,
 * which persists the set and reports `app_view_state` [F-04]; the tiles on
 * screen are published as `ui.visibleUids` (A30.2).
 */

import { useEffect, useId, useMemo, useRef, useState } from 'react';

import { EmptyState } from '../components/EmptyState';
import { PlusIcon } from '../components/icons';
import { useHotkeyScope, useHotkeys } from '../hooks/useHotkeys';
import type { HotkeyBinding } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import type { SessionUid } from '../lib/gen/SessionUid';
import { vendorBadge } from '../lib/vendor';
import type { WidthBand } from '../shell/layout';
import { appStore, useAppStore, useShallow } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import { noteFocused, requestTileFocus } from './focus';
import { cellStyle, spatialNext, splitGrid } from './layout';
import type { Direction } from './layout';
import { cancelPendingAdd, preselectedTile, replaceWithPending, usePendingAdd } from './replace';
import { Tile } from './Tile';
import { TileStrip } from './TileStrip';
import type { StripItem } from './TileStrip';
import { useBand } from './useBand';
import './split.css';

export interface SplitScreenProps {
  /** Tests: the width band (default: the window's). */
  band?: WidthBand;
}

const ARROWS: ReadonlyArray<[string, Direction]> = [
  ['mod+alt+arrowleft', 'left'],
  ['mod+alt+arrowright', 'right'],
  ['mod+alt+arrowup', 'up'],
  ['mod+alt+arrowdown', 'down'],
];

/** Focus a tile on the user's request: store first (app_view_state), then the DOM. */
function focusSplitTile(uid: SessionUid): void {
  const s = appStore.getState();
  s.setSplitFocus(uid);
  noteFocused('split', uid);
  requestTileFocus(uid);
}

export function SplitScreen({ band: forced }: SplitScreenProps = {}) {
  const { t } = useI18n();
  const band = useBand(forced);
  const uids = useAppStore(useShallow((s) => s.ui.split.uids));
  const focused = useAppStore((s) => s.ui.split.focused);
  const zoomed = useAppStore((s) => s.ui.split.zoomed);
  const setSplitFocus = useAppStore((s) => s.setSplitFocus);
  const toggleSplitZoom = useAppStore((s) => s.toggleSplitZoom);
  const removeFromSplit = useAppStore((s) => s.removeFromSplit);
  const setPaletteOpen = useAppStore((s) => s.setPaletteOpen);
  const sheetOpen = useAppStore((s) => s.ui.sheet !== null);
  const strip = useAppStore(
    useShallow((s) =>
      s.ui.split.uids.map((uid) => {
        const session = sessionOf(s.world, uid);
        return `${session?.title ?? (session ? vendorBadge(session.vendor) : '')}\u0000${session?.activity.state.kind ?? ''}`;
      }),
    ),
  );
  const root = useRef<HTMLDivElement>(null);
  useHotkeyScope('T', true, root);

  const n = uids.length;
  const narrow = band === 'narrow';
  const zoomedUid = zoomed && uids.includes(zoomed) ? zoomed : null;
  // What is on screen: the zoomed tile, else (below 900 px) the focused tile, else all.
  const shown: SessionUid[] = zoomedUid ? [zoomedUid] : narrow ? [focused && uids.includes(focused) ? focused : (uids[0] as SessionUid)].filter(Boolean) : uids;
  const grid = useMemo(() => splitGrid(zoomedUid ? 1 : shown.length, zoomedUid ? 'narrow' : band), [zoomedUid, shown.length, band]);

  // Publish the tiles actually rendered (A30.2, F-04): not the ones in the
  // strip or hidden below 900 px. Cleared when the Split unmounts.
  const shownKey = shown.join('|');
  useEffect(() => {
    appStore.getState().setVisibleUids(shown);
  }, [shownKey]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => () => appStore.getState().setVisibleUids(null), []);

  const bindings: HotkeyBinding[] = [];
  for (let i = 0; i < 6; i++) {
    bindings.push({
      id: 'tile.focus',
      combo: `mod+${i + 1}`,
      run: () => {
        const uid = uids[i];
        if (!uid) return;
        // ⌘n while zoomed keeps the zoom and moves it to tile n.
        if (zoomedUid && zoomedUid !== uid) toggleSplitZoom(uid);
        focusSplitTile(uid);
      },
    });
  }
  for (const [combo, dir] of ARROWS) {
    bindings.push({
      id: 'tile.move',
      combo,
      allowRepeat: true,
      run: () => {
        if (!focused || zoomedUid) return;
        const at = uids.indexOf(focused);
        if (at < 0) return;
        let next: number | null;
        if (narrow) next = dir === 'left' || dir === 'up' ? (at > 0 ? at - 1 : null) : at + 1 < n ? at + 1 : null;
        else next = spatialNext(grid.cells, at, dir);
        const uid = next === null ? undefined : uids[next];
        if (uid) focusSplitTile(uid);
      },
    });
  }
  bindings.push({
    id: 'tile.zoom',
    combo: 'mod+shift+enter',
    run: () => {
      if (focused && uids.includes(focused)) toggleSplitZoom(focused);
    },
  });
  useHotkeys(bindings, { scope: 'T', enabled: !sheetOpen });

  // A tile that closed while it held focus hands focus to the tile the store
  // moved it to (the previous one in reading order, ux-ia §10) — never otherwise.
  const before = useRef<readonly SessionUid[]>(uids);
  useEffect(() => {
    const removed = before.current.some((u) => !uids.includes(u));
    before.current = uids;
    if (!removed || !focused) return;
    const active = typeof document === 'undefined' ? null : document.activeElement;
    if (active === null || active === document.body || !active.isConnected) requestTileFocus(focused);
  }, [uids, focused]);

  const pendingAdd = usePendingAdd();

  const stripItems: StripItem[] = uids.map((uid, i) => {
    const [name = '', state = ''] = (strip[i] ?? '').split('\u0000');
    return { key: uid, name: name || t('common.unknown'), state: (state || null) as StripItem['state'], swatch: null, shown: shown.includes(uid) };
  });

  return (
    <div ref={root} className="g-split" data-band={band}>
      <div className="g-split-head">
        <h1 className="g-split-title">{t('split.title')}</h1>
        <span className="g-split-count">{t('split.count', { n })}</span>
        {band === 'wide' && n > 0 && <span className="g-split-hint">{t('split.hint')}</span>}
        <span className="g-split-fill" />
        <button type="button" className="g-sbtn" aria-keyshortcuts="Meta+K" onClick={() => setPaletteOpen(true)}>
          <PlusIcon size="sm" />
          <span>{t('split.add')}</span>
        </button>
      </div>
      {pendingAdd && <ReplaceChoice pending={pendingAdd} uids={uids} />}
      {n === 0 ? (
        <div className="g-split-empty">
          <EmptyState message={t('split.empty')} actionLabel={t('split.add')} onAction={() => setPaletteOpen(true)} />
        </div>
      ) : (
        <>
          {narrow && !zoomedUid && <TileStrip items={stripItems} onPick={(key) => focusSplitTile(key as SessionUid)} />}
          <div
            className="g-tiles"
            data-count={shown.length}
            data-zoomed={zoomedUid ? 'true' : undefined}
            style={{
              gridTemplateColumns: `repeat(${grid.columns}, minmax(0, 1fr))`,
              gridTemplateRows: `repeat(${grid.rows}, minmax(0, 1fr))`,
            }}
          >
            {shown.map((uid, i) => {
              const cell = grid.cells[i];
              return (
                <Tile
                  key={uid}
                  uid={uid}
                  index={uids.indexOf(uid)}
                  count={n}
                  context="split"
                  focused={focused === uid}
                  zoomed={zoomedUid === uid}
                  style={cell ? cellStyle(cell) : undefined}
                  onFocused={() => {
                    setSplitFocus(uid);
                    noteFocused('split', uid);
                  }}
                  onToggleZoom={() => {
                    toggleSplitZoom(uid);
                    setSplitFocus(uid);
                  }}
                  onClose={() => removeFromSplit(uid)}
                />
              );
            })}
          </div>
        </>
      )}
    </div>
  );
}

/** "Up to 6 sessions side by side." — pick the tile the new session replaces. */
function ReplaceChoice({ pending, uids }: { pending: SessionUid; uids: readonly SessionUid[] }) {
  const { t } = useI18n();
  const names = useAppStore(
    useShallow((s) =>
      [pending, ...uids].map((uid) => {
        const session = sessionOf(s.world, uid);
        return session?.title ?? (session ? vendorBadge(session.vendor) : t('common.unknown'));
      }),
    ),
  );
  const [choice, setChoice] = useState<SessionUid | null>(() => preselectedTile(uids));
  const groupId = useId();
  const picked = choice && uids.includes(choice) ? choice : preselectedTile(uids);
  return (
    <div className="g-split-replace" role="group" aria-labelledby={`${groupId}-t`}>
      <p className="g-split-replace-text" id={`${groupId}-t`}>
        {t('split.max')} {t('split.replaceWith', { name: names[0] ?? '' })}
      </p>
      <div className="g-split-replace-options" role="radiogroup" aria-label={t('split.replaceTitle')}>
        {uids.map((uid, i) => (
          <label key={uid} className="g-split-replace-option">
            <input type="radio" name={groupId} value={uid} checked={picked === uid} onChange={() => setChoice(uid)} />
            <span className="g-split-replace-name">
              ⌘{i + 1} {names[i + 1]}
            </span>
          </label>
        ))}
      </div>
      <span className="g-split-replace-actions">
        <button type="button" className="g-sbtn" onClick={() => cancelPendingAdd()}>
          {t('common.cancel')}
        </button>
        <button type="button" className="g-sbtn g-sbtn-primary" disabled={!picked} onClick={() => picked && replaceWithPending(picked)}>
          {t('split.replace')}
        </button>
      </span>
    </div>
  );
}
