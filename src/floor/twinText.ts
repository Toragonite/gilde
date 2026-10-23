/**
 * The text twin's content (pixel-office §7, ux-ia §10): the same bays, teams,
 * members, pipeline items and standalone sessions the canvas draws, as
 * sentences. Pure, so the twin, the tests and the canvas agree.
 */

import type { Locale } from '../i18n';
import { translate } from '../i18n';
import type { FloorScope } from './layout';
import { NO_PROJECT } from './layout';
import type { FlItem, FlSeat, FlTeam, FlVm } from './model';
import { currentItem, orderedItems } from './model';
import { floorDuration, ownershipWord, stateWord } from './words';

export interface TwinGroup {
  key: string;
  label: string;
  teams: FlTeam[];
  solo: FlSeat[];
}

/** Bays in canvas order: project order, then "no project"; global shows live teams and standalone sessions. */
export function twinGroups(vm: FlVm, scope: FloorScope, soloIndex: (bay: string, id: string) => number | undefined = () => undefined): TwinGroup[] {
  const global = scope.kind === 'global';
  const teams = global ? vm.teams.filter((t) => t.alive) : vm.teams.filter((t) => scope.kind === 'team' && t.key === scope.teamKey);
  const known = new Set(vm.projects.map((p) => p.id as string));
  const bucket = (pid: string | null) => (pid && known.has(pid) ? pid : NO_PROJECT);
  const order: { id: string; label: string }[] = vm.projects.map((p) => ({ id: p.id as string, label: p.name }));
  order.push({ id: NO_PROJECT, label: translate('en', 'floor.noProject') });
  const out: TwinGroup[] = [];
  for (const p of order) {
    const ts = teams.filter((t) => bucket(t.projectId) === p.id);
    const key = (global ? 'g:' : 't:') + p.id;
    const solo = global
      ? vm.solo
          .map((id) => vm.seats.get(id))
          .filter((s): s is FlSeat => !!s && bucket(s.projectId) === p.id)
          .map((s, i) => ({ s, i, idx: soloIndex(key, s.id) }))
          .sort((a, b) => (a.idx ?? 1e9 + a.i) - (b.idx ?? 1e9 + b.i))
          .map((x) => x.s)
      : [];
    if (ts.length === 0 && solo.length === 0) continue;
    out.push({ key, label: p.label, teams: ts, solo });
  }
  return out;
}

export function groupLabel(locale: Locale, g: TwinGroup, noProject: boolean): string {
  const sessions = g.teams.reduce((n, t) => n + t.line.length, 0) + g.solo.length;
  const project = noProject ? translate(locale, 'floor.noProject') : g.label;
  return translate(locale, 'floor.twinRoom', { project, sessions });
}

export function teamLabel(locale: Locale, vm: FlVm, t: FlTeam): string {
  const blocked = t.items.filter((i) => i.station === 'blocked').length;
  const done = (t.approx ? '≈' : '') + t.done;
  let s = t.noTaskList
    ? translate(locale, 'floor.twinTeamNoList', { team: t.name })
    : t.inferred
      ? translate(locale, 'floor.twinTeamInferred', { team: t.name, done, all: t.total })
      : translate(locale, 'floor.twinTeam', { team: t.name, done, all: t.total, blocked });
  if (t.inFlight !== null) s += translate(locale, 'floor.twinFlight', { flight: t.inFlight });
  const who = t.bottlenecks.map((k) => vm.seats.get(k)?.name).filter(Boolean);
  if (who.length) s += translate(locale, 'floor.twinBottleneck', { who: who.join(', ') });
  return s;
}

export function elapsedWord(locale: Locale, vm: FlVm, it: FlItem): string | null {
  if (it.started === null) return null;
  const time = floorDuration(locale, Math.max(0, vm.now - it.started));
  return translate(locale, it.approx ? 'floor.elapsedSince' : 'floor.elapsed', { time });
}

export function seatLabel(locale: Locale, vm: FlVm, seat: FlSeat, team: FlTeam | null): string {
  let s = translate(locale, 'floor.twinSeat', { name: seat.name, vendor: seat.vendor, state: stateWord(locale, seat), own: ownershipWord(locale, seat) });
  if (team && seat.role) s += translate(locale, 'floor.twinRole', { role: translate(locale, seat.role === 'lead' ? 'floor.role.lead' : 'floor.role.member') });
  const cur = team ? currentItem(team, seat.id as never) : null;
  if (cur) s += translate(locale, 'floor.twinCurrent', { task: cur.title, time: elapsedWord(locale, vm, cur) ?? '—' });
  return s;
}

export function stationWord(locale: Locale, it: FlItem): string {
  if (it.rawStation === 'reported') return translate(locale, 'floor.stn.reported');
  return translate(locale, ('floor.stn.' + it.station) as 'floor.stn.todo');
}

export function itemLabel(locale: Locale, vm: FlVm, t: FlTeam, it: FlItem): string {
  const owner = it.owner && it.station !== 'todo' ? vm.seats.get(it.owner)?.name : undefined;
  let s = translate(locale, 'floor.twinItem', {
    task: it.title + (it.inferred ? ' (' + translate(locale, 'floor.inferred') + ')' : ''),
    station: stationWord(locale, it),
    owner: owner ?? translate(locale, 'floor.unassigned'),
  });
  if (it.station === 'blocked' && it.blockedBy) {
    const bl = t.items.find((x) => x.key === it.blockedBy);
    if (bl) s += ', ' + translate(locale, 'floor.blockedBy', { task: bl.title });
  }
  if (it.station === 'doing' || it.station === 'you') {
    const el = elapsedWord(locale, vm, it);
    if (el) s += ', ' + el;
  }
  if (it.noReport) s += ', ' + translate(locale, 'floor.noReport');
  return s;
}

export { orderedItems };
