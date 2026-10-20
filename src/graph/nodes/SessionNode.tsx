/**
 * Session node — the interactive lane on the canvas (T25, DESIGN §9-6).
 *
 * Body: agent badge, status word, elapsed clock, live RSS (a bar that only goes
 * amber past the configured soft ceiling) and the 60-sample output-rate
 * sparkline. That last row is the "how is it running" the graph exists for.
 *
 * `useNodeMetrics` is a separate subscription from the node itself so the 1 Hz
 * watchdog patch repaints this component and nothing else on the canvas.
 *
 * `OrphanNode` lives here too: an orphan is a session that outlived its owner,
 * and rendering it through the same frame keeps the two visually comparable.
 */

import type { NodeProps } from '@xyflow/react';

import { Bar } from '../../components/Bar';
import { Sparkline } from '../../components/Sparkline';
import { useI18n } from '../../i18n/useI18n';
import { formatBytes, formatElapsedClock } from '../../lib/format';
import { getAccountModel, getSoftRssBytes } from '../useGraphLayout';
import { NodeShell, NodeSub, NodeTitle, StatusLine, useNode, useNodeMetrics } from './NodeShell';

/**
 * The session title without the agent name the badge already carries. Session
 * titles are commonly `"claude · pty-manager"`, and rendering that next to a
 * `claude` badge says the same word twice in a 168px box.
 */
export function sessionLabel(title: string | undefined, vendor: string | undefined): string {
  const trimmed = (title ?? '').trim();
  if (!trimmed) return vendor ?? '';
  if (!vendor) return trimmed;
  if (trimmed === vendor) return vendor;
  const prefix = `${vendor} · `;
  return trimmed.startsWith(prefix) ? trimmed.slice(prefix.length) : trimmed;
}

export function SessionNode({ id }: NodeProps) {
  const { t, locale } = useI18n();
  const node = useNode(id);
  const metrics = useNodeMetrics(id);
  if (!node) return null;

  const soft = getSoftRssBytes();
  // DESIGN §9-6 asks for agent + model on this node. The interactive lane has
  // no model of its own — the CLI is launched with the prompt as argv and no
  // model flag — so the model shown is the one pinned on the session's account
  // slot, and a session on an unpinned slot honestly shows none.
  const model = getAccountModel(node.meta.accountId);
  const rss = metrics?.rssBytes ?? 0;
  const rate = metrics?.outputRate ?? [];
  const elapsed = metrics?.elapsedMs;

  return (
    <NodeShell id={id} kind="session" title={node.meta.cwd}>
      <NodeTitle
        status={node.status}
        confidence={node.confidence}
        badge={node.meta.vendor ?? undefined}
        label={sessionLabel(node.sublabel, node.meta.vendor) || node.label}
      >
        {node.badges.includes('attention') && node.status !== 'waiting' ? (
          <span className="badge" data-tone="warn">
            {t('graph.attention')}
          </span>
        ) : null}
      </NodeTitle>
      <NodeSub>
        <StatusLine status={node.status} />
        {model ? <> · {model}</> : null}
        {elapsed !== undefined ? <> · {formatElapsedClock(elapsed)}</> : null}
        {rss > 0 ? <> · {formatBytes(rss, locale)}</> : null}
      </NodeSub>
      <div className="gilde-node-meters">
        <Bar value={soft > 0 ? rss / soft : 0} warnAt={1} criticalAt={1.5} label={t('graph.rss')} />
        <Sparkline values={rate} width={56} height={12} label={t('graph.outputRate')} />
      </div>
    </NodeShell>
  );
}

/**
 * An `oursOrphan` process: ours by the three identity fences, with nothing left
 * that owns it. The sub line is the masked argv Rust already produced — the
 * graph never re-derives it.
 */
export function OrphanNode({ id }: NodeProps) {
  const { t, locale } = useI18n();
  const node = useNode(id);
  const metrics = useNodeMetrics(id);
  if (!node) return null;

  const rss = metrics?.rssBytes ?? 0;
  return (
    <NodeShell id={id} kind="orphan" title={node.sublabel}>
      <NodeTitle status={node.status} label={node.label}>
        <span className="badge" data-tone="error">
          {t('graph.orphan')}
        </span>
      </NodeTitle>
      <NodeSub tone="error">
        {node.meta.pid !== undefined ? <>pid {node.meta.pid} · </> : null}
        {formatBytes(rss, locale)}
      </NodeSub>
      {metrics?.elapsedMs !== undefined ? (
        <NodeSub>{formatElapsedClock(metrics.elapsedMs)}</NodeSub>
      ) : null}
    </NodeShell>
  );
}
