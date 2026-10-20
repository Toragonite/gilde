/**
 * New session dialog — ⌘T (DESIGN §9-8, M1-PLAN T20 acceptance 10).
 *
 * Pre-filled with the active worktree and the agent that was last launched
 * *in that worktree*, because that is the session the user is overwhelmingly
 * about to open again. An agent that is not installed on this machine is
 * offered as a disabled row with the reason, never as a button that spawns a
 * broken process (DESIGN §3 `agents`, R11: cursor-agent and gemini genuinely
 * cannot run here, and the UI says so).
 */

import { useEffect, useMemo, useState } from 'react';

import { Badge } from '../components/Badge';
import { Sheet } from '../components/Sheet';
import { useI18n } from '../i18n/useI18n';
import { cmd } from '../lib/commands';
import type {
  AgentBinary,
  AgentKindKey,
  GraphNode,
  OpenSessionReq,
  WorktreeId,
} from '../lib/ipc';
import { nodesOfKind } from '../store/graphSlice';
import { lastAgentFor, resolveActiveWorktree } from '../store/sessionSlice';
import { useAppStore } from '../store/useAppStore';

/** The pty is opened with a real winsize; xterm's first fit corrects it. */
const INITIAL_COLS = 80;
const INITIAL_ROWS = 24;

function worktreeLabel(node: GraphNode, projects: Map<string, GraphNode>): string {
  const project = node.parentId ? projects.get(node.parentId) : undefined;
  return project ? `${project.label} · ${node.label}` : node.label;
}

/** The control the dialog opens focused (BACKLOG 14, 15c). */
export type NewSessionFocusTarget = 'primary' | 'worktree' | 'agent';

/**
 * Which control the dialog opens on.
 *
 * `Sheet` focuses its first action, which is Cancel — right for a destructive
 * sheet (UI-GUIDELINES §2-5) and wrong here: opening a session is the whole
 * point of this dialog, so Enter must open one. When the form is already valid
 * — a prefilled worktree and an installed agent, which is the ordinary case —
 * the primary takes the focus. When it is not, the focus goes to the first
 * thing that is missing, in the order the form asks for it, because that is
 * where the user has to go next either way.
 *
 * Pure and exhaustive, so the rule is tested without a DOM: the component only
 * turns the answer into one `data-default-focus` attribute, and `Sheet` does
 * the focusing on the same commit — there is no timer in this path.
 */
export function defaultFocusTarget(form: {
  worktreeSelected: boolean;
  agentReady: boolean;
}): NewSessionFocusTarget {
  if (!form.worktreeSelected) return 'worktree';
  if (!form.agentReady) return 'agent';
  return 'primary';
}

export interface NewSessionActionsProps {
  busy: boolean;
  canSubmit: boolean;
  onCancel: () => void;
  onSubmit: () => void;
}

/**
 * The dialog's action row. Split out of the sheet body so it can be rendered on
 * its own: `Sheet` puts the dialog in a body portal, which needs a DOM, and the
 * one thing worth pinning here — Cancel first, primary marked as the focus
 * target — is pure markup.
 *
 * The marker follows `canSubmit`, which is `defaultFocusTarget() === 'primary'`
 * plus "not already submitting": a form that cannot be opened yet leaves the
 * marker to the field that is missing (BACKLOG 15c), so `Sheet` never has to
 * choose between two marked controls.
 */
export function NewSessionActions({ busy, canSubmit, onCancel, onSubmit }: NewSessionActionsProps) {
  const { t } = useI18n();
  return (
    <>
      <button type="button" className="btn" onClick={onCancel}>
        {t('common.cancel')}
      </button>
      <button
        type="button"
        className="btn primary"
        data-default-focus={canSubmit ? 'true' : undefined}
        disabled={!canSubmit}
        onClick={onSubmit}
      >
        {busy ? t('session.new.opening') : t('session.new.submit')}
      </button>
    </>
  );
}

export interface NewSessionFormProps {
  worktrees: GraphNode[];
  /** `project:<id>` → node, for the `project · worktree` option labels. */
  projects: Map<string, GraphNode>;
  worktreeId: WorktreeId | '';
  agent: AgentKindKey | '';
  prompt: string;
  /** Every agent the machine has, installed or not, shell last. */
  agentRows: AgentBinary[];
  focusTarget: NewSessionFocusTarget;
  onWorktreeChange: (id: WorktreeId) => void;
  onAgentChange: (kind: AgentKindKey) => void;
  onPromptChange: (text: string) => void;
  onDetect: () => void;
  onSubmit: () => void;
}

/**
 * The dialog body, split out for the same reason `NewSessionActions` is: `Sheet`
 * renders into a body portal, so the form cannot be server-rendered inside the
 * dialog, and the thing worth pinning about it — which control carries
 * `data-default-focus` for a given form state (BACKLOG 15c) — is pure markup.
 *
 * It owns no state. Every value and every setter comes from `NewSessionDialog`,
 * so there is still exactly one copy of "what the form currently says".
 */
export function NewSessionForm({
  worktrees,
  projects,
  worktreeId,
  agent,
  prompt,
  agentRows,
  focusTarget,
  onWorktreeChange,
  onAgentChange,
  onPromptChange,
  onDetect,
  onSubmit,
}: NewSessionFormProps) {
  const { t } = useI18n();
  const chosen = agentRows.find((binary) => binary.kind === agent);
  const firstInstalled = agentRows.find((binary) => binary.installed) ?? null;

  return (
    <div className="col gap-12 newsession">
      <label className="col gap-4">
        <span className="muted s11">{t('session.new.worktree')}</span>
        {worktrees.length === 0 ? (
          <span className="s12">{t('session.new.noWorktree')}</span>
        ) : (
          <select
            className="input"
            value={worktreeId}
            data-default-focus={focusTarget === 'worktree' ? 'true' : undefined}
            onChange={(event) => onWorktreeChange(event.target.value as WorktreeId)}
          >
            {worktrees.map((node) => (
              <option key={node.id} value={node.id.slice('worktree:'.length)}>
                {worktreeLabel(node, projects)}
              </option>
            ))}
          </select>
        )}
      </label>

      <div className="col gap-4">
        <span className="muted s11">{t('session.new.agent')}</span>
        {agentRows.length === 0 ? (
          <div className="row gap-8">
            <span className="s12">{t('session.new.noAgents')}</span>
            <button
              type="button"
              className="btn"
              data-default-focus={focusTarget === 'agent' ? 'true' : undefined}
              onClick={onDetect}
            >
              {t('session.new.detect')}
            </button>
          </div>
        ) : (
          <div className="row gap-6 newsession-agents">
            {agentRows.map((binary) => (
              <button
                key={binary.kind}
                type="button"
                className="chip"
                data-selected={binary.kind === agent ? 'true' : undefined}
                data-default-focus={
                  focusTarget === 'agent' && binary.kind === firstInstalled?.kind
                    ? 'true'
                    : undefined
                }
                disabled={!binary.installed}
                title={
                  binary.installed ? (binary.absPath ?? undefined) : t('session.new.notInstalled')
                }
                onClick={() => onAgentChange(binary.kind)}
              >
                {binary.kind}
                {binary.version ? <span className="muted s11">{binary.version}</span> : null}
              </button>
            ))}
          </div>
        )}
        {chosen && !chosen.installed ? (
          <span className="s11 muted">{t('session.new.installHint')}</span>
        ) : null}
        {chosen?.installed && !chosen.verified ? (
          <span className="s11 muted">
            <Badge tone="warn">{t('badge.unverified')}</Badge> {t('session.new.unverifiedHint')}
          </span>
        ) : null}
      </div>

      <label className="col gap-4">
        <span className="muted s11">{t('session.new.prompt')}</span>
        <textarea
          className="textarea newsession-prompt"
          value={prompt}
          placeholder={t('session.new.promptPlaceholder')}
          onChange={(event) => onPromptChange(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && event.metaKey) {
              event.preventDefault();
              onSubmit();
            }
          }}
        />
      </label>
    </div>
  );
}

export function NewSessionDialog() {
  const { t } = useI18n();

  const open = useAppStore((s) => s.newSession.open);
  const requestedWorktreeId = useAppStore((s) => s.newSession.worktreeId);
  const setNewSessionOpen = useAppStore((s) => s.setNewSessionOpen);
  const graph = useAppStore((s) => s.graph);
  const sessions = useAppStore((s) => s.sessions);
  const lastAgentByWorktree = useAppStore((s) => s.lastAgentByWorktree);
  const agents = useAppStore((s) => s.agents);
  const selectedNodeId = useAppStore((s) => s.ui.selectedNodeId);
  const tabs = useAppStore((s) => s.ui.tabs);
  const panes = useAppStore((s) => s.ui.panes);
  const activePaneId = useAppStore((s) => s.ui.activePaneId);
  const openSession = useAppStore((s) => s.openSession);
  const openTab = useAppStore((s) => s.openTab);
  const setAgents = useAppStore((s) => s.setAgents);

  const [worktreeId, setWorktreeId] = useState<WorktreeId | ''>('');
  const [agent, setAgent] = useState<AgentKindKey | ''>('');
  const [prompt, setPrompt] = useState('');
  const [busy, setBusy] = useState(false);
  /** Whether the fields below have been filled in for the current opening. */
  const [prefilled, setPrefilled] = useState(false);

  const worktrees = useMemo(() => nodesOfKind(graph, 'worktree'), [graph]);
  const projects = useMemo(() => {
    const map = new Map<string, GraphNode>();
    for (const node of nodesOfKind(graph, 'project')) map.set(node.id, node);
    return map;
  }, [graph]);

  const focusedSessionId = useMemo(() => {
    const slot = panes.slots.find((s) => s.id === activePaneId);
    const tab = tabs.find((candidate) => candidate.id === slot?.tabId);
    return tab && tab.descriptor.view === 'terminal' ? tab.descriptor.sessionId : null;
  }, [panes, activePaneId, tabs]);

  // Prefill on every open, not once: the active worktree moves while the app
  // runs. **During render**, not in an effect (BACKLOG 15c).
  //
  // The one commit of delay an effect costs was the whole of the focus bug.
  // Effects run child-first, so `Sheet` decides where focus goes *before* this
  // component's effects have run: on the opening commit the worktree and the
  // agent were still empty, the primary button was therefore still `disabled`,
  // and `Sheet` — correctly — declined to focus a dead button and fell back to
  // the first field. A `setTimeout(0)` then raced to move the focus back, and
  // lost outright whenever `agents` had not been answered yet, which is exactly
  // what the ring on the worktree select was.
  //
  // A render-phase update is re-rendered before React commits anything, so the
  // first DOM that exists is already the prefilled one, `disabled` is honest
  // when `Sheet` reads it, and no timer is involved in any of it.
  if (open !== prefilled) {
    setPrefilled(open);
    if (open) {
      const selected = selectedNodeId ? graph.nodes.get(selectedNodeId) : undefined;
      const resolved =
        requestedWorktreeId ??
        resolveActiveWorktree({
          selectedNodeId,
          selectedParentId: selected?.parentId ?? null,
          focusedSessionId,
          sessions,
          worktreeIds: worktrees.map((node) => node.id.slice('worktree:'.length)),
        });
      setWorktreeId(resolved ?? '');
      setAgent(lastAgentFor(lastAgentByWorktree, sessions, resolved) ?? '');
      setPrompt('');
    }
  }

  // Remember the agent per worktree while the dialog is open, too.
  useEffect(() => {
    if (!open || !worktreeId) return;
    const remembered = lastAgentFor(lastAgentByWorktree, sessions, worktreeId);
    if (remembered) setAgent(remembered);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [worktreeId]);

  if (!open) return null;

  const selectable: AgentBinary[] = agents.filter((binary) => binary.kind !== 'shell');
  const shell = agents.find((binary) => binary.kind === 'shell');
  const rows = shell ? [...selectable, shell] : selectable;
  const chosen = rows.find((binary) => binary.kind === agent);
  const canSubmit = Boolean(worktreeId) && Boolean(chosen?.installed) && !busy;

  // Where the focus ring goes on this commit (BACKLOG 15c). Exactly one control
  // below carries the marker; `Sheet` reads it in the same commit that renders
  // it, so the prefilled form opens on 세션 열기 and an incomplete one opens on
  // the field it is incomplete because of.
  const focusTarget = defaultFocusTarget({
    worktreeSelected: Boolean(worktreeId),
    agentReady: Boolean(chosen?.installed),
  });

  const close = () => setNewSessionOpen(false);

  const submit = async () => {
    if (!canSubmit || !worktreeId || !agent) return;
    setBusy(true);
    try {
      const node = graph.nodes.get(`worktree:${worktreeId}`);
      const projectId = node?.parentId?.startsWith('project:')
        ? node.parentId.slice('project:'.length)
        : '';
      const req: OpenSessionReq = {
        projectId,
        worktreeId,
        agent,
        cols: INITIAL_COLS,
        rows: INITIAL_ROWS,
        ...(prompt.trim() ? { prompt: prompt.trim() } : {}),
      };
      const info = await openSession(req);
      if (!info) return;
      openTab({
        title: info.agent,
        sublabel: node?.label,
        descriptor: { view: 'terminal', sessionId: info.id },
        status: 'idle',
        closable: true,
      });
      close();
    } finally {
      setBusy(false);
    }
  };

  const detect = () => {
    void cmd
      .agentsDetect(true)
      .then((found) => setAgents(found))
      .catch(() => undefined);
  };

  return (
    <Sheet
      open={open}
      title={t('session.new.title')}
      onClose={close}
      actions={
        <NewSessionActions
          busy={busy}
          canSubmit={canSubmit}
          onCancel={close}
          onSubmit={() => void submit()}
        />
      }
    >
      <NewSessionForm
        worktrees={worktrees}
        projects={projects}
        worktreeId={worktreeId}
        agent={agent}
        prompt={prompt}
        agentRows={rows}
        focusTarget={focusTarget}
        onWorktreeChange={setWorktreeId}
        onAgentChange={setAgent}
        onPromptChange={setPrompt}
        onDetect={detect}
        onSubmit={() => void submit()}
      />
    </Sheet>
  );
}
