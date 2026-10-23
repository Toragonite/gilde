/**
 * The Floor's polite announcements (pixel-office §7): a task finished, a task
 * unblocked, a teammate joined or left. Debounced 2 s and coalesced into one
 * sentence; never for animations. Entering the queue is announced by the
 * shell's own throttled summary (ux-ia §10), so the Floor adds nothing there.
 */

import { useEffect, useRef } from 'react';

import type { Locale } from '../i18n';
import { translate } from '../i18n';
import { announce } from '../lib/notify';
import type { FlVm } from './model';

interface Facts {
  tasks: Map<string, { station: string; title: string }>;
  members: Map<string, { name: string; team: string; here: boolean }>;
}

export const ANNOUNCE_DEBOUNCE_MS = 2000;

export function factsOf(vm: FlVm): Facts {
  const tasks = new Map<string, { station: string; title: string }>();
  const members = new Map<string, { name: string; team: string; here: boolean }>();
  for (const t of vm.teams) {
    for (const it of t.items) tasks.set(it.key, { station: it.station, title: it.title });
    for (const id of t.line) {
      const s = vm.seats.get(id);
      if (s) members.set(id, { name: s.name, team: t.name, here: !s.ghost });
    }
  }
  return { tasks, members };
}

/** What changed between two moments, as sentences. */
export function changes(locale: Locale, a: Facts, b: Facts): string[] {
  const out: string[] = [];
  for (const [k, v] of b.tasks) {
    const o = a.tasks.get(k);
    if (!o) continue;
    if (v.station === 'done' && o.station !== 'done') out.push(translate(locale, 'floor.taskDone', { task: v.title }));
    else if (o.station === 'blocked' && v.station !== 'blocked' && v.station !== 'done') out.push(translate(locale, 'floor.unblocked', { task: v.title }));
  }
  for (const [k, v] of b.members) {
    const o = a.members.get(k);
    if ((!o && v.here) || (o && !o.here && v.here)) out.push(translate(locale, 'floor.joined', { name: v.name, team: v.team }));
    else if (o && o.here && !v.here) out.push(translate(locale, 'floor.left', { name: v.name, team: v.team }));
  }
  return out;
}

export function useFloorAnnouncements(vm: FlVm, locale: Locale): void {
  const prev = useRef<Facts | null>(null);
  const pending = useRef<string[]>([]);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    const cur = factsOf(vm);
    if (prev.current) {
      const said = changes(locale, prev.current, cur);
      if (said.length) {
        pending.current.push(...said);
        if (timer.current) clearTimeout(timer.current);
        timer.current = setTimeout(() => {
          timer.current = null;
          const text = pending.current.join(' ');
          pending.current = [];
          announce(text.length > 300 ? text.slice(0, 299) + '…' : text, { politeness: 'polite' });
        }, ANNOUNCE_DEBOUNCE_MS);
      }
    }
    prev.current = cur;
  }, [vm, locale]);
  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );
}
