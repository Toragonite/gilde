/**
 * The showcase backend: what answers the generated command wrappers
 * (`lib/commands.ts`) in showcase mode instead of Tauri. Its type is the one
 * T13 committed in `lib/mode.ts` (contract A8.3):
 * `(name, req, channel?) => Promise<unknown>`.
 *
 * It plays the core against a `ShowcaseWorld`:
 *  - `store_subscribe` returns the snapshot of the scene's moment and keeps the
 *    channel; the patch script (`timeline.ts`) then arrives on it with strictly
 *    increasing revs, on timers (a live page) or through `advanceTo` (gates
 *    and tests, deterministic);
 *  - the read commands (bootstrap, settings, New session options, History,
 *    conversations, PTY screens, team message bodies, quota, health, Changes,
 *    the quit preview) answer from the world;
 *  - a few actions change the world the way the core would (answering a card,
 *    marking a reply seen, Later, dismissing an ended row), published as
 *    patches;
 *  - every other command is refused as `UNSUPPORTED{cap:"showcase"}`, which the
 *    screens already treat as "not available here".
 * Answers cross a JSON round trip, as over IPC: the store never holds a
 * reference into the world.
 */

import type { ChannelLike } from '../lib/channel';
import type { CommandName } from '../lib/gen/commands';
import type * as Generated from '../lib/gen/commands';
import type { ConvoEnvelope } from '../lib/gen/ConvoEnvelope';
import type { ConvoSourceKind } from '../lib/gen/ConvoSourceKind';
import type { DecisionKind } from '../lib/gen/DecisionKind';
import type { IpcError } from '../lib/gen/IpcError';
import type { LaunchInfo } from '../lib/gen/LaunchInfo';
import type { Locale } from '../lib/gen/Locale';
import type { PtyChunk } from '../lib/gen/PtyChunk';
import type { RequestId } from '../lib/gen/RequestId';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { Settings } from '../lib/gen/Settings';
import type { StoreOp } from '../lib/gen/StoreOp';
import type { StorePatch } from '../lib/gen/StorePatch';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import type { SubId } from '../lib/gen/SubId';
import type { Theme } from '../lib/gen/Theme';
import type { JsonValue } from '../lib/gen/serde_json/JsonValue';
import type { ShowcaseBackend } from '../lib/mode';
import { foldSnapshot, sessionChange, snapshotAt } from './timeline';
import type { TimelineStep } from './timeline';
import { PROJECT_NAMES, historyConversationOf } from './world';
import type { ShowcaseWorld } from './world';

/**
 * A string only the showcase carries. `scripts/check-showcase-tree-shaking.mjs`
 * asserts it is absent from a normal build and present in a showcase build; it
 * is the detail of every refusal and the page hook's `marker`, so it is used at
 * runtime and survives minification.
 */
export const SHOWCASE_MARKER = 'gilde-showcase-demo-backend';

type Fns = typeof Generated;
type Req<K extends CommandName> = Parameters<Fns[K]>[0];
type Res<K extends CommandName> = Awaited<ReturnType<Fns[K]>>;
type Handler<K extends CommandName> = (req: Req<K>, channel: ChannelLike<unknown> | undefined) => Res<K> | Promise<Res<K>>;
type Handlers = { [K in CommandName]?: Handler<K> };

export interface BackendConfig {
  locale: Locale;
  theme: Theme;
  /** Where in the patch script the scene starts (ms). */
  startAt: number;
  /** Deliver the later steps on timers (a page someone watches) or only through `advanceTo`. */
  live: boolean;
  /** `app_bootstrap.uiState`: the persisted UI keys the scene restores (lastView, splitSet, …). */
  uiState: Record<string, JsonValue>;
  launch: LaunchInfo;
  /** The WebView's build id, echoed by `app_bootstrap` (the showcase skips the §4.7 check). */
  buildId: string;
}

export interface ShowcaseBackendHandle {
  backend: ShowcaseBackend;
  /** Every command asked, in order. */
  asked: CommandName[];
  /** Deliver every script step at or before `ms` now; returns how many steps have run. */
  advanceTo: (ms: number) => number;
  /** The snapshot the patches reached. */
  current: () => StoreSnapshot;
  /** Stop the timers. */
  dispose: () => void;
}

/** What crosses the IPC boundary is JSON: a copy, `undefined` fields dropped. */
function overIpc<T>(value: T): T {
  return value === undefined ? value : (JSON.parse(JSON.stringify(value)) as T);
}

function ipcError(code: IpcError['code'], key: string, params: IpcError['params'], detail: string | null = null): IpcError {
  return { code, key, params, detail, retryable: false };
}

function refusal(name: string): IpcError {
  return ipcError('UNSUPPORTED', 'error.unsupported', { cap: 'showcase' }, `${SHOWCASE_MARKER}: ${name}`);
}

function notFound(entity: string): IpcError {
  return ipcError('NOT_FOUND', 'error.notFound', { entity });
}

function sourceOf(s: Session): ConvoSourceKind {
  if (s.control.kind === 'watching' || s.control.kind === 'notRunning') return 'transcript';
  if (s.control.mode === 'conversation') return 'stream';
  if (s.control.mode === 'turnBased') return 'turnMemory';
  return s.vendor === 'claude' ? 'transcript' : 'none';
}

/** A short terminal screen for PTY views: plain ASCII lines with SGR colour. */
function terminalScreen(s: Session): string {
  const lines = [
    `\u001b[2m~/code/${PROJECT_NAMES[0]}\u001b[0m $ ${s.vendor}`,
    '',
    `\u001b[1m> ${(s.title ?? 'session').replace(/[^\x20-\x7e]/g, '?')}\u001b[0m`,
    '',
    '  Reading src/client/retry.ts',
    '  Editing src/client/retry.ts (+6 -2)',
    '  Running npm test -- retry',
    '',
    '\u001b[32m  12 passed\u001b[0m',
    '',
    '> ',
  ];
  return lines.join('\r\n');
}

/** The screen of a running non-member pane (A30.1): a plain command and its output. */
function paneScreen(title: string | null): string {
  return [`\u001b[2m~/code/${PROJECT_NAMES[0]}\u001b[0m $ ${(title ?? 'pane').replace(/[^\x20-\x7e]/g, '?')}`, '', '  listening on 127.0.0.1:5173', ''].join('\r\n');
}

function toBase64(text: string): string {
  return globalThis.btoa(text);
}

/**
 * A `pty_snapshot` answer as the core sends it (`src-tauri/src/pty/ring.rs`,
 * A25.4): the 16-byte `GLDP` header — the four tag bytes, version 1, three reserved zero
 * bytes, `next_seq` as a little-endian u64 — then the ring's bytes. Spelled
 * out here rather than taken from the terminal's reader, so a drift between
 * the two shows up instead of agreeing with itself.
 */
export function ptySnapshotBytes(ring: Uint8Array): ArrayBuffer {
  const out = new Uint8Array(16 + ring.length);
  out.set([0x47, 0x4c, 0x44, 0x50], 0);
  out[4] = 1;
  new DataView(out.buffer).setBigUint64(8, BigInt(ring.length), true);
  out.set(ring, 16);
  return out.buffer;
}

/** Default settings (§9.6), with the scene's language and theme. */
export function defaultSettings(locale: Locale, theme: Theme): Settings {
  return {
    locale,
    theme,
    attention: { noProgressAfterSecs: 600, watchedFinishedTurns: false },
    notify: { watchedSessions: true, sound: false, showDetails: false },
    ide: { editor: 'vscode' },
    executable: { claudeBinary: null, codexBinary: null, cursorBinary: null, agyBinary: null, sidecarNodePath: null, pathsOverride: null, envPassthrough: [] },
  };
}

/** The backend of one scene. Its state (snapshot, settings, subscriptions) lives for the page. */
export function createShowcaseBackend(world: ShowcaseWorld, steps: readonly TimelineStep[], config: BackendConfig): ShowcaseBackendHandle {
  const start = snapshotAt(world, steps, config.startAt);
  let snapshot = start.snapshot;
  let applied = start.applied;
  let settings = defaultSettings(config.locale, config.theme);
  let storeChannel: ChannelLike<StorePatch> | null = null;
  const convoSubs = new Map<SubId, { uid: SessionUid; channel: ChannelLike<ConvoEnvelope>; seq: number }>();
  const answered = new Map<RequestId, DecisionKind>();
  const later = new Map<SessionUid, number>();
  const timers: ReturnType<typeof setTimeout>[] = [];
  const asked: CommandName[] = [];
  let subCounter = 0;
  let timersStarted = false;

  const sessionOf = (uid: SessionUid): Session => {
    const s = snapshot.sessions.find((x) => x.uid === uid);
    if (!s) throw notFound('session');
    return s;
  };

  /** A running non-member pane's terminal (A30.1 pane tiles). */
  const paneTileOf = (pty: string) => {
    for (const team of snapshot.teams) {
      const tile = team.paneTiles.find((p) => p.pty === pty);
      if (tile) return tile;
    }
    return null;
  };

  const publish = (ops: StoreOp[]) => {
    if (ops.length === 0) return;
    const patch: StorePatch = { fromRev: snapshot.rev, toRev: snapshot.rev + 1, ops };
    snapshot = foldSnapshot(snapshot, patch);
    storeChannel?.onmessage(overIpc(patch));
  };

  const advanceTo = (ms: number): number => {
    while (applied < steps.length) {
      const step = steps[applied];
      if (!step || step.at > ms) break;
      publish(step.make(snapshot, world.now + step.at));
      applied++;
    }
    return applied;
  };

  const startTimers = () => {
    if (timersStarted || !config.live) return;
    timersStarted = true;
    for (const step of steps.slice(applied)) {
      timers.push(setTimeout(() => advanceTo(step.at), Math.max(0, step.at - config.startAt)));
    }
  };

  const emitConvo = (subId: SubId, event: ConvoEnvelope['event']) => {
    const sub = convoSubs.get(subId);
    if (!sub) return;
    sub.seq += 1;
    sub.channel.onmessage(overIpc({ subId, seq: sub.seq, event }));
  };

  const handlers: Handlers = {
    // -- app, store, UI state ------------------------------------------------
    app_bootstrap: () => ({
      buildId: config.buildId,
      version: config.buildId,
      locale: config.locale,
      theme: config.theme,
      launch: config.launch,
      uiState: config.uiState,
      platform: { macosMajor: 15, arm64: true },
      features: { orchestrator: snapshot.integrations.orchestrator !== 'absent' },
      compat: snapshot.compat,
    }),
    app_mark: () => ({}),
    app_input_activity: () => ({}),
    app_view_state: () => ({}),
    ui_state_set: () => ({}),
    store_subscribe: (_req, channel) => {
      storeChannel = (channel as ChannelLike<StorePatch> | undefined) ?? null;
      startTimers();
      return snapshot;
    },
    settings_get: () => settings,
    settings_set: ({ patch }) => {
      settings = {
        ...settings,
        locale: patch.locale ?? settings.locale,
        theme: patch.theme ?? settings.theme,
        attention: {
          noProgressAfterSecs: patch.attention?.noProgressAfterSecs ?? settings.attention.noProgressAfterSecs,
          watchedFinishedTurns: patch.attention?.watchedFinishedTurns ?? settings.attention.watchedFinishedTurns,
        },
        notify: {
          watchedSessions: patch.notify?.watchedSessions ?? settings.notify.watchedSessions,
          sound: patch.notify?.sound ?? settings.notify.sound,
          showDetails: patch.notify?.showDetails ?? settings.notify.showDetails,
        },
        ide: { editor: patch.ide?.editor ?? settings.ide.editor },
      };
      return settings;
    },

    // -- sessions --------------------------------------------------------------
    newsession_options: ({ projectId }) => {
      const locations = snapshot.locations.filter((l) => l.projectId === projectId);
      const main = locations.find((l) => l.kind !== 'worktree') ?? null;
      return {
        vendors: [
          { vendor: 'claude', installed: true, version: '2.1.307' },
          { vendor: 'codex', installed: true, version: '0.46.0' },
          { vendor: 'cursor', installed: true, version: null },
          { vendor: 'agy', installed: false, version: null },
        ],
        accounts: snapshot.accounts.map((a) => ({ accountId: a.id, vendor: a.vendor, label: a.label, isDefault: a.isDefault, signIn: a.signIn })),
        models: [
          { id: 'sonnet', label: null, isDefault: true },
          { id: 'opus', label: null, isDefault: false },
          { id: 'haiku', label: null, isDefault: false },
        ],
        modes: ['conversation', 'terminal'],
        permissionModes: ['default', 'acceptEdits', 'plan'],
        locations: [
          { location: { kind: 'here', locationId: main?.id ?? null }, branch: main?.branch ?? null, worktreeName: null },
          ...locations.filter((l) => l.kind === 'worktree').map((l) => ({ location: { kind: 'existingWorktree' as const, locationId: l.id }, branch: l.branch, worktreeName: l.worktreeName })),
          { location: { kind: 'newWorktree', slug: '', base: null }, branch: null, worktreeName: null },
        ],
        teamAllowed: true,
        teamMode: 'panes',
        defaults: { vendor: 'claude', accountId: snapshot.accounts.find((a) => a.vendor === 'claude' && a.isDefault)?.id ?? null, model: 'sonnet', controlMode: 'conversation', permissionMode: 'default', location: { kind: 'here', locationId: main?.id ?? null } },
        notices: [],
      };
    },
    session_technical: ({ uid }) => {
      const s = sessionOf(uid);
      return {
        pid: 41000 + Number.parseInt(uid.slice(-4), 16),
        startedAt: s.startedAt ?? world.now,
        commandRedacted: [`~/.local/bin/${s.vendor}`, '--session-id', '***', '--permission-mode', 'default', '<prompt>'],
        parentChain: ['launchd', 'Gilde Code'],
      };
    },
    session_mark_seen: ({ uid }) => {
      const s = sessionOf(uid);
      if (s.activity.state.kind === 'newReply') {
        publish(sessionChange(snapshot, uid, (x) => ({ ...x, activity: { ...x.activity, state: { kind: 'ready' } } }), Date.now(), later));
      }
      return {};
    },
    session_dismiss: ({ uid }) => {
      const s = sessionOf(uid);
      if (s.control.kind === 'notRunning') publish([{ kind: 'remove', entityKind: 'session', id: uid }]);
      return {};
    },
    queue_later: ({ uid }) => {
      sessionOf(uid);
      const until = Date.now() + 30 * 60_000;
      later.set(uid, until);
      publish(sessionChange(snapshot, uid, (x) => x, Date.now(), later).filter((op) => op.kind === 'queue'));
      return { until };
    },

    // -- turns and interactions ----------------------------------------------------
    interaction_answer: ({ uid, requestId, decision }) => {
      const s = sessionOf(uid);
      const card = s.pending.find((c) => c.requestId === requestId);
      if (!card) {
        const previous = answered.get(requestId);
        if (previous) return { outcome: 'alreadyAnswered', previous };
        throw notFound('request');
      }
      answered.set(requestId, decision.kind);
      publish(sessionChange(snapshot, uid, (x) => {
        const pending = x.pending.filter((c) => c.requestId !== requestId);
        const state = pending.length > 0 ? x.activity.state : decision.kind === 'denyAndStop' ? { kind: 'ready' as const } : { kind: 'working' as const };
        return { ...x, pending, activity: { ...x.activity, state, since: Date.now() } };
      }, Date.now(), later));
      return { outcome: 'answered', previous: null };
    },
    interaction_detail: ({ uid, requestId }) => {
      const card = sessionOf(uid).pending.find((c) => c.requestId === requestId);
      if (!card) throw notFound('request');
      return { inputPretty: JSON.stringify({ command: card.tool?.object ?? '' }, null, 2), toolName: card.tool?.tool ?? 'Tool', decisionReason: null };
    },

    // -- content channels -------------------------------------------------------------
    convo_subscribe: ({ uid, tailBlocks }, channel) => {
      // World first, then the history index (A29.2(b)).
      const held = snapshot.sessions.find((x) => x.uid === uid) ?? null;
      const row = held ? null : (world.history.find((r) => r.uid === uid) ?? null);
      if (!held && !row) throw notFound('session');
      if (row?.titleOnly) throw ipcError('UNSUPPORTED', 'error.unsupported', { cap: 'noReadableConversation' });
      subCounter += 1;
      const subId = `sub-${subCounter}` as SubId;
      convoSubs.set(subId, { uid, channel: channel as ChannelLike<ConvoEnvelope>, seq: 0 });
      const blocks = row ? historyConversationOf(row, world.now) : [...(world.convo.get(uid) ?? [])];
      const tail = tailBlocks && tailBlocks > 0 ? blocks.slice(-tailBlocks) : blocks;
      // The first event follows the answer, as from the core.
      setTimeout(() => emitConvo(subId, { kind: 'reset', blocks: tail, hasOlder: tail.length < blocks.length, reason: 'initial' }), 0);
      return { subId, source: held ? sourceOf(held) : 'transcript', note: null };
    },
    convo_unsubscribe: ({ subId }) => {
      convoSubs.delete(subId);
      return {};
    },
    convo_load_older: ({ subId, beforeBlockId, count }) => {
      const sub = convoSubs.get(subId);
      if (!sub) throw notFound('subscription');
      const blocks = world.convo.get(sub.uid) ?? [];
      const at = blocks.findIndex((b) => b.id === beforeBlockId);
      const older = at > 0 ? blocks.slice(Math.max(0, at - count), at) : [];
      setTimeout(() => emitConvo(subId, { kind: 'older', blocks: [...older], hasOlder: at - older.length > 0 }), 0);
      return {};
    },
    pty_attach: ({ ptyId }, channel) => {
      const s = snapshot.sessions.find((x) => x.control.kind === 'inGilde' && x.control.pty === ptyId);
      const pane = s ? null : paneTileOf(ptyId);
      if (!s && !pane) throw notFound('pty');
      const screen = s ? terminalScreen(s) : paneScreen(pane?.title ?? null);
      setTimeout(() => (channel as ChannelLike<PtyChunk> | undefined)?.onmessage({ kind: 'data', offset: 0, b64: toBase64(screen) }), 0);
      return { offset: screen.length, cols: 100, rows: 30, title: s ? s.title : (pane?.title ?? null) };
    },
    pty_detach: () => ({}),
    pty_snapshot: ({ ptyId }) => {
      const s = snapshot.sessions.find((x) => x.control.kind === 'inGilde' && x.control.pty === ptyId);
      const pane = s ? null : paneTileOf(ptyId);
      if (!s && !pane) throw notFound('pty');
      return ptySnapshotBytes(new TextEncoder().encode(s ? terminalScreen(s) : paneScreen(pane?.title ?? null)));
    },

    // -- changes -------------------------------------------------------------------
    changes_get: ({ uid }) => {
      const s = sessionOf(uid);
      if (!s.locationId) throw notFound('location');
      return {
        location: s.locationId,
        base: { branch: 'main', commitShort: '3f9c2ab', newerOnMain: 2 },
        files: [
          { pathRel: 'src/client/retry.ts', change: 'modified', added: 6, removed: 2, parts: ['resetBackoff'], madeBy: { kind: 'thisSession' } },
          { pathRel: 'src/client/retry.test.ts', change: 'added', added: 24, removed: 0, parts: [], madeBy: { kind: 'thisSession' } },
        ],
        commits: [],
        sharedFolder: null,
        actions: { commit: { kind: 'enabled' }, merge: { kind: 'enabled' }, discard: { kind: 'enabled' } },
      };
    },

    // -- teams -----------------------------------------------------------------------
    team_messages_page: ({ teamKey, before, count }) => {
      const all = snapshot.messages.filter((m) => m.teamKey === teamKey && m.at < before).sort((a, b) => b.at - a.at);
      return { messages: all.slice(0, count), hasMore: all.length > count };
    },
    team_message_body: ({ messageKey }) => {
      const text = world.messageBodies.get(messageKey) ?? (snapshot.messages.some((m) => m.key === messageKey) ? 'The refresh path passes now; moving to key rotation.' : null);
      if (text === null) throw notFound('message');
      return { text, summary: null, source: 'recipientTranscript' };
    },
    team_task_title: ({ taskKey }) => ({ title: world.taskTitles.get(taskKey) ?? null }),

    // -- accounts, quota, health ---------------------------------------------------------
    quota_refresh: ({ accountId }) => {
      const a = snapshot.accounts.find((x) => x.id === accountId);
      if (!a) throw notFound('account');
      return { quota: a.quota ?? { windows: [], fetchedAt: Date.now(), stale: false, unavailable: 'notReported' } };
    },
    ide_editors: () => ({ found: [{ editor: 'vscode', bundleId: 'com.microsoft.VSCode', appPath: '/Applications/Visual Studio Code.app' }], chosen: settings.ide.editor }),
    health_processes: () => ({ processes: snapshot.leftovers }),
    health_process_detail: ({ processKey }) => {
      const p = snapshot.leftovers.find((x) => x.key === processKey);
      if (!p) throw notFound('process');
      return { commandRedacted: ['~/.local/bin/claude', '--agent-id', 'db-agent@auth-split', '--settings', '***'], parentChain: ['launchd'], startedAt: p.startSec * 1000 };
    },

    // -- history, quit ------------------------------------------------------------------
    history_query: ({ queryId, text, filters, cursor, limit }) => {
      const needle = text.trim().toLowerCase();
      const rows = world.history.filter((r) =>
        (needle === '' || (r.title ?? '').toLowerCase().includes(needle)) &&
        (filters.projectId === null || r.projectId === filters.projectId) &&
        (filters.vendor === null || r.vendor === filters.vendor) &&
        (filters.from === null || (r.startedAt ?? 0) >= filters.from) &&
        (filters.to === null || (r.startedAt ?? 0) <= filters.to));
      const offset = cursor ? Number.parseInt(cursor, 10) || 0 : 0;
      const page = rows.slice(offset, offset + Math.max(1, limit));
      const next = offset + page.length < rows.length ? (String(offset + page.length) as Res<'history_query'>['nextCursor']) : null;
      return { queryId, rows: page, nextCursor: next, coverage: { titlesLoaded: world.history.length, total: world.history.length }, superseded: false };
    },
    history_row: ({ uid }) => {
      const row = world.history.find((r) => r.uid === uid);
      if (!row) throw notFound('session');
      return row;
    },
    undo_cancel: ({ undoToken }) => {
      const recovery = snapshot.boot.recovery;
      if (!recovery || recovery.undoToken !== undoToken) throw notFound('token');
      publish([{ kind: 'boot', status: { ...snapshot.boot, recovery: null } }]);
      return { cancelled: true };
    },
    history_preview: ({ uid }) => {
      const row = world.history.find((r) => r.uid === uid);
      if (!row) throw notFound('session');
      if (row.titleOnly) throw ipcError('UNSUPPORTED', 'error.unsupported', { cap: 'titleOnly' });
      return { firstPrompt: row.title, lastReply: 'Done. The change is merged and the tests pass.', turns: 6, startedAt: row.startedAt, endedAt: row.endedAt, continuedIn: null };
    },
    app_quit_preview: () => {
      const live = snapshot.sessions.filter((s) => s.control.kind === 'inGilde');
      return {
        sessions: live.map((s) => ({ uid: s.uid, projectId: s.projectId, waiting: s.pending.length > 0 ? (s.pending[0]?.kind === 'question' ? 'answer' : 'approval') : null, teamLead: s.team?.role === 'lead' })),
        elsewhere: snapshot.sessions.filter((s) => s.control.kind === 'watching').length,
        teams: snapshot.teams.filter((t) => t.hosted).map((t) => ({ teamKey: t.key, members: snapshot.members.filter((m) => m.teamKey === t.key).length })),
        confirmToken: 'showcase-quit' as Res<'app_quit_preview'>['confirmToken'],
      };
    },
  };

  const backend: ShowcaseBackend = async (name, req, channel) => {
    asked.push(name);
    const handler = handlers[name] as ((r: unknown, c: ChannelLike<unknown> | undefined) => unknown) | undefined;
    if (handler === undefined) throw refusal(name);
    const result = await handler(req, channel);
    return result instanceof ArrayBuffer ? result : overIpc(result);
  };

  return {
    backend,
    asked,
    advanceTo,
    current: () => snapshot,
    dispose: () => {
      for (const t of timers) clearTimeout(t);
      timers.length = 0;
    },
  };
}
