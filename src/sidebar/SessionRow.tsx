/**
 * Session row — two lines, 42 px (docs/ui-mockups/Main.dc.html, UI-GUIDELINES
 * §2-1b).
 *
 *   line 1  title (the last prompt summary the backend put on the node) · dot
 *   line 2  agent badge · status · elapsed · RSS
 *
 * The dot is the shared `StatusDot`: hollow and dashed below 0.5 confidence,
 * pulsing on `working` and `waiting`. A second, warn-toned dot appears when the
 * session rang the bell — the one signal that means a human is wanted — and it
 * carries a text label, because status is never colour alone (UI-GUIDELINES
 * §2-1a).
 *
 * BACKLOG 8: a session opened without a prompt has no summary, and the node
 * label then falls back to the vendor — so the row read `claude` above a
 * `claude` badge, twice for one fact. `sessionTitle` drops a title that only
 * repeats the badge and names the worktree instead, then "새 세션".
 *
 * Runs (`kind: 'run'`) render through the same row: both lanes hang under the
 * worktree, so the sidebar answers "what is running here" once.
 */

import { Badge } from '../components/Badge';
import { RelativeTime } from '../components/RelativeTime';
import { StatusDot, statusMessageKey, statusTone } from '../components/StatusDot';
import { useI18n } from '../i18n/useI18n';
import { formatBytes } from '../lib/format';
import type { GraphNode } from '../lib/ipc';
import { sessionIdOfNode } from '../store/sessionSlice';
import { useAppStore } from '../store/useAppStore';
import { agentDisplayName } from '../lib/agents';

/**
 * The line-2 status text: the status word, plus the cause when we know one
 * (M1.5 BACKLOG 6).
 *
 * A session whose pty died with a crashed host comes back as `exited`, which on
 * its own reads as "it finished" — the one thing that did not happen. The row
 * says `종료됨 · 호스트 비정상 종료` so the sidebar and the tab badge tell the
 * same story, and it stays one element so the ellipsis rule applies to the
 * whole phrase.
 */
export function sessionStateText(status: string, hostCrash: boolean, crashLabel: string): string {
  return hostCrash ? `${status} · ${crashLabel}` : status;
}

/**
 * The line-1 title: the prompt summary, else the worktree it runs in, else
 * "새 세션". A summary that is only the agent's own name is not a summary —
 * the badge on line 2 already says it.
 */
export function sessionTitle(
  label: string,
  vendor: string,
  worktreeLabel: string | undefined,
  fallback: string,
): string {
  const summary = label.trim();
  if (summary && summary.toLowerCase() !== vendor.trim().toLowerCase()) return summary;
  return worktreeLabel?.trim() || fallback;
}

export interface SessionRowProps {
  node: GraphNode;
  depth: number;
  selected: boolean;
  /** BEL arrived and the tab has not been looked at since (DESIGN §6-4). */
  attention: boolean;
  onSelect: () => void;
  onOpen: () => void;
}

export function SessionRow({ node, depth, selected, attention, onSelect, onOpen }: SessionRowProps) {
  const { t, locale } = useI18n();
  // The worktree label is one lookup in the same projection the row came from,
  // so the fallback title cannot disagree with the row above it.
  const parentId = node.parentId;
  const worktreeLabel = useAppStore((s) =>
    parentId ? s.graph.nodes.get(parentId)?.label : undefined,
  );
  // BACKLOG 6: the cause lives on the terminal lane's view, not on the node —
  // the projection only knows the row exited. Selecting the boolean (not the
  // view) keeps this row out of every status event's re-render.
  const sessionId = sessionIdOfNode(node);
  const hostCrash = useAppStore((s) =>
    sessionId ? s.sessions.get(sessionId)?.hostCrash === true : false,
  );

  const tone = statusTone(node.status);
  const vendor = node.meta.vendor ?? node.sublabel ?? t(`node.${node.kind}`);
  const title = sessionTitle(node.label, vendor, worktreeLabel, t('session.untitled'));
  const rss = node.metrics.rssBytes;
  const since = node.metrics.lastActivityAt ?? node.updatedAt;
  const stateText = sessionStateText(
    t(statusMessageKey(node.status)),
    hostCrash,
    t('session.hostCrash'),
  );

  return (
    <div
      className="sb-session"
      style={{ marginLeft: 8 + depth * 12, marginRight: 8 }}
      role="treeitem"
      aria-selected={selected}
      aria-label={title}
      onClick={onSelect}
      onDoubleClick={onOpen}
    >
      <div className="sb-session-line">
        <span className="sb-session-title" title={node.meta.cwd}>
          {title}
        </span>
        {attention ? (
          <StatusDot status="waiting" title={t('badge.attention')} />
        ) : null}
        <StatusDot status={node.status} confidence={node.confidence} />
      </div>

      <div className="sb-session-meta">
        <Badge>{agentDisplayName(vendor)}</Badge>
        <span
          className="sb-state"
          data-tone={tone === 'neutral' ? undefined : tone}
          title={stateText}
        >
          {stateText}
        </span>
        <RelativeTime at={since} className="ell" />
        {attention ? <span className="sr-only">{t('badge.attention')}</span> : null}
        <div className="grow" />
        {rss ? <span className="mono s11">{formatBytes(rss, locale)}</span> : null}
      </div>
    </div>
  );
}
