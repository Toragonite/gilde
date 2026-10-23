/**
 * The Floor's text twin (pixel-office §7, ux-ia §10, F7): a DOM `tree`
 * labelled "작업장 / Floor", rendered from the same store as the canvas —
 * project → team (progress, in flight, bottleneck) → members, then the
 * pipeline with one item per task; standalone sessions follow. It **is** the
 * "Show as list" view; in picture mode it stays in the accessibility tree
 * (visually hidden) and keyboard selection on the Floor is focus here
 * (roving tabindex), mirrored on the canvas.
 */

import { useMemo } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent, MouseEvent as ReactMouseEvent } from 'react';

import { translate } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import type { TeamKey } from '../lib/gen/TeamKey';
import { useRovingTabIndex } from '../hooks/useRovingTabIndex';
import type { FloorScope } from './layout';
import { NO_PROJECT } from './layout';
import type { FlVm } from './model';
import { orderedItems } from './model';
import { groupLabel, itemLabel, seatLabel, teamLabel, twinGroups } from './twinText';

export interface TwinRow {
  key: string;
  level: number;
  label: string;
  kind: 'bay' | 'team' | 'seat' | 'pipeline' | 'item' | 'standalone';
  /** The seat a row selects (members and standalone sessions). */
  seat: string | null;
  /** The seat a row opens (a member, a session, or a task's owner). */
  opens: string | null;
  team: TeamKey | null;
  children: TwinRow[];
}

export function twinRows(locale: 'ko' | 'en', vm: FlVm, scope: FloorScope, soloIndex?: (bay: string, id: string) => number | undefined): TwinRow[] {
  const rows: TwinRow[] = [];
  for (const g of twinGroups(vm, scope, soloIndex)) {
    const bay: TwinRow = { key: 'bay:' + g.key, level: 1, label: groupLabel(locale, g, g.key.endsWith(NO_PROJECT)), kind: 'bay', seat: null, opens: null, team: null, children: [] };
    for (const t of g.teams) {
      const team: TwinRow = { key: 'team:' + t.key, level: 2, label: teamLabel(locale, vm, t), kind: 'team', seat: null, opens: null, team: t.key, children: [] };
      for (const id of t.line) {
        const s = vm.seats.get(id);
        if (!s) continue;
        team.children.push({ key: 'seat:' + id, level: 3, label: seatLabel(locale, vm, s, t), kind: 'seat', seat: id, opens: id, team: t.key, children: [] });
      }
      const pipeLabel = t.inferred ? `${trPipeline(locale)} · ${trInferredLabel(locale)}` : t.noTaskList ? `${trPipeline(locale)} · ${trNoList(locale)}` : trPipeline(locale);
      const pipe: TwinRow = { key: 'pipe:' + t.key, level: 3, label: pipeLabel, kind: 'pipeline', seat: null, opens: null, team: t.key, children: [] };
      for (const it of orderedItems(t)) {
        pipe.children.push({ key: 'item:' + it.key, level: 4, label: itemLabel(locale, vm, t, it), kind: 'item', seat: null, opens: it.owner && it.station !== 'todo' ? it.owner : null, team: t.key, children: [] });
      }
      team.children.push(pipe);
      bay.children.push(team);
    }
    if (g.solo.length) {
      const solo: TwinRow = { key: 'solo:' + g.key, level: 2, label: trStandalone(locale), kind: 'standalone', seat: null, opens: null, team: null, children: [] };
      for (const s of g.solo) solo.children.push({ key: 'seat:' + s.id, level: 3, label: seatLabel(locale, vm, s, null), kind: 'seat', seat: s.id, opens: s.id, team: null, children: [] });
      bay.children.push(solo);
    }
    rows.push(bay);
  }
  return rows;
}

const trPipeline = (l: 'ko' | 'en') => translate(l, 'floor.pipeline');
const trInferredLabel = (l: 'ko' | 'en') => translate(l, 'floor.inferredLabel');
const trNoList = (l: 'ko' | 'en') => translate(l, 'floor.noTaskList');
const trStandalone = (l: 'ko' | 'en') => translate(l, 'floor.standalone');

/** Pre-order, with each row's position among its siblings (`aria-posinset` / `aria-setsize`). */
export function flatten(rows: readonly TwinRow[]): { row: TwinRow; pos: number; size: number }[] {
  const out: { row: TwinRow; pos: number; size: number }[] = [];
  const walk = (list: readonly TwinRow[]) =>
    list.forEach((r, i) => {
      out.push({ row: r, pos: i + 1, size: list.length });
      walk(r.children);
    });
  walk(rows);
  return out;
}

export interface TwinProps {
  vm: FlVm;
  scope: FloorScope;
  visible: boolean;
  selected: string | null;
  /** The pinned name card's id, for the selected row's `aria-describedby`. */
  describedBy?: string | null;
  soloIndex?: (bay: string, id: string) => number | undefined;
  onFocusRow: (row: TwinRow) => void;
  onActivate: (row: TwinRow, event: { metaKey: boolean }) => void;
}

export function Twin({ vm, scope, visible, selected, describedBy, soloIndex, onFocusRow, onActivate }: TwinProps) {
  const { t, locale } = useI18n();
  const rows = useMemo(() => twinRows(locale, vm, scope, soloIndex), [locale, vm, scope, soloIndex]);
  const flat = useMemo(() => flatten(rows), [rows]);
  const roving = useRovingTabIndex(flat.length, { orientation: 'vertical' });

  // A flat tree (levels by aria-level): each item's name is its own text, never its descendants'.
  return (
    <div className={visible ? 'fl-twin' : 'fl-twin fl-sr-only'} data-fl-twin="">
      <ul role="tree" aria-label={t('floor.name')}>
        {flat.map(({ row: r, pos, size }, i) => {
          const props = roving.getItemProps(i);
          const sel = r.seat !== null && r.seat === selected;
          const onClick = (e: ReactMouseEvent) => onActivate(r, { metaKey: e.metaKey });
          const onKeyDown = (e: ReactKeyboardEvent<HTMLElement>) => props.onKeyDown(e);
          return (
            <li
              key={r.key}
              role="treeitem"
              aria-level={r.level}
              aria-posinset={pos}
              aria-setsize={size}
              aria-selected={sel}
              aria-describedby={describedBy && sel ? describedBy : undefined}
              className={'fl-twin-row fl-lv' + r.level + (sel ? ' fl-sel' : '')}
              data-fl-row={r.key}
              data-flsel={r.seat ?? undefined}
              tabIndex={props.tabIndex}
              ref={props.ref}
              data-roving-index={props['data-roving-index']}
              onKeyDown={onKeyDown}
              onFocus={() => {
                props.onFocus();
                onFocusRow(r);
              }}
              onClick={onClick}
            >
              {r.label}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
