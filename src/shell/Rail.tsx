/**
 * Rail — 48px, the views plus settings (DESIGN §9-1).
 *
 * Top to bottom: Team, Worker accounts, then — only with the experimental
 * lanes on (`src/features.ts`) — Sessions, Dispatch, Graph and Monitor.
 *
 * With the lanes on, the bottom of the rail is a health strip: one labelled dot
 * per detected agent CLI and one for the sidecar — "can this machine run an
 * agent right now". The preview launches nothing, so it has no such strip; the
 * connection to the orchestrator is the status bar's one indicator.
 */

import type { ComponentType } from 'react';

import { experimentalLanes, railViews } from '../features';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { Tooltip } from '../components/Tooltip';
import { StatusDot } from '../components/StatusDot';
import type { NodeStatus } from '../lib/ipc';
import type { AgentBinary, SidecarLifecycle } from '../lib/ipc';
import { agentDisplayName } from '../lib/agents';
import { useAppStore } from '../store/useAppStore';
import type { ViewKey } from '../store/uiSlice';
import {
  ActivityIcon,
  GraphIcon,
  LayersIcon,
  SettingsIcon,
  GaugeIcon,
  TerminalIcon,
  UsersIcon,
} from '../components/icons';
import type { IconProps } from '../components/icons';

const VIEW_ICON: Record<ViewKey, ComponentType<IconProps>> = {
  team: UsersIcon,
  workers: GaugeIcon,
  sessions: TerminalIcon,
  dispatch: LayersIcon,
  graph: GraphIcon,
  monitor: ActivityIcon,
};

export const VIEW_LABEL: Record<ViewKey, MessageKey> = {
  team: 'view.team',
  workers: 'view.workers',
  sessions: 'view.sessions',
  dispatch: 'view.dispatch',
  graph: 'view.graph',
  monitor: 'view.monitor',
};

/** The rail dot's sentence: which agent, which version, and what the dot means. */
export function agentDotLabel(agent: AgentBinary, t: (key: MessageKey) => string): string {
  const state: MessageKey =
    !agent.ok || !agent.absPath
      ? 'rail.agent.missing'
      : agent.verified
        ? 'rail.agent.ready'
        : 'rail.agent.unverified';
  const version = agent.version ? ` ${agent.version}` : '';
  return `${agentDisplayName(agent.kind)}${version} · ${t(state)}`;
}

/** Detected + verified is green, detected but unverified is amber, absent is neutral. */
export function agentDotStatus(agent: AgentBinary): NodeStatus {
  if (!agent.ok || !agent.absPath) return 'idle';
  return agent.verified ? 'ok' : 'stalled';
}

export function sidecarDotStatus(state: SidecarLifecycle | undefined): NodeStatus {
  switch (state) {
    case 'ready':
      return 'ok';
    case 'starting':
    case 'restarting':
    case 'degraded':
      return 'stalled';
    case 'down':
      return 'error';
    default:
      return 'idle';
  }
}

export function Rail() {
  const { t } = useI18n();
  const activeView = useAppStore((s) => s.ui.activeView);
  const setActiveView = useAppStore((s) => s.setActiveView);
  const setSettingsOpen = useAppStore((s) => s.setSettingsOpen);
  const agents = useAppStore((s) => s.agents);
  const sidecar = useAppStore((s) => s.sidecar);
  const lanes = experimentalLanes();

  return (
    <nav className="rail" aria-label={t('rail.label')}>
      {railViews().map((view) => {
        const Icon = VIEW_ICON[view];
        const label = t(VIEW_LABEL[view]);
        return (
          <Tooltip key={view} label={label}>
            <button
              type="button"
              className="rail-btn"
              aria-current={activeView === view ? 'true' : undefined}
              aria-label={label}
              onClick={() => setActiveView(view)}
            >
              <Icon />
            </button>
          </Tooltip>
        );
      })}

      <div className="grow" />

      {lanes ? (
        <div className="rail-dots">
          {agents.map((agent) => {
            const label = agentDotLabel(agent, t);
            return (
              <Tooltip key={agent.kind} label={label}>
                <span role="img" aria-label={label}>
                  <StatusDot status={agentDotStatus(agent)} pulse={false} />
                </span>
              </Tooltip>
            );
          })}
          <Tooltip
            label={`${t('rail.sidecar')} · ${t(
              (sidecar ? `sidecar.${sidecar.state}` : 'common.unknown') as MessageKey,
            )}`}
          >
            <StatusDot status={sidecarDotStatus(sidecar?.state)} pulse={false} />
          </Tooltip>
        </div>
      ) : null}

      <Tooltip label={t('view.settings')}>
        <button
          type="button"
          className="rail-btn"
          aria-label={t('view.settings')}
          onClick={() => setSettingsOpen(true)}
        >
          <SettingsIcon />
        </button>
      </Tooltip>
    </nav>
  );
}
