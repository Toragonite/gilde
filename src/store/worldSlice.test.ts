/**
 * The World mirror (contract §4.2.1 store_subscribe, §4.3 StorePatch, §3.3.3).
 * T-ui-revGap, T-ui-mergedInto, T-ui-reload, plus the folding rules.
 */

import type { ChannelLike } from '../lib/channel';
import type { StorePatch } from '../lib/gen/StorePatch';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import { flush, withFakeCore } from '../test/backend';
import { patch, pending, project, queueItem, session, snapshot, uid, upsertSession } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, sleep, test } from '../test/harness';
import { createAppStore } from './useAppStore';
import { decidePatch, emptyWorld, foldOps, resolveUid, retryDelayMs, sessionOf, worldFromSnapshot } from './worldSlice';

/** A fake core that answers store_subscribe with the next snapshot and keeps each channel. */
function subscribeHandler(snapshots: StoreSnapshot[]) {
  const channels: ChannelLike<StorePatch>[] = [];
  let i = 0;
  const handler = (_req: unknown, channel: ChannelLike<unknown> | undefined) => {
    channels.push(channel as ChannelLike<StorePatch>);
    const snap = snapshots[Math.min(i, snapshots.length - 1)] as StoreSnapshot;
    i += 1;
    return snap;
  };
  return { channels, handler };
}

test('decidePatch: apply only from the current rev; duplicates drop; anything else is a gap', () => {
  assertEqual(decidePatch(5, { fromRev: 5, toRev: 6 }), 'apply', 'next patch');
  assertEqual(decidePatch(5, { fromRev: 5, toRev: 9 }), 'apply', 'a coalesced patch spanning revs');
  assertEqual(decidePatch(5, { fromRev: 4, toRev: 5 }), 'stale', 'already applied');
  assertEqual(decidePatch(5, { fromRev: 2, toRev: 3 }), 'stale', 'old duplicate');
  assertEqual(decidePatch(5, { fromRev: 6, toRev: 7 }), 'gap', 'skipped a patch');
  assertEqual(decidePatch(5, { fromRev: 4, toRev: 7 }), 'gap', 'overlapping range');
});

test('foldOps keeps the identity of untouched kinds and entities', () => {
  const a = session(1);
  const b = session(2);
  const { world } = worldFromSnapshot(snapshot(1, { sessions: [a, b], projects: [project('p')] }), emptyWorld());
  const b2 = { ...b, title: 'renamed' };
  const out = foldOps(world, [upsertSession(b2)]).world;
  assertEqual(out.projects, world.projects, 'projects map untouched');
  assertEqual(out.sessions.get(a.uid), world.sessions.get(a.uid), 'session a keeps its identity');
  assertEqual(out.sessions.get(b.uid)?.title, 'renamed', 'session b updated');
  assert(out.sessions !== world.sessions, 'the sessions map is a new map');
  assertEqual(world.sessions.get(b.uid)?.title, 'session 2', 'the previous state is not mutated');
});

test('foldOps: remove, queue, boot, notices, quit and a Resync that stops folding', () => {
  const { world } = worldFromSnapshot(snapshot(1, { sessions: [session(1)] }), emptyWorld());
  const out = foldOps(world, [
    { kind: 'remove', entityKind: 'session', id: uid(1) },
    { kind: 'queue', queue: { rev: 2, items: [queueItem(3)], needYou: 1, also: 0, pausedUntil: null } },
    { kind: 'boot', status: { spawnGateOpen: true, firstDiscoveryDone: false, agentsDetected: false, memoryOnly: 'disk', agents: [], agentsScannedAt: null, recovery: null } },
    { kind: 'notices', notices: [{ key: 'notice.cantReadFolders', params: {} }] },
    { kind: 'quit', stopped: 1, total: 3, phase: 'stopping' },
    { kind: 'resync' },
    upsertSession(session(9)),
  ]);
  assertEqual(out.world.sessions.size, 0, 'session removed');
  assertEqual(out.world.queue.needYou, 1, 'queue replaced');
  assertEqual(out.world.boot.memoryOnly, 'disk', 'boot replaced');
  assertEqual(out.world.notices.length, 1, 'notices replaced');
  assertDeepEqual(out.world.quit, { stopped: 1, total: 3, phase: 'stopping' }, 'quit progress');
  assertEqual(out.resync, true, 'Resync asked');
  assert(!out.world.sessions.has(uid(9)), 'ops after Resync are not applied');
});

test('T-ui-mergedInto: a tombstone is never shown and every UI reference moves to the survivor', () => {
  const store = createAppStore();
  const older = uid(1);
  const newer = uid(2);
  const other = uid(3);
  store.getState().applyStoreSnapshot(snapshot(10, { sessions: [session(1), session(2), session(3)] }));
  const s = store.getState();
  s.openSession(newer, { focusCard: true });
  s.addToSplit(other);
  s.addToSplit(newer);
  s.setSplitFocus(newer);
  s.toggleSplitZoom(newer);
  s.setDraft(newer, 'half-written reply');
  s.navigate({ screen: 'home' });
  s.navigate({ screen: 'session', uid: newer, sub: 'changes' });

  store.getState().applyStorePatch(patch(10, 11, [upsertSession(session(2, { mergedInto: older }))]));

  const after = store.getState();
  assert(!after.world.sessions.has(newer), 'the tombstone is not in the session list');
  assertEqual(after.world.mergedInto.get(newer), older, 'the tombstone is recorded');
  assertEqual(sessionOf(after.world, newer)?.uid, older, 'a lookup by the old uid finds the survivor');
  assertDeepEqual(after.ui.view, { screen: 'session', uid: older, sub: 'changes' }, 'the open screen moved');
  assertDeepEqual(after.ui.split.uids, [other, older], 'the Split set moved, order kept');
  assertEqual(after.ui.split.focused, older, 'tile focus moved');
  assertEqual(after.ui.split.zoomed, older, 'zoom moved');
  assertEqual(after.ui.drafts[older], 'half-written reply', 'the draft moved');
  assert(!(newer in after.ui.drafts), 'no draft left under the tombstone');
  assertEqual(after.ui.focusCard?.uid, older, 'the focus-card request moved');
  assert(
    after.ui.back.every((v) => v.screen !== 'session' || v.uid !== newer),
    'history entries moved too',
  );
});

test('T-ui-mergedInto: a Split holding both uids collapses to one tile; drafts are not lost', () => {
  const store = createAppStore();
  store.getState().applyStoreSnapshot(snapshot(1, { sessions: [session(1), session(2)] }));
  store.getState().addToSplit(uid(1));
  store.getState().addToSplit(uid(2));
  store.getState().setDraft(uid(1), 'kept');
  store.getState().setDraft(uid(2), 'also kept');
  store.getState().applyStorePatch(patch(1, 2, [upsertSession(session(2, { mergedInto: uid(1) }))]));
  assertDeepEqual(store.getState().ui.split.uids, [uid(1)], 'one tile');
  assertEqual(store.getState().ui.drafts[uid(1)], 'kept\nalso kept', 'both drafts survive');
  assertEqual(resolveUid(store.getState().world, uid(2)), uid(1), 'resolveUid');
});

test('T-ui-revGap: a gap resubscribes, and the new snapshot replaces the mirror', async () => {
  const sub = subscribeHandler([
    snapshot(5, { sessions: [session(1)] }),
    snapshot(20, { sessions: [session(1, { title: 'fresh' }), session(2)] }),
  ]);
  await withFakeCore({ store_subscribe: sub.handler }, async (core) => {
    const store = createAppStore();
    await store.getState().subscribeWorld();
    assertEqual(store.getState().world.status, 'live', 'live after the snapshot');
    assertEqual(store.getState().world.rev, 5, 'rev from the snapshot');

    sub.channels[0]?.onmessage(patch(5, 6, [upsertSession(session(1, { title: 'one' }))]));
    assertEqual(store.getState().world.rev, 6, 'in-order patch applied');
    sub.channels[0]?.onmessage(patch(5, 6, [upsertSession(session(1, { title: 'dup' }))]));
    assertEqual(sessionOf(store.getState().world, uid(1))?.title, 'one', 'a duplicate is dropped');

    sub.channels[0]?.onmessage(patch(8, 9, [upsertSession(session(7))]));
    assertEqual(store.getState().world.status, 'resyncing', 'a gap starts a resync');
    assert(!store.getState().world.sessions.has(uid(7)), 'the gapped patch is not applied');
    await flush();
    assertEqual(core.calls('store_subscribe').length, 2, 'store_subscribe called again');
    assertEqual(store.getState().world.rev, 20, 'the new snapshot applied');
    assertEqual(sessionOf(store.getState().world, uid(1))?.title, 'fresh', 'its content replaced the old');
    assertEqual(store.getState().world.resyncCount, 1, 'counted');

    sub.channels[0]?.onmessage(patch(20, 21, [upsertSession(session(8))]));
    assert(!store.getState().world.sessions.has(uid(8)), 'the replaced channel is ignored');
    sub.channels[1]?.onmessage(patch(20, 21, [upsertSession(session(8))]));
    assert(store.getState().world.sessions.has(uid(8)), 'the new channel is live');
    store.getState().unsubscribeWorld();
  });
});

test('T-ui-revGap: a Resync op resubscribes', async () => {
  const sub = subscribeHandler([snapshot(1), snapshot(50)]);
  await withFakeCore({ store_subscribe: sub.handler }, async (core) => {
    const store = createAppStore();
    await store.getState().subscribeWorld();
    sub.channels[0]?.onmessage(patch(1, 2, [{ kind: 'resync' }]));
    await flush();
    assertEqual(core.calls('store_subscribe').length, 2, 'resubscribed');
    assertEqual(store.getState().world.rev, 50, 'fresh snapshot');
    store.getState().unsubscribeWorld();
  });
});

test('patches that arrive before the snapshot are buffered and applied after it', async () => {
  let channel: ChannelLike<StorePatch> | undefined;
  let answer: (s: StoreSnapshot) => void = () => {};
  await withFakeCore(
    {
      store_subscribe: (_req, ch) => {
        channel = ch as ChannelLike<StorePatch>;
        return new Promise<StoreSnapshot>((resolve) => {
          answer = resolve;
        });
      },
    },
    async () => {
      const store = createAppStore();
      const done = store.getState().subscribeWorld();
      await flush();
      channel?.onmessage(patch(3, 4, [upsertSession(session(1, { title: 'before snapshot, stale' }))]));
      channel?.onmessage(patch(4, 5, [upsertSession(session(2))]));
      channel?.onmessage(patch(5, 6, [upsertSession(session(3))]));
      assertEqual(store.getState().world.sessions.size, 0, 'nothing applied before the snapshot');
      answer(snapshot(4, { sessions: [session(1)] }));
      await done;
      const w = store.getState().world;
      assertEqual(w.rev, 6, 'buffered patches applied in order');
      assertEqual(w.sessions.get(uid(1))?.title, 'session 1', 'the patch older than the snapshot was dropped');
      assert(w.sessions.has(uid(2)) && w.sessions.has(uid(3)), 'the newer ones applied');
      store.getState().unsubscribeWorld();
    },
  );
});

test('T-ui-reload: a fresh mirror re-renders pending cards from the snapshot', async () => {
  const withCard = session(4, { pending: [pending('r1')], activity: { ...session(4).activity, state: { kind: 'needsApproval' } } });
  await withFakeCore({ store_subscribe: () => snapshot(9, { sessions: [withCard] }) }, async () => {
    const store = createAppStore();
    await store.getState().subscribeWorld();
    const s = sessionOf(store.getState().world, uid(4));
    assertEqual(s?.pending.length, 1, 'the pending card is back after a reload');
    assertEqual(s?.pending[0]?.requestId, 'r1', 'the same request');
    store.getState().unsubscribeWorld();
  });
});

test('a failed subscribe retries with backoff; a notBuilt stub stops retrying', async () => {
  assertEqual(retryDelayMs(1), 500, 'first retry after 0.5 s');
  assertEqual(retryDelayMs(3), 2000, 'doubling');
  assertEqual(retryDelayMs(10), 8000, 'capped at 8 s');
  await withFakeCore({}, async () => {
    const store = createAppStore();
    await store.getState().subscribeWorld();
    assertEqual(store.getState().world.status, 'unavailable', 'a notBuilt stub is unavailable');
    assertEqual(store.getState().world.lastError?.params.cap, 'notBuilt', 'the error is kept');
    store.getState().unsubscribeWorld();
  });
  let calls = 0;
  await withFakeCore(
    {
      store_subscribe: () => {
        calls += 1;
        if (calls === 1) throw { code: 'NOT_READY', key: 'error.notReady', params: { reason: 'boot' }, detail: null, retryable: true };
        return snapshot(3);
      },
    },
    async () => {
      const store = createAppStore();
      await store.getState().subscribeWorld();
      assertEqual(store.getState().world.status, 'retrying', 'retrying after NOT_READY');
      await sleep(650);
      await flush();
      assertEqual(calls, 2, 'retried once');
      assertEqual(store.getState().world.status, 'live', 'live after the retry');
      store.getState().unsubscribeWorld();
    },
  );
});
