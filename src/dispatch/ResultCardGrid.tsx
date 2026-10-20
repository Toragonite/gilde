/**
 * The dispatch board — a pane view (`descriptor.view === 'dispatch'`), never a
 * modal (DESIGN §9-1: a pane is content agnostic, and the board is just a tile).
 *
 *   ┌ tasks ─────────┬ #id · title · elapsed · [전부 중지] · [1|2|3|4] · [비교] ┐
 *   │ a1b2c3 title   │ ┌ ResultCard ─────────┬ ResultCard ─────────┐          │
 *   │ claude codex   │ │ claude · done       │ codex · working     │          │
 *   └────────────────┴─┴─────────────────────┴─────────────────────┴──────────┘
 *
 * The list on the left is the same one the mockup (`docs/ui-mockups/Graph.dc.html`)
 * puts in the sidebar column; it lives inside the pane instead because the
 * shell's sidebar slot has one owner (T21's project tree) and a board that only
 * works while a particular sidebar is showing would not be a tile.
 *
 * Keyboard (acceptance 8), live only while this pane is the active one:
 * `j`/`k` move between cards, `↵` expands, `s` stops, `t` opens a terminal in
 * the run's worktree, `⌘⇧C` toggles compare mode — equal body heights and
 * synchronised scrolling.
 *
 * Restoring after a restart is `dispatch_list` + `dispatch_get` and nothing
 * else (acceptance 10): the cards come back from the durable rows, and the
 * stream picks up wherever it is.
 */

import { useCallback, useEffect, useMemo, useRef } from 'react';

import { Badge } from '../components/Badge';
import { EmptyState } from '../components/EmptyState';
import { Mono } from '../components/Mono';
import { RelativeTime } from '../components/RelativeTime';
import { StatusDot } from '../components/StatusDot';
import { PlusIcon } from '../components/icons';
import { useHotkeys } from '../hooks/useHotkeys';
import type { HotkeyBinding } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { formatElapsedClock } from '../lib/format';
import type { DispatchTask, DispatchTaskStatus, NodeStatus, RunId, TaskId } from '../lib/ipc';
import type { PaneViewProps } from '../panes/Pane';
import { useAppStore } from '../store/useAppStore';
import { isTerminalRun, runsOfTask, taskProgress } from '../store/dispatchSlice';
import type { RunView } from '../store/dispatchSlice';
import { ResultCard, useElapsedTick } from './ResultCard';
import './dispatch.css';
import { agentDisplayName } from '../lib/agents';

// ---------------------------------------------------------------------------
// Task status presentation
// ---------------------------------------------------------------------------

/** Task status → the shared status token. `partial` is amber, not red. */
export function taskNodeStatus(status: DispatchTaskStatus): NodeStatus {
  switch (status) {
    case 'queued':
      return 'idle';
    case 'running':
      return 'working';
    case 'done':
      return 'done';
    case 'partial':
      return 'stalled';
    case 'error':
      return 'error';
    case 'cancelled':
      return 'exited';
  }
}

export function taskStatusKey(status: DispatchTaskStatus): MessageKey {
  return `dispatch.task.${status}` as MessageKey;
}

/** The short handle a card header shows. Task ids are opaque and long. */
export function shortTaskId(id: TaskId): string {
  return id.length <= 6 ? id : id.slice(id.length - 6);
}

// ---------------------------------------------------------------------------
// Task list
// ---------------------------------------------------------------------------

interface TaskRowProps {
  task: DispatchTask;
  runs: RunView[];
  selected: boolean;
  onSelect: () => void;
}

function TaskRow({ task, runs, selected, onSelect }: TaskRowProps) {
  const { t } = useI18n();
  const progress = taskProgress(runs);
  const vendors = useMemo(() => {
    const seen: string[] = [];
    for (const view of runs) if (!seen.includes(view.run.vendor)) seen.push(view.run.vendor);
    return seen;
  }, [runs]);

  return (
    <button
      type="button"
      className="dsp-task"
      aria-selected={selected}
      aria-label={task.title}
      onClick={onSelect}
    >
      <span className="row gap-8">
        <Mono small className="muted">
          {shortTaskId(task.id)}
        </Mono>
        <span className="ell grow s12">{task.title}</span>
        <StatusDot status={taskNodeStatus(task.status)} />
      </span>
      <span className="row gap-6 muted s11">
        {vendors.map((vendor) => (
          <Badge key={vendor}>{agentDisplayName(vendor)}</Badge>
        ))}
        <span>
          {progress.total > 0
            ? t('dispatch.task.progress', { done: progress.done, total: progress.total })
            : t(taskStatusKey(task.status))}
        </span>
        <RelativeTime at={task.createdAt} />
      </span>
    </button>
  );
}

// ---------------------------------------------------------------------------
// Board
// ---------------------------------------------------------------------------

export function ResultCardGrid({ descriptor, active }: PaneViewProps) {
  const { t } = useI18n();

  const order = useAppStore((s) => s.dispatch.order);
  const tasks = useAppStore((s) => s.dispatch.tasks);
  const runsById = useAppStore((s) => s.dispatch.runs);
  const runsByTask = useAppStore((s) => s.dispatch.runsByTask);
  const selectedTaskId = useAppStore((s) => s.dispatch.selectedTaskId);
  const focusedRunId = useAppStore((s) => s.dispatch.focusedRunId);
  const expanded = useAppStore((s) => s.dispatch.expanded);
  const compare = useAppStore((s) => s.dispatch.compare);
  const columns = useAppStore((s) => s.dispatch.columns);
  const loaded = useAppStore((s) => s.dispatch.loaded);
  const graph = useAppStore((s) => s.graph);

  const loadTasks = useAppStore((s) => s.loadTasks);
  const selectTask = useAppStore((s) => s.selectTask);
  const focusRun = useAppStore((s) => s.focusRun);
  const moveRunFocus = useAppStore((s) => s.moveRunFocus);
  const toggleRunExpanded = useAppStore((s) => s.toggleRunExpanded);
  const setCompare = useAppStore((s) => s.setCompare);
  const setColumns = useAppStore((s) => s.setColumns);
  const cancelTask = useAppStore((s) => s.cancelTask);
  const cancelRun = useAppStore((s) => s.cancelRun);
  const openRunTerminal = useAppStore((s) => s.openRunTerminal);
  const setComposerOpen = useAppStore((s) => s.setComposerOpen);

  useEffect(() => {
    if (!loaded) void loadTasks();
  }, [loaded, loadTasks]);

  // A tab opened for a specific task (the composer does that on submit) wins
  // over whatever the board had selected — on the first paint, not one effect
  // later, so the pane never flashes somebody else's results.
  const wantedTaskId = descriptor.view === 'dispatch' ? descriptor.taskId : undefined;
  const shownTaskId = wantedTaskId ?? selectedTaskId;
  useEffect(() => {
    if (wantedTaskId && wantedTaskId !== selectedTaskId) selectTask(wantedTaskId);
  }, [wantedTaskId, selectedTaskId, selectTask]);

  const runs = useMemo(
    () => runsOfTask({ runs: runsById, runsByTask }, shownTaskId),
    [runsById, runsByTask, shownTaskId],
  );

  const task = shownTaskId ? tasks.get(shownTaskId) : undefined;
  const anyLive = runs.some((view) => !isTerminalRun(view.run.status));
  const now = useElapsedTick(anyLive);

  // ── compare mode: one height, one scroll position ────────────────────────
  const bodies = useRef(new Map<RunId, HTMLPreElement>());
  const syncing = useRef(false);

  const registerBody = useCallback(
    (runId: RunId) => (element: HTMLPreElement | null) => {
      if (element) bodies.current.set(runId, element);
      else bodies.current.delete(runId);
    },
    [],
  );

  const onBodyScroll = useCallback(
    (runId: RunId) => (element: HTMLPreElement) => {
      if (!compare || syncing.current) return;
      syncing.current = true;
      const max = Math.max(1, element.scrollHeight - element.clientHeight);
      const ratio = element.scrollTop / max;
      for (const [id, other] of bodies.current) {
        if (id === runId) continue;
        const otherMax = Math.max(0, other.scrollHeight - other.clientHeight);
        other.scrollTop = ratio * otherMax;
      }
      // Scroll events are delivered asynchronously, so the flag has to survive
      // past this call or the cards we just moved would echo back and forth.
      if (typeof requestAnimationFrame === 'function') {
        requestAnimationFrame(() => {
          syncing.current = false;
        });
      } else {
        syncing.current = false;
      }
    },
    [compare],
  );

  // ── keyboard (acceptance 8) ──────────────────────────────────────────────
  //
  // Single-letter bindings only claim a key while this pane is the active one
  // and no sheet is over it; `useHotkeys` already refuses to fire inside a text
  // entry, so typing in a card's resume box still reaches the box.
  const boardHasKeys = () => !useAppStore.getState().dispatch.composer.open;
  const bindings = useMemo<HotkeyBinding[]>(
    () => [
      { id: 'dsp-next', combo: 'j', when: boardHasKeys, run: () => moveRunFocus(1) },
      { id: 'dsp-prev', combo: 'k', when: boardHasKeys, run: () => moveRunFocus(-1) },
      {
        id: 'dsp-expand',
        when: boardHasKeys,
        combo: 'enter',
        run: () => {
          if (focusedRunId) toggleRunExpanded(focusedRunId);
        },
      },
      {
        id: 'dsp-stop',
        when: boardHasKeys,
        combo: 's',
        run: () => {
          if (focusedRunId) void cancelRun(focusedRunId);
        },
      },
      {
        id: 'dsp-terminal',
        when: boardHasKeys,
        combo: 't',
        run: () => {
          if (focusedRunId) void openRunTerminal(focusedRunId);
        },
      },
      { id: 'dsp-compare', combo: 'mod+shift+c', run: () => setCompare(!compare) },
    ],
    [moveRunFocus, focusedRunId, toggleRunExpanded, cancelRun, openRunTerminal, setCompare, compare],
  );
  useHotkeys(bindings, active);

  const taskRows = order.map((id) => tasks.get(id)).filter((row): row is DispatchTask => Boolean(row));

  return (
    <div className="dsp-board">
      <aside className="dsp-tasks" aria-label={t('dispatch.title')}>
        <div className="hdr">
          <span className="strong s12">{t('dispatch.title')}</span>
          <Mono small className="muted">
            {taskRows.length}
          </Mono>
          <div className="grow" />
          <button
            type="button"
            className="btn icon ghost"
            aria-label={t('dispatch.new')}
            title={t('dispatch.new')}
            onClick={() => setComposerOpen(true)}
          >
            <PlusIcon size="sm" />
          </button>
        </div>
        {taskRows.length === 0 ? (
          <EmptyState
            message={t('dispatch.empty')}
            actionLabel={t('dispatch.new')}
            onAction={() => setComposerOpen(true)}
          />
        ) : (
          <div className="scroll-y dsp-task-list">
            {taskRows.map((row) => (
              <TaskRow
                key={row.id}
                task={row}
                runs={runsOfTask({ runs: runsById, runsByTask }, row.id)}
                selected={row.id === shownTaskId}
                onSelect={() => selectTask(row.id)}
              />
            ))}
          </div>
        )}
      </aside>

      <div className="dsp-main">
        <div className="hdr">
          {task ? (
            <>
              <Mono small className="muted">
                {shortTaskId(task.id)}
              </Mono>
              <span className="strong ell s12">{task.title}</span>
              <div className="grow" />
              <Mono small className="muted">
                {formatElapsedClock(Math.max(0, (task.finishedAt ?? now) - task.createdAt))}
              </Mono>
              <div className="seg" role="group" aria-label={t('dispatch.columns')}>
                {([1, 2, 3, 4] as const).map((n) => (
                  <button
                    key={n}
                    type="button"
                    aria-pressed={columns === n}
                    onClick={() => setColumns(n)}
                  >
                    {n}
                  </button>
                ))}
              </div>
              <button
                type="button"
                className="btn"
                aria-pressed={compare}
                onClick={() => setCompare(!compare)}
              >
                {t('dispatch.compare')}
              </button>
              <button
                type="button"
                className="btn danger"
                disabled={!runs.some((view) => !isTerminalRun(view.run.status))}
                onClick={() => void cancelTask(task.id)}
              >
                {t('dispatch.task.stopAll')}
              </button>
            </>
          ) : (
            <span className="muted s12">{t('dispatch.title')}</span>
          )}
        </div>

        {!task ? (
          <EmptyState
            message={t('dispatch.empty')}
            actionLabel={t('dispatch.new')}
            onAction={() => setComposerOpen(true)}
          />
        ) : runs.length === 0 ? (
          <EmptyState message={t('dispatch.noRuns')} />
        ) : (
          <div
            className="scroll-y dsp-grid"
            data-columns={columns}
            data-compare={compare ? 'true' : undefined}
          >
            {runs.map((view) => (
              <ResultCard
                key={view.run.id}
                view={view}
                focused={view.run.id === focusedRunId}
                expanded={Boolean(expanded[view.run.id])}
                compare={compare}
                worktree={
                  view.run.worktreeId
                    ? graph.nodes.get(`worktree:${view.run.worktreeId}`)
                    : undefined
                }
                onFocus={() => focusRun(view.run.id)}
                registerBody={registerBody(view.run.id)}
                onBodyScroll={onBodyScroll(view.run.id)}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
