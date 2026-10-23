/**
 * The replay ring (pixel-office §2.7, contract §7.5): structure only, ≤ 20,000
 * events and ≤ 60 minutes, replays a past moment exactly, and the flow strip
 * sweeps it.
 */

import { createAppStore } from '../store/useAppStore';
import { assert, assertEqual, test } from '../test/harness';
import { ReplayRing, startFloorRing, structureOf } from './ring';
import { T0, showcaseSnapshot } from './testing';

function storeAt(ms: number) {
  const store = createAppStore();
  store.getState().applyStoreSnapshot(showcaseSnapshot(12, ['native'], ms));
  return store;
}

test('T-floor-ring: replay rebuilds the structure of a past moment and holds no content', () => {
  const ring = new ReplayRing();
  const a = storeAt(0);
  ring.record(a.getState().world, T0);
  const b = storeAt(15_000);
  ring.record(b.getState().world, T0 + 15_000);
  const atA = ring.at(T0 + 1000);
  const atB = ring.at(T0 + 20_000);
  assert(atA && atB, 'both moments');
  const want = structureOf(a.getState().world, T0);
  for (const [uid, f] of want.sessions) assertEqual(atA.sessions.get(uid)?.kind, f.kind, 'the earlier state comes back');
  const wantB = structureOf(b.getState().world, T0 + 15_000);
  for (const [uid, f] of wantB.sessions) assertEqual(atB.sessions.get(uid)?.kind, f.kind, 'the later state comes back');
  const text = JSON.stringify([...atB.sessions.values(), ...atB.tasks.values(), ...atB.msgs.values()]);
  assert(!/Run the test suite|Edit src|refresh path/.test(text), 'no titles, card text or message text in the ring');
});

test('T-floor-ring: at most 20,000 events and 60 minutes are kept', () => {
  const ring = new ReplayRing(50, 60 * 60_000);
  const store = storeAt(0);
  const base = store.getState().world;
  const uid = [...base.sessions.keys()][0];
  assert(uid, 'a session');
  for (let i = 0; i < 200; i++) {
    const s = base.sessions.get(uid);
    assert(s, 'session');
    const world = { ...base, sessions: new Map(base.sessions).set(uid, { ...s, activity: { ...s.activity, since: T0 + i } }) };
    ring.record(world, T0 + i * 1000);
  }
  assert(ring.size <= 50, `bounded by count (${ring.size})`);
  const old = new ReplayRing(100_000, 60_000);
  for (let i = 0; i < 300; i++) {
    const s = base.sessions.get(uid);
    assert(s, 'session');
    old.record({ ...base, sessions: new Map(base.sessions).set(uid, { ...s, activity: { ...s.activity, since: T0 + i } }) }, T0 + i * 1000);
  }
  assert((old.start ?? 0) >= T0 + 299_000 - 60_000 - 1000, 'bounded by age');
  assert(old.approxBytes() < 2_000_000, 'within the 2 MB budget');
});

test('T-floor-ring: the strip sweep visits the window start and every event in it, in order', () => {
  const ring = new ReplayRing();
  ring.record(storeAt(0).getState().world, T0);
  ring.record(storeAt(5_000).getState().world, T0 + 5_000);
  ring.record(storeAt(15_000).getState().world, T0 + 15_000);
  const seen: number[] = [];
  ring.sweep(T0 + 1000, T0 + 20_000, (t, _s, ev) => {
    if (!ev) assertEqual(seen.length, 0, 'the window start comes first');
    seen.push(t);
  });
  assert(seen.length > 1, 'events inside the window');
  for (let i = 1; i < seen.length; i++) assert((seen[i] as number) >= (seen[i - 1] as number), 'in time order');
});

test('T-floor-ring: recording follows the store from the moment it starts', () => {
  const ring = new ReplayRing();
  const store = storeAt(0);
  const stop = startFloorRing(store, ring);
  const before = ring.version;
  store.getState().applyStoreSnapshot(showcaseSnapshot(12, ['native'], 15_000));
  assert(ring.version > before, 'a store change was recorded');
  stop();
  const after = ring.version;
  store.getState().applyStoreSnapshot(showcaseSnapshot(12, ['native'], 0));
  assertEqual(ring.version, after, 'stopped');
});
