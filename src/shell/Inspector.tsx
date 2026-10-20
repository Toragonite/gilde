/**
 * Inspector — the supervision facts for whatever is selected (DESIGN §9-1).
 *
 * It reads the selected node straight out of the graph projection, so what the
 * inspector claims and what the graph draws are the same data. Status is never
 * shown without its evidence: every observation behind the decision is rendered
 * as a sentence in the active locale (DESIGN §6-4, §9-4).
 *
 * Per-kind action rows (Stop, Restart, Reveal worktree, kill group) are
 * registered by the owning task through `registerInspectorActions`.
 */

import type { ComponentType, ReactNode } from 'react';

import { useI18n } from '../i18n/useI18n';
import type { MessageKey, MessageParams } from '../i18n';
import { Badge } from '../components/Badge';
import { EmptyState } from '../components/EmptyState';
import { Mono } from '../components/Mono';
import { Sparkline } from '../components/Sparkline';
import { StatusPill, statusMessageKey } from '../components/StatusDot';
import {
  formatBytes,
  formatCostUsd,
  formatDuration,
  formatElapsedClock,
  formatNumber,
  formatPercent,
  truncatePath,
} from '../lib/format';
import type { GraphNode, Locale, NodeKind, StatusEvidence } from '../lib/ipc';
import { sessionIdOfNode } from '../store/sessionSlice';
import { useAppStore } from '../store/useAppStore';

export interface InspectorActionProps {
  node: GraphNode;
}

const actionPanels = new Map<NodeKind, ComponentType<InspectorActionProps>>();

/** Register the action row for one node kind (T20 sessions, T21 worktrees, …). */
export function registerInspectorActions(
  kind: NodeKind,
  component: ComponentType<InspectorActionProps>,
): () => void {
  actionPanels.set(kind, component);
  return () => {
    if (actionPanels.get(kind) === component) actionPanels.delete(kind);
  };
}

/**
 * One `StatusEvidence` as a human sentence. Shared with T20's evidence popover
 * so the two surfaces cannot word the same fact differently.
 */
export function evidenceSentence(
  evidence: StatusEvidence,
  t: (key: MessageKey, params?: MessageParams) => string,
  locale: Locale,
): string {
  switch (evidence.kind) {
    case 'outputBurst':
      return t('evidence.outputBurst', {
        rate: `${formatBytes(evidence.bytesPerS, locale)}/s`,
      });
    case 'outputQuiet':
      return t('evidence.outputQuiet', { duration: formatDuration(evidence.ms, locale) });
    case 'patternMatch':
      return `${t('evidence.patternMatch', { class: evidence.class })} · ${evidence.line}`;
    case 'foregroundGroup':
      return t('evidence.foregroundGroup', {
        pgid: evidence.pgid,
        cpu: formatPercent(evidence.cpuPct / 100, locale, 1),
      });
    case 'altScreen':
      return evidence.on ? t('evidence.altScreen.on') : t('evidence.altScreen.off');
    case 'screenStable':
      return t('evidence.screenStable', { duration: formatDuration(evidence.ms, locale) });
    case 'bell':
      return t('evidence.bell');
    case 'childExited':
      return t('evidence.childExited', { code: evidence.code ?? '—' });
  }
}

/**
 * One shared empty array: a selector that returns a fresh `[]` on every store
 * read re-renders forever under zustand v5's `Object.is` comparison.
 */
const EMPTY_EVIDENCE: readonly StatusEvidence[] = [];

/**
 * The evidence behind a node's status — **one** source per node (BACKLOG M1.5
 * #4).
 *
 * The inspector said "기록된 근거가 없습니다" while the pill's popover, one panel
 * to the left, listed the output quiet, the CPU and the matched pattern for the
 * very same session. Two readings of one fact is a bug in itself, and this was
 * the plumbing behind it: the popover reads `SessionView.evidence`, which the
 * `gilde://session/status` stream refreshes the moment the engine decides, while
 * the graph node's copy only arrives on the next projection tick and is dropped
 * from a patch whenever it did not change.
 *
 * So a session node reads the session view, which is the live copy, and falls
 * back to the projection's copy when the terminal lane has nothing to say — the
 * session is not in the store at all (a detached or already-closed row that
 * survives in the graph), or it is there but has not had a status event yet, its
 * list of observations still empty. The engine never publishes a status without
 * evidence (`Verdict::ensure_evidence`), so an empty list means "no event yet"
 * and never "we looked and found nothing". Every other kind has no second source
 * and keeps the node's own.
 */
export function statusEvidenceOf(
  node: GraphNode,
  sessions: ReadonlyMap<string, { evidence: StatusEvidence[] }>,
): readonly StatusEvidence[] {
  const sessionId = sessionIdOfNode(node);
  if (sessionId) {
    const view = sessions.get(sessionId);
    if (view && view.evidence.length > 0) return view.evidence;
  }
  return node.evidence ?? EMPTY_EVIDENCE;
}

/**
 * Whether the 상태 근거 section belongs on this node at all (BACKLOG 25).
 *
 * Selecting a worktree used to draw `상태 근거 · 기록된 근거가 없습니다`, which
 * reads as "we looked at this worktree and found no reasons" — but there was
 * never anything to look at: the status engine of DESIGN §6-4 runs per **pty
 * session**, and a worktree's `clean` / `dirty` comes from git, not from three
 * observations about a child process. An empty state is only honest where the
 * thing it describes could have been full.
 *
 * A session therefore always keeps the section, empty or not (that is the
 * BACKLOG M1.5 #4 case: "no status event yet" has to stay visible), and every
 * other kind gets it only when it actually carries evidence — a fact that
 * exists is never hidden.
 */
export function showsEvidenceSection(
  node: GraphNode,
  evidence: readonly StatusEvidence[],
): boolean {
  return node.kind === 'session' || evidence.length > 0;
}

const KIND_LABEL: Record<NodeKind, MessageKey> = {
  project: 'node.project',
  worktree: 'node.worktree',
  session: 'node.session',
  dispatch: 'node.dispatch',
  run: 'node.run',
  account: 'node.account',
  orphan: 'node.orphan',
};

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <span className="k">{label}</span>
      <span className="v">{children}</span>
    </>
  );
}

export interface InspectorProps {
  /** The shell passes its drag handle in, so it lives inside the grid area. */
  children?: ReactNode;
}

export function Inspector({ children }: InspectorProps) {
  const { t, locale } = useI18n();
  const node = useAppStore((s) => (s.ui.selectedNodeId ? s.graph.nodes.get(s.ui.selectedNodeId) : undefined));
  // Same `StatusEvidence[]` the pill's popover renders (BACKLOG M1.5 #4). The
  // selector returns the stored array itself, so this re-renders when the
  // evidence changes and not when anything else in the store does.
  const evidence = useAppStore((s) => {
    const selected = s.ui.selectedNodeId ? s.graph.nodes.get(s.ui.selectedNodeId) : undefined;
    return selected ? statusEvidenceOf(selected, s.sessions) : EMPTY_EVIDENCE;
  });

  if (!node) {
    return (
      <aside className="inspector" aria-label={t('inspector.title')}>
        {children}
        <div className="hdr">
          <span className="strong s12">{t('inspector.title')}</span>
        </div>
        <EmptyState message={t('inspector.empty')} />
      </aside>
    );
  }

  const Actions = actionPanels.get(node.kind);
  const metrics = node.metrics;

  // BACKLOG 13: the branch was printed twice — once under the header (a
  // worktree's `sublabel` *is* its branch) and once again under 감독 사실 from
  // `meta.branch`. It is one fact, so it gets one row: the header row for a
  // worktree, the supervision row for everything else that carries a branch.
  // `sublabel` is not a branch on every kind (a project's is its repo slug, a
  // run's its vendor), so it is only labelled as one where it is one.
  const headerBranch = node.kind === 'worktree' ? (node.sublabel ?? node.meta.branch ?? null) : null;
  const headerRepo = node.kind === 'project' ? (node.sublabel ?? null) : null;
  const factBranch = headerBranch ? null : (node.meta.branch ?? null);

  return (
    <aside className="inspector" aria-label={t('inspector.title')}>
      {children}
      <div className="hdr">
        <span className="strong s12">{t(KIND_LABEL[node.kind])}</span>
        {node.meta.vendor ? <Badge>{node.meta.vendor}</Badge> : null}
        <StatusPill
          status={node.status}
          confidence={node.confidence}
          label={t(statusMessageKey(node.status))}
        />
      </div>

      <div className="scroll-y inspector-body">
        <div className="sec">{node.label}</div>
        {headerBranch || headerRepo ? (
          <div className="kv">
            {headerBranch ? <Row label={t('inspector.branch')}>{headerBranch}</Row> : null}
            {headerRepo ? <Row label={t('inspector.repo')}>{headerRepo}</Row> : null}
          </div>
        ) : null}

        {node.badges.length > 0 ? (
          <div className="row gap-6 inspector-badges">
            {node.badges.map((badge) => (
              <Badge key={badge}>{t(`badge.${badge}` as MessageKey)}</Badge>
            ))}
          </div>
        ) : null}

        <div className="sec">{t('inspector.section.supervision')}</div>
        <div className="kv">
          {node.meta.pid !== undefined ? (
            <Row label={t('inspector.pid')}>{node.meta.pid}</Row>
          ) : null}
          {node.meta.pgid !== undefined ? (
            <Row label={t('inspector.pgid')}>{node.meta.pgid}</Row>
          ) : null}
          {factBranch ? <Row label={t('inspector.branch')}>{factBranch}</Row> : null}
          {node.meta.cwd ? (
            <Row label={t('inspector.cwd')}>
              <span title={node.meta.cwd}>{truncatePath(node.meta.cwd, 3)}</span>
            </Row>
          ) : null}
          {node.meta.accountId ? (
            <Row label={t('inspector.account')}>{node.meta.accountId}</Row>
          ) : null}
          {node.confidence !== undefined ? (
            <Row label={t('inspector.confidence')}>{formatPercent(node.confidence, locale)}</Row>
          ) : null}
        </div>

        {showsEvidenceSection(node, evidence) ? (
          <>
            <div className="sec">{t('inspector.section.evidence')}</div>
            {evidence.length > 0 ? (
              <div className="col gap-4 inspector-evidence">
                {evidence.map((item, i) => (
                  <div className="row gap-8 s12" key={`${item.kind}-${i}`}>
                    <span>{evidenceSentence(item, t, locale)}</span>
                  </div>
                ))}
              </div>
            ) : (
              <div className="inspector-evidence muted s12">{t('inspector.noEvidence')}</div>
            )}
          </>
        ) : null}

        <div className="sec">{t('inspector.section.metrics')}</div>
        <div className="kv">
          {metrics.rssBytes !== undefined ? (
            <Row label={t('inspector.rss')}>{formatBytes(metrics.rssBytes, locale)}</Row>
          ) : null}
          {metrics.cpuPct !== undefined ? (
            <Row label={t('inspector.cpu')}>{formatPercent(metrics.cpuPct / 100, locale, 1)}</Row>
          ) : null}
          {metrics.elapsedMs !== undefined ? (
            <Row label={t('inspector.elapsed')}>{formatElapsedClock(metrics.elapsedMs)}</Row>
          ) : null}
          {metrics.insertions !== undefined || metrics.deletions !== undefined ? (
            <Row label={t('inspector.changes')}>
              {`+${formatNumber(metrics.insertions ?? 0, locale)} −${formatNumber(
                metrics.deletions ?? 0,
                locale,
              )}`}
            </Row>
          ) : null}
          {metrics.ahead !== undefined ? (
            <Row label={t('inspector.ahead')}>{formatNumber(metrics.ahead, locale)}</Row>
          ) : null}
          {metrics.behind !== undefined ? (
            <Row label={t('inspector.behind')}>{formatNumber(metrics.behind, locale)}</Row>
          ) : null}
          {metrics.inputTokens !== undefined || metrics.outputTokens !== undefined ? (
            <Row label={t('inspector.tokens')}>
              {`${formatNumber(metrics.inputTokens ?? 0, locale)} / ${formatNumber(
                metrics.outputTokens ?? 0,
                locale,
              )}`}
            </Row>
          ) : null}
          {metrics.costUsd !== undefined ? (
            <Row label={t('inspector.cost')}>{formatCostUsd(metrics.costUsd, locale)}</Row>
          ) : null}
        </div>

        {metrics.outputRate && metrics.outputRate.length > 0 ? (
          <div className="row gap-8 inspector-spark">
            <Sparkline values={metrics.outputRate} width={200} height={20} />
          </div>
        ) : null}

        <Mono small className="muted inspector-id" ellipsis>
          {node.id}
        </Mono>

        {Actions ? <Actions node={node} /> : null}
      </div>
    </aside>
  );
}
