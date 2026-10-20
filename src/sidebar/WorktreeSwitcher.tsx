/**
 * ⌘P — the worktree / session switcher (T27, DESIGN §7-4, §9-8).
 *
 * The rule this screen exists to keep: **switching worktrees never runs `git
 * checkout`.** A worktree is a directory that already exists, so "switching" is
 * focusing a session whose cwd is that directory, or opening one there. The only
 * `git checkout` in this codebase is the one `worktree/mod.rs` runs while
 * *creating* a worktree, and `palette.test.tsx` greps the Rust tree to keep it
 * that way.
 *
 * Rows are built from the graph projection and the session map — the same two
 * sources the sidebar and the graph dashboard read, so the three cannot
 * disagree about what exists or how dirty it is.
 *
 * Enter, in order (M1-PLAN T27 acceptance 3):
 *   live session here → focus its tab (the most recently active one)
 *   no session       → the inline "new session" row expands; Enter runs it
 *   no match at all  → "worktree <slug> 생성…"
 */

import { useEffect, useMemo, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';

import { Badge } from '../components/Badge';
import { EmptyState } from '../components/EmptyState';
import { StatusDot } from '../components/StatusDot';
import { useI18n } from '../i18n/useI18n';
import { formatBytes } from '../lib/format';
import type {
  AgentBinary,
  AgentKindKey,
  GraphNode,
  NodeStatus,
  ProjectId,
  SessionId,
  WorktreeId,
} from '../lib/ipc';
import { PaletteOverlay, PaletteRow, stepSelection } from '../palette/CommandPalette';
import { agentLabel, fuzzyBest, openSessionWithAgent } from '../palette/commands';
import type { GraphView } from '../store/graphSlice';
import { lastAgentFor } from '../store/sessionSlice';
import type { SessionView } from '../store/sessionSlice';
import { ALIAS_PROVIDER_OPTIONS, previewSlug } from '../store/worktreeSlice';
import { useAppStore } from '../store/useAppStore';
import '../palette/palette.css';

// ---------------------------------------------------------------------------
// Row model (pure — exercised by palette.test.tsx)
// ---------------------------------------------------------------------------

/** A session as the switcher needs it: is it alive, and when did it last move. */
export interface SwitcherSession {
  id: SessionId;
  nodeId: string;
  label: string;
  status: NodeStatus;
  live: boolean;
  lastActivityAt: number;
  rssBytes: number;
}

export interface SwitcherRow {
  worktreeId: WorktreeId;
  nodeId: string;
  projectId: ProjectId | null;
  projectLabel: string;
  label: string;
  branch: string;
  status: NodeStatus;
  dirtyFiles: number;
  insertions: number;
  deletions: number;
  ahead: number;
  /** Summed over the sessions living in this worktree. */
  rssBytes: number;
  sessions: SwitcherSession[];
  liveSessions: number;
  /** Newest activity anywhere in this worktree, in epoch ms. */
  recency: number;
  /** What the fuzzy filter matches: project, worktree, branch, session names. */
  haystacks: string[];
}

/** A session node in one of these states has no tab worth focusing. */
const DEAD_STATUS: readonly NodeStatus[] = ['exited', 'error', 'detached', 'done'];

function entityId(nodeId: string): string {
  const at = nodeId.indexOf(':');
  return at < 0 ? nodeId : nodeId.slice(at + 1);
}

/**
 * One pass over the projection. Sessions are read from the graph (every surface
 * shows the same numbers) and their liveness is confirmed against the session
 * map when the pty lane knows about them.
 */
export function buildSwitcherRows(
  graph: GraphView,
  sessions: ReadonlyMap<SessionId, SessionView>,
): SwitcherRow[] {
  const projects = new Map<string, GraphNode>();
  const worktrees: GraphNode[] = [];
  const children = new Map<string, GraphNode[]>();

  for (const node of graph.nodes.values()) {
    if (node.kind === 'project') projects.set(node.id, node);
    else if (node.kind === 'worktree') worktrees.push(node);
    else if (node.kind === 'session' || node.kind === 'run') {
      if (!node.parentId) continue;
      const list = children.get(node.parentId);
      if (list) list.push(node);
      else children.set(node.parentId, [node]);
    }
  }

  const rows: SwitcherRow[] = [];
  for (const node of worktrees) {
    const project = node.parentId ? projects.get(node.parentId) : undefined;
    const branch = node.meta.branch ?? node.sublabel ?? '';
    const kids = children.get(node.id) ?? [];

    const rowSessions: SwitcherSession[] = [];
    let rss = 0;
    let recency = node.metrics.lastActivityAt ?? node.updatedAt;

    for (const kid of kids) {
      const id = entityId(kid.id);
      const view = kid.kind === 'session' ? sessions.get(id) : undefined;
      const lastActivityAt = kid.metrics.lastActivityAt ?? view?.updatedAt ?? kid.updatedAt;
      const bytes = kid.metrics.rssBytes ?? 0;
      rss += bytes;
      if (lastActivityAt > recency) recency = lastActivityAt;
      rowSessions.push({
        id,
        nodeId: kid.id,
        label: kid.label,
        status: kid.status,
        live: !DEAD_STATUS.includes(kid.status),
        lastActivityAt,
        rssBytes: bytes,
      });
    }

    rowSessions.sort((a, b) => b.lastActivityAt - a.lastActivityAt);

    rows.push({
      worktreeId: entityId(node.id),
      nodeId: node.id,
      projectId: project ? entityId(project.id) : null,
      projectLabel: project?.label ?? '',
      label: node.label,
      branch,
      status: node.status,
      dirtyFiles: node.metrics.dirtyFiles ?? 0,
      insertions: node.metrics.insertions ?? 0,
      deletions: node.metrics.deletions ?? 0,
      ahead: node.metrics.ahead ?? 0,
      rssBytes: rss,
      sessions: rowSessions,
      liveSessions: rowSessions.filter((session) => session.live).length,
      recency,
      haystacks: [
        node.label,
        branch,
        project?.label ?? '',
        ...rowSessions.map((session) => session.label),
      ].filter((text) => text.length > 0),
    });
  }

  return rows;
}

/** Recency is bucketed to the minute so that "recency → dirty" has real effect. */
const RECENCY_BUCKET_MS = 60_000;

/**
 * Filter and order: match quality first when there is a query, then recency,
 * then dirty-first (M1-PLAN T27 acceptance 2).
 *
 * Bucketing recency matters at boot: every worktree node is projected in the
 * same instant, so without it "dirty first" would never be reached.
 */
export function rankSwitcherRows(query: string, rows: readonly SwitcherRow[]): SwitcherRow[] {
  const trimmed = query.trim();
  const scored: { row: SwitcherRow; score: number }[] = [];
  for (const row of rows) {
    const match = fuzzyBest(trimmed, row.haystacks);
    if (!match) continue;
    scored.push({ row, score: match.score });
  }
  scored.sort((a, b) => {
    if (trimmed.length > 0 && b.score !== a.score) return b.score - a.score;
    const bucketA = Math.floor(a.row.recency / RECENCY_BUCKET_MS);
    const bucketB = Math.floor(b.row.recency / RECENCY_BUCKET_MS);
    if (bucketB !== bucketA) return bucketB - bucketA;
    const dirtyA = a.row.dirtyFiles > 0 ? 1 : 0;
    const dirtyB = b.row.dirtyFiles > 0 ? 1 : 0;
    if (dirtyB !== dirtyA) return dirtyB - dirtyA;
    return a.row.label.localeCompare(b.row.label);
  });
  return scored.map((entry) => entry.row);
}

/** The session a ⌘P Enter should reveal: the most recently active live one. */
export function newestLiveSession(row: SwitcherRow): SwitcherSession | null {
  for (const session of row.sessions) if (session.live) return session;
  return null;
}

// ---------------------------------------------------------------------------
// Flattened keyboard model
// ---------------------------------------------------------------------------

type SwitcherItem =
  | { kind: 'worktree'; row: SwitcherRow }
  | { kind: 'agent'; row: SwitcherRow; agent: AgentKindKey }
  | { kind: 'create'; name: string; projectId: ProjectId | null };

export function agentOrder(
  agents: readonly AgentBinary[],
  suggested: AgentKindKey | null,
): AgentBinary[] {
  const installed = agents.filter((binary) => binary.installed);
  const first = installed.filter((binary) => binary.kind === suggested);
  const rest = installed.filter((binary) => binary.kind !== suggested);
  return [...first, ...rest];
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/** Stable empty list, so a closed switcher memoizes to the same reference. */
const NO_ROWS: SwitcherRow[] = [];

export function WorktreeSwitcher() {
  const { t, td, locale } = useI18n();

  const open = useAppStore((s) => s.ui.switcherOpen);
  const setSwitcherOpen = useAppStore((s) => s.setSwitcherOpen);
  const graph = useAppStore((s) => s.graph);
  const sessions = useAppStore((s) => s.sessions);
  const agents = useAppStore((s) => s.agents);
  const tabs = useAppStore((s) => s.ui.tabs);
  const lastAgentByWorktree = useAppStore((s) => s.lastAgentByWorktree);
  const worktreeError = useAppStore((s) => s.worktree.error);
  const postCreateCmd = useAppStore((s) => s.worktree.postCreateCmd);
  const focusTab = useAppStore((s) => s.focusTab);
  const openTab = useAppStore((s) => s.openTab);
  const selectNode = useAppStore((s) => s.selectNode);
  const createWorktree = useAppStore((s) => s.createWorktree);
  const setNewSessionOpen = useAppStore((s) => s.setNewSessionOpen);

  const [query, setQuery] = useState('');
  const [index, setIndex] = useState(0);
  const [expanded, setExpanded] = useState<WorktreeId | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setQuery('');
    setIndex(0);
    setExpanded(null);
    setBusy(false);
  }, [open]);

  // Closed, nothing is rebuilt: this component stays mounted while the graph
  // patches at 10 Hz (DESIGN §9-3).
  const rows = useMemo(
    () => (open ? buildSwitcherRows(graph, sessions) : NO_ROWS),
    [open, graph, sessions],
  );
  const ranked = useMemo(() => rankSwitcherRows(query, rows), [query, rows]);

  const projectForCreate = useMemo<ProjectId | null>(() => {
    if (rows.length > 0) {
      const first = ranked[0] ?? rows[0];
      if (first.projectId) return first.projectId;
    }
    for (const node of graph.nodes.values()) {
      if (node.kind === 'project') return entityId(node.id);
    }
    return null;
  }, [graph, ranked, rows]);

  const items = useMemo<SwitcherItem[]>(() => {
    const out: SwitcherItem[] = [];
    for (const row of ranked) {
      out.push({ kind: 'worktree', row });
      if (expanded === row.worktreeId) {
        for (const binary of agentOrder(agents, lastAgentFor(lastAgentByWorktree, sessions, row.worktreeId))) {
          out.push({ kind: 'agent', row, agent: binary.kind });
        }
      }
    }
    if (ranked.length === 0 && query.trim().length > 0) {
      out.push({ kind: 'create', name: query.trim(), projectId: projectForCreate });
    }
    return out;
  }, [ranked, expanded, agents, lastAgentByWorktree, sessions, query, projectForCreate]);

  useEffect(() => {
    if (index < items.length) return;
    setIndex(items.length === 0 ? 0 : items.length - 1);
  }, [items, index]);

  if (!open) return null;

  const close = () => setSwitcherOpen(false);

  /** DESIGN §7-4: reveal the session, never touch the user's own checkout. */
  const revealSession = (row: SwitcherRow, session: SwitcherSession) => {
    const existing = tabs.find(
      (tab) => tab.descriptor.view === 'terminal' && tab.descriptor.sessionId === session.id,
    );
    if (existing) focusTab(existing.id);
    else {
      openTab({
        title: session.label,
        sublabel: row.label,
        descriptor: { view: 'terminal', sessionId: session.id },
        status: session.status,
        closable: true,
      });
    }
    selectNode(session.nodeId);
  };

  const activate = (at: number) => {
    const item = items[at];
    if (!item || busy) return;

    if (item.kind === 'worktree') {
      const session = newestLiveSession(item.row);
      if (session) {
        revealSession(item.row, session);
        close();
        return;
      }
      // No live session: the inline row expands and the next Enter runs it.
      selectNode(item.row.nodeId);
      if (agentOrder(agents, null).length === 0) {
        setNewSessionOpen(true, item.row.worktreeId);
        close();
        return;
      }
      setExpanded(item.row.worktreeId);
      setIndex(at + 1);
      return;
    }

    if (item.kind === 'agent') {
      close();
      void openSessionWithAgent(item.agent, item.row.worktreeId);
      return;
    }

    if (!item.projectId) return;
    setBusy(true);
    void createWorktree({
      projectId: item.projectId,
      taskName: item.name,
      aliasProviders: ALIAS_PROVIDER_OPTIONS.filter((option) => option.defaultOn).map(
        (option) => option.key,
      ),
      ...(postCreateCmd[item.projectId] ? { postCreateCmd: postCreateCmd[item.projectId] } : {}),
    })
      .then((worktree) => {
        if (!worktree) return;
        close();
        // Straight into the flow the name implied: a worktree, then a session.
        setNewSessionOpen(true, worktree.id);
      })
      .finally(() => setBusy(false));
  };

  const onKeyDown = (event: ReactKeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      setIndex((current) => stepSelection(current, 1, items.length, () => true));
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      setIndex((current) => stepSelection(current, -1, items.length, () => true));
    } else if (event.key === 'Enter') {
      event.preventDefault();
      activate(index);
    }
  };

  const activeItem = items[index];
  const activeId = activeItem
    ? activeItem.kind === 'create'
      ? 'gilde-sw-create'
      : activeItem.kind === 'agent'
        ? `gilde-sw-${activeItem.row.worktreeId}-${activeItem.agent}`
        : `gilde-sw-${activeItem.row.worktreeId}`
    : undefined;

  return (
    <PaletteOverlay
      open={open}
      label={t('switcher.title')}
      query={query}
      placeholder={t('switcher.placeholder')}
      hint={busy ? t('worktree.creating') : t('switcher.hint')}
      onQuery={(value) => {
        setQuery(value);
        setIndex(0);
        setExpanded(null);
      }}
      onClose={close}
      onKeyDown={onKeyDown}
      {...(activeId ? { activeId } : {})}
    >
      {worktreeError ? (
        <div className="palette-row" role="alert">
          <span className="ell grow s11" data-tone="error">
            {td(worktreeError.key, worktreeError.params)}
          </span>
        </div>
      ) : null}

      {items.length === 0 ? (
        <EmptyState message={rows.length === 0 ? t('sidebar.projects.empty') : t('switcher.empty')} />
      ) : (
        items.map((item, at) => {
          if (item.kind === 'create') {
            return (
              <PaletteRow
                key="create"
                id="gilde-sw-create"
                active={at === index}
                disabled={item.projectId === null}
                {...(item.projectId === null ? { title: t('sidebar.projects.empty') } : {})}
                onPick={() => activate(at)}
                onHover={() => setIndex(at)}
              >
                <span className="ell grow">
                  {t('switcher.create', { slug: previewSlug(item.name) })}
                </span>
              </PaletteRow>
            );
          }

          if (item.kind === 'agent') {
            return (
              <div
                key={`${item.row.worktreeId}-${item.agent}`}
                id={`gilde-sw-${item.row.worktreeId}-${item.agent}`}
                role="option"
                aria-selected={at === index}
                className="sw-sub"
                data-active={at === index ? 'true' : undefined}
                onMouseMove={() => setIndex(at)}
                onMouseDown={(event) => {
                  event.preventDefault();
                  activate(at);
                }}
              >
                <span className="grow ell">
                  {t('switcher.newSession', { agent: agentLabel(item.agent) })}
                </span>
                <Badge>{agentLabel(item.agent)}</Badge>
              </div>
            );
          }

          const row = item.row;
          return (
            <div
              key={row.worktreeId}
              id={`gilde-sw-${row.worktreeId}`}
              role="option"
              aria-selected={at === index}
              aria-label={`${row.projectLabel} ${row.label}`}
              className="sw-row"
              data-active={at === index ? 'true' : undefined}
              onMouseMove={() => setIndex(at)}
              onMouseDown={(event) => {
                event.preventDefault();
                activate(at);
              }}
            >
              <StatusDot status={row.status} />
              <span className="sw-main">
                {row.projectLabel ? <span className="sw-project s11">{row.projectLabel}</span> : null}
                <span className="sw-name ell">{row.label}</span>
                <span className="sw-branch ell">{row.branch}</span>
              </span>
              <span className="sw-metrics">
                {row.insertions > 0 || row.deletions > 0 ? (
                  <span>{`+${row.insertions} −${row.deletions}`}</span>
                ) : null}
                {row.dirtyFiles > 0 ? (
                  <span data-tone="warn">{t('worktree.dirtyShort', { n: row.dirtyFiles })}</span>
                ) : null}
                {row.ahead > 0 ? <span>{`↑${row.ahead}`}</span> : null}
                {row.liveSessions > 0 ? (
                  <span>{t('worktree.sessionShort', { n: row.liveSessions })}</span>
                ) : null}
                {row.rssBytes > 0 ? <span>{formatBytes(row.rssBytes, locale)}</span> : null}
              </span>
            </div>
          );
        })
      )}
    </PaletteOverlay>
  );
}
