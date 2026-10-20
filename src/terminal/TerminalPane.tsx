/**
 * TerminalPane — one session in one tile (DESIGN §9-1, §9-4).
 *
 * Layout follows `docs/ui-mockups/Main.dc.html` exactly: a 32 px header
 * (agent badge · worktree chip · status pill · elapsed · RSS · kebab), the
 * terminal surface, and a composer pinned to the bottom. Nothing here paints a
 * colour of its own; the four semantic tokens and the neutral scale are the
 * whole palette (UI-GUIDELINES §2-1a).
 *
 * Two behaviours are worth calling out:
 *
 *  - **Literal keys.** When the status engine says `waitingForInput`, the
 *    composer is replaced by the matched prompt line and `y` / `n` / `1` / `2` /
 *    `Esc` / `Enter` buttons (DESIGN §6-5). Pasting a paragraph into a yes/no
 *    dialog is exactly what this prevents, and the first session of a fresh
 *    worktree really does start in this state — every agent CLI asks for folder
 *    trust before it will do anything (PINNED-VERSIONS §9-3).
 *  - **Destructive actions confirm.** Stop and Detach state their target and
 *    their consequence in one line, with Cancel focused (UI-GUIDELINES §2-5).
 *
 * The module has no side effects: `terminal/register.ts` is what plugs the pane
 * view, the ⌘T overlay, the `newSession` intent and these inspector actions
 * into the T05 shell seams.
 */

import { useEffect, useMemo, useRef, useState } from 'react';

import { Badge, Chip, Kbd } from '../components/Badge';
import { EmptyState } from '../components/EmptyState';
import { Mono } from '../components/Mono';
import { Sheet } from '../components/Sheet';
import { GitBranchIcon, MoreIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { cmd } from '../lib/commands';
import { formatBytes, formatElapsedClock } from '../lib/format';
import type { CloseMode, GraphNode, SessionId, SessionStatus } from '../lib/ipc';
import type { PaneViewProps } from '../panes/Pane';
import type { InspectorActionProps } from '../shell/Inspector';
import { isSessionOver, sessionIdOfNode } from '../store/sessionSlice';
import type { SessionView } from '../store/sessionSlice';
import { useAppStore } from '../store/useAppStore';
import { EvidencePopover } from './EvidencePopover';
import { SessionStatusPill } from './SessionStatusPill';
import { XTermHost } from './XTermHost';
import { readSnapshotHeader, SNAPSHOT_HEADER_LEN } from './terminalPool';

import './terminal.css';

// ---------------------------------------------------------------------------
// Session actions, shared by the kebab and the inspector
// ---------------------------------------------------------------------------

export type SessionActionId =
  | 'stop'
  | 'gentle'
  | 'restart'
  | 'reveal'
  | 'transcript'
  | 'detach'
  | 'dismiss';

export interface SessionAction {
  id: SessionActionId;
  label: MessageKey;
  danger?: boolean;
  /** Asks first: it ends or orphans a process (UI-GUIDELINES §2-5). */
  confirm?: boolean;
}

const SESSION_ACTIONS: readonly SessionAction[] = [
  { id: 'stop', label: 'session.menu.stop', danger: true, confirm: true },
  { id: 'gentle', label: 'session.menu.gentle', confirm: true },
  { id: 'restart', label: 'session.menu.restart', confirm: true },
  { id: 'reveal', label: 'session.menu.reveal' },
  { id: 'transcript', label: 'session.menu.transcript' },
  { id: 'detach', label: 'session.menu.detach', confirm: true },
];

/**
 * M1.5 BACKLOG 6: what a host-crash row may be asked to do.
 *
 * There is no process behind an adopted row and no live session in the manager,
 * so `session_restart`, `session_signal`, `session_send_prompt`, `session_attach`
 * and `session_snapshot` all answer `not_found`. Offering them would be offering
 * an error. `reveal` is kept because it opens the worktree directory and never
 * touches the session at all; `dismiss` is the one thing left to do with the row.
 */
const CRASH_ACTIONS: readonly SessionAction[] = [
  { id: 'reveal', label: 'session.menu.reveal' },
  { id: 'dismiss', label: 'session.hostCrash.dismiss' },
];

/** Every action the menu can name, live or crashed — the lookup table for `ask`. */
const ALL_ACTIONS: readonly SessionAction[] = [...SESSION_ACTIONS, ...CRASH_ACTIONS];

/** The actions a session may be asked for, given what is actually behind it. */
export function sessionActionsFor(hostCrash: boolean): readonly SessionAction[] {
  return hostCrash ? CRASH_ACTIONS : SESSION_ACTIONS;
}

const CLOSE_MODE: Partial<Record<SessionActionId, CloseMode>> = {
  stop: 'standard',
  gentle: 'gentle',
  detach: 'detach',
};

/**
 * The scrollback as text. The ring is the durable copy (DESIGN §6-7), so the
 * transcript is exactly what `session_snapshot` returns, decoded once.
 */
async function transcriptOf(sessionId: SessionId): Promise<string> {
  const buffer = await cmd.sessionSnapshot(sessionId);
  const bytes = new Uint8Array(buffer);
  readSnapshotHeader(bytes);
  return new TextDecoder().decode(bytes.subarray(SNAPSHOT_HEADER_LEN));
}

function useSessionActions(sessionId: SessionId) {
  const closeSession = useAppStore((s) => s.closeSession);
  const restartSession = useAppStore((s) => s.restartSession);
  const dismissCrashedSession = useAppStore((s) => s.dismissCrashedSession);
  const pushToast = useAppStore((s) => s.pushToast);

  return async (action: SessionActionId): Promise<void> => {
    // Read on use, not on render: subscribing to the whole session map here
    // would re-render every pane on every status event.
    const view = useAppStore.getState().sessions.get(sessionId);
    switch (action) {
      case 'dismiss': {
        // M1.5 BACKLOG 6: nothing is signalled — the row is marked closed so the
        // next launch does not offer it again, and the tabs showing it go away.
        await dismissCrashedSession(sessionId);
        return;
      }
      case 'stop':
      case 'gentle':
      case 'detach': {
        const mode = CLOSE_MODE[action] as CloseMode;
        const report = await closeSession(sessionId, mode);
        if (!report) return;
        pushToast({
          level: mode === 'detach' ? 'warn' : 'info',
          key: mode === 'detach' ? 'session.detached' : 'session.closed',
          params: { survivors: report.survivors.length, fence: report.fence },
          detail: null,
          actionId: null,
          undoToken: null,
          ttlMs: 6000,
        });
        return;
      }
      case 'restart': {
        const info = await restartSession(sessionId);
        if (info) {
          pushToast({
            level: 'info',
            key: 'session.restarted',
            params: {},
            detail: null,
            actionId: null,
            undoToken: null,
            ttlMs: 4000,
          });
        }
        return;
      }
      case 'reveal': {
        const cwd = view?.info.cwd;
        if (cwd) await cmd.appReveal(cwd).catch(() => undefined);
        return;
      }
      case 'transcript': {
        try {
          const text = await transcriptOf(sessionId);
          await navigator.clipboard.writeText(text);
          pushToast({
            level: 'success',
            key: 'session.transcript.copied',
            params: {},
            detail: null,
            actionId: null,
            undoToken: null,
            ttlMs: 4000,
          });
        } catch (e) {
          pushToast({
            level: 'error',
            key: 'session.transcript.failed',
            params: {},
            detail: e instanceof Error ? e.message : null,
            actionId: null,
            undoToken: null,
            ttlMs: 0,
          });
        }
      }
    }
  };
}

const CONFIRM_BODY: Record<string, MessageKey> = {
  stop: 'session.confirm.stop',
  gentle: 'session.confirm.gentle',
  restart: 'session.confirm.restart',
  detach: 'session.confirm.detach',
};

interface ConfirmState {
  action: SessionActionId;
  title: string;
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

function useElapsed(since: number, live: boolean): string {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!live) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [live]);
  return formatElapsedClock(Math.max(0, now - since));
}

interface HeaderProps {
  view: SessionView;
  worktree: GraphNode | undefined;
  rssBytes: number | undefined;
  onAction: (action: SessionActionId) => void;
}

/** BACKLOG 6: the header says why it exited, not just that it did. */
function crashBadge(view: SessionView): boolean {
  return view.hostCrash === true;
}

function TerminalHeader({ view, worktree, rssBytes, onAction }: HeaderProps) {
  const { t, locale } = useI18n();
  const [menuOpen, setMenuOpen] = useState(false);
  const [evidenceOpen, setEvidenceOpen] = useState(false);
  const menu = useRef<HTMLDivElement>(null);
  const selectNode = useAppStore((s) => s.selectNode);
  const elapsed = useElapsed(view.info.openedAt, !isSessionOver(view.status));

  useEffect(() => {
    if (!menuOpen) return;
    const onPointerDown = (event: MouseEvent) => {
      if (menu.current && !menu.current.contains(event.target as Node)) setMenuOpen(false);
    };
    window.addEventListener('mousedown', onPointerDown, true);
    return () => window.removeEventListener('mousedown', onPointerDown, true);
  }, [menuOpen]);

  return (
    <div className="hdr term-hdr">
      <Badge title={view.info.cwd}>{view.info.agent}</Badge>

      <Chip
        title={worktree?.sublabel ?? view.info.cwd}
        onClick={() => selectNode(`worktree:${view.info.worktreeId}`)}
      >
        <GitBranchIcon className="i sm" />
        {worktree?.label ?? view.info.worktreeId.slice(0, 8)}
      </Chip>

      <SessionStatusPill
        status={view.status}
        confidence={view.confidence}
        expanded={evidenceOpen}
        onClick={() => setEvidenceOpen((open) => !open)}
      />

      {crashBadge(view) ? <Badge tone="warn">{t('session.hostCrash')}</Badge> : null}

      {view.attention ? <Badge tone="warn">{t('badge.attention')}</Badge> : null}

      <Mono small className="muted" title={t('inspector.elapsed')}>
        {elapsed}
      </Mono>
      {rssBytes !== undefined ? (
        <Mono small className="muted" title={t('inspector.rss')}>
          {formatBytes(rssBytes, locale)}
        </Mono>
      ) : null}

      <div className="grow" />

      <button
        type="button"
        className="btn icon ghost"
        aria-label={t('session.menu.title')}
        aria-expanded={menuOpen ? 'true' : 'false'}
        onClick={() => setMenuOpen((open) => !open)}
      >
        <MoreIcon className="i sm" />
      </button>

      {evidenceOpen ? (
        <EvidencePopover
          status={view.status}
          confidence={view.confidence}
          evidence={view.evidence}
          onClose={() => setEvidenceOpen(false)}
        />
      ) : null}

      {menuOpen ? (
        <div className="popover term-menu col" ref={menu} role="menu">
          {sessionActionsFor(crashBadge(view)).map((action) => (
            <button
              key={action.id}
              type="button"
              role="menuitem"
              className={action.danger ? 'btn ghost danger' : 'btn ghost'}
              onClick={() => {
                setMenuOpen(false);
                onAction(action.id);
              }}
            >
              {t(action.label)}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Literal keys and composer
// ---------------------------------------------------------------------------

/** DESIGN §6-5: never paste a paragraph into a yes/no dialog. */
const LITERAL_KEYS: readonly { id: string; label: string; bytes: string }[] = [
  { id: 'y', label: 'y', bytes: 'y\r' },
  { id: 'n', label: 'n', bytes: 'n\r' },
  { id: '1', label: '1', bytes: '1' },
  { id: '2', label: '2', bytes: '2' },
  { id: 'enter', label: '↵', bytes: '\r' },
  { id: 'esc', label: 'Esc', bytes: '\x1b' },
];

function LiteralKeys({ sessionId, prompt }: { sessionId: SessionId; prompt: string }) {
  const { t } = useI18n();
  const writeSession = useAppStore((s) => s.writeSession);

  return (
    <div className="term-keys">
      <span className="mono s11 muted ell grow" title={prompt}>
        {prompt}
      </span>
      {LITERAL_KEYS.map((key) => (
        <button
          key={key.id}
          type="button"
          className="btn"
          aria-label={t('session.keys.send', { key: key.label })}
          onClick={() => void writeSession(sessionId, key.bytes)}
        >
          <span className="mono">{key.label}</span>
        </button>
      ))}
    </div>
  );
}

function Composer({ view, worktree }: { view: SessionView; worktree: GraphNode | undefined }) {
  const { t } = useI18n();
  const sendPrompt = useAppStore((s) => s.sendPrompt);
  const [text, setText] = useState('');
  const [sending, setSending] = useState(false);
  const over = isSessionOver(view.status);

  const submit = async () => {
    const value = text.trim();
    if (!value || sending || over) return;
    setSending(true);
    try {
      if (await sendPrompt(view.id, value)) setText('');
    } finally {
      setSending(false);
    }
  };

  return (
    <div className="term-composer">
      <div className="term-composer-box">
        {/* M1.5 BACKLOG 11: no label element wraps this field, so it needs an
            explicit accessible name. Without it the macOS AX tree renders
            `AXTextArea [missing value]`, VoiceOver announces nothing, and
            scripts/gui-smoke has no name to address the composer by. A
            placeholder is not an accessible name. */}
        <textarea
          className="textarea term-composer-input"
          value={text}
          disabled={over}
          aria-label={t('session.composer.label', { agent: view.info.agent })}
          placeholder={t('session.composer.placeholder', { agent: view.info.agent })}
          onChange={(event) => setText(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && event.metaKey) {
              event.preventDefault();
              void submit();
            }
          }}
        />
        <div className="row gap-6 term-composer-actions">
          <Badge>{view.info.agent}</Badge>
          <Chip title={view.info.cwd}>
            <GitBranchIcon className="i sm" />
            {worktree?.label ?? view.info.title}
          </Chip>
          <div className="grow" />
          <Kbd>⌘↵</Kbd>
          <button
            type="button"
            className="btn primary"
            disabled={over || sending || !text.trim()}
            onClick={() => void submit()}
          >
            {t('session.composer.send')}
          </button>
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Pane
// ---------------------------------------------------------------------------

function promptOf(status: SessionStatus): string | null {
  return status.kind === 'waitingForInput' ? status.prompt : null;
}

export function TerminalPane({ descriptor, active }: PaneViewProps) {
  const { t } = useI18n();
  const sessionId = descriptor.view === 'terminal' ? descriptor.sessionId : null;

  const view = useAppStore((s) => (sessionId ? s.sessions.get(sessionId) : undefined));
  const loadSessions = useAppStore((s) => s.loadSessions);
  const setAttention = useAppStore((s) => s.setAttention);
  const setNewSessionOpen = useAppStore((s) => s.setNewSessionOpen);
  const node = useAppStore((s) => (sessionId ? s.graph.nodes.get(`session:${sessionId}`) : undefined));
  const worktree = useAppStore((s) =>
    view ? s.graph.nodes.get(`worktree:${view.info.worktreeId}`) : undefined,
  );
  const runAction = useSessionActions(sessionId ?? '');
  const [confirm, setConfirm] = useState<ConfirmState | null>(null);

  // A pane can be restored from a persisted layout before `session_list` ran.
  const missing = Boolean(sessionId) && !view;
  useEffect(() => {
    if (missing) void loadSessions();
  }, [missing, loadSessions]);

  // Looking at the tab is what clears the "the agent wants you" badge.
  useEffect(() => {
    if (active && sessionId && view?.attention) setAttention(sessionId, false);
  }, [active, sessionId, view?.attention, setAttention]);

  const prompt = useMemo(() => (view ? promptOf(view.status) : null), [view]);

  if (!sessionId) return null;

  if (!view) {
    return (
      <EmptyState
        message={t('session.notFound')}
        actionLabel={t('session.new')}
        onAction={() => setNewSessionOpen(true)}
      />
    );
  }

  const ask = (action: SessionActionId) => {
    const meta = ALL_ACTIONS.find((candidate) => candidate.id === action);
    if (!meta?.confirm) {
      void runAction(action);
      return;
    }
    setConfirm({ action, title: t(meta.label) });
  };

  // M1.5 BACKLOG 6: an adopted row has no pty behind it, so mounting `XTermHost`
  // would fire `session_attach` and answer a restored tab with an error toast on
  // every boot. The tab is kept — that is the point of the restore — and it says
  // what happened, with the one action that applies.
  if (view.hostCrash) {
    return (
      <div className="term-pane">
        <TerminalHeader
          view={view}
          worktree={worktree}
          rssBytes={undefined}
          onAction={ask}
        />
        <EmptyState
          message={t('session.hostCrash.body')}
          actionLabel={t('session.hostCrash.dismiss')}
          onAction={() => void runAction('dismiss')}
        />
      </div>
    );
  }

  return (
    <div className="term-pane">
      <TerminalHeader
        view={view}
        worktree={worktree}
        rssBytes={node?.metrics.rssBytes}
        onAction={ask}
      />

      {view.scrollbackLost ? (
        <div className="term-banner s11 mono">{t('session.scrollbackLost')}</div>
      ) : null}

      <XTermHost sessionId={sessionId} active={active} />

      {prompt !== null ? <LiteralKeys sessionId={sessionId} prompt={prompt} /> : null}

      <Composer view={view} worktree={worktree} />

      <Sheet
        open={confirm !== null}
        title={confirm?.title ?? ''}
        onClose={() => setConfirm(null)}
        actions={
          <>
            <button type="button" className="btn" onClick={() => setConfirm(null)}>
              {t('common.cancel')}
            </button>
            <button
              type="button"
              className={confirm?.action === 'stop' ? 'btn danger' : 'btn'}
              onClick={() => {
                const action = confirm?.action;
                setConfirm(null);
                if (action) void runAction(action);
              }}
            >
              {confirm ? t(ALL_ACTIONS.find((a) => a.id === confirm.action)!.label) : ''}
            </button>
          </>
        }
      >
        {confirm
          ? t(CONFIRM_BODY[confirm.action], {
              agent: view.info.agent,
              worktree: worktree?.label ?? view.info.worktreeId,
              pid: view.info.pid,
            })
          : null}
      </Sheet>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Inspector actions for a session node (DESIGN §9-1)
// ---------------------------------------------------------------------------

export function SessionInspectorActions({ node }: InspectorActionProps) {
  const { t } = useI18n();
  const sessionId = sessionIdOfNode(node) ?? '';
  const view = useAppStore((s) => s.sessions.get(sessionId));
  const runAction = useSessionActions(sessionId);
  const [confirm, setConfirm] = useState<SessionActionId | null>(null);

  if (!view) return null;

  return (
    <>
      <div className="sec">{t('session.actions')}</div>
      <div className="row gap-6 inspector-actions">
        {/* BACKLOG 6: the same gate the kebab uses — a crashed row is offered
            only what cannot answer `not_found`. */}
        {sessionActionsFor(view.hostCrash === true).map((action) => (
          <button
            key={action.id}
            type="button"
            className={action.danger ? 'btn danger' : 'btn'}
            onClick={() => (action.confirm ? setConfirm(action.id) : void runAction(action.id))}
          >
            {t(action.label)}
          </button>
        ))}
      </div>
      <Sheet
        open={confirm !== null}
        title={confirm ? t(ALL_ACTIONS.find((a) => a.id === confirm)!.label) : ''}
        onClose={() => setConfirm(null)}
        actions={
          <>
            <button type="button" className="btn" onClick={() => setConfirm(null)}>
              {t('common.cancel')}
            </button>
            <button
              type="button"
              className={confirm === 'stop' ? 'btn danger' : 'btn'}
              onClick={() => {
                const action = confirm;
                setConfirm(null);
                if (action) void runAction(action);
              }}
            >
              {confirm ? t(ALL_ACTIONS.find((a) => a.id === confirm)!.label) : ''}
            </button>
          </>
        }
      >
        {confirm
          ? t(CONFIRM_BODY[confirm], {
              agent: view.info.agent,
              worktree: view.info.worktreeId,
              pid: view.info.pid,
            })
          : null}
      </Sheet>
    </>
  );
}

