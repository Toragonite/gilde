/**
 * One display row of the conversation (ux-ia §3.4). Agent text goes through
 * the AST markdown renderer only (contract §9.1); every fixed line is i18n;
 * paths open in the IDE (`⌘`-click in prose and tool rows, Open/Diff on edits)
 * — Gilde has no editor or diff viewer.
 */

import { memo, useRef, useState } from 'react';
import type { MouseEvent as ReactMouseEvent, ReactNode } from 'react';

import { StateGlyph } from '../components/StateGlyph';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { formatDiffStat } from '../lib/format';
import type { InteractionOutcome } from '../lib/gen/InteractionOutcome';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { SystemNote } from '../lib/gen/SystemNote';
import type { ToolVerb } from '../lib/gen/ToolVerb';
import { Markdown } from '../markdown';
import { interactionDisplay } from '../store/convo';
import type { Row, ToolBlock } from './rows';
import { editTotals, firstLine, verbCounts } from './rows';

export interface RowContext {
  /** The vendor badge (`claude`), for speaker lines and `{agent}`. */
  agent: string;
  pending: readonly PendingInteraction[];
  /** Open a path in the IDE (relative to the session's location). */
  openPath: (path: string, line?: number | null) => void;
  /** Open a path's diff in the IDE. */
  openDiff: (path: string) => void;
  timeOf: (at: number) => string;
}

const VERB_KEY: Readonly<Record<ToolVerb, MessageKey>> = {
  read: 'convo.verb.read',
  edit: 'convo.verb.edit',
  create: 'convo.verb.create',
  run: 'convo.verb.run',
  search: 'convo.verb.search',
  reach: 'convo.verb.reach',
  delegate: 'convo.verb.delegate',
  message: 'convo.verb.message',
  task: 'convo.verb.task',
  ask: 'convo.verb.ask',
  use: 'convo.verb.use',
};

const COUNT_KEY: Readonly<Record<ToolVerb, MessageKey>> = {
  read: 'convo.tool.read',
  edit: 'convo.tool.edit',
  create: 'convo.tool.create',
  run: 'convo.tool.run',
  search: 'convo.tool.search',
  reach: 'convo.tool.reach',
  delegate: 'convo.tool.delegate',
  message: 'convo.tool.message',
  task: 'convo.tool.task',
  ask: 'convo.tool.ask',
  use: 'convo.tool.use',
};

const OUTCOME_KEY: Readonly<Record<Exclude<InteractionOutcome, 'abandoned'>, MessageKey>> = {
  pending: 'convo.outcome.pending',
  allowed: 'convo.outcome.allowed',
  allowedForSession: 'convo.outcome.allowedForSession',
  denied: 'convo.outcome.denied',
  answered: 'convo.outcome.answered',
  withdrawn: 'approval.answeredElsewhere',
  sessionEnded: 'card.sessionEnded',
};

const PATH_VERBS: ReadonlySet<ToolVerb> = new Set<ToolVerb>(['read', 'edit', 'create']);

function capitalize(s: string): string {
  return s.length > 0 ? s[0]!.toUpperCase() + s.slice(1) : s;
}

export const BlockRow = memo(function BlockRow({ row, ctx }: { row: Row; ctx: RowContext }) {
  switch (row.kind) {
    case 'user':
      return <UserRow row={row} ctx={ctx} />;
    case 'assistant':
      return <AssistantRow row={row} ctx={ctx} />;
    case 'thinking':
      return <ThinkingRow row={row} />;
    case 'tools':
      return <ToolsRow row={row} ctx={ctx} />;
    case 'interaction':
      return <InteractionRow row={row} ctx={ctx} />;
    case 'system':
      return <SystemRow row={row} />;
    case 'unknown':
      return <UnknownRow row={row} />;
  }
});

function UserRow({ row, ctx }: { row: Extract<Row, { kind: 'user' }>; ctx: RowContext }) {
  const { t } = useI18n();
  const k = row.block.kind;
  const who =
    k.origin === 'human'
      ? t('convo.you')
      : k.origin === 'peer'
        ? t('convo.origin.peer')
        : k.origin === 'teammate'
          ? t('convo.origin.teammate')
          : k.origin === 'taskNotification'
            ? t('convo.origin.task')
            : t('convo.origin.gilde');
  return (
    <div className="g-msg g-msg-user" data-row="user">
      <div className="g-msg-who">
        <b>{who}</b>
        <time className="mono">{ctx.timeOf(row.block.at)}</time>
        {k.delivery === 'queued' && <span className="g-msg-chip">{t('convo.delivery.queued')}</span>}
        {k.delivery === 'cancelled' && (
          <span className="g-msg-chip" data-delivery="cancelled">
            {t('convo.delivery.cancelled')}
          </span>
        )}
      </div>
      <div className="g-msg-body g-msg-usertext">{k.text}</div>
    </div>
  );
}

function AssistantRow({ row, ctx }: { row: Extract<Row, { kind: 'assistant' }>; ctx: RowContext }) {
  const k = row.block.kind;
  // A path in prose opens the IDE on ⌘-click (ux-ia §3.4); a plain mouse click
  // does nothing, so reading never launches an editor. Keyboard activation
  // (no pointer) opens it, so the path stays reachable without a mouse.
  const click = useRef({ pointer: false, meta: false });
  return (
    <div className="g-msg" data-row="assistant" data-streaming={k.streaming ? 'true' : undefined}>
      {row.speaker && (
        <div className="g-msg-who">
          <b>{k.agent ? `${ctx.agent} · ${k.agent}` : ctx.agent}</b>
          <time className="mono">{ctx.timeOf(row.block.at)}</time>
        </div>
      )}
      <div
        className="g-msg-body"
        onClickCapture={(event: ReactMouseEvent) => {
          click.current = { pointer: event.detail > 0, meta: event.metaKey };
        }}
      >
        <Markdown
          text={k.markdown}
          onOpenPath={(p) => {
            const c = click.current;
            click.current = { pointer: false, meta: false };
            if (!c.pointer || c.meta) ctx.openPath(p);
          }}
        />
      </div>
    </div>
  );
}

function ThinkingRow({ row }: { row: Extract<Row, { kind: 'thinking' }> }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const k = row.block.kind;
  const label = k.seconds !== null ? t('convo.thought', { seconds: k.seconds }) : t('convo.thinking');
  if (!k.text) {
    return (
      <div className="g-tool g-tool-muted" data-row="thinking">
        {label}
      </div>
    );
  }
  return (
    <div data-row="thinking">
      <button type="button" className="g-tool" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
        <span className="g-chev" aria-hidden="true">
          ▸
        </span>
        <span className="g-tool-muted">{label}</span>
      </button>
      {open && <div className="g-tool-items g-thinking">{k.text}</div>}
    </div>
  );
}

function statusText(b: ToolBlock, t: ReturnType<typeof useI18n>['t']): { text: string; tone: 'ok' | 'warn' | 'muted' } | null {
  const k = b.kind;
  if (k.incomplete) return { text: t('convo.status.incomplete'), tone: 'warn' };
  switch (k.status) {
    case 'running':
      return { text: t('convo.status.running'), tone: 'muted' };
    case 'error': {
      const line = firstLine(k.resultPreview);
      return { text: line !== '' ? `${t('convo.status.error')} · ${line}` : t('convo.status.error'), tone: 'warn' };
    }
    case 'denied':
      return { text: t('convo.status.denied'), tone: 'warn' };
    case 'ok': {
      const line = firstLine(k.resultPreview);
      return line !== '' ? { text: line, tone: 'muted' } : null;
    }
    default:
      return null;
  }
}

function ToolLine({ b, ctx, showTime }: { b: ToolBlock; ctx: RowContext; showTime: boolean }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const k = b.kind;
  const status = statusText(b, t);
  const object = k.summary.object ?? k.summary.tool;
  const isPath = PATH_VERBS.has(k.summary.verb) && k.summary.object !== null;
  const detail = k.inputPreview !== '' || (k.resultPreview ?? '') !== '';
  return (
    <div className="g-toolwrap">
      <button
        type="button"
        className="g-tool"
        aria-expanded={detail ? open : undefined}
        onClick={(event) => {
          if (event.metaKey && isPath) {
            ctx.openPath(k.summary.object as string);
            return;
          }
          if (detail) setOpen((o) => !o);
        }}
      >
        <span className="g-chev" aria-hidden="true">
          {detail ? '▸' : ' '}
        </span>
        <span className="g-tool-verb">{t(VERB_KEY[k.summary.verb])}</span>
        <span className="g-tool-obj mono">{object}</span>
        {status && <span className={`g-tool-res g-tool-${status.tone}`}>· {status.text}</span>}
        {showTime && <time className="mono">{ctx.timeOf(b.at)}</time>}
      </button>
      {open && (
        <div className="g-tool-items">
          {k.inputPreview !== '' && <pre className="mono">{k.inputPreview}</pre>}
          {k.resultPreview ? <pre className="mono">{k.resultPreview}</pre> : null}
        </div>
      )}
    </div>
  );
}

function EditFiles({ blocks, ctx }: { blocks: ToolBlock[]; ctx: RowContext }) {
  const { t, locale } = useI18n();
  const totals = editTotals(blocks);
  return (
    <div className="g-edit-files">
      {totals.files.map((f) => (
        <div key={f.path} className="g-edit-file">
          <span className="g-edit-path mono">{f.path}</span>
          <span className="g-edit-stat mono">{formatDiffStat(f.added, f.removed, locale)}</span>
          {f.change === 'added' && <span className="g-tool-muted">{t('convo.file.new')}</span>}
          {f.change === 'deleted' && <span className="g-tool-muted">{t('convo.file.deleted')}</span>}
          <span className="g-edit-actions">
            {f.change !== 'deleted' && (
              <button type="button" className="g-cbtn g-cbtn-ghost" aria-keyshortcuts="Meta+O" onClick={() => ctx.openPath(f.path)}>
                {t('convo.open')}
              </button>
            )}
            <button type="button" className="g-cbtn g-cbtn-ghost" aria-keyshortcuts="Meta+Shift+O" onClick={() => ctx.openDiff(f.path)}>
              {t('convo.diff')}
            </button>
          </span>
        </div>
      ))}
    </div>
  );
}

function ToolsRow({ row, ctx }: { row: Extract<Row, { kind: 'tools' }>; ctx: RowContext }) {
  const { t, locale } = useI18n();
  const edits = row.category === 'edit';
  const [open, setOpen] = useState(edits);
  const blocks = row.blocks;
  const last = blocks[blocks.length - 1] as ToolBlock;
  const first = blocks[0] as ToolBlock;
  if (blocks.length === 1 && !edits) {
    return (
      <div data-row="tools">
        <ToolLine b={first} ctx={ctx} showTime />
      </div>
    );
  }
  let summary: ReactNode;
  if (edits) {
    const totals = editTotals(blocks);
    summary = (
      <>
        <span className="g-tool-verb">{capitalize(t('convo.tool.edit', { n: totals.files.length }))}</span>
        <span className="g-tool-res mono">{formatDiffStat(totals.added, totals.removed, locale)}</span>
      </>
    );
  } else {
    summary = <span className="g-tool-verb">{capitalize(verbCounts(blocks).map((v) => t(COUNT_KEY[v.verb], { n: v.n })).join(' · '))}</span>;
  }
  const at = first.at === last.at ? ctx.timeOf(first.at) : `${ctx.timeOf(first.at)}–${ctx.timeOf(last.at)}`;
  const running = blocks.some((b) => b.kind.status === 'running');
  return (
    <div data-row="tools" data-category={row.category}>
      <button type="button" className="g-tool" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
        <span className="g-chev" aria-hidden="true">
          {open ? '▾' : '▸'}
        </span>
        {summary}
        {running && <span className="g-tool-res g-tool-muted">· {t('convo.status.running')}</span>}
        <time className="mono">{at}</time>
      </button>
      {open &&
        (edits ? (
          <EditFiles blocks={blocks} ctx={ctx} />
        ) : (
          <div className="g-tool-items">
            {blocks.map((b) => (
              <ToolLine key={b.id} b={b} ctx={ctx} showTime={false} />
            ))}
          </div>
        ))}
    </div>
  );
}

function InteractionRow({ row, ctx }: { row: Extract<Row, { kind: 'interaction' }>; ctx: RowContext }) {
  const { t } = useI18n();
  const k = row.block.kind;
  const shown = interactionDisplay(k.outcome, k.requestId, ctx.pending);
  const approval = k.interactionKind === 'toolApproval' || k.interactionKind === 'screenPrompt' || k.interactionKind === 'watchedApproval';
  let text: string;
  // While its card is up, the row names the wait (the card itself is in the dock).
  if (shown === 'card') text = approval ? t('state.needsApproval') : t('state.needsAnswer');
  else if (shown === 'abandoned') text = t('card.abandonedOnResume', { agent: ctx.agent });
  else text = t(OUTCOME_KEY[shown]);
  const muted = shown === 'withdrawn' || shown === 'sessionEnded' || shown === 'abandoned';
  return (
    <div className={`g-tool${muted ? ' g-tool-muted' : ''}`} data-row="interaction" data-outcome={shown}>
      <StateGlyph state={approval ? 'needsApproval' : 'needsAnswer'} />
      <span>{text}</span>
      <time className="mono">{ctx.timeOf(row.block.at)}</time>
    </div>
  );
}

function systemText(note: SystemNote, t: ReturnType<typeof useI18n>['t']): string {
  switch (note.kind) {
    case 'compactBoundary':
      return t('convo.sys.compacted');
    case 'apiRetry':
      return t('convo.sys.apiRetry', { attempt: note.attempt });
    case 'permissionDenied':
      return t('convo.sys.permissionDenied', { tool: note.tool });
    case 'interrupted':
      return t('turn.interrupted');
    case 'backgroundTask':
      return t(`convo.sys.task.${note.status}` as MessageKey);
    case 'teamEvent':
      return note.member ? `${t('convo.sys.teamEvent')} · ${note.member}` : t('convo.sys.teamEvent');
    case 'error':
      switch (note.sub.kind) {
        case 'usageLimit':
          return t('state.error.usageLimit');
        case 'signInNeeded':
          return t('state.error.signIn');
        case 'didntStart':
          return t('state.error.didntStart');
        default:
          return t('state.error.agent');
      }
    case 'rowTooLarge':
      return t('convo.sys.rowTooLarge');
    case 'newConversation':
      return t('turn.newConversation');
  }
}

function SystemRow({ row }: { row: Extract<Row, { kind: 'system' }> }) {
  const { t } = useI18n();
  const k = row.block.kind;
  const error = k.note.kind === 'error';
  return (
    <div className="g-sysline" data-row="system" data-note={k.note.kind}>
      {error && <StateGlyph state="error" />}
      <span>{systemText(k.note, t)}</span>
      {k.text ? <span className="g-sysline-text mono">{k.text}</span> : null}
    </div>
  );
}

function UnknownRow({ row }: { row: Extract<Row, { kind: 'unknown' }> }) {
  const { t } = useI18n();
  return (
    <div className="g-sysline" data-row="unknown">
      {t('convo.unknownBlock', { type: row.block.kind.typeName })}
    </div>
  );
}
