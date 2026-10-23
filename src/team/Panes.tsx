/**
 * Team › Panes (ux-ia §3.8, §3.9, J5): one tile per member, all at once.
 *
 *  - The lead takes the left column (40 % by default; the divider drags and
 *    moves with ←/→, ⇧ for larger steps); teammates tile on the right in two
 *    columns (one at 900–1199 px), rebalanced as they join. Teammates that do
 *    not fit fold into the strip; picking one swaps it with the least
 *    recently focused tile. Below 900 px one tile shows with the strip. The
 *    divider and the tile order are part of the team's layout and survive a
 *    reload (`TeamLayout.leadRatio`, `tileOrder`, A30.3).
 *  - Every member tile is the member's session (T15's parts via `Tile`). A
 *    pane teammate's prompt renders in the lead: the lead's tile carries the
 *    card, the member's tile a mirrored copy that answers in the lead [F-15].
 *  - Panes without a member session follow the members in creation order
 *    (`Team.paneTiles`, A30.1): "starting…", "Teammate did not start" after
 *    30 s, or "Unknown pane started by {lead}" — never focused, never zoomed,
 *    never numbered (`PaneTileView`).
 *  - A new tile appears with a 1 px outline pulse and a polite announcement;
 *    focus stays where it was (ux-ia §3.8 "Focus is never stolen").
 *  - A teammate that exits 0 loses its tile; a crash keeps "Crashed · exit n"
 *    with its last output and [Close tile] (§7.4.6).
 *  - `⋯` per tile: Add to split, Tell {member}… / Ask the lead to stop
 *    {member} (prefilled at the lead, never sent), Close pane… — each only
 *    when the team's capabilities allow it.
 *
 * Keys (scope T): `⌘1…⌘6`, `⌥⌘` arrows, `⇧⌘⏎` zoom, `⇧⌘⌫` Stop… for the
 * focused member (Close pane… for a hosted pane, else the session's stop
 * sheet). Focus and zoom live in the team's layout, which persists them and
 * feeds `app_view_state`; the sessions actually rendered are published as
 * `ui.visibleUids` (F-04, A30.2).
 */

import { useEffect, useMemo, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from 'react';

import { useHotkeys } from '../hooks/useHotkeys';
import type { HotkeyBinding } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import { formatNumber } from '../lib/format';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { PaneTile } from '../lib/gen/PaneTile';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { Team } from '../lib/gen/Team';
import type { TeamKey } from '../lib/gen/TeamKey';
import { announce } from '../lib/notify';
import type { WidthBand } from '../shell/layout';
import { noteFocused, focusOrder, requestTileFocus } from '../split/focus';
import { LEAD_RATIO_DEFAULT, cellStyle, clampRatio, leastRecent, memberCapacity, panesGrid, spatialNext } from '../split/layout';
import type { Cell, Direction } from '../split/layout';
import { addToSplitOrAsk } from '../split/replace';
import { Tile } from '../split/Tile';
import type { TileMenuItem } from '../split/Tile';
import { TileStrip } from '../split/TileStrip';
import type { StripItem } from '../split/TileStrip';
import { MAX_TILE_ORDER } from '../store/uiSlice';
import { appStore, useAppStore } from '../store/useAppStore';
import { messagesOfTeam, sessionOf, tasksOfTeam } from '../store/worldSlice';
import type { WorldState } from '../store/worldSlice';
import { prefillLead, prefillText, setTeamMode, tellLine } from './actions';
import {
  CAP,
  canAskStop,
  canClosePane,
  canTell,
  flat,
  leadMember,
  memberRoute,
  memberTiles,
  messageCount,
  orderedMembers,
  taskCount,
} from './model';
import type { Count, MemberTileSpec } from './model';
import { PaneTileView, paneGlyph, paneKey, paneTilePhase } from './PaneTileView';

// ---------------------------------------------------------------------------
// View memory that is not layout: tiles the user closed (crashes, dead panes)
// ---------------------------------------------------------------------------

const dismissed = new Map<TeamKey, Set<string>>();
const NO_KEYS: ReadonlySet<string> = new Set();

function dismissedOf(teamKey: TeamKey): ReadonlySet<string> {
  return dismissed.get(teamKey) ?? NO_KEYS;
}

/** Keep the known order, drop members without a tile, append newcomers. */
export function reconcileOrder(previous: readonly MemberKey[], current: readonly MemberKey[]): MemberKey[] {
  const kept = previous.filter((k) => current.includes(k));
  for (const k of current) if (!kept.includes(k)) kept.push(k);
  return kept;
}

const surfaceOf = (teamKey: TeamKey) => `team:${teamKey}`;

/** The teammates' tile order now: the saved `tileOrder` reconciled with who has a tile. */
export function currentOrder(world: WorldState, teamKey: TeamKey): MemberKey[] {
  const keys = memberTiles(world, orderedMembers(world, teamKey), dismissedOf(teamKey))
    .filter((s) => s.member.role !== 'lead')
    .map((s) => s.key);
  return reconcileOrder(appStore.getState().ui.teamLayouts[teamKey]?.tileOrder ?? [], keys);
}

/**
 * Bring a teammate's tile on screen: if it sits in the strip, it swaps with
 * the least recently focused visible teammate (ux-ia §3.9); the new order is
 * saved in the layout.
 */
export function bringIntoView(world: WorldState, teamKey: TeamKey, key: MemberKey, capacity: number): void {
  const order = currentOrder(world, teamKey);
  const at = order.indexOf(key);
  if (at < 0 || at < capacity || capacity <= 0) return;
  const visible = order.slice(0, capacity);
  const out = leastRecent(focusOrder(surfaceOf(teamKey)) as readonly MemberKey[], visible);
  if (!out) return;
  const next = [...order];
  next[order.indexOf(out)] = key;
  next[at] = out;
  appStore.getState().setTeamLayout(teamKey, { tileOrder: next.slice(0, MAX_TILE_ORDER) });
}

/** Focus a member's tile on the user's request (store first, then the DOM). */
export function focusMemberTile(world: WorldState, teamKey: TeamKey, key: MemberKey, band: WidthBand): void {
  const s = appStore.getState();
  bringIntoView(world, teamKey, key, memberCapacity(band));
  const zoomed = s.ui.teamLayouts[teamKey]?.zoomed ?? null;
  s.setTeamLayout(teamKey, { focused: key, zoomed: zoomed && zoomed !== key ? key : zoomed });
  noteFocused(surfaceOf(teamKey), key);
  const member = world.members.get(key);
  const session = sessionOf(world, member?.sessionUid ?? null);
  if (session) requestTileFocus(session.uid);
}

/** Tests only. */
export function resetPanesForTests(): void {
  dismissed.clear();
}

export function countText(count: Count, t: ReturnType<typeof useI18n>['t'], locale: ReturnType<typeof useI18n>['locale']): string {
  if (count.kind === 'unknown') return t('common.unknown');
  const value = formatNumber(count.value, locale);
  return count.kind === 'approx' ? t('team.count.approx', { value }) : value;
}

const ARROWS: ReadonlyArray<[string, Direction]> = [
  ['mod+alt+arrowleft', 'left'],
  ['mod+alt+arrowright', 'right'],
  ['mod+alt+arrowup', 'up'],
  ['mod+alt+arrowdown', 'down'],
];

/** One tile of the Panes grid: a member's session, or a pane without one (A30.1). */
type Entry = { kind: 'member'; key: string; spec: MemberTileSpec } | { kind: 'pane'; key: string; tile: PaneTile };

const byCreation = (a: PaneTile, b: PaneTile) => a.createdAt - b.createdAt || (a.paneId < b.paneId ? -1 : a.paneId > b.paneId ? 1 : 0);

export function Panes({ team, band }: { team: Team; band: WidthBand }) {
  const { t, locale } = useI18n();
  const world = useAppStore((s) => s.world);
  const layout = useAppStore((s) => s.ui.teamLayouts[team.key] ?? null);
  const sheetOpen = useAppStore((s) => s.ui.sheet !== null);
  const setTeamLayout = useAppStore((s) => s.setTeamLayout);
  const setVisibleUids = useAppStore((s) => s.setVisibleUids);
  const openSheet = useAppStore((s) => s.openSheet);
  const [dragRatio, setDragRatio] = useState<number | null>(null);
  const [closed, setClosed] = useState<ReadonlySet<string>>(() => dismissedOf(team.key));
  const [fresh, setFresh] = useState<ReadonlySet<string>>(NO_KEYS);
  const [pinnedPane, setPinnedPane] = useState<string | null>(null);
  const gridRef = useRef<HTMLDivElement>(null);
  const ratio = dragRatio ?? clampRatio(layout?.leadRatio ?? LEAD_RATIO_DEFAULT);

  const members = useMemo(() => orderedMembers(world, team.key), [world, team.key]);
  const specs = useMemo(() => memberTiles(world, members, closed), [world, members, closed]);
  const leadSpec = specs.find((s) => s.member.role === 'lead') ?? null;
  const others = specs.filter((s) => s.member.role !== 'lead');
  const order = reconcileOrder(layout?.tileOrder ?? [], others.map((s) => s.key));
  const panes = useMemo(() => [...team.paneTiles].sort(byCreation).filter((p) => !closed.has(paneKey(p))), [team.paneTiles, closed]);

  const entries = new Map<string, Entry>();
  for (const spec of specs) entries.set(spec.key, { kind: 'member', key: spec.key, spec });
  for (const tile of panes) entries.set(paneKey(tile), { kind: 'pane', key: paneKey(tile), tile });
  const entry = (key: string) => entries.get(key) ?? null;
  const leadEntry = leadSpec ? (entry(leadSpec.key) as Entry) : null;
  // Teammates in their saved order, then the panes in creation order.
  const unified = [...order, ...panes.map(paneKey)];

  const focusedKey = layout?.focused && entries.get(layout.focused)?.kind === 'member' ? layout.focused : null;
  const zoomedKey = layout?.zoomed && entries.get(layout.zoomed)?.kind === 'member' ? layout.zoomed : null;
  const capacity = memberCapacity(band);
  const narrow = band === 'narrow';
  const pinned = pinnedPane && entries.has(pinnedPane) ? pinnedPane : null;

  // The unzoomed arrangement.
  let baseShown: Entry[];
  let baseStrip: Entry[];
  if (narrow) {
    const one = (pinned && entry(pinned)) || (focusedKey && entry(focusedKey)) || leadEntry || (unified[0] ? entry(unified[0]) : null);
    baseShown = one ? [one] : [];
    baseStrip = [...(leadEntry ? [leadEntry] : []), ...unified.map(entry).filter((e): e is Entry => !!e)];
  } else {
    let visibleKeys = unified.slice(0, capacity);
    if (pinned && !visibleKeys.includes(pinned) && capacity > 0) visibleKeys = [...visibleKeys.slice(0, capacity - 1), pinned];
    baseShown = [...(leadEntry ? [leadEntry] : []), ...visibleKeys.map(entry).filter((e): e is Entry => !!e)];
    baseStrip = unified.filter((k) => !visibleKeys.includes(k)).map(entry).filter((e): e is Entry => !!e);
  }
  // `⌘1…⌘6` count members only, in the unzoomed arrangement (panes are never focused).
  const numbered: MemberTileSpec[] = (narrow ? baseStrip : [...baseShown, ...baseStrip]).flatMap((e) => (e.kind === 'member' ? [e.spec] : []));
  const shown: Entry[] = zoomedKey ? [entry(zoomedKey) as Entry] : baseShown;
  const stripEntries: Entry[] = zoomedKey ? [] : baseStrip;
  const indexOf = (key: string) => numbered.findIndex((s) => s.key === key);
  const leadShown = !!leadEntry && shown[0]?.key === leadEntry.key;
  const slots = zoomedKey || narrow ? 0 : shown.length - (leadShown ? 1 : 0);
  const grid = useMemo(() => (zoomedKey || narrow || !leadShown ? panesGrid(0, band) : panesGrid(slots, band)), [zoomedKey, narrow, band, leadShown, slots]);
  // Without a lead tile the teammates use the whole width.
  const teammatesOnly = !zoomedKey && !narrow && !leadShown && shown.length > 0;
  const withDivider = leadShown && grid.columns > 1 && !zoomedKey && !narrow;

  // Publish the sessions on screen (A30.2); clear on unmount.
  const visibleUids = shown.flatMap((e) => (e.kind === 'member' && e.spec.uid ? [e.spec.uid] : [])) as SessionUid[];
  const visibleKey = visibleUids.join('|');
  useEffect(() => {
    setVisibleUids(visibleUids);
  }, [visibleKey]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => () => appStore.getState().setVisibleUids(null), []);

  // New tiles: a pulse and one polite announcement; focus never moves.
  const known = useRef<Set<string> | null>(null);
  const allKeys = [...specs.map((s) => s.key), ...panes.map(paneKey)];
  const keysJoined = allKeys.join('|');
  useEffect(() => {
    if (known.current === null) {
      known.current = new Set(allKeys);
      return;
    }
    const added = allKeys.filter((k) => !(known.current as Set<string>).has(k));
    for (const k of allKeys) known.current.add(k);
    if (added.length === 0) return;
    setFresh(new Set(added));
    for (const k of added) {
      const e = entries.get(k);
      if (e?.kind === 'member') {
        announce(t('floor.joined', { name: flat(e.spec.member.name, CAP.name), team: flat(team.name, CAP.name) }), { politeness: 'polite', key: `team-joined:${k}` });
      }
    }
    const timer = setTimeout(() => setFresh(NO_KEYS), 1200);
    return () => clearTimeout(timer);
  }, [keysJoined]); // eslint-disable-line react-hooks/exhaustive-deps

  const focusKey = (key: MemberKey) => {
    setPinnedPane(null);
    focusMemberTile(world, team.key, key, band);
  };

  const stopFocused = () => {
    if (!focusedKey) return;
    const e = entry(focusedKey);
    if (e?.kind !== 'member') return;
    if (canClosePane(team, e.spec.member)) {
      openSheet('closePane');
      return;
    }
    const session = sessionOf(world, e.spec.uid);
    if (session && session.caps.stopSession !== 'none') openSheet('stopSession');
  };

  // Spatial moves skip pane tiles (they never take focus).
  const memberCells: { cell: Cell; key: MemberKey }[] = [];
  shown.forEach((e, i) => {
    const cell = grid.cells[i];
    if (e.kind === 'member' && cell) memberCells.push({ cell, key: e.key as MemberKey });
  });

  const bindings: HotkeyBinding[] = [];
  for (let i = 0; i < 6; i++) {
    bindings.push({
      id: 'tile.focus',
      combo: `mod+${i + 1}`,
      run: () => {
        const spec = numbered[i];
        if (spec) focusKey(spec.key);
      },
    });
  }
  for (const [combo, dir] of ARROWS) {
    bindings.push({
      id: 'tile.move',
      combo,
      allowRepeat: true,
      run: () => {
        if (!focusedKey || zoomedKey) return;
        const step = dir === 'left' || dir === 'up' ? -1 : 1;
        if (narrow) {
          const at = numbered.findIndex((s) => s.key === focusedKey);
          const target = numbered[at + step];
          if (target) focusKey(target.key);
          return;
        }
        const at = memberCells.findIndex((c) => c.key === focusedKey);
        if (at < 0) return;
        const next = teammatesOnly
          ? at + step >= 0 && at + step < memberCells.length
            ? at + step
            : null
          : spatialNext(memberCells.map((c) => c.cell), at, dir);
        const target = next === null ? undefined : memberCells[next];
        if (target) focusKey(target.key);
      },
    });
  }
  bindings.push({
    id: 'tile.zoom',
    combo: 'mod+shift+enter',
    run: () => {
      if (focusedKey) setTeamLayout(team.key, { zoomed: zoomedKey === focusedKey ? null : focusedKey });
    },
  });
  bindings.push({ id: 'stop', combo: 'mod+shift+backspace', run: stopFocused });
  useHotkeys(bindings, { scope: 'T', enabled: !sheetOpen });

  const menuFor = (spec: MemberTileSpec): TileMenuItem[] => {
    const m = spec.member;
    const name = flat(m.name, CAP.name);
    const items: TileMenuItem[] = [];
    if (spec.uid) items.push({ id: 'split', label: t('session.addToSplit'), run: () => spec.uid && addToSplitOrAsk(spec.uid) });
    if (canTell(world, team, m) && memberRoute(world, team, m) !== 'pane') {
      items.push({ id: 'tell', label: t('team.action.tell', { member: name }), run: () => prefillLead(team.key, tellLine(name)) });
    }
    if (canAskStop(world, team, m)) {
      items.push({
        id: 'askStop',
        label: t('team.action.askStop', { member: name }),
        run: () => prefillLead(team.key, prefillText('team.prefill.askStop', { member: name })),
      });
    }
    if (canClosePane(team, m)) {
      items.push({
        id: 'closePane',
        label: t('team.action.closePane'),
        run: () => {
          setTeamLayout(team.key, { focused: m.key });
          openSheet('closePane');
        },
      });
    }
    return items;
  };

  const dismiss = (key: string) => {
    const next = new Set(closed);
    next.add(key);
    dismissed.set(team.key, next);
    setClosed(next);
  };

  // The divider between the lead and the teammates: saved in the layout (A30.3).
  const saveRatio = (r: number) => {
    setDragRatio(null);
    setTeamLayout(team.key, { leadRatio: clampRatio(r) });
  };
  const onDividerKey = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const step = event.shiftKey ? 0.1 : 0.02;
    if (event.key === 'ArrowLeft') saveRatio(ratio - step);
    else if (event.key === 'ArrowRight') saveRatio(ratio + step);
    else if (event.key === 'Home') saveRatio(0);
    else if (event.key === 'End') saveRatio(1);
    else return;
    event.preventDefault();
  };
  const onDividerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    const el = gridRef.current;
    if (!el) return;
    event.currentTarget.setPointerCapture?.(event.pointerId);
    let last = ratio;
    const move = (e: PointerEvent) => {
      const r = el.getBoundingClientRect();
      if (r.width <= 0) return;
      last = clampRatio((e.clientX - r.left) / r.width);
      setDragRatio(last);
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      saveRatio(last);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };

  let columns: string;
  if (zoomedKey || narrow || grid.columns === 1) columns = teammatesOnly ? `repeat(${Math.min(2, shown.length)}, minmax(0, 1fr))` : 'minmax(0, 1fr)';
  else {
    const rest = (1 - ratio) / (grid.columns - 1);
    columns = `minmax(0, ${ratio}fr) 6px repeat(${grid.columns - 1}, minmax(0, ${rest}fr))`;
  }
  const rows = teammatesOnly ? Math.ceil(shown.length / 2) : grid.rows;

  const tasks = tasksOfTeam(world, team.key);
  const messages = messagesOfTeam(world, team.key);
  const lead = leadMember(world, team);
  const leadName = lead ? flat(lead.name, CAP.name) : t('team.lead');
  const now = clock.wall();

  const stripItems: StripItem[] = stripEntries.map((e) => {
    if (e.kind === 'pane') {
      const phase = paneTilePhase(e.tile, now);
      return {
        key: e.key,
        name: e.tile.title ? flat(e.tile.title, CAP.name) : phase.startsWith('unknown') ? t('team.unknownPane', { lead: leadName }) : t('team.placeholder'),
        state: paneGlyph(phase) as StripItem['state'],
        swatch: null,
        shown: false,
      };
    }
    const session = sessionOf(world, e.spec.uid);
    return {
      key: e.key,
      name: flat(e.spec.member.name, CAP.name),
      state: session?.activity.state.kind ?? null,
      swatch: e.spec.member.colour,
      shown: shown.some((x) => x.key === e.key),
    };
  });
  const pickStrip = (key: string) => {
    const e = entry(key);
    if (e?.kind === 'pane') setPinnedPane(key);
    else if (e?.kind === 'member') focusKey(e.key as MemberKey);
  };

  return (
    <div className="g-panes" data-band={band}>
      {stripItems.length > 0 && <TileStrip items={stripItems} prefix={narrow ? undefined : `+${stripItems.length}`} onPick={pickStrip} />}
      <div
        ref={gridRef}
        className="g-panes-grid"
        data-zoomed={zoomedKey ? 'true' : undefined}
        style={{ gridTemplateColumns: columns, gridTemplateRows: `repeat(${Math.max(1, rows)}, minmax(0, 1fr))` }}
      >
        {shown.map((e, i) => {
          let style;
          if (teammatesOnly) style = undefined;
          else if (zoomedKey || narrow || grid.columns === 1) style = { gridColumn: '1 / -1', gridRow: '1 / -1' };
          else {
            const cell = grid.cells[i];
            style = cell ? cellStyle(cell, i === 0 ? 0 : 1) : undefined;
          }
          if (e.kind === 'pane') {
            return (
              <PaneTileView
                key={e.key}
                tile={e.tile}
                leadName={leadName}
                index={numbered.length + panes.findIndex((p) => paneKey(p) === e.key)}
                count={numbered.length + panes.length}
                style={style}
                isNew={fresh.has(e.key)}
                onClose={() => {
                  if (pinned === e.key) setPinnedPane(null);
                  dismiss(e.key);
                }}
              />
            );
          }
          const spec = e.spec;
          const m = spec.member;
          const isLead = m.role === 'lead';
          return (
            <Tile
              key={spec.key}
              uid={spec.uid}
              index={indexOf(spec.key)}
              count={numbered.length}
              name={flat(m.name, CAP.name)}
              swatch={m.colour}
              roleLabel={isLead ? t('team.lead') : null}
              context="team"
              focused={focusedKey === spec.key}
              zoomed={zoomedKey === spec.key}
              style={style}
              isNew={fresh.has(spec.key)}
              onFocused={() => {
                setTeamLayout(team.key, { focused: spec.key });
                noteFocused(surfaceOf(team.key), spec.key);
              }}
              onToggleZoom={() => setTeamLayout(team.key, { focused: spec.key, zoomed: zoomedKey === spec.key ? null : spec.key })}
              menu={menuFor(spec)}
              notice={
                spec.kind === 'crashed'
                  ? {
                      text: t('tile.crashed', { code: spec.code ?? t('common.unknown') }),
                      action: { label: t('action.closeTile'), run: () => dismiss(spec.key) },
                    }
                  : null
              }
              emptyText={isLead ? t('tile.gone') : t('team.placeholder')}
              allowAddToSplit
              onAddToSplit={(uid) => addToSplitOrAsk(uid)}
            />
          );
        })}
        {withDivider && (
          <div
            className="g-panes-divider"
            role="separator"
            aria-orientation="vertical"
            aria-label={t('team.divider')}
            aria-valuemin={20}
            aria-valuemax={80}
            aria-valuenow={Math.round(ratio * 100)}
            tabIndex={0}
            style={{ gridColumn: '2 / span 1', gridRow: '1 / -1' }}
            onKeyDown={onDividerKey}
            onPointerDown={onDividerDown}
          />
        )}
      </div>
      <div className="g-panes-foot">
        <span>
          {t('team.footer', {
            tasks: countText(taskCount(team, tasks), t, locale),
            messages: countText(messageCount(team, messages.length), t, locale),
          })}
        </span>
        <button type="button" className="g-tbtn" aria-keyshortcuts="Meta+Alt+2" onClick={() => setTeamMode(team.key, 'board')}>
          {t('team.toBoard')} <span className="g-tile-key">⌥⌘2</span>
        </button>
      </div>
    </div>
  );
}
