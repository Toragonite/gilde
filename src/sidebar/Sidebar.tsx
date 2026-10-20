/**
 * The sidebar (M1-PLAN T21, DESIGN §9-1, docs/ui-mockups/Main.dc.html).
 *
 *   32 px header  세션 · count · [검색] [프로젝트 추가]
 *   filter box    (toggled by the header's search button)
 *   refusal line  one localized sentence, cause + next action
 *   tree          project → worktree → session|run
 *   orphans       count badge, opens the Monitor
 *
 * It renders the graph projection and nothing else. The only backend call it
 * makes is `loadRoster()` on mount, and that call does not feed the tree — it
 * folds the durable rows back into the same projection (see `worktreeSlice`),
 * which is what keeps this panel and the graph dashboard from disagreeing.
 *
 * "Switching to a worktree" never runs `git checkout` (DESIGN §7-4): it focuses
 * a session whose cwd is that worktree, or offers to open one.
 *
 * ## The view switch (M1.5 BACKLOG 3)
 *
 * The rail is not a highlight: `ui.activeView` is one store field, and this is
 * the half of it that owns the left column. `Sidebar` is a switch and nothing
 * else — each view's panel is a component below it — so the shell keeps its one
 * sidebar mount point and the rail keeps its one source of truth.
 *
 *   team                         teams with their members, and the watched
 *                                project folders (team/TeamSidebar.tsx, F4)
 *   workers                      none — the shell hides the column there
 *   sessions | monitor | graph   the project → worktree → session tree
 *   dispatch                     the task list and the way into the composer
 *
 * The dispatch panel renders task rows out of `dispatchSlice` with the same
 * exported helpers the board uses (`taskNodeStatus`, `taskStatusKey`,
 * `shortTaskId`, `taskProgress`), so the two never disagree about what a task
 * is called or how far along it is. The composer itself is not mounted here: it
 * is a sheet whose data loading is gated on `composer.open`, so mounting a
 * second copy inline would render a form that never loads its roster and would
 * duplicate every one of its accessible names. The footer button runs the same
 * `newDispatch` intent ⌘⇧D does.
 */

import { useEffect, useMemo, useState } from 'react';

import { Badge } from '../components/Badge';
import { EmptyState } from '../components/EmptyState';
import { Mono } from '../components/Mono';
import { StatusDot } from '../components/StatusDot';
import { CloseIcon, FolderPlusIcon, PlusIcon, SearchIcon } from '../components/icons';
import { shortTaskId, taskNodeStatus, taskStatusKey } from '../dispatch/ResultCardGrid';
import { runIntent } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import type { GraphNode, TaskId } from '../lib/ipc';
import { runsOfTask, taskProgress } from '../store/dispatchSlice';
import { TeamSidebar } from '../team/TeamSidebar';
import { useAppStore } from '../store/useAppStore';
import { OrphanGroup } from './OrphanGroup';
import { ProjectTree, buildRows } from './ProjectTree';
import type { TreeHandlers } from './ProjectTree';
import { isNarrowRow } from './WorktreeRow';
import './sidebar.css';
import { agentDisplayName } from '../lib/agents';

function entityId(nodeId: string): string {
  const at = nodeId.indexOf(':');
  return at < 0 ? nodeId : nodeId.slice(at + 1);
}

export function SessionsSidebar() {
  const { t, td } = useI18n();

  const graph = useAppStore((s) => s.graph);
  const sessions = useAppStore((s) => s.sessions);
  const collapsed = useAppStore((s) => s.worktree.collapsed);
  const filter = useAppStore((s) => s.worktree.filter);
  const error = useAppStore((s) => s.worktree.error);
  const loaded = useAppStore((s) => s.worktree.loaded);
  const selectedNodeId = useAppStore((s) => s.ui.selectedNodeId);
  // BACKLOG 15d: the one width the shell lays the sidebar column out with.
  const sidebarWidth = useAppStore((s) => s.ui.sidebarWidth);
  const tabs = useAppStore((s) => s.ui.tabs);

  const selectNode = useAppStore((s) => s.selectNode);
  const toggleCollapsed = useAppStore((s) => s.toggleCollapsed);
  const setSidebarFilter = useAppStore((s) => s.setSidebarFilter);
  const clearWorktreeError = useAppStore((s) => s.clearWorktreeError);
  const loadRoster = useAppStore((s) => s.loadRoster);
  const addProject = useAppStore((s) => s.addProject);
  const setNewWorktreeOpen = useAppStore((s) => s.setNewWorktreeOpen);
  const openRemoveDialog = useAppStore((s) => s.openRemoveDialog);
  const requestOrphanFilter = useAppStore((s) => s.requestOrphanFilter);
  const setNewSessionOpen = useAppStore((s) => s.setNewSessionOpen);
  const openTab = useAppStore((s) => s.openTab);
  const focusTab = useAppStore((s) => s.focusTab);
  const setActiveView = useAppStore((s) => s.setActiveView);
  const setDrawerTab = useAppStore((s) => s.setDrawerTab);

  const [searchOpen, setSearchOpen] = useState(false);

  useEffect(() => {
    void loadRoster();
  }, [loadRoster]);

  const rows = useMemo(() => buildRows(graph, collapsed, filter), [graph, collapsed, filter]);

  const orphans = useMemo(() => {
    const out: GraphNode[] = [];
    for (const node of graph.nodes.values()) if (node.kind === 'orphan') out.push(node);
    out.sort((a, b) => (b.metrics.rssBytes ?? 0) - (a.metrics.rssBytes ?? 0));
    return out;
  }, [graph]);

  const sessionCount = useMemo(() => {
    let n = 0;
    for (const node of graph.nodes.values()) if (node.kind === 'session' || node.kind === 'run') n++;
    return n;
  }, [graph]);

  /** Reveal a session: focus the tab that already shows it, or open one. */
  const revealSession = (node: GraphNode) => {
    const sessionId = entityId(node.id);
    const existing = tabs.find(
      (tab) => tab.descriptor.view === 'terminal' && tab.descriptor.sessionId === sessionId,
    );
    if (existing) {
      focusTab(existing.id);
      return;
    }
    openTab({
      title: node.meta.vendor ?? node.label,
      sublabel: node.label,
      descriptor: { view: 'terminal', sessionId },
      status: node.status,
      closable: true,
    });
  };

  /** DESIGN §7-4: focus the most recent session here, or offer a new one. */
  const openWorktree = (node: GraphNode) => {
    let newest: GraphNode | null = null;
    for (const child of graph.nodes.values()) {
      if (child.parentId !== node.id) continue;
      if (child.kind !== 'session' && child.kind !== 'run') continue;
      if (!newest || child.updatedAt > newest.updatedAt) newest = child;
    }
    if (newest) revealSession(newest);
    else setNewSessionOpen(true, entityId(node.id));
  };

  const handlers: TreeHandlers = {
    onSelect: selectNode,
    onToggle: toggleCollapsed,
    onOpenWorktree: openWorktree,
    onNewWorktree: (projectNodeId) => setNewWorktreeOpen(true, entityId(projectNodeId)),
    onNewSession: (worktreeNodeId) => setNewSessionOpen(true, entityId(worktreeNodeId)),
    onRemoveWorktree: (node) =>
      void openRemoveDialog(entityId(node.id), node.label, node.meta.branch ?? node.sublabel ?? ''),
    onOpenSession: revealSession,
    attention: (node) => sessions.get(entityId(node.id))?.attention ?? false,
  };

  const openMonitor = () => {
    requestOrphanFilter();
    setActiveView('monitor');
    setDrawerTab('monitor');
  };

  return (
    <div className="sb">
      <div className="hdr">
        <span className="strong s12">{t('view.sessions')}</span>
        <span className="muted s12">{sessionCount}</span>
        <div className="grow" />
        <button
          type="button"
          className="btn icon ghost"
          title={t('sidebar.search')}
          aria-label={t('sidebar.search')}
          aria-pressed={searchOpen}
          onClick={() => {
            setSearchOpen((open) => {
              if (open) setSidebarFilter('');
              return !open;
            });
          }}
        >
          <SearchIcon size="sm" />
        </button>
        <button
          type="button"
          className="btn icon ghost"
          title={t('sidebar.addProject')}
          aria-label={t('sidebar.addProject')}
          onClick={() => void addProject()}
        >
          <FolderPlusIcon size="sm" />
        </button>
        <button
          type="button"
          className="btn icon ghost"
          title={t('worktree.new')}
          aria-label={t('worktree.new')}
          onClick={() => setNewWorktreeOpen(true)}
        >
          <PlusIcon size="sm" />
        </button>
      </div>

      {searchOpen ? (
        <div className="sb-filter">
          <input
            type="text"
            value={filter}
            placeholder={t('sidebar.searchPlaceholder')}
            aria-label={t('sidebar.search')}
            onChange={(event) => setSidebarFilter(event.target.value)}
          />
        </div>
      ) : null}

      {error ? (
        <div className="sb-error" role="alert">
          <span className="sb-error-text">{td(error.key, error.params)}</span>
          <button
            type="button"
            className="sb-iconbtn"
            title={t('common.close')}
            aria-label={t('common.close')}
            onClick={clearWorktreeError}
          >
            <CloseIcon size="sm" />
          </button>
        </div>
      ) : null}

      {rows.length === 0 ? (
        <EmptyState
          message={
            filter.trim()
              ? t('sidebar.noMatches')
              : loaded
                ? t('sidebar.projects.empty')
                : t('common.loading')
          }
          actionLabel={filter.trim() ? undefined : t('sidebar.addProject')}
          onAction={filter.trim() ? undefined : () => void addProject()}
        />
      ) : (
        <ProjectTree
          rows={rows}
          selectedNodeId={selectedNodeId}
          collapsed={collapsed}
          narrow={isNarrowRow(sidebarWidth)}
          handlers={handlers}
        />
      )}

      <OrphanGroup
        nodes={orphans}
        total={graph.totals.orphans}
        selectedNodeId={selectedNodeId}
        onOpenMonitor={openMonitor}
        onSelect={selectNode}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Dispatch view (BACKLOG 3, docs/ui-mockups/Graph.dc.html)
// ---------------------------------------------------------------------------

interface DispatchTaskRowProps {
  taskId: TaskId;
  selected: boolean;
  onSelect: (taskId: TaskId) => void;
}

/** One task: id, title, the vendors it fanned out to, and how far along it is. */
function DispatchTaskRow({ taskId, selected, onSelect }: DispatchTaskRowProps) {
  const { t } = useI18n();
  const task = useAppStore((s) => s.dispatch.tasks.get(taskId));
  const dispatch = useAppStore((s) => s.dispatch);
  if (!task) return null;

  const runs = runsOfTask(dispatch, taskId);
  const progress = taskProgress(runs);
  const vendors = [...new Set(runs.map((run) => run.run.vendor))];

  return (
    <div
      className="tree-row"
      role="button"
      tabIndex={0}
      aria-selected={selected}
      onClick={() => onSelect(taskId)}
      onKeyDown={(event) => {
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          onSelect(taskId);
        }
      }}
    >
      <Mono small className="muted">
        {shortTaskId(task.id)}
      </Mono>
      <span className="ell grow s12">{task.title}</span>
      {vendors.map((vendor) => (
        <Badge key={vendor}>{agentDisplayName(vendor)}</Badge>
      ))}
      <span className="s11 muted">
        {progress.total > 0
          ? t('dispatch.task.progress', { done: progress.done, total: progress.total })
          : t(taskStatusKey(task.status))}
      </span>
      <StatusDot status={taskNodeStatus(task.status)} />
    </div>
  );
}

/**
 * The dispatch view's sidebar: the task list, and the way to start one.
 *
 * `loadTasks()` on mount is the same call the board makes; both fold into the
 * one `dispatchSlice`, so opening this panel never produces a second, disagreeing
 * copy of the list.
 */
export function DispatchSidebar() {
  const { t } = useI18n();
  const order = useAppStore((s) => s.dispatch.order);
  const loaded = useAppStore((s) => s.dispatch.loaded);
  const selectedTaskId = useAppStore((s) => s.dispatch.selectedTaskId);
  const loadTasks = useAppStore((s) => s.loadTasks);
  const selectTask = useAppStore((s) => s.selectTask);

  useEffect(() => {
    void loadTasks();
  }, [loadTasks]);

  // The same intent ⌘⇧D runs: it reveals the board and opens the composer.
  const newDispatch = () => {
    runIntent('newDispatch');
  };

  return (
    <div className="sb">
      <div className="hdr">
        <span className="strong s12">{t('dispatch.title')}</span>
        <span className="muted s12">{order.length}</span>
        <div className="grow" />
        <button
          type="button"
          className="btn icon ghost"
          title={t('dispatch.new')}
          aria-label={t('dispatch.new')}
          onClick={newDispatch}
        >
          <PlusIcon size="sm" />
        </button>
      </div>

      {order.length === 0 ? (
        <EmptyState
          message={loaded ? t('dispatch.empty') : t('common.loading')}
          actionLabel={t('dispatch.new')}
          onAction={newDispatch}
        />
      ) : (
        <div className="scroll-y sidebar-tree">
          {order.map((taskId) => (
            <DispatchTaskRow
              key={taskId}
              taskId={taskId}
              selected={taskId === selectedTaskId}
              onSelect={selectTask}
            />
          ))}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// The switch
// ---------------------------------------------------------------------------

/**
 * The registered sidebar. One field (`ui.activeView`) decides which panel the
 * left column shows; `AppShell` names the landmark after the same field, so the
 * accessibility tree says which view a caller is inside of.
 */
export function Sidebar() {
  const activeView = useAppStore((s) => s.ui.activeView);
  if (activeView === 'team') return <TeamSidebar />;
  if (activeView === 'workers') return null;
  if (activeView === 'dispatch') return <DispatchSidebar />;
  return <SessionsSidebar />;
}
