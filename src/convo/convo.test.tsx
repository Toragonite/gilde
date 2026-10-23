/**
 * Conversation renderer tests (ux-ia §3.4, contract §4.2.4, §15.2 T15):
 * tool-run folding, edits with +/−, interaction outcomes incl. Abandoned
 * (A19.2), the Cancelled delivery chip (§15.5), the follow-the-tail pill,
 * eviction and unreadable conversations.
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { ChannelLike } from '../lib/channel';
import type { Block } from '../lib/gen/Block';
import type { BlockKind } from '../lib/gen/BlockKind';
import type { ConvoEnvelope } from '../lib/gen/ConvoEnvelope';
import type { IdeOpenReq } from '../lib/gen/IdeOpenReq';
import type { RequestId } from '../lib/gen/RequestId';
import type { Session } from '../lib/gen/Session';
import type { SubId } from '../lib/gen/SubId';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore } from '../test/backend';
import { block, pending, session, snapshot, text, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import { ConversationView, splitPathLine } from './ConversationView';
import { editTotals, toRows, verbCounts } from './rows';
import type { ToolBlock } from './rows';

const SUB = 'v_00000000000000c1' as SubId;
let rev = 300;

function tool(id: string, seq: number, verb: ToolBlock['kind']['summary']['verb'], object: string, extra: Partial<Extract<BlockKind, { kind: 'toolCall' }>> = {}): Block {
  return block(id, seq, {
    kind: 'toolCall',
    toolUseId: `toolu_${id}`,
    summary: { tool: verb === 'run' ? 'Bash' : 'Read', verb, object },
    inputPreview: object,
    status: 'ok',
    resultPreview: null,
    fileChange: null,
    incomplete: false,
    ...extra,
  });
}

function setSessions(sessions: Session[]) {
  act(() => {
    appStore.getState().applyStoreSnapshot(snapshot(++rev, { sessions }));
  });
}

function harness(run: (core: FakeCore, send: (event: ConvoEnvelope['event']) => Promise<void>) => Promise<void>) {
  return async () => {
    resetHotkeysForTests();
    appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
    let channel: ChannelLike<ConvoEnvelope> | undefined;
    let seq = 0;
    try {
      await withFakeCore(
        {
          convo_subscribe: (_req, ch) => {
            channel = ch as ChannelLike<ConvoEnvelope>;
            return { subId: SUB, source: 'stream', note: null };
          },
          convo_unsubscribe: () => ({}),
          convo_load_older: () => ({}),
          ide_open: () => ({}),
        },
        (core) =>
          run(core, async (event) => {
            await act(async () => {
              await flush();
              channel?.onmessage({ subId: SUB, seq: seq++, event });
            });
          }),
      );
    } finally {
      cleanup();
      appStore.setState({ world: emptyWorld(), ui: initialUiState() });
    }
  };
}

const ROW_PX = 20;
const measure = () => ROW_PX;

// ---------------------------------------------------------------------------
// Pure folding
// ---------------------------------------------------------------------------

test('toRows: consecutive tool calls fold per category; text breaks a run; speakers only at run starts', () => {
  const rows = toRows([
    block('u', 1, { kind: 'userText', text: 'split it', origin: 'human', delivery: null, clientMsgId: null }),
    text('a1', 2, 'reading first'),
    tool('r1', 3, 'read', 'a.ts'),
    tool('r2', 4, 'search', 'auth'),
    tool('r3', 5, 'read', 'b.ts'),
    tool('e1', 6, 'edit', 'a.ts', { fileChange: { pathRel: 'a.ts', added: 3, removed: 1, change: 'modified' } }),
    tool('e2', 7, 'create', 'c.ts', { fileChange: { pathRel: 'c.ts', added: 9, removed: 0, change: 'added' } }),
    tool('x1', 8, 'run', 'npm test'),
    text('a2', 9, 'done'),
    text('a3', 10, 'and more'),
  ]);
  assertDeepEqual(rows.map((r) => r.kind), ['user', 'assistant', 'tools', 'tools', 'tools', 'assistant', 'assistant'], 'row kinds');
  const tools = rows.filter((r) => r.kind === 'tools');
  assertDeepEqual(tools.map((r) => (r.kind === 'tools' ? [r.category, r.blocks.length] : [])), [['other', 3], ['edit', 2], ['run', 1]], 'runs');
  const speakers = rows.filter((r) => r.kind === 'assistant').map((r) => (r.kind === 'assistant' ? r.speaker : null));
  assertDeepEqual(speakers, [true, true, false], 'a speaker line only where a run starts');
  const other = tools[0];
  assert(other?.kind === 'tools', 'tools row');
  assertDeepEqual(verbCounts(other.blocks), [{ verb: 'read', n: 2 }, { verb: 'search', n: 1 }], 'Read 2 files · searched 1 pattern');
  const edits = tools[1];
  assert(edits?.kind === 'tools', 'edits row');
  const totals = editTotals(edits.blocks);
  assertEqual(totals.added, 12, '+12');
  assertEqual(totals.removed, 1, '−1');
});

test('splitPathLine: path:line opens at the line; anything else is a path', () => {
  assertDeepEqual(splitPathLine('src/a.ts:42'), { path: 'src/a.ts', line: 42 }, 'line');
  assertDeepEqual(splitPathLine('src/a.ts:42:7'), { path: 'src/a.ts', line: 42 }, 'line:col');
  assertDeepEqual(splitPathLine('src/a.ts'), { path: 'src/a.ts', line: null }, 'no line');
  assertDeepEqual(splitPathLine('src/a.ts:0'), { path: 'src/a.ts:0', line: null }, 'line 0 is not a line');
});

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

domTest(
  'Rendering: the summary line, edits with Open/Diff to the IDE, the Ran line with its result',
  harness(async (core, send) => {
    setSessions([session(1)]);
    const view = render(<ConversationView uid={uid(1)} measure={measure} viewportHeight={2000} />);
    await send({
      kind: 'reset',
      reason: 'initial',
      hasOlder: false,
      blocks: [
        tool('r1', 1, 'read', 'a.ts'),
        tool('r2', 2, 'search', 'auth'),
        text('t', 3, 'now editing'),
        tool('e1', 4, 'edit', 'src/auth/verify.ts', { fileChange: { pathRel: 'src/auth/verify.ts', added: 88, removed: 0, change: 'added' } }),
        text('t2', 5, 'tests'),
        tool('x1', 6, 'run', 'npm test -- auth', { resultPreview: '\n24 passed\n' }),
      ],
    });
    assert(view.getByText('Read 1 file · searched 1 pattern') !== null, 'folded summary');
    assert(view.getByText('src/auth/verify.ts') !== null, 'edit file listed (expanded by default)');
    assert(view.getByText(translate('en', 'convo.file.new')) !== null, 'new');
    assert(view.getByText('· 24 passed') !== null, 'the result line');
    fireEvent.click(view.getByText(translate('en', 'convo.diff')));
    await act(async () => flush());
    assertDeepEqual(core.calls('ide_open').map((c) => (c.req as IdeOpenReq).target), [{ kind: 'diff', uid: uid(1), path: 'src/auth/verify.ts' }], 'Diff opens the IDE');
  }),
);

domTest(
  'Outcomes: a pending request shows a waiting line while its card is in the store; Abandoned renders card.abandonedOnResume',
  harness(async (_core, send) => {
    setSessions([session(1, { pending: [pending('r1')] })]);
    const view = render(<ConversationView uid={uid(1)} measure={measure} viewportHeight={2000} />);
    await send({
      kind: 'reset',
      reason: 'initial',
      hasOlder: false,
      blocks: [
        block('i1', 1, { kind: 'interaction', requestId: 'r1' as RequestId, interactionKind: 'toolApproval', outcome: 'pending' }),
        block('i2', 2, { kind: 'interaction', requestId: 'r0' as RequestId, interactionKind: 'toolApproval', outcome: 'abandoned' }),
        block('i3', 3, { kind: 'interaction', requestId: 'r9' as RequestId, interactionKind: 'toolApproval', outcome: 'withdrawn' }),
      ],
    });
    assert(view.getByText(translate('en', 'state.needsApproval')) !== null, 'the wait is named while the card is up');
    assert(view.getByText(translate('en', 'card.abandonedOnResume', { agent: 'claude' })) !== null, 'A19.2 Abandoned');
    assert(view.getByText(translate('en', 'approval.answeredElsewhere')) !== null, 'withdrawn line');
  }),
);

domTest(
  'Delivery: a queued message shows Queued; a dropped one shows the Cancelled chip',
  harness(async (_core, send) => {
    setSessions([session(1)]);
    const view = render(<ConversationView uid={uid(1)} measure={measure} viewportHeight={2000} />);
    await send({
      kind: 'reset',
      reason: 'initial',
      hasOlder: false,
      blocks: [block('u1', 1, { kind: 'userText', text: 'also update the docs', origin: 'human', delivery: 'cancelled', clientMsgId: null })],
    });
    const chip = view.container.querySelector('[data-delivery="cancelled"]');
    assertEqual(chip?.textContent, translate('en', 'convo.delivery.cancelled'), 'the Cancelled chip');
    assert(view.getByText(translate('en', 'convo.you')) !== null, 'You');
  }),
);

domTest(
  'T-ui-followTail: at the bottom new rows follow; scrolled up, a "{n} new ↓" pill counts them and jumps back',
  harness(async (_core, send) => {
    setSessions([session(1)]);
    const view = render(<ConversationView uid={uid(1)} measure={measure} viewportHeight={200} />);
    const many = Array.from({ length: 40 }, (_, i) => text(`b${i}`, i + 1, `line ${i}`));
    await send({ kind: 'reset', reason: 'initial', hasOlder: false, blocks: many });
    assertEqual(view.container.querySelector('[data-follow-pill]'), null, 'no pill at the bottom');
    await send({ kind: 'appended', block: block('u40', 41, { kind: 'userText', text: 'next', origin: 'human', delivery: null, clientMsgId: null }) });
    assertEqual(view.container.querySelector('[data-follow-pill]'), null, 'following: still no pill');

    const list = view.container.querySelector('[role="log"]') as HTMLElement;
    act(() => {
      list.scrollTop = 0;
      fireEvent.scroll(list);
    });
    await send({ kind: 'appended', block: block('u41', 42, { kind: 'userText', text: 'one', origin: 'human', delivery: null, clientMsgId: null }) });
    await send({ kind: 'appended', block: block('u42', 43, { kind: 'userText', text: 'two', origin: 'human', delivery: null, clientMsgId: null }) });
    const pill = view.container.querySelector('[data-follow-pill]');
    assert(pill !== null, 'the pill appears');
    assertEqual(pill?.textContent, `${translate('en', 'convo.newBelow', { n: 2 })} ↓`, 'it counts the new rows');
    act(() => fireEvent.click(pill as Element));
    assertEqual(view.container.querySelector('[data-follow-pill]'), null, 'jumping back hides it');
  }),
);

domTest(
  'Eviction offers Resume (a new subscription); an unreadable conversation says so',
  harness(async (core, send) => {
    setSessions([session(1)]);
    const view = render(<ConversationView uid={uid(1)} measure={measure} viewportHeight={400} />);
    await send({ kind: 'reset', reason: 'initial', hasOlder: false, blocks: [text('a', 1, 'hi')] });
    await send({ kind: 'evicted' });
    fireEvent.click(view.getByText(translate('en', 'convo.resume')));
    await act(async () => flush());
    assertEqual(core.calls('convo_subscribe').length, 2, 'resubscribed');
    cleanup();

    core.on('convo_subscribe', () => {
      throw { code: 'UNSUPPORTED', key: 'error.unsupported', params: { cap: 'noReadableConversation' }, detail: null, retryable: false };
    });
    const v2 = render(<ConversationView uid={uid(1)} measure={measure} viewportHeight={400} />);
    await act(async () => flush());
    assert(v2.getByText(translate('en', 'observed.unreadable')) !== null, 'ux-ia #83');
  }),
);
