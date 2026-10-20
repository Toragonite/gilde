/**
 * The tree: project → worktree → session|run, flattened and windowed.
 *
 * `buildRows` is the whole data path and it takes **one input, the graph
 * projection** (M1-PLAN T21 acceptance 1). No command runs here, no list is
 * read from another slice, so the sidebar and the graph dashboard render the
 * same `Map<string, GraphNode>` and cannot disagree about what exists.
 *
 * Windowing: rows have two fixed heights (26 px for a project, worktree or
 * orphan; 42 px for a two-line session), so the offset of every row is a prefix
 * sum computed once per row list. Past `VIRTUALIZE_ABOVE` rows only the visible
 * slice is mounted — a machine with a dozen worktrees and four agents each is
 * well past the point where mounting the whole tree costs a frame.
 */

import { useMemo, useRef, useState } from 'react';

import { useElementSize } from '../hooks/useResizeObserver';
import type { GraphNode } from '../lib/ipc';
import type { GraphView } from '../store/graphSlice';
import { ProjectRow } from './ProjectRow';
import { SessionRow } from './SessionRow';
import { WorktreeRow } from './WorktreeRow';

// ---------------------------------------------------------------------------
// Row model
// ---------------------------------------------------------------------------

export const ROW_H = 26;
export const SESSION_ROW_H = 42;
/** Above this many rows the list is windowed (M1-PLAN T21 acceptance 8). */
export const VIRTUALIZE_ABOVE = 200;

export type SidebarRow =
  | { kind: 'project'; node: GraphNode; depth: number; children: number }
  | { kind: 'worktree'; node: GraphNode; depth: number; children: number }
  | { kind: 'session'; node: GraphNode; depth: number };

export function rowHeight(row: SidebarRow): number {
  return row.kind === 'session' ? SESSION_ROW_H : ROW_H;
}

function byLabel(a: GraphNode, b: GraphNode): number {
  return a.label.localeCompare(b.label);
}

function childrenOfKind(graph: GraphView, parentId: string, kinds: GraphNode['kind'][]): GraphNode[] {
  const out: GraphNode[] = [];
  for (const node of graph.nodes.values()) {
    if (node.parentId === parentId && kinds.includes(node.kind)) out.push(node);
  }
  out.sort(byLabel);
  return out;
}

function matches(node: GraphNode, needle: string): boolean {
  if (!needle) return true;
  const hay = `${node.label} ${node.sublabel ?? ''} ${node.meta.branch ?? ''} ${node.meta.vendor ?? ''}`;
  return hay.toLowerCase().includes(needle);
}

/**
 * Flatten the projection into rows.
 *
 * A filter keeps a node when it matches or when one of its descendants does, so
 * typing a session name still shows the worktree it lives in. Collapsing hides
 * children but never changes the counts printed on the parent — a collapsed
 * worktree still says how many sessions are inside it.
 */
export function buildRows(
  graph: GraphView,
  collapsed: Readonly<Record<string, true>>,
  filter = '',
): SidebarRow[] {
  const needle = filter.trim().toLowerCase();
  const rows: SidebarRow[] = [];

  const projects: GraphNode[] = [];
  for (const node of graph.nodes.values()) if (node.kind === 'project') projects.push(node);
  projects.sort(byLabel);

  for (const project of projects) {
    const worktrees = childrenOfKind(graph, project.id, ['worktree']);

    const visibleWorktrees: { node: GraphNode; sessions: GraphNode[] }[] = [];
    for (const worktree of worktrees) {
      const sessions = childrenOfKind(graph, worktree.id, ['session', 'run']);
      const hits = sessions.filter((node) => matches(node, needle));
      if (needle && !matches(worktree, needle) && hits.length === 0 && !matches(project, needle)) {
        continue;
      }
      // A worktree matched by name shows all of its sessions; a worktree kept
      // only because a session matched shows just that session.
      const shown = needle && !matches(worktree, needle) && !matches(project, needle) ? hits : sessions;
      visibleWorktrees.push({ node: worktree, sessions: shown });
    }

    if (needle && visibleWorktrees.length === 0 && !matches(project, needle)) continue;

    rows.push({
      kind: 'project',
      node: project,
      depth: 0,
      children: worktrees.length,
    });
    if (collapsed[project.id]) continue;

    for (const { node, sessions } of visibleWorktrees) {
      const all = childrenOfKind(graph, node.id, ['session', 'run']);
      rows.push({ kind: 'worktree', node, depth: 1, children: all.length });
      if (collapsed[node.id]) continue;
      for (const session of sessions) {
        rows.push({ kind: 'session', node: session, depth: 2 });
      }
    }
  }

  return rows;
}

/** Prefix sums, so a windowed list can place a row without measuring the DOM. */
export function rowOffsets(rows: readonly SidebarRow[]): { offsets: number[]; total: number } {
  const offsets: number[] = new Array(rows.length);
  let total = 0;
  for (let i = 0; i < rows.length; i++) {
    offsets[i] = total;
    total += rowHeight(rows[i]);
  }
  return { offsets, total };
}

/** First and last (exclusive) row that intersects the viewport, plus overscan. */
export function visibleRange(
  offsets: readonly number[],
  total: number,
  scrollTop: number,
  viewport: number,
  overscan = 6,
): [number, number] {
  const count = offsets.length;
  if (count === 0) return [0, 0];
  const height = viewport > 0 ? viewport : total;

  let first = 0;
  let low = 0;
  let high = count - 1;
  while (low <= high) {
    const mid = (low + high) >> 1;
    if (offsets[mid] <= scrollTop) {
      first = mid;
      low = mid + 1;
    } else {
      high = mid - 1;
    }
  }

  let last = first;
  const bottom = scrollTop + height;
  while (last < count && offsets[last] < bottom) last++;

  return [Math.max(0, first - overscan), Math.min(count, last + overscan)];
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export interface TreeHandlers {
  onSelect: (nodeId: string) => void;
  onToggle: (nodeId: string) => void;
  onOpenWorktree: (node: GraphNode) => void;
  onNewWorktree: (projectNodeId: string) => void;
  onNewSession: (worktreeNodeId: string) => void;
  onRemoveWorktree: (node: GraphNode) => void;
  onOpenSession: (node: GraphNode) => void;
  /** BEL is a session-lane fact; it does not live on the graph node. */
  attention: (node: GraphNode) => boolean;
}

export interface ProjectTreeProps {
  rows: readonly SidebarRow[];
  selectedNodeId: string | null;
  collapsed: Readonly<Record<string, true>>;
  /**
   * The sidebar is too narrow to print a worktree's status word next to its
   * name (BACKLOG 15d). It is measured once, in `Sidebar`, off the one width
   * the shell lays the column out with — a row cannot measure itself without a
   * `ResizeObserver` per row, and every row is the same width anyway.
   */
  narrow: boolean;
  handlers: TreeHandlers;
}

export function ProjectTree({
  rows,
  selectedNodeId,
  collapsed,
  narrow,
  handlers,
}: ProjectTreeProps) {
  const scroller = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const { height } = useElementSize(scroller);

  const { offsets, total } = useMemo(() => rowOffsets(rows), [rows]);
  const windowed = rows.length > VIRTUALIZE_ABOVE;
  const [first, last] = windowed
    ? visibleRange(offsets, total, scrollTop, height)
    : [0, rows.length];

  const render = (row: SidebarRow) => {
    const selected = row.node.id === selectedNodeId;
    switch (row.kind) {
      case 'project':
        return (
          <ProjectRow
            node={row.node}
            depth={row.depth}
            collapsed={Boolean(collapsed[row.node.id])}
            worktreeCount={row.children}
            selected={selected}
            onToggle={() => handlers.onToggle(row.node.id)}
            onSelect={() => handlers.onSelect(row.node.id)}
            onNewWorktree={() => handlers.onNewWorktree(row.node.id)}
          />
        );
      case 'worktree':
        return (
          <WorktreeRow
            node={row.node}
            depth={row.depth}
            collapsed={Boolean(collapsed[row.node.id])}
            sessionCount={row.children}
            narrow={narrow}
            selected={selected}
            onToggle={() => handlers.onToggle(row.node.id)}
            onSelect={() => handlers.onSelect(row.node.id)}
            onOpen={() => handlers.onOpenWorktree(row.node)}
            onNewSession={() => handlers.onNewSession(row.node.id)}
            onRemove={() => handlers.onRemoveWorktree(row.node)}
          />
        );
      case 'session':
        return (
          <SessionRow
            node={row.node}
            depth={row.depth}
            selected={selected}
            attention={handlers.attention(row.node)}
            onSelect={() => handlers.onSelect(row.node.id)}
            onOpen={() => handlers.onOpenSession(row.node)}
          />
        );
    }
  };

  return (
    <div
      ref={scroller}
      className="sb-tree"
      role="tree"
      onScroll={(event) => {
        if (windowed) setScrollTop(event.currentTarget.scrollTop);
      }}
    >
      {windowed ? (
        <div className="sb-viewport" style={{ height: total }}>
          {rows.slice(first, last).map((row, i) => (
            <div key={row.node.id} className="sb-slot" style={{ top: offsets[first + i] }}>
              {render(row)}
            </div>
          ))}
        </div>
      ) : (
        rows.map((row) => <div key={row.node.id}>{render(row)}</div>)
      )}
    </div>
  );
}
