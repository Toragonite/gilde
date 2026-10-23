/**
 * What the palette offers (ux-ia §7.4): every entity — live sessions first,
 * then ended ones, projects, teams, members, accounts — and the v1 commands.
 *
 *  - Entries match on their Korean and their English names ("작업장" and
 *    "Floor") whatever the UI language, plus keywords.
 *  - Prefixes narrow: `>` commands, `#` projects, `@` accounts.
 *  - A command that cannot run now stays listed, disabled, with its reason.
 *  - Entities rank before commands at equal match quality.
 */

import type { MessageKey, MessageParams } from '../i18n';
import { translate } from '../i18n';
import type { Locale } from '../lib/gen/Locale';
import type { Session } from '../lib/gen/Session';
import { vendorBadge } from '../lib/vendor';
import type { SheetContext, UiState } from '../store/uiSlice';
import type { WorldState } from '../store/worldSlice';
import { sessionOf } from '../store/worldSlice';
import type { T } from '../shell/words';
import { displayName, isLive, ownerApp, stateWord } from '../shell/words';
import { fuzzyBest } from './match';
import type { FuzzyMatch } from './match';

export type EntryGroup = 'sessions' | 'projects' | 'teams' | 'members' | 'accounts' | 'commands';

export type PaletteAction =
  | { kind: 'openSession'; uid: Session['uid'] }
  | { kind: 'navigate'; view: UiState['view'] }
  | { kind: 'sheet'; sheet: NonNullable<UiState['sheet']>; context?: SheetContext }
  | { kind: 'run'; id: string };

export interface PaletteEntry {
  id: string;
  group: EntryGroup;
  /** In the UI language. */
  label: string;
  /** Secondary text (project · vendor · state …). */
  detail: string;
  /** Every name it matches on (both languages, keywords). */
  haystack: string[];
  action: PaletteAction;
  /** Set when the command cannot run now: the reason, shown in place of the detail. */
  disabledReason: string | null;
  /** 0 = first among equals (live sessions), larger = later; per-project and per-team commands (6) follow the general ones (5). */
  rank: number;
}

const GROUP_ORDER: readonly EntryGroup[] = ['sessions', 'projects', 'teams', 'members', 'accounts', 'commands'];

/** Both languages of a message, for matching. */
function both(key: MessageKey, params?: MessageParams): string[] {
  return [translate('ko', key, params), translate('en', key, params)];
}

export interface EntryContext {
  world: WorldState;
  ui: UiState;
  t: T;
  locale: Locale;
}

/** The session the current view is about, if any (for "Add to Split", "Stop session…" …). */
function sessionInFront(ctx: EntryContext): Session | null {
  const view = ctx.ui.view;
  if (view.screen === 'session') return sessionOf(ctx.world, view.uid);
  if (view.screen === 'split' && ctx.ui.split.focused) return sessionOf(ctx.world, ctx.ui.split.focused);
  return null;
}

export function buildEntries(ctx: EntryContext): PaletteEntry[] {
  const { world, ui, t, locale } = ctx;
  const out: PaletteEntry[] = [];

  // Sessions: live first, then ended (still in the store today).
  for (const s of world.sessions.values()) {
    const name = displayName(t, world, s, locale);
    const project = s.projectId ? (world.projects.get(s.projectId)?.name ?? '') : '';
    out.push({
      id: `session:${s.uid}`,
      group: 'sessions',
      label: name,
      detail: [project, vendorBadge(s.vendor), stateWord(t, s)].filter(Boolean).join(' · '),
      haystack: [name, project],
      action: { kind: 'openSession', uid: s.uid },
      disabledReason: null,
      rank: isLive(s) ? 0 : 1,
    });
  }

  for (const p of world.projects.values()) {
    out.push({
      id: `project:${p.id}`,
      group: 'projects',
      label: p.name,
      detail: t('shell.running', { n: p.liveSessions }),
      haystack: [p.name, `#${p.name}`],
      action: { kind: 'sheet', sheet: 'newSession', context: { projectId: p.id } },
      disabledReason: null,
      rank: 2,
    });
    out.push({
      id: `newIn:${p.id}`,
      group: 'commands',
      label: t('sidebar.newIn', { project: p.name }),
      detail: '',
      haystack: both('sidebar.newIn', { project: p.name }),
      action: { kind: 'sheet', sheet: 'newSession', context: { projectId: p.id } },
      disabledReason: null,
      rank: 6,
    });
  }

  for (const team of world.teams.values()) {
    const project = team.projectId ? (world.projects.get(team.projectId)?.name ?? '') : '';
    out.push({
      id: `team:${team.key}`,
      group: 'teams',
      label: t('sidebar.team', { name: team.name }),
      detail: project,
      haystack: [...both('sidebar.team', { name: team.name }), team.name],
      action: { kind: 'navigate', view: { screen: 'team', teamKey: team.key, mode: ui.teamLayouts[team.key]?.mode ?? 'panes' } },
      disabledReason: null,
      rank: 3,
    });
    out.push({
      id: `floorOf:${team.key}`,
      group: 'commands',
      label: t('palette.cmd.floorOf', { team: team.name }),
      detail: '',
      haystack: both('palette.cmd.floorOf', { team: team.name }),
      action: { kind: 'navigate', view: { screen: 'team', teamKey: team.key, mode: 'floor' } },
      disabledReason: null,
      rank: 6,
    });
  }

  for (const m of world.members.values()) {
    const team = world.teams.get(m.teamKey);
    if (!m.sessionUid || !team) continue;
    out.push({
      id: `member:${m.key}`,
      group: 'members',
      label: t('sidebar.member', { member: m.name, team: team.name }),
      detail: m.role === 'lead' ? t('sidebar.lead') : vendorBadge(m.vendor),
      haystack: [m.name, team.name],
      action: { kind: 'openSession', uid: m.sessionUid },
      disabledReason: null,
      rank: 3,
    });
  }

  for (const a of world.accounts.values()) {
    out.push({
      id: `account:${a.id}`,
      group: 'accounts',
      label: `${vendorBadge(a.vendor)} ${a.label}`,
      detail: a.isDefault ? t('account.default') : '',
      haystack: [a.label, `@${a.label}`, a.vendor],
      action: { kind: 'navigate', view: { screen: 'accounts', section: 'accounts' } },
      disabledReason: null,
      rank: 4,
    });
  }

  // Commands (v1).
  const cmd = (id: string, key: MessageKey, action: PaletteAction, disabledReason: string | null = null, params?: MessageParams, extra: string[] = []) => {
    out.push({ id, group: 'commands', label: t(key, params), detail: '', haystack: [...both(key, params), ...extra], action, disabledReason, rank: 5 });
  };
  cmd('newSession', 'action.newSession', { kind: 'sheet', sheet: 'newSession' });
  cmd('home', 'nav.home', { kind: 'navigate', view: { screen: 'home' } });
  cmd('history', 'nav.history', { kind: 'navigate', view: { screen: 'history' } });
  cmd('accounts', 'nav.accountsHealth', { kind: 'navigate', view: { screen: 'accounts', section: 'accounts' } });
  cmd('floor', 'shell.screen.floor', { kind: 'navigate', view: { screen: 'floor' } }, null, undefined, both('action.openFloor'));
  cmd('showAsList', 'action.showAsList', { kind: 'navigate', view: { screen: 'floor' } });
  if (ui.split.uids.length > 0) cmd('split', 'palette.cmd.showSplit', { kind: 'navigate', view: { screen: 'split' } });

  const front = sessionInFront(ctx);
  if (front) {
    const name = displayName(t, world, front, locale);
    const owner = ownerApp(t, front.control);
    cmd('addToSplit', 'palette.cmd.addToSplit', { kind: 'run', id: 'addToSplit' }, null); // a full Split asks which tile to replace (A30.5)
    if (front.caps.stopSession !== 'none') cmd('stopSession', 'action.stopSession', { kind: 'sheet', sheet: 'stopSession' });
    if (front.caps.ownerFocus && owner) cmd('goToOwner', 'action.goToOwner', { kind: 'run', id: 'goToOwner' }, null, { owner });
    for (const option of front.caps.continueOptions) {
      if (option === 'resume') cmd('continue', 'palette.cmd.continue', { kind: 'run', id: 'continue' }, null, { name });
      if (option === 'fork') cmd('continueCopy', 'palette.cmd.continueCopy', { kind: 'run', id: 'continueCopy' }, null, { name });
    }
    const writeBlocked =
      front.control.kind === 'watching' ? t('palette.why.notInGilde', { owner: owner ?? t('origin.unknown') }) : front.caps.changes === 'none' ? t('palette.why.noChanges') : null;
    cmd('commit', 'palette.cmd.commit', { kind: 'sheet', sheet: 'commit' }, writeBlocked);
    cmd('merge', 'palette.cmd.merge', { kind: 'sheet', sheet: 'merge' }, writeBlocked);
    cmd('discard', 'palette.cmd.discard', { kind: 'sheet', sheet: 'discardWorktree' }, writeBlocked);
  }

  const launch = ui.launch;
  const stopped = launch && (launch.kind === 'afterQuit' || launch.kind === 'afterCrash') && launch.count > 0;
  cmd('continueAll', 'palette.cmd.continueAll', { kind: 'run', id: 'continueAll' }, stopped ? null : t('palette.why.noStopped'));
  cmd('cleanUp', 'palette.cmd.cleanUp', { kind: 'sheet', sheet: 'cleanUp' }, world.leftovers.size > 0 ? null : t('palette.why.noLeftovers'));
  cmd('showProcesses', 'palette.cmd.showProcesses', { kind: 'navigate', view: { screen: 'accounts', section: 'health' } });

  for (const a of world.accounts.values()) {
    if (a.isDefault || !a.usableForNew) continue;
    cmd(`makeDefault:${a.id}`, 'palette.cmd.makeDefault', { kind: 'run', id: `makeDefault:${a.id}` }, null, { account: a.label, vendor: vendorBadge(a.vendor) });
  }

  if (world.queue.pausedUntil === null) {
    cmd('pause', 'notify.pause1h', { kind: 'run', id: 'pause1h' });
    cmd('pauseUntilResume', 'notify.pauseUntilResume', { kind: 'run', id: 'pauseUntilResume' });
  } else {
    cmd('resume', 'notify.resume', { kind: 'run', id: 'resume' });
  }
  cmd('toggleSidebar', 'palette.cmd.toggleSidebar', { kind: 'run', id: 'toggleSidebar' });
  cmd('settings', 'palette.cmd.settings', { kind: 'navigate', view: { screen: 'settings' } });
  cmd('help', 'palette.cmd.help', { kind: 'sheet', sheet: 'help' });
  return out;
}

export interface RankedEntry {
  entry: PaletteEntry;
  match: FuzzyMatch;
}

/** Parse the query's prefix: `>` commands, `#` projects, `@` accounts. */
export function parseQuery(raw: string): { group: EntryGroup | null; text: string } {
  const trimmed = raw.trimStart();
  const first = trimmed[0];
  if (first === '>') return { group: 'commands', text: trimmed.slice(1).trim() };
  if (first === '#') return { group: 'projects', text: trimmed.slice(1).trim() };
  if (first === '@') return { group: 'accounts', text: trimmed.slice(1).trim() };
  return { group: null, text: trimmed.trim() };
}

/** Matching entries, best first; groups keep their order at equal quality. */
export function rankEntries(entries: readonly PaletteEntry[], raw: string, limit = 50): RankedEntry[] {
  const { group, text } = parseQuery(raw);
  const pool = group ? entries.filter((e) => e.group === group) : entries;
  const ranked: RankedEntry[] = [];
  for (const entry of pool) {
    const match = text === '' ? { score: 0, positions: [] } : fuzzyBest(text, [entry.label, ...entry.haystack]);
    if (match) ranked.push({ entry, match });
  }
  ranked.sort((a, b) => {
    if (text !== '' && b.match.score !== a.match.score) return b.match.score - a.match.score;
    if (a.entry.rank !== b.entry.rank) return a.entry.rank - b.entry.rank;
    return GROUP_ORDER.indexOf(a.entry.group) - GROUP_ORDER.indexOf(b.entry.group);
  });
  return ranked.slice(0, limit);
}
