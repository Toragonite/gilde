/**
 * `useConversation`: one subscription per mounted view, events folded in seq
 * order, unsubscribe on unmount, tombstones resolved, older blocks on demand.
 */

import { act, render } from '@testing-library/react';

import type { ChannelLike } from '../lib/channel';
import type { ConvoEnvelope } from '../lib/gen/ConvoEnvelope';
import type { ConvoLoadOlderReq } from '../lib/gen/ConvoLoadOlderReq';
import type { ConvoSubscribeReq } from '../lib/gen/ConvoSubscribeReq';
import type { ConvoUnsubscribeReq } from '../lib/gen/ConvoUnsubscribeReq';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { SubId } from '../lib/gen/SubId';
import { appStore } from '../store/useAppStore';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import { patch, session, snapshot, text, uid, upsertSession } from '../test/fixtures';
import { assert, assertEqual, assertMatch, domTest } from '../test/harness';
import { useConversation } from './useConversation';
import type { Conversation } from './useConversation';

const SUB = 'v_00000000000000aa' as SubId;

function Probe({ uid: u, out }: { uid: SessionUid | null; out: { current: Conversation | null } }) {
  const convo = useConversation(u, { tailBlocks: 50, viewLabel: 'test' });
  out.current = convo;
  return <div data-testid="blocks">{convo.view.blocks.map((b) => b.id).join(',')}</div>;
}

domTest('useConversation subscribes, folds events and unsubscribes on unmount', async () => {
  let channel: ChannelLike<ConvoEnvelope> | undefined;
  await withFakeCore(
    {
      convo_subscribe: (_req, ch) => {
        channel = ch as ChannelLike<ConvoEnvelope>;
        return { subId: SUB, source: 'stream', note: null };
      },
      convo_unsubscribe: () => ({}),
      convo_load_older: () => ({}),
    },
    async (core) => {
      appStore.setState({ world: emptyWorld() });
      const out: { current: Conversation | null } = { current: null };
      const view = render(<Probe uid={uid(1)} out={out} />);
      await act(async () => flush());
      const req = core.calls('convo_subscribe')[0]?.req as ConvoSubscribeReq;
      assertEqual(req.uid, uid(1), 'subscribed for the session');
      assertEqual(req.tailBlocks, 50, 'tail size passed');
      assertMatch(req.viewId, /^test-[0-9a-f]{16}$/, 'a minted view id');

      await act(async () => {
        channel?.onmessage({ subId: SUB, seq: 0, event: { kind: 'reset', blocks: [text('b1', 1, 'a')], hasOlder: true, reason: 'initial' } });
        channel?.onmessage({ subId: SUB, seq: 1, event: { kind: 'appended', block: text('b2', 2, 'b') } });
        channel?.onmessage({ subId: SUB, seq: 1, event: { kind: 'appended', block: text('dup', 3, 'x') } });
      });
      assertEqual(view.getByTestId('blocks').textContent, 'b1,b2', 'events folded; the duplicate seq dropped');
      assertEqual(out.current?.view.source, 'stream', 'source from the subscribe answer');

      await act(async () => {
        await out.current?.loadOlder(500);
      });
      const older = core.calls('convo_load_older')[0]?.req as ConvoLoadOlderReq;
      assertEqual(older.beforeBlockId, 'b1', 'before the oldest block shown');
      assertEqual(older.count, 200, 'count capped at 200');

      view.unmount();
      await flush();
      const unsub = core.calls('convo_unsubscribe')[0]?.req as ConvoUnsubscribeReq;
      assertEqual(unsub.subId, SUB, 'unsubscribed on unmount');
    },
  );
});

domTest('useConversation follows a merged uid to its survivor and reports an unreadable session', async () => {
  await withFakeCore(
    {
      convo_subscribe: (req) => {
        const r = req as ConvoSubscribeReq;
        if (r.uid === uid(1)) return { subId: SUB, source: 'transcript', note: null };
        throw { code: 'UNSUPPORTED', key: 'error.unsupported', params: { cap: 'noReadableConversation' }, detail: null, retryable: false };
      },
      convo_unsubscribe: () => ({}),
    },
    async (core) => {
      appStore.setState({ world: emptyWorld() });
      appStore.getState().applyStoreSnapshot(snapshot(1, { sessions: [session(1), session(2)] }));
      appStore.getState().applyStorePatch(patch(1, 2, [upsertSession(session(2, { mergedInto: uid(1) }))]));
      const out: { current: Conversation | null } = { current: null };
      const merged = render(<Probe uid={uid(2)} out={out} />);
      await act(async () => flush());
      assertEqual((core.calls('convo_subscribe')[0]?.req as ConvoSubscribeReq).uid, uid(1), 'the survivor is subscribed');
      merged.unmount();

      const other = render(<Probe uid={uid(9)} out={out} />);
      await act(async () => flush());
      assertEqual(out.current?.view.status, 'unavailable', 'no readable conversation');
      assertEqual(out.current?.error?.params.cap, 'noReadableConversation', 'the reason is kept');
      other.unmount();
      appStore.setState({ world: emptyWorld() });
      assert(true, 'done');
    },
  );
});
