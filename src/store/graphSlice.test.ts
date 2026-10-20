/**
 * Graph slice tests — the revision-gap contract (T05 acceptance 2) and the
 * layout-stability contract the graph view depends on (DESIGN §9-6).
 */

import { assert, assertEqual, test } from '../test/harness';
import type { GraphNode, GraphPatch, GraphSnapshot, Totals } from '../lib/ipc';
import {
  applyOps,
  decidePatch,
  emptyGraph,
  emptyTotals,
  setGraphSnapshotProvider,
  topologyDigest,
} from './graphSlice';
import { useAppStore } from './useAppStore';

// -- fixtures ----------------------------------------------------------------

function node(id: string, kind: GraphNode['kind'] = 'session'): GraphNode {
  return {
    id,
    kind,
    parentId: null,
    label: id,
    status: 'idle',
    badges: [],
    metrics: {},
    meta: {},
    updatedAt: 0,
  };
}

function totals(sessions: number): Totals {
  return { ...emptyTotals(), sessions };
}

function snapshot(rev: number, ids: string[]): GraphSnapshot {
  return {
    nodes: ids.map((id) => node(id)),
    edges: [],
    topologyHash: `server-${rev}`,
    totals: totals(ids.length),
    rev,
    at: 1,
  };
}

function patch(rev: number, ops: GraphPatch['ops']): GraphPatch {
  return { rev, ops };
}

function reset(): void {
  useAppStore.setState({ graph: emptyGraph() });
}

/** Let the resync promise chain settle. */
function flush(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

// -- decidePatch -------------------------------------------------------------

test('decidePatch classifies apply / stale / gap', () => {
  assertEqual(decidePatch(0, 1), 'apply', 'first patch after boot applies');
  assertEqual(decidePatch(7, 8), 'apply', 'the next revision applies');
  assertEqual(decidePatch(7, 7), 'stale', 'a duplicate is dropped');
  assertEqual(decidePatch(7, 3), 'stale', 'an out-of-order older patch is dropped');
  assertEqual(decidePatch(7, 9), 'gap', 'a skipped revision is a gap');
});

// -- the gap scenario --------------------------------------------------------

test('a revision gap resyncs from graph_snapshot() instead of diverging', async () => {
  reset();
  let calls = 0;
  setGraphSnapshotProvider(async () => {
    calls += 1;
    return snapshot(42, ['session:a', 'session:b', 'worktree:w']);
  });

  const store = useAppStore.getState;

  // rev 1 applies normally.
  store().applyGraphPatch(patch(1, [{ op: 'upsertNode', node: node('session:a') }]));
  assertEqual(store().graph.rev, 1, 'the first patch applied');
  assertEqual(calls, 0, 'no resync for a healthy patch');

  // rev 5 skips 2..4 — the stream diverged, so the whole projection is refetched.
  store().applyGraphPatch(patch(5, [{ op: 'upsertNode', node: node('session:z') }]));
  assertEqual(calls, 1, 'the gap triggered exactly one snapshot call');
  assert(!store().graph.nodes.has('session:z'), 'the gap patch was not applied blindly');

  await flush();

  assertEqual(store().graph.rev, 42, 'the snapshot revision replaced the local one');
  assertEqual(store().graph.nodes.size, 3, 'the snapshot replaced the node set');
  assertEqual(store().graph.resyncing, false, 'the resync flag was cleared');
  assertEqual(store().graph.resyncCount, 1, 'the resync was counted');
  assertEqual(store().graph.serverTopologyHash, 'server-42', 'the server hash was recorded');

  // The stream continues from the snapshot revision.
  store().applyGraphPatch(patch(43, [{ op: 'totals', totals: totals(9) }]));
  assertEqual(store().graph.rev, 43, 'patching resumes after the resync');
  assertEqual(store().graph.totals.sessions, 9, 'totals came from the patch');
  assertEqual(calls, 1, 'no second resync was needed');
});

test('a duplicate patch is ignored and does not resync', async () => {
  reset();
  let calls = 0;
  setGraphSnapshotProvider(async () => {
    calls += 1;
    return snapshot(99, []);
  });
  const store = useAppStore.getState;
  store().applyGraphPatch(patch(1, [{ op: 'upsertNode', node: node('session:a') }]));
  store().applyGraphPatch(patch(1, [{ op: 'upsertNode', node: node('session:b') }]));
  assertEqual(store().graph.rev, 1, 'the revision did not move');
  assertEqual(store().graph.nodes.size, 1, 'the duplicate was dropped');
  assertEqual(calls, 0, 'a duplicate is not a gap');
});

// -- layout stability --------------------------------------------------------

test('metrics-only patches keep topologyHash and untouched node identity', () => {
  reset();
  const store = useAppStore.getState;
  store().applyGraphSnapshot(snapshot(1, ['session:a', 'session:b']));

  const hash = store().graph.topologyHash;
  const untouched = store().graph.nodes.get('session:b');

  for (let i = 0; i < 100; i++) {
    store().applyGraphPatch(
      patch(1 + i + 1, [
        { op: 'patchNode', id: 'session:a', metrics: { rssBytes: 1000 + i, cpuPct: i } },
      ]),
    );
  }

  assertEqual(store().graph.rev, 101, 'all 100 metric patches applied');
  assertEqual(store().graph.topologyHash, hash, 'the topology digest never moved');
  assertEqual(
    store().graph.nodes.get('session:b'),
    untouched,
    'an untouched node kept its object identity',
  );
  assertEqual(store().graph.nodes.get('session:a')?.metrics.rssBytes, 1099, 'metrics merged');
  assertEqual(store().graph.nodes.get('session:a')?.metrics.cpuPct, 99, 'both fields merged');
});

test('adding or removing a node changes the topology digest', () => {
  const base = emptyGraph();
  const added = applyOps(base, [{ op: 'upsertNode', node: node('session:a') }]);
  assert(added.structureChanged, 'inserting a node is a structural change');

  const digestA = topologyDigest(added.nodes.keys(), added.edges.keys());
  const again = applyOps({ ...base, nodes: added.nodes }, [
    { op: 'upsertNode', node: { ...node('session:a'), status: 'working' } },
  ]);
  assert(!again.structureChanged, 're-upserting the same id is not structural');
  assertEqual(
    topologyDigest(again.nodes.keys(), again.edges.keys()),
    digestA,
    'the digest is unchanged when the id set is unchanged',
  );

  const removed = applyOps({ ...base, nodes: again.nodes }, [
    { op: 'removeNode', id: 'session:a' },
  ]);
  assert(removed.structureChanged, 'removing a node is a structural change');
  assert(
    topologyDigest(removed.nodes.keys(), removed.edges.keys()) !== digestA,
    'the digest changes when the node set changes',
  );
});

test('metrics merge field-wise and edges patch in place', () => {
  const base = emptyGraph();
  const withNode = applyOps(base, [
    {
      op: 'upsertNode',
      node: { ...node('run:1', 'run'), metrics: { inputTokens: 10, costUsd: 0.5 } },
    },
    {
      op: 'upsertEdge',
      edge: { id: 'e1', source: 'worktree:w', target: 'run:1', kind: 'runs-in', active: false },
    },
  ]);
  const merged = applyOps({ ...base, nodes: withNode.nodes, edges: withNode.edges }, [
    { op: 'patchNode', id: 'run:1', metrics: { outputTokens: 7 } },
    { op: 'patchEdge', id: 'e1', active: true },
  ]);

  assertEqual(merged.nodes.get('run:1')?.metrics.inputTokens, 10, 'existing metric survived');
  assertEqual(merged.nodes.get('run:1')?.metrics.outputTokens, 7, 'new metric merged in');
  assertEqual(merged.nodes.get('run:1')?.metrics.costUsd, 0.5, 'unrelated metric survived');
  assertEqual(merged.edges.get('e1')?.active, true, 'the edge became active');
  assert(!merged.structureChanged, 'patching is never structural');
});
