/**
 * What the overview surfaces do — the sidebar, Needs you, Home and the
 * palette all act through these, so a queue item does the same thing
 * wherever it is clicked.
 *
 *  - The primary action of a queue item is the core's (`AttentionItem.primary`,
 *    contract §3.5.7): the WebView never re-derives it.
 *  - Answers name the exact `requestId` they were shown and go to the session
 *    whose card answers it (`answer_in`, F-15): a teammate's request that
 *    renders in its lead is answered in the lead. What an allow or a deny
 *    sends is the card stack's own decision (`shortcutAction`, A44.2).
 *  - Failures of the user's own commands become an error toast (`te`); a
 *    request that was answered or withdrawn elsewhere is not an error.
 */

import { answerShortcut } from '../approval/answer';
import type { AnswerOutcome, ShortcutRun } from '../approval/answer';
import type { AccountId } from '../lib/gen/AccountId';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { PrimaryAction } from '../lib/gen/PrimaryAction';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { AnswerVia } from '../lib/gen/AnswerVia';
import type { ContinueHow } from '../lib/gen/ContinueHow';
import { commands } from '../lib/commands';
import { newClientMsgId, newClientRequestId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { appStore } from '../store/useAppStore';
import { viewForItem } from '../store/uiSlice';
import type { WorldState } from '../store/worldSlice';
import { sessionOf } from '../store/worldSlice';

function fail(error: unknown): void {
  appStore.getState().pushErrorToast(error as IpcError);
}

/** Open what a queue item is about (its session with the card focused, or Health). */
export function openItem(item: AttentionItem): void {
  const view = viewForItem(item);
  const s = appStore.getState();
  if (view.screen === 'session') s.openSession(view.uid, { focusCard: true });
  else s.navigate(view);
}

/** Later: hide the item for 30 min (Q5). The aggregated leftover item cannot be Later-ed. */
export function laterItem(item: AttentionItem): void {
  if (item.subject.kind !== 'session') return;
  void commands.queue_later({ uid: item.subject.uid }).catch(fail);
}

/** Mark a New reply seen from a list (`⌫`, ux-ia §4.6). */
export function dismissNewReply(uid: SessionUid): void {
  void commands.session_dismiss({ uid, what: 'newReply' }).catch(fail);
}

function continueSession(uid: SessionUid, how: ContinueHow): void {
  void commands
    .session_continue({ clientRequestId: newClientRequestId(), uid, how, controlMode: null, model: null, permissionMode: null })
    .catch(fail);
}

/** Another account of the same vendor that can start sessions (Usage limit → Continue on {other}). */
export function otherAccount(world: WorldState, session: Session): AccountId | null {
  for (const a of world.accounts.values()) {
    if (a.vendor === session.vendor && a.id !== session.accountId && a.usableForNew && a.signIn !== 'signedOut') return a.id;
  }
  return null;
}

/** The label key a primary action uses on Home and in the palette. */
export function primaryLabelKey(primary: PrimaryAction) {
  switch (primary) {
    case 'answer':
      return 'home.answer' as const;
    case 'goToOwner':
      return 'action.goToOwner' as const;
    case 'continueOnOtherAccount':
      return 'action.continueOn' as const;
    case 'signIn':
      return 'action.signIn' as const;
    case 'showLog':
      return 'action.showLog' as const;
    case 'startAsTerminal':
      return 'action.startAsTerminal' as const;
    case 'retry':
      return 'action.retry' as const;
    case 'continueInGilde':
    case 'continue':
      return 'action.continue' as const;
    case 'continueCopy':
      return 'action.continueCopy' as const;
    case 'review':
      return 'action.review' as const;
    case 'cleanUp':
      return 'action.cleanUp' as const;
    case 'stop':
      return 'action.stop' as const;
    case 'open':
    case 'message':
      return 'home.open' as const;
  }
}

/** Run the core's primary action of a queue item. */
export function runPrimary(item: AttentionItem): void {
  const store = appStore.getState();
  if (item.subject.kind === 'leftovers') {
    if (item.primary === 'cleanUp') store.openSheet('cleanUp');
    else store.navigate({ screen: 'accounts', section: 'health' });
    return;
  }
  const uid = item.subject.uid;
  const session = sessionOf(store.world, uid);
  switch (item.primary) {
    case 'goToOwner':
      void commands.owner_focus({ uid }).catch(fail);
      return;
    case 'continueOnOtherAccount': {
      const other = session ? otherAccount(store.world, session) : null;
      if (other) continueSession(uid, { kind: 'otherAccount', accountId: other });
      else openItem(item);
      return;
    }
    case 'signIn':
      if (session?.accountId) {
        void commands.account_sign_in({ clientRequestId: newClientRequestId(), accountId: session.accountId }).catch(fail);
      } else {
        openItem(item);
      }
      return;
    case 'showLog':
      store.openSession(uid, { sub: 'terminal' });
      return;
    case 'startAsTerminal':
      void commands.session_retry_as_terminal({ clientRequestId: newClientRequestId(), uid }).catch(fail);
      return;
    case 'retry':
      void commands.turn_retry({ uid, clientMsgId: newClientMsgId() }).catch(fail);
      return;
    case 'continueInGilde':
    case 'continue':
      continueSession(uid, { kind: 'resume' });
      return;
    case 'continueCopy':
      continueSession(uid, { kind: 'fork' });
      return;
    case 'cleanUp':
      store.openSheet('cleanUp');
      return;
    case 'stop':
      store.openSession(uid);
      store.openSheet('stopSession');
      return;
    case 'answer':
    case 'review':
    case 'message':
    case 'open':
      openItem(item);
      return;
  }
}

// ---------------------------------------------------------------------------
// Cards
// ---------------------------------------------------------------------------

/** The card a queue item is about, and the session that answers it (F-15). */
export interface CardTarget {
  /** The session the item names (a teammate, for a request that renders in its lead). */
  session: Session;
  interaction: PendingInteraction;
  /** Where the answer goes: `answer_in`, else the session itself. */
  answerUid: SessionUid;
}

/**
 * Find the pending card of a queue item. A teammate's request that the lead
 * renders (`answer_in` = lead, Q3) lives on the lead with `on_behalf_of` =
 * the member; a mirrored copy on the member itself is accepted too.
 */
export function cardOf(world: WorldState, item: AttentionItem): CardTarget | null {
  if (item.subject.kind !== 'session') return null;
  const session = sessionOf(world, item.subject.uid);
  if (!session) return null;
  const answerIn = item.subject.answerIn;
  if (answerIn) {
    const lead = sessionOf(world, answerIn);
    const onBehalf = item.subject.onBehalfOf;
    const onLead = lead?.pending.find((p) => (onBehalf ? p.onBehalfOf === onBehalf : true)) ?? null;
    const mirrored = session.pending.find((p) => p.answerIn === answerIn) ?? session.pending[0] ?? null;
    const interaction = onLead ?? mirrored;
    if (!interaction || !lead) return null;
    return { session, interaction, answerUid: lead.uid };
  }
  const interaction = session.pending[0];
  if (!interaction) return null;
  return { session, interaction, answerUid: interaction.answerIn ?? session.uid };
}

/** Cards Home answers inline: an approval of a session Gilde controls. */
export function answerableInline(target: CardTarget): boolean {
  const kind = target.interaction.kind;
  return kind === 'toolApproval' || kind === 'screenPrompt';
}

/** Allow (`⌘⏎`, the card's primary answer) or deny (`⌘⌫`). */
export type AnswerChoice = 'allowOnce' | 'deny';

/** A settled answer: true when the core took it; a failure of the user's own answer is a toast. */
function settled(outcome: AnswerOutcome): boolean {
  if (outcome.ok) return outcome.result === 'answered' || outcome.result === 'sent';
  if (!('busy' in outcome)) fail(outcome.error);
  return false;
}

/**
 * `⌘⏎` / `⌘⌫` on a card a list row shows (Home's Next rows). The decision is
 * the card stack's own `shortcutAction` (A44.2): inert on `default_to_no` for
 * allow and on screen prompts that are not permission-shaped at High
 * confidence, so Home keeps no copy of that rule. The caller applies its
 * guard first (keyed on the request, A44.1). Returns what happened at once:
 * `sent` (the answer is on its way), `inert` (nothing sent) or `open` (the
 * card itself is needed).
 */
export function answerByKey(target: CardTarget, choice: AnswerChoice): ShortcutRun['kind'] {
  const run = answerShortcut(target.answerUid, target.interaction, choice === 'allowOnce' ? 'primary' : 'deny', 'shortcut');
  if (run.kind === 'sent') void run.outcome.then(settled);
  return run.kind;
}

/**
 * Answer a card with its allow or its deny, decided as `⌘⏎` / `⌘⌫` decide it
 * (A44.2): a screen prompt sends its own key for that answer (TOCTOU-guarded
 * by `promptRev`) only when it is permission-shaped at High confidence; a
 * structured card sends the decision. Resolves true when the core took it;
 * inert, or answered or withdrawn elsewhere, resolves false.
 */
export async function answerCard(target: CardTarget, choice: AnswerChoice, via: AnswerVia): Promise<boolean> {
  const run = answerShortcut(target.answerUid, target.interaction, choice === 'allowOnce' ? 'primary' : 'deny', via);
  if (run.kind !== 'sent') return false;
  return settled(await run.outcome);
}
