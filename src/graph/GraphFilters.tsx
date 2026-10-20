/**
 * Graph filters and search (T25, DESIGN §9-6).
 *
 * Filtering is a pure function of the projection ([`filterGraph`]) so the graph
 * and the tiles view cannot disagree about what is on screen — they call the
 * same function with the same state.
 *
 * Search highlights instead of filtering: a query dims everything it does not
 * match but leaves the shape of the graph intact, because losing the structure
 * is what makes a search result unreadable.
 */

import { useI18n } from '../i18n/useI18n';
import { SearchIcon } from '../components/icons';
import type { GraphNode, NodeStatus } from '../lib/ipc';
import type { GraphView } from '../store/graphSlice';

export interface GraphFilterState {
  /** A `project:…` node id, or `null` for every project. */
  projectId: string | null;
  status: NodeStatus | 'all';
  activeOnly: boolean;
  hideExited: boolean;
  query: string;
}

export const DEFAULT_FILTERS: GraphFilterState = {
  projectId: null,
  status: 'all',
  activeOnly: false,
  hideExited: false,
  query: '',
};

/** Statuses offered in the dropdown — the ones a session can actually be in. */
export const FILTER_STATUSES: readonly NodeStatus[] = [
  'working',
  'waiting',
  'stalled',
  'idle',
  'error',
  'done',
  'exited',
  'orphan',
];

const STRUCTURAL: ReadonlySet<GraphNode['kind']> = new Set(['project', 'worktree']);
const ACTIVE: ReadonlySet<NodeStatus> = new Set(['working', 'waiting', 'stalled']);
const FINISHED: ReadonlySet<NodeStatus> = new Set(['exited', 'done', 'detached']);

function ancestors(graph: GraphView, nodeId: string): string[] {
  const out: string[] = [];
  let cursor = graph.nodes.get(nodeId)?.parentId ?? null;
  for (let hops = 0; cursor && hops < 16; hops++) {
    out.push(cursor);
    cursor = graph.nodes.get(cursor)?.parentId ?? null;
  }
  return out;
}

function inProject(graph: GraphView, node: GraphNode, projectId: string): boolean {
  if (node.id === projectId) return true;
  if (ancestors(graph, node.id).includes(projectId)) return true;
  // An orphan hangs off the root but still names the worktree it was found in.
  const worktreeId = node.meta.worktreeId;
  if (worktreeId) {
    const worktree = graph.nodes.get(`worktree:${worktreeId}`);
    if (worktree && ancestors(graph, worktree.id).includes(projectId)) return true;
  }
  // Accounts are shared across projects; hiding them would be a lie.
  return node.kind === 'account';
}

export interface FilterResult {
  /** Nodes the canvas draws. */
  visible: Set<string>;
  /** Nodes the query matched; empty when there is no query. */
  matched: Set<string>;
}

/** Apply the filter state to a projection. Pure — unit tested directly. */
export function filterGraph(graph: GraphView, filters: GraphFilterState): FilterResult {
  const scope = new Set<string>();
  for (const node of graph.nodes.values()) {
    if (filters.projectId && !inProject(graph, node, filters.projectId)) continue;
    scope.add(node.id);
  }

  const leafFilter = filters.activeOnly || filters.hideExited || filters.status !== 'all';
  let visible: Set<string>;
  if (!leafFilter) {
    visible = scope;
  } else {
    visible = new Set<string>();
    for (const id of scope) {
      const node = graph.nodes.get(id);
      if (!node || STRUCTURAL.has(node.kind)) continue;
      if (filters.activeOnly && !ACTIVE.has(node.status)) continue;
      if (filters.hideExited && FINISHED.has(node.status)) continue;
      if (filters.status !== 'all' && node.status !== filters.status) continue;
      visible.add(id);
      for (const ancestor of ancestors(graph, id)) {
        if (scope.has(ancestor)) visible.add(ancestor);
      }
    }
  }

  const matched = new Set<string>();
  const query = filters.query.trim().toLowerCase();
  if (query) {
    for (const id of visible) {
      const node = graph.nodes.get(id);
      if (!node) continue;
      const haystack = [node.label, node.sublabel ?? '', node.meta.branch ?? '', node.meta.cwd ?? '']
        .join(' ')
        .toLowerCase();
      if (haystack.includes(query)) matched.add(id);
    }
  }
  return { visible, matched };
}

export function projectOptions(graph: GraphView): GraphNode[] {
  const out: GraphNode[] = [];
  for (const node of graph.nodes.values()) if (node.kind === 'project') out.push(node);
  out.sort((a, b) => a.label.localeCompare(b.label));
  return out;
}

// ---------------------------------------------------------------------------
// Control strip
// ---------------------------------------------------------------------------

export interface GraphFiltersProps {
  filters: GraphFilterState;
  projects: readonly GraphNode[];
  matches: number;
  onChange: (next: GraphFilterState) => void;
}

export function GraphFilters({ filters, projects, matches, onChange }: GraphFiltersProps) {
  const { t } = useI18n();

  return (
    <>
      <select
        className="input"
        aria-label={t('graph.filter.project')}
        value={filters.projectId ?? ''}
        onChange={(e) => onChange({ ...filters, projectId: e.target.value || null })}
      >
        <option value="">{t('graph.filter.allProjects')}</option>
        {projects.map((project) => (
          <option key={project.id} value={project.id}>
            {project.label}
          </option>
        ))}
      </select>

      <select
        className="input"
        aria-label={t('graph.filter.status')}
        value={filters.status}
        onChange={(e) => onChange({ ...filters, status: e.target.value as GraphFilterState['status'] })}
      >
        <option value="all">{t('graph.filter.allStatuses')}</option>
        {FILTER_STATUSES.map((status) => (
          <option key={status} value={status}>
            {t(`status.${status}` as 'status.idle')}
          </option>
        ))}
      </select>

      <button
        type="button"
        className="chip"
        aria-pressed={filters.activeOnly}
        data-selected={filters.activeOnly ? 'true' : undefined}
        onClick={() => onChange({ ...filters, activeOnly: !filters.activeOnly })}
      >
        {t('graph.filter.activeOnly')}
      </button>
      <button
        type="button"
        className="chip"
        aria-pressed={filters.hideExited}
        data-selected={filters.hideExited ? 'true' : undefined}
        onClick={() => onChange({ ...filters, hideExited: !filters.hideExited })}
      >
        {t('graph.filter.hideExited')}
      </button>

      <label className="input gilde-search">
        <SearchIcon size="sm" />
        <input
          type="search"
          value={filters.query}
          placeholder={t('graph.search')}
          aria-label={t('graph.search')}
          onChange={(e) => onChange({ ...filters, query: e.target.value })}
        />
      </label>
      {filters.query.trim() ? (
        <span className="s11 muted">{t('graph.matches', { n: matches })}</span>
      ) : null}
    </>
  );
}
