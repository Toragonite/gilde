/**
 * Home's one focus card [M15]: the most urgent item that needs the user, with
 * the action that resolves it right there.
 *
 * Input safety (contract §8.6), because the card carries answer buttons while
 * the queue re-ranks live:
 *  - it holds its item while the pointer or focus is on it, even if the
 *    ranking changes (a new arrival waits in Next);
 *  - an item handled elsewhere leaves a "처리되었습니다" ghost for 1.5 s instead
 *    of pulling the next item under the pointer; the user's own answer leaves
 *    none (T13's `useHeldFocus`);
 *  - for 700 ms after the item changes — and after window activation or a
 *    notification click [F-30] — answering keys and button clicks are ignored,
 *    visibly (T13's `useAnswerGuard`); key repeat never answers.
 *
 * The card's body follows the item: anything waiting on a card (approval,
 * question, plan, screen prompt, a watched request) is T15's card itself
 * (`FocusCard` from `src/session`, A23.5), routed to `answer_in` [F-15];
 * every other state says what happened in one line and offers the core's
 * primary action (`AttentionItem.primary`).
 */

import type { FocusEvent as ReactFocusEvent } from 'react';

import { GuardNote } from '../components/GuardNote';
import { StateGlyph } from '../components/StateGlyph';
import type { AnswerGuard } from '../hooks/useAnswerGuard';
import type { Ghost } from '../hooks/useHold';
import { useI18n } from '../i18n/useI18n';
import type { I18n } from '../i18n/useI18n';
import { formatBytes, formatDuration } from '../lib/format';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { Session } from '../lib/gen/Session';
import { vendorBadge } from '../lib/vendor';
import type { WorldState } from '../store/worldSlice';
import { sessionOf } from '../store/worldSlice';
import type { CardTarget } from '../shell/actions';
import { cardOf, laterItem, openItem, otherAccount, primaryLabelKey, runPrimary } from '../shell/actions';
import { itemAge } from '../shell/useAges';
import { itemGlyph, itemName, itemStateWord, ownerApp, ownerWord, waitAge } from '../shell/words';
import { FocusCard as SessionFocusCard } from '../session';
import { accountName } from './model';

/** What the item is about, in one sentence (ux-ia §4.3 evidence). */
export function whatLine(i18n: Pick<I18n, 't' | 'td' | 'locale'>, world: WorldState, item: AttentionItem): string {
  const { t, td, locale } = i18n;
  if (item.subject.kind === 'leftovers') {
    return t('home.leftoverLine', { n: item.subject.count, size: formatBytes(item.subject.bytes, locale) });
  }
  const session = sessionOf(world, item.subject.uid);
  if (!session) return itemStateWord(t, world, item);
  const agent = vendorBadge(session.vendor);
  const state = session.activity.state;
  const pending = cardOf(world, item)?.interaction ?? session.pending[0] ?? null;
  switch (state.kind) {
    case 'needsApproval': {
      if (session.control.kind === 'watching') {
        const owner = ownerWord(t, session.control) ?? '';
        if (pending?.tool?.verb === 'edit' && pending.tool.object) return t('observed.waitingEdit', { file: pending.tool.object });
        return pending ? `${t('observed.waitingApproval', { owner })} · ${pending.title}` : t('observed.waitingApproval', { owner });
      }
      return pending?.title ?? t('state.needsApproval');
    }
    case 'needsAnswer': {
      const question = pending?.questions[0]?.text ?? null;
      if (pending?.kind === 'planApproval') return t('home.planLine', { agent });
      if (pending?.kind === 'setupPrompt') return t('home.setupLine', { agent });
      return question ? `“${question}”` : t('home.questionLine', { agent });
    }
    case 'error': {
      const sub = state.sub;
      const account = sub.kind === 'usageLimit' && sub.accountId ? world.accounts.get(sub.accountId) : session.accountId ? world.accounts.get(session.accountId) : undefined;
      const accountLabel = account ? accountName(account) : t('account.unknown');
      switch (sub.kind) {
        case 'usageLimit':
          return sub.resetsAt !== null
            ? t('error.usageLimit', { account: accountLabel, time: new Intl.DateTimeFormat(locale, { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }).format(new Date(sub.resetsAt)) })
            : t('home.usageLimitNoReset', { account: accountLabel });
        case 'signInNeeded':
          return t('home.signInLine', { account: accountLabel });
        case 'didntStart':
          return t('error.didntStart', { agent });
        case 'agentError':
          return t('home.agentErrorLine', { agent });
      }
      return t('state.error');
    }
    case 'crashed':
      if (state.exit.signal !== null) return t('home.crashedSignal', { signal: state.exit.signal });
      if (state.exit.code !== null) return t('home.crashedCode', { code: state.exit.code });
      return t('state.crashed');
    case 'noProgress':
      if (session.activity.evidence) return td(session.activity.evidence.key, session.activity.evidence.params);
      return t('home.noProgressFor', { time: formatDuration(state.forSecs * 1000, locale) });
    case 'newReply':
      return t('home.newReplyLine', { agent });
    default:
      return itemStateWord(t, world, item);
  }
}

/** The label of the core's primary action for an item, with its parameters. */
export function primaryLabel(t: I18n['t'], world: WorldState, item: AttentionItem, session: Session | null): string {
  const key = primaryLabelKey(item.primary);
  if (key === 'action.goToOwner') return t(key, { owner: (session && ownerApp(t, session.control)) ?? t('origin.unknown') });
  if (key === 'action.continueOn') {
    const other = session ? otherAccount(world, session) : null;
    const account = other ? world.accounts.get(other) : undefined;
    return t(key, { account: account ? accountName(account) : t('account.unknown') });
  }
  return t(key);
}

interface FocusCardProps {
  world: WorldState;
  item: AttentionItem | null;
  ghost: Ghost | null;
  /** The host's 700 ms guard for this item (HomeScreen owns it, so ⌘⏎ shares it). */
  guard: AnswerGuard;
  engagement: {
    onMouseEnter: () => void;
    onMouseLeave: () => void;
    onFocus: () => void;
    onBlur: (event: ReactFocusEvent<HTMLElement>) => void;
  };
  markOwnAction: (key: string) => void;
  /** Move focus into the card (Home's arrival only; never for a newly arrived item). */
  autoFocus: boolean;
}

function tone(item: AttentionItem): string {
  if (item.band === 3 || item.band === 6) return 'error';
  if (item.band === 5) return 'ok';
  return 'warn';
}

export function FocusCard({ world, item, ghost, guard, engagement, markOwnAction, autoFocus }: FocusCardProps) {
  const i18n = useI18n();
  const { t, locale } = i18n;
  const target: CardTarget | null = item ? cardOf(world, item) : null;

  if (ghost || !item) {
    const name = ghost?.name ?? '';
    return (
      <section className="fh" aria-label={`${name}, ${t('home.handled')}`} {...engagement} data-ghost="true">
        <div className="fh-card fh-ghost" tabIndex={-1} data-region-focus="">
          <span className="fh-ghost-name ell">{name}</span>
          <span className="muted">{t('home.handled')}</span>
        </div>
      </section>
    );
  }

  const session = item.subject.kind === 'session' ? sessionOf(world, item.subject.uid) : null;
  const name = itemName(t, world, item, locale);
  const word = itemStateWord(t, world, item);
  const age = waitAge(t, itemAge(item, world.queue.rev), locale);
  const project = session?.projectId ? (world.projects.get(session.projectId)?.name ?? null) : null;
  const owner = session ? ownerWord(t, session.control) : null;
  const meta = session
    ? [vendorBadge(session.vendor), owner ? `${t('ownership.watching')} · ${owner}` : null, item.subject.kind === 'session' && item.subject.answerIn ? t('team.answeredAtLead') : null]
        .filter(Boolean)
        .join(' · ')
    : '';
  const own = () => markOwnAction(item.key);
  const label = t('home.focusAria', { name: `${name}, ${word}` });

  const pathLine = (
    <div className="fh-path">
      {project ? <span className="fh-project ell">{project} ›</span> : null}
      {item.subject.kind === 'session' ? (
        <button type="button" className="fh-name fh-open ell" aria-keyshortcuts="Enter" onClick={() => openItem(item)}>
          {name}
        </button>
      ) : (
        <span className="fh-name ell">{name}</span>
      )}
      {meta ? <span className="fh-meta ell">{meta}</span> : null}
      <span className="grow" />
      {item.subject.kind === 'session' ? (
        <button
          type="button"
          className="btn ghost fh-later"
          aria-keyshortcuts="L"
          {...guard.props}
          onClick={guard.guard(() => {
            own();
            laterItem(item);
          })}
        >
          {t('action.later')}
          <span className="kbd" aria-hidden="true">
            L
          </span>
        </button>
      ) : null}
    </div>
  );

  let body;
  if (target) {
    // T15's card (A23.5): it answers, guards, routes to answer_in and offers
    // Go to {owner} for a watched request by itself. Any press inside it is
    // the user's own action, so the item leaves no "handled" ghost.
    const list = sessionOf(world, target.answerUid)?.pending ?? target.session.pending;
    const at = list.findIndex((p) => p.requestId === target.interaction.requestId);
    body = (
      <div
        className="fh-cardhost"
        onPointerDownCapture={own}
        onKeyDownCapture={(event) => {
          if (event.metaKey || /^(Digit|Numpad|Enter|Space)/.test(event.code)) own();
        }}
      >
        <SessionFocusCard
          session={target.session}
          interaction={target.interaction}
          answerUid={target.answerUid}
          autoFocus={autoFocus}
          index={Math.max(0, at)}
          count={Math.max(1, list.length)}
          onAnswered={own}
        />
      </div>
    );
  } else {
    const primary = item.primary === 'open' || item.primary === 'message' ? null : primaryLabel(t, world, item, session);
    body = (
      <div className="fh-card" data-tone={tone(item)} tabIndex={-1}>
        <div className="fh-state">
          <StateGlyph state={itemGlyph(world, item)} />
          <span className="fh-word" data-tone={tone(item)}>
            {word}
          </span>
          <span className="grow" />
          <span className="fh-age mono">{age}</span>
        </div>
        {session && session.control.kind === 'watching' && item.reason.kind === 'needsApproval' ? (
          <p className="fh-text">{t('observed.banner', { owner: owner ?? '' })}</p>
        ) : null}
        <p className="fh-text fh-what">{whatLine(i18n, world, item)}</p>
        {primary ? (
          <div className="fh-actions">
            <button
              type="button"
              className="btn primary"
              aria-keyshortcuts={item.primary === 'goToOwner' ? 'Meta+Enter' : undefined}
              {...guard.props}
              onClick={guard.guard(() => {
                own();
                runPrimary(item);
              })}
            >
              {primary}
              {item.primary === 'goToOwner' ? <span className="kbd">⌘⏎</span> : null}
            </button>
            <GuardNote guard={guard} />
          </div>
        ) : null}
      </div>
    );
  }

  return (
    <section className="fh" aria-label={label} {...engagement} data-item={item.key}>
      {pathLine}
      {body}
    </section>
  );
}
