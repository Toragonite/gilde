/**
 * The Team screen (ux-ia §3.8–§3.11, J5; contract §15.2 T16, A23.2, A23.5):
 *
 *   header   ▲ Team auth-split · project › location · lead + 3 · In Gilde · 1.4 GB · 38m
 *                                                  [Panes | Board | Floor] [⋯]
 *   lines    hosting facts (in-process fallback, panes outside Gilde, a
 *            backgrounded lead, a stale roster, team ended)
 *   body     Panes (tiles) · Board (columns) · Floor (`<TeamFloor teamKey />`)
 *
 * `⌥⌘1/2/3` switch modes like the Session screen's sub-views (no history
 * entry; the mode is kept in `teamLayout:<TeamKey>`). A team whose panes
 * run in-process shows Board and Floor only (ux-ia §3.8 "Canary failure").
 * The `⋯` popover holds the team-level actions the capabilities allow
 * (Tell everyone…, Ask the lead to add a task…, prefilled and never sent) and
 * the facts kept out of the header (raw team id, the adapter's fidelity note).
 */

import { useEffect, useId, useMemo, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';

import { ownerWord } from '../approval';
import { useNow } from '../approval/useNow';
import { Popover, popoverTriggerProps } from '../components/Popover';
import { StateGlyph } from '../components/StateGlyph';
import { MoreIcon } from '../components/icons';
import { TeamFloor } from '../floor';
import { isImeComposing, useHotkeyScope, useHotkeys } from '../hooks/useHotkeys';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { formatBytes, formatDuration } from '../lib/format';
import type { Team } from '../lib/gen/Team';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamViewMode } from '../lib/gen/TeamViewMode';
import type { WidthBand } from '../shell/layout';
import { useBand } from '../split/useBand';
import { useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import type { WorldState } from '../store/worldSlice';
import { prefillLead, prefillText, setTeamMode } from './actions';
import { Board } from './Board';
import { CAP, canAddTask, canTellAll, flat, isPresent, leadMember, orderedMembers, panesAvailable, teamMemory } from './model';
import { Panes } from './Panes';
import './team.css';

export interface TeamScreenProps {
  teamKey: TeamKey;
  mode: TeamViewMode;
  /** Tests: the width band (default: the window's). */
  band?: WidthBand;
}

const MODES: readonly TeamViewMode[] = ['panes', 'board', 'floor'];
const MODE_LABEL: Record<TeamViewMode, MessageKey> = { panes: 'team.mode.panes', board: 'team.mode.board', floor: 'team.mode.floor' };

export function TeamScreen({ teamKey, mode, band: forced }: TeamScreenProps) {
  const { t } = useI18n();
  const team = useAppStore((s) => s.world.teams.get(teamKey) ?? null);
  const band = useBand(forced);
  const root = useRef<HTMLDivElement>(null);
  useHotkeyScope('T', true, root);

  const available = team ? panesAvailable(team) : true;
  const shown: TeamViewMode = mode === 'panes' && !available ? 'board' : mode;
  const modes = MODES.filter((m) => m !== 'panes' || available);

  // Panes asked for but not available: show the Board and say so in the view.
  useEffect(() => {
    if (team && shown !== mode) setTeamMode(teamKey, shown);
  }, [team, shown, mode, teamKey]);

  const sheetOpen = useAppStore((s) => s.ui.sheet !== null);
  useHotkeys(
    MODES.map((m, i) => ({
      id: `team.view.${m}`,
      combo: `mod+alt+${i + 1}`,
      run: () => {
        if (modes.includes(m)) setTeamMode(teamKey, m);
      },
    })),
    { scope: 'T', enabled: !!team && !sheetOpen },
  );

  if (!team) {
    return (
      <div className="g-team" ref={root}>
        <p className="g-team-gone">{t('team.gone')}</p>
      </div>
    );
  }

  return (
    <div className="g-team" ref={root} data-mode={shown} data-band={band}>
      <TeamHeader team={team} shown={shown} modes={modes} />
      <TeamLines team={team} />
      <div className="g-team-body" role="tabpanel" aria-label={t(MODE_LABEL[shown])}>
        {shown === 'panes' && <Panes team={team} band={band} />}
        {shown === 'board' && <Board team={team} band={band} />}
        {shown === 'floor' && (
          <div className="g-team-floor">
            <TeamFloor teamKey={team.key} />
          </div>
        )}
      </div>
    </div>
  );
}

function TeamHeader({ team, shown, modes }: { team: Team; shown: TeamViewMode; modes: readonly TeamViewMode[] }) {
  const { t, locale } = useI18n();
  const now = useNow();
  const world = useAppStore((s) => s.world);
  const tabsRef = useRef<HTMLDivElement>(null);
  const members = useMemo(() => orderedMembers(world, team.key), [world, team.key]);
  const lead = leadMember(world, team);
  const leadSession = sessionOf(world, lead?.sessionUid ?? null);
  const project = team.projectId ? (world.projects.get(team.projectId) ?? null) : null;
  const location = leadSession?.locationId ? (world.locations.get(leadSession.locationId) ?? null) : null;
  const teammates = members.filter((m) => m.role !== 'lead' && isPresent(m)).length;
  const memory = teamMemory(world, members);
  const ownership =
    team.source === 'orchestrator'
      ? t('origin.orchestrator')
      : leadSession?.control.kind === 'inGilde'
        ? t('ownership.inGilde')
        : leadSession?.control.kind === 'watching'
          ? t('ownership.runsIn', { owner: ownerWord(leadSession, t) })
          : null;
  const where = [project?.name ?? null, location?.worktreeName ? `wt/${location.worktreeName}` : (location?.branch ?? null)].filter(Boolean).join(' › ');
  const meta = [
    where || null,
    t('team.leadPlus', { members: teammates }),
    ownership,
    memory !== null ? formatBytes(memory, locale) : null,
    team.startedAt ? formatDuration(Math.max(0, (team.endedAt ?? now) - team.startedAt), locale) : null,
  ].filter((x): x is string => typeof x === 'string' && x !== '');

  const onTabKey = (event: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (event.metaKey || event.ctrlKey || event.altKey || isImeComposing(event.nativeEvent)) return;
    const at = modes.indexOf(shown);
    let next = -1;
    if (event.key === 'ArrowRight') next = (at + 1) % modes.length;
    else if (event.key === 'ArrowLeft') next = (at - 1 + modes.length) % modes.length;
    if (next < 0) return;
    event.preventDefault();
    const m = modes[next] as TeamViewMode;
    setTeamMode(team.key, m);
    requestAnimationFrame(() => tabsRef.current?.querySelector<HTMLButtonElement>(`[data-mode="${m}"]`)?.focus());
  };

  return (
    <div className="g-team-head">
      <StateGlyph state={team.glyphState.kind} />
      <h1 className="g-team-title">{t('team.title', { name: flat(team.name, CAP.name) })}</h1>
      <span className="g-team-meta">{meta.join(' · ')}</span>
      <span className="g-team-fill" />
      <div ref={tabsRef} className="g-team-modes" role="tablist" aria-label={t('team.modes')}>
        {modes.map((m) => (
          <button
            key={m}
            type="button"
            role="tab"
            className="g-team-mode"
            data-mode={m}
            aria-selected={shown === m}
            tabIndex={shown === m ? 0 : -1}
            aria-keyshortcuts={`Meta+Alt+${MODES.indexOf(m) + 1}`}
            onClick={() => setTeamMode(team.key, m)}
            onKeyDown={onTabKey}
          >
            {t(MODE_LABEL[m])}
          </button>
        ))}
      </div>
      <TeamMenu team={team} world={world} />
    </div>
  );
}

/** Facts about how the team is hosted, one line each (ux-ia §3.8, §7.4.7–§7.4.9). */
function TeamLines({ team }: { team: Team }) {
  const { t } = useI18n();
  const lines: MessageKey[] = [];
  if (team.endedAt !== null) lines.push('team.ended');
  if (team.paneHosting === 'inProcessFallback') lines.push('team.inProcess');
  if (team.paneHosting === 'outsideGilde') lines.push('team.outsideGilde');
  if (team.externalHost === 'background') lines.push('team.backgroundedLead');
  if (team.rosterStale) lines.push('team.rosterStale');
  if (lines.length === 0) return null;
  return (
    <div className="g-team-lines">
      {lines.map((key) => (
        <p key={key} className="g-team-line">
          {t(key)}
        </p>
      ))}
    </div>
  );
}

/** The team's `⋯`: team-level via-lead actions and the facts kept out of the header. */
function TeamMenu({ team, world }: { team: Team; world: WorldState }) {
  const { t, td } = useI18n();
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLButtonElement>(null);
  const id = useId();
  const tellAll = canTellAll(world, team);
  const addTask = canAddTask(world, team);
  const run = (fn: () => void) => () => {
    setOpen(false);
    fn();
  };
  return (
    <>
      <button
        ref={anchor}
        type="button"
        className="g-tbtn g-tbtn-icon"
        aria-label={t('team.menu')}
        {...popoverTriggerProps(open, id)}
        onClick={() => setOpen((o) => !o)}
      >
        <MoreIcon size="sm" />
      </button>
      <Popover open={open} anchor={anchor} onClose={() => setOpen(false)} id={id} label={t('team.menu')} className="g-team-menu">
        {(tellAll || addTask) && (
          <div className="g-team-menu-actions">
            {tellAll && (
              <button type="button" className="g-tile-menuitem" onClick={run(() => prefillLead(team.key, `${prefillText('team.prefill.tellAll')} `))}>
                {t('team.action.tellAll')}
              </button>
            )}
            {addTask && (
              <button type="button" className="g-tile-menuitem" onClick={run(() => prefillLead(team.key, `${prefillText('team.prefill.addTask')} `))}>
                {t('team.action.addTask')}
              </button>
            )}
          </div>
        )}
        <p className="g-team-menu-fact">{t('team.rawId', { id: flat(team.rawId, CAP.label) })}</p>
        {team.caps.fidelityNote && <p className="g-team-menu-fact">{td(team.caps.fidelityNote, null)}</p>}
      </Popover>
    </>
  );
}
