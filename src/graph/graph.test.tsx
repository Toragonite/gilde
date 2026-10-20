/**
 * Graph dashboard unit tests (T25).
 *
 * The four things a type check cannot see, and which the acceptance criteria
 * are written against:
 *
 *  1. **dagre runs only when the topology changes.** A hundred metrics-only
 *     patches must leave every node position byte-identical (DESIGN §9-6).
 *  2. A node the user dragged keeps its position across a topology change.
 *  3. Filters and search are one pure function shared by both views.
 *  4. The canvas renders as a panel from the graph store alone — no fixture, no
 *     fetch, and every status word next to its dot.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { assert, assertDeepEqual, assertEqual } from '../test/harness';
import { laneTest as test, laneUiState } from '../test/lanes';
import { emptyGraph, emptyTotals, applyOps, topologyDigest } from '../store/graphSlice';
import type { GraphView } from '../store/graphSlice';
import { useAppStore } from '../store/useAppStore';
import type { AppStore } from '../store/useAppStore';
import type { GraphEdge, GraphNode, GraphOp } from '../lib/ipc';
import {
  GraphDashboard,
  buildFlowEdges,
  buildFlowNodes,
  localFocus,
  newestWorkUnder,
} from './GraphDashboard';
import { DEFAULT_FILTERS, filterGraph, projectOptions } from './GraphFilters';
import { edgeTone, worseTone } from './edges/StatusEdge';
import { sessionLabel } from './nodes/SessionNode';
import { TilesView, sortForTiles } from './TilesView';
import {
  LayoutCache,
  clearPins,
  computeLayout,
  getAccountModel,
  manualKey,
  manualPositions,
  nodeSize,
  pinNode,
  resetLayoutStore,
  setAccountModels,
  subscribeAccountModels,
  accountModelsRevision,
} from './useGraphLayout';

// ---------------------------------------------------------------------------
// Fixtures — shaped exactly like what Rust projects, never like demo data
// ---------------------------------------------------------------------------

function node(
  id: string,
  kind: GraphNode['kind'],
  label: string,
  parentId: string | null = null,
  status: GraphNode['status'] = 'idle',
): GraphNode {
  return {
    id,
    kind,
    parentId,
    label,
    status,
    badges: [],
    metrics: {},
    meta: {},
    updatedAt: 1,
  };
}

function edge(kind: GraphEdge['kind'], source: string, target: string, active = false): GraphEdge {
  return { id: `${kind}:${source}>${target}`, source, target, kind, active };
}

/** project → 2 worktrees → 2 sessions + 1 run, plus a task and an orphan. */
function seededGraph(): GraphView {
  const nodes: GraphNode[] = [
    node('project:p1', 'project', 'gilde-code'),
    node('worktree:w1', 'worktree', 'pty-manager', 'project:p1', 'dirty'),
    node('worktree:w2', 'worktree', 'sidecar-rpc', 'project:p1', 'clean'),
    node('session:s1', 'session', 'claude', 'worktree:w1', 'working'),
    node('session:s2', 'session', 'codex', 'worktree:w1', 'waiting'),
    node('session:s3', 'session', 'claude', 'worktree:w2', 'exited'),
    node('dispatch:t1', 'dispatch', 'ring buffer', 'project:p1', 'working'),
    node('run:r1', 'run', 'claude', 'worktree:w2', 'done'),
    node('orphan:4242', 'orphan', 'claude', null, 'orphan'),
  ];
  nodes[3].meta = { vendor: 'claude', worktreeId: 'w1', pid: 101 };
  nodes[4].meta = { vendor: 'codex', worktreeId: 'w1', pid: 102 };
  nodes[5].meta = { vendor: 'claude', worktreeId: 'w2', pid: 103 };
  nodes[7].meta = { vendor: 'claude', worktreeId: 'w2' };
  nodes[8].meta = { pid: 4242, worktreeId: 'w1' };
  nodes[3].metrics = { rssBytes: 1_200_000_000, elapsedMs: 252_000, outputRate: [1, 4, 2] };

  const edges: GraphEdge[] = [
    edge('contains', 'project:p1', 'worktree:w1'),
    edge('contains', 'project:p1', 'worktree:w2'),
    edge('contains', 'project:p1', 'dispatch:t1'),
    edge('contains', 'worktree:w1', 'session:s1', true),
    edge('contains', 'worktree:w1', 'session:s2'),
    edge('contains', 'worktree:w2', 'session:s3'),
    edge('contains', 'worktree:w2', 'run:r1'),
    edge('dispatched-to', 'dispatch:t1', 'run:r1'),
    edge('runs-in', 'orphan:4242', 'worktree:w1'),
  ];

  const graph = emptyGraph();
  const { nodes: nodeMap, edges: edgeMap } = applyOps(graph, [
    ...nodes.map((n): GraphOp => ({ op: 'upsertNode', node: n })),
    ...edges.map((e): GraphOp => ({ op: 'upsertEdge', edge: e })),
  ]);
  return {
    ...graph,
    rev: 1,
    nodes: nodeMap,
    edges: edgeMap,
    topologyHash: topologyDigest(nodeMap.keys(), edgeMap.keys()),
    lastPatchAt: Date.now(),
  };
}

function seedStore(graph: GraphView, patch: Partial<AppStore> = {}): void {
  useAppStore.setState({
    graph,
    ui: laneUiState(),
    toasts: [],
    agents: [],
    sidecar: null,
    ...patch,
  });
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
}

// ---------------------------------------------------------------------------
// 1. Layout
// ---------------------------------------------------------------------------

test('dagre lays the ranks out left to right', () => {
  const graph = seededGraph();
  const positions = computeLayout([...graph.nodes.values()], [...graph.edges.values()], {});
  const project = positions.get('project:p1')!;
  const worktree = positions.get('worktree:w1')!;
  const session = positions.get('session:s1')!;
  assert(worktree.x > project.x, 'worktrees sit right of their project');
  assert(session.x > worktree.x, 'sessions sit right of their worktree');
  assertEqual(positions.size, graph.nodes.size, 'every node was placed');
});

test('siblings of one kind get distinct positions', () => {
  // Regression: dagre writes the computed x/y into the label object it was
  // handed, so passing the shared `NODE_SIZE[kind]` entry stacked every node of
  // a kind on the last one's coordinates — three sessions drawn as one.
  const graph = seededGraph();
  const positions = computeLayout([...graph.nodes.values()], [...graph.edges.values()], {});
  const seen = new Set<string>();
  for (const [id, at] of positions) {
    const key = `${at.x},${at.y}`;
    assert(!seen.has(key), `${id} landed on top of another node at ${key}`);
    seen.add(key);
  }
});

test('a metrics-only patch never re-runs dagre and never moves a node', () => {
  const cache = new LayoutCache();
  let graph = seededGraph();
  const first = cache.positions(graph, {});
  const before = JSON.stringify([...first.entries()].sort());

  for (let i = 0; i < 100; i++) {
    const { nodes, edges, totals } = applyOps(graph, [
      {
        op: 'patchNode',
        id: 'session:s1',
        metrics: { rssBytes: 1_000_000 + i, cpuPct: i / 10, outputRate: [i, i + 1] },
      },
    ]);
    // Exactly what the slice does: a metrics patch leaves the digest alone.
    graph = { ...graph, rev: graph.rev + 1, nodes, edges, totals, lastPatchAt: Date.now() };
    cache.positions(graph, {});
  }

  const after = cache.positions(graph, {});
  assertEqual(cache.runs, 1, 'dagre ran exactly once for 100 metric patches');
  assert(after === first, 'the same Map instance came back, so nothing can have moved');
  assertEqual(JSON.stringify([...after.entries()].sort()), before, 'positions are byte-identical');
  assertEqual(graph.nodes.get('session:s1')!.metrics.rssBytes, 1_000_099, 'the metric did land');
});

test('adding a node re-runs the layout exactly once', () => {
  const cache = new LayoutCache();
  const graph = seededGraph();
  cache.positions(graph, {});
  const { nodes, edges } = applyOps(graph, [
    { op: 'upsertNode', node: node('session:s4', 'session', 'agy', 'worktree:w2') },
  ]);
  const next: GraphView = {
    ...graph,
    nodes,
    edges,
    topologyHash: topologyDigest(nodes.keys(), edges.keys()),
  };
  cache.positions(next, {});
  cache.positions(next, {});
  assertEqual(cache.runs, 2, 'one layout per topology, not per render');
  assert(cache.positions(next, {}).has('session:s4'), 'the new node was placed');
});

test('a node the user dragged keeps its position across a topology change', () => {
  const graph = seededGraph();
  const manual = { 'session:s1': { x: 640, y: 96, pinned: true } };
  const pinnedLayout = computeLayout([...graph.nodes.values()], [...graph.edges.values()], manual);
  assertDeepEqual(pinnedLayout.get('session:s1'), { x: 640, y: 96 }, 'the pin wins over dagre');

  // Topology changes: a node is added, everything is laid out again.
  const { nodes, edges } = applyOps(graph, [
    { op: 'upsertNode', node: node('session:s4', 'session', 'agy', 'worktree:w2') },
  ]);
  const after = computeLayout([...nodes.values()], [...edges.values()], manual);
  assertDeepEqual(after.get('session:s1'), { x: 640, y: 96 }, 'and survives the change');
  assert(after.get('session:s2')!.x !== 640, 'unpinned nodes still follow dagre');
});

test('the manual key ignores everything except pinned coordinates', () => {
  assertEqual(manualKey({}), '', 'nothing pinned');
  assertEqual(manualKey({ a: { x: 1, y: 2, pinned: false } }), '', 'unpinned entries do not count');
  assertEqual(manualKey({ a: { x: 1, y: 2, pinned: true } }), 'a@1.0,2.0', 'one pin');
});

test('pinning writes through the layout store and reset clears it', () => {
  resetLayoutStore();
  assertDeepEqual(manualPositions(null), {}, 'nothing pinned to start with');
  // A long debounce so the test never reaches the Tauri command.
  pinNode(null, 'session:s1', { x: 10, y: 20 }, 60_000);
  assertDeepEqual(manualPositions(null), { 'session:s1': { x: 10, y: 20, pinned: true } }, 'pinned');
  assertDeepEqual(manualPositions('p1'), {}, 'layouts are scoped per project');
  clearPins(null, 60_000);
  assertDeepEqual(manualPositions(null), {}, 'reset removes every pin');
  resetLayoutStore();
});

test('two hundred nodes lay out once and then cost nothing to pan', () => {
  // Acceptance §12 is about pan/zoom at 200 nodes. Panning is a transform on
  // the React Flow viewport: it re-enters neither the layout nor the node
  // components, and the arrays handed to React Flow are memoized on the
  // topology. What this test pins is the part that could regress in code —
  // the layout is computed once and every later frame reuses it.
  const nodes: GraphNode[] = [node('project:p', 'project', 'p')];
  const edges: GraphEdge[] = [];
  for (let w = 0; w < 20; w++) {
    nodes.push(node(`worktree:w${w}`, 'worktree', `wt-${w}`, 'project:p', 'clean'));
    edges.push(edge('contains', 'project:p', `worktree:w${w}`));
    for (let s = 0; s < 9; s++) {
      const id = `session:s${w}-${s}`;
      nodes.push(node(id, 'session', 'claude', `worktree:w${w}`, 'working'));
      edges.push(edge('contains', `worktree:w${w}`, id));
    }
  }
  assertEqual(nodes.length, 201, 'a 200-node canvas');

  const started = Date.now();
  const positions = computeLayout(nodes, edges, {});
  const elapsed = Date.now() - started;
  assertEqual(positions.size, nodes.length, 'every node placed');
  assert(elapsed < 500, `dagre took ${elapsed}ms for 200 nodes`);

  const graph = emptyGraph();
  const applied = applyOps(graph, [
    ...nodes.map((n): GraphOp => ({ op: 'upsertNode', node: n })),
    ...edges.map((e): GraphOp => ({ op: 'upsertEdge', edge: e })),
  ]);
  const view: GraphView = {
    ...graph,
    nodes: applied.nodes,
    edges: applied.edges,
    topologyHash: topologyDigest(applied.nodes.keys(), applied.edges.keys()),
  };
  const cache = new LayoutCache();
  const first = cache.positions(view, {});
  for (let frame = 0; frame < 60; frame++) cache.positions(view, {});
  assertEqual(cache.runs, 1, 'sixty frames of panning re-enter dagre zero times');
  assert(cache.positions(view, {}) === first, 'and reuse the same position map');
});

test('node sizes are fixed per kind so dagre reserves the right space', () => {
  assert(nodeSize('session').width >= 168, 'at least the mockup box');
  assertEqual(nodeSize('session').width, nodeSize('run').width, 'both lanes look alike');
  assert(nodeSize('session').height > nodeSize('project').height, 'sessions carry more rows');
});

// ---------------------------------------------------------------------------
// 2. Filters and search
// ---------------------------------------------------------------------------

test('with no filters every node is on the canvas', () => {
  const graph = seededGraph();
  const { visible, matched } = filterGraph(graph, DEFAULT_FILTERS);
  assertEqual(visible.size, graph.nodes.size, 'every node is visible');
  assertEqual(matched.size, 0, 'no query means no highlight');
});

test('"active only" keeps working sessions and the ancestors that carry them', () => {
  const graph = seededGraph();
  const { visible } = filterGraph(graph, { ...DEFAULT_FILTERS, activeOnly: true });
  assert(visible.has('session:s1'), 'working session');
  assert(visible.has('session:s2'), 'waiting session');
  assert(!visible.has('session:s3'), 'exited session is out');
  assert(!visible.has('run:r1'), 'finished run is out');
  assert(visible.has('worktree:w1'), 'its worktree stays so the edge has an end');
  assert(visible.has('project:p1'), 'and so does the project');
  assert(!visible.has('worktree:w2'), 'a worktree with nothing active is out');
});

test('"hide exited" drops finished work only', () => {
  const graph = seededGraph();
  const { visible } = filterGraph(graph, { ...DEFAULT_FILTERS, hideExited: true });
  assert(!visible.has('session:s3'), 'exited');
  assert(!visible.has('run:r1'), 'done');
  assert(visible.has('session:s1'), 'working stays');
  assert(visible.has('orphan:4242'), 'an orphan is not "finished" — it is still running');
});

test('the project filter scopes the canvas, orphans included', () => {
  const graph = seededGraph();
  const { visible } = filterGraph(graph, { ...DEFAULT_FILTERS, projectId: 'project:p1' });
  assert(visible.has('session:s1'), 'a session of this project');
  assert(visible.has('orphan:4242'), 'the orphan names the worktree it was found in');

  const other = filterGraph(graph, { ...DEFAULT_FILTERS, projectId: 'project:missing' });
  assertEqual(other.visible.size, 0, 'an unknown project scopes everything out');
});

test('a status filter narrows to one status and keeps the path to it', () => {
  const graph = seededGraph();
  const { visible } = filterGraph(graph, { ...DEFAULT_FILTERS, status: 'waiting' });
  assertDeepEqual(
    [...visible].sort(),
    ['project:p1', 'session:s2', 'worktree:w1'],
    'the waiting session and the path to it',
  );
});

test('search highlights matches without changing the shape of the graph', () => {
  const graph = seededGraph();
  const { visible, matched } = filterGraph(graph, { ...DEFAULT_FILTERS, query: 'codex' });
  assertEqual(visible.size, graph.nodes.size, 'nothing is hidden by a query');
  assertDeepEqual([...matched], ['session:s2'], 'only the codex session matched');

  const none = filterGraph(graph, { ...DEFAULT_FILTERS, query: 'zzz' });
  assertEqual(none.matched.size, 0, 'a query that matches nothing highlights nothing');
});

test('the project dropdown lists the projects in the projection', () => {
  assertDeepEqual(
    projectOptions(seededGraph()).map((p) => p.id),
    ['project:p1'],
    'one project in the projection',
  );
});

// ---------------------------------------------------------------------------
// 3. React Flow arrays
// ---------------------------------------------------------------------------

test('flow nodes carry no live data, so metrics cannot rebuild them', () => {
  const graph = seededGraph();
  const positions = computeLayout([...graph.nodes.values()], [...graph.edges.values()], {});
  const { visible, matched } = filterGraph(graph, DEFAULT_FILTERS);
  const nodes = buildFlowNodes(graph, positions, visible, matched, false);
  assertEqual(nodes.length, graph.nodes.size, 'one flow node per graph node');
  for (const n of nodes) {
    assertDeepEqual(n.data, {}, `${n.id} carries no snapshot of its own state`);
    assertEqual(n.hidden, false, `${n.id} is on the canvas`);
  }
  assertEqual(nodes[0].id, 'dispatch:t1', 'the array is sorted, so it is stable');
  assertEqual(nodes.find((n) => n.id === 'session:s1')!.type, 'session', 'kind picks the component');
});

test('a filtered node is hidden and an unmatched node is dimmed', () => {
  const graph = seededGraph();
  const positions = computeLayout([...graph.nodes.values()], [...graph.edges.values()], {});
  const filters = { ...DEFAULT_FILTERS, hideExited: true, query: 'codex' };
  const { visible, matched } = filterGraph(graph, filters);
  const nodes = buildFlowNodes(graph, positions, visible, matched, true);
  const byId = new Map(nodes.map((n) => [n.id, n]));
  assertEqual(byId.get('session:s3')!.hidden, true, 'exited is hidden');
  assertEqual(byId.get('session:s2')!.className, undefined, 'the match is not dimmed');
  assertEqual(byId.get('session:s1')!.className, 'gilde-dim', 'everything else is');
});

test('an edge is hidden when either end is', () => {
  const graph = seededGraph();
  const { visible } = filterGraph(graph, { ...DEFAULT_FILTERS, activeOnly: true });
  const edges = buildFlowEdges(graph, visible);
  const byId = new Map(edges.map((e) => [e.id, e]));
  assertEqual(byId.get('contains:worktree:w1>session:s1')!.hidden, false, 'both ends visible');
  assertEqual(byId.get('contains:worktree:w2>run:r1')!.hidden, true, 'the run was filtered out');
  assertEqual(byId.get('contains:worktree:w1>session:s1')!.type, 'status', 'one edge type');
});

// ---------------------------------------------------------------------------
// 4. Navigation
// ---------------------------------------------------------------------------

test('local focus resolves the same entities graph_focus does', () => {
  const graph = seededGraph();
  assertDeepEqual(
    localFocus(graph, 'session:s1'),
    { kind: 'session', sessionId: 's1', worktreeId: 'w1' },
    'a session resolves to itself and its worktree',
  );
  assertDeepEqual(
    localFocus(graph, 'run:r1'),
    { kind: 'run', worktreeId: 'w2' },
    'a run resolves to its worktree',
  );
  assertDeepEqual(
    localFocus(graph, 'worktree:w2'),
    { kind: 'worktree', worktreeId: 'w2' },
    'a worktree resolves to itself',
  );
  assertEqual(localFocus(graph, 'session:missing'), null, 'an unknown node resolves to nothing');
  assertEqual(localFocus(graph, 'banana:1'), null, 'and so does a malformed id');
});

test('opening a worktree finds the session that moved most recently', () => {
  const graph = seededGraph();
  graph.nodes.set('session:s2', { ...graph.nodes.get('session:s2')!, updatedAt: 900 });
  assertEqual(newestWorkUnder(graph, 'worktree:w1')!.id, 'session:s2', 'the newest session');
  assertEqual(newestWorkUnder(graph, 'worktree:missing'), null, 'nothing under an unknown worktree');
});

// ---------------------------------------------------------------------------
// 5. Rendering
// ---------------------------------------------------------------------------

test('the dashboard renders as a panel from the graph store alone', () => {
  const graph = seededGraph();
  seedStore(graph);
  const html = renderToStaticMarkup(
    <GraphDashboard
      descriptor={{ view: 'graph' }}
      tab={{ id: 'tab-graph', title: '그래프', descriptor: { view: 'graph' }, closable: true }}
      paneId="pane-1"
      active
    />,
  );
  assert(html.includes('class="gilde-graph"'), 'it is a pane, not a modal');
  assert(!html.includes('sheet-backdrop'), 'nothing about it is modal');
  assert(html.includes('react-flow'), 'the canvas mounted');
  assert(html.includes('gilde-legend'), 'the legend is on the canvas');
  assert(html.includes('그래프'), 'the header is localized');
  assert(!html.includes('undefined'), 'nothing rendered the string "undefined"');
});

test('the empty canvas says so instead of drawing nothing', () => {
  seedStore(emptyGraph());
  const html = renderToStaticMarkup(
    <GraphDashboard
      descriptor={{ view: 'graph' }}
      tab={{ id: 'tab-graph', title: '그래프', descriptor: { view: 'graph' }, closable: true }}
      paneId="pane-1"
      active
    />,
  );
  assert(html.includes('표시할 노드가 없습니다.'), 'the honest Korean empty state');
  assert(!html.includes('react-flow__node'), 'and not a single fabricated node');
});

test('the tiles view renders the same snapshot with a status word on every card', () => {
  const graph = seededGraph();
  seedStore(graph);
  const visible = [...graph.nodes.values()];
  const html = renderToStaticMarkup(
    <TilesView
      nodes={visible}
      matched={new Set()}
      dimUnmatched={false}
      selectedId={null}
      onSelect={() => undefined}
      onOpen={() => undefined}
    />,
  );
  assertEqual(
    (html.match(/class="tile gilde-tile"/g) ?? []).length,
    visible.length,
    'one card per node in the snapshot',
  );
  // Status is never colour alone: the word is next to the dot on every card.
  assert(html.includes('작업 중'), 'working');
  assert(html.includes('입력 대기'), 'waiting');
  assert(html.includes('종료됨'), 'exited');
  assert(html.includes('고아'), 'orphan');
  assert(html.includes('1.2GB') || html.includes('1.2 GB'), 'live RSS on the session card');
});

test('tiles are ordered by rank, then by label', () => {
  const graph = seededGraph();
  assertDeepEqual(
    sortForTiles([...graph.nodes.values()]).map((n) => n.id),
    [
      'project:p1',
      'worktree:w1',
      'worktree:w2',
      'dispatch:t1',
      'session:s1',
      'session:s3',
      'session:s2',
      'run:r1',
      'orphan:4242',
    ],
    'rank order, then label',
  );
});

test('an edge is coloured only when one of its ends needs a human', () => {
  // A canvas where every line is green says nothing, so `clean` and `working`
  // leave the line neutral; only waiting/stalled, error/orphan and done speak.
  assertEqual(edgeTone('clean'), undefined, 'clean is not an alert');
  assertEqual(edgeTone('working'), undefined, 'neither is working');
  assertEqual(edgeTone('waiting'), 'warn', 'waiting needs a human');
  assertEqual(edgeTone('orphan'), 'error', 'an orphan is a problem');
  assertEqual(edgeTone('done'), 'ok', 'a finished run is worth seeing');
  // The orphan end decides, whichever way the edge points.
  assertEqual(worseTone(edgeTone('orphan'), edgeTone('dirty')), 'error', 'the worse end wins');
  assertEqual(worseTone(undefined, edgeTone('done')), 'ok', 'a single tone survives');
  assertEqual(worseTone(undefined, undefined), undefined, 'neutral by default');
});

test('a session node does not say the agent name twice', () => {
  // Session titles arrive as `"claude · pty-manager"`; the badge already says
  // `claude`, and repeating it inside a 184px box costs the worktree name.
  assertEqual(sessionLabel('claude · pty-manager', 'claude'), 'pty-manager', 'prefix stripped');
  assertEqual(sessionLabel('claude', 'claude'), 'claude', 'a bare vendor title stands');
  assertEqual(sessionLabel('', 'codex'), 'codex', 'an empty title falls back to the vendor');
  assertEqual(sessionLabel('refactor ring', 'claude'), 'refactor ring', 'anything else is kept');
});

test('a session node shows the model its account pins and invents none when it does not', () => {
  // DESIGN §9-6 wants agent + model on the session node. The interactive lane
  // has no model of its own (`session_open` passes the prompt as argv and no
  // model flag), so the model shown is `AccountSlot.modelHint` — the one thing
  // anyone actually pinned for that session — and a session on an unpinned
  // slot shows none rather than a guess.
  const graph = seededGraph();
  const s1 = graph.nodes.get('session:s1')!;
  s1.meta = { ...s1.meta, accountId: 'acct-work' };
  const s2 = graph.nodes.get('session:s2')!;
  s2.meta = { ...s2.meta, accountId: 'acct-ambient' };
  seedStore(graph);
  setAccountModels([
    ['acct-work', 'opus-4-5'],
    ['acct-ambient', '  '], // an empty hint is not a model
  ]);
  try {
    assertEqual(getAccountModel('acct-work'), 'opus-4-5', 'the pinned model resolves');
    assertEqual(getAccountModel('acct-ambient'), undefined, 'a blank hint is no model');
    assertEqual(getAccountModel(undefined), undefined, 'a session with no account has no model');

    const canvas = renderToStaticMarkup(
      <GraphDashboard
        descriptor={{ view: 'graph' }}
        tab={{ id: 'tab-graph', title: '그래프', descriptor: { view: 'graph' }, closable: true }}
        paneId="pane-1"
        active
      />,
    );
    assertEqual(
      (canvas.match(/opus-4-5/g) ?? []).length,
      1,
      'the model is on the one session whose account pins it, and nowhere else',
    );

    const tiles = renderToStaticMarkup(
      <TilesView
        nodes={[...graph.nodes.values()]}
        matched={new Set()}
        dimUnmatched={false}
        selectedId={null}
        onSelect={() => undefined}
        onOpen={() => undefined}
      />,
    );
    assertEqual((tiles.match(/opus-4-5/g) ?? []).length, 1, 'the tiles view agrees with the canvas');
  } finally {
    setAccountModels([]);
  }
});

test('the account model map is a store the canvas can subscribe to', () => {
  // `accounts_list` resolves *after* the first render, and the node arrays are
  // memoised on the topology — so a one-shot read left every session node
  // showing no model until something unrelated changed the graph. The canvas
  // subscribes to this revision instead (`GraphDashboard`), and it may only
  // tick when the contents actually changed, or the subscription would cost a
  // re-layout on every refresh.
  const seen: number[] = [];
  const unsubscribe = subscribeAccountModels(() => seen.push(accountModelsRevision()));
  try {
    const before = accountModelsRevision();
    setAccountModels([['acct-work', 'opus-4-5']]);
    assertEqual(seen.length, 1, 'a new model notifies once');
    assertEqual(accountModelsRevision(), before + 1, 'and moves the revision');

    setAccountModels([['acct-work', 'opus-4-5']]);
    assertEqual(seen.length, 1, 'the same map again is not a change');
    assertEqual(accountModelsRevision(), before + 1, 'so the revision holds');

    setAccountModels([]);
    assertEqual(seen.length, 2, 'and emptying it is');
  } finally {
    unsubscribe();
    setAccountModels([]);
  }
});

test('totals from the projection reach the store untouched', () => {
  const graph = seededGraph();
  const { totals } = applyOps(graph, [
    { op: 'totals', totals: { ...emptyTotals(), sessions: 3, orphans: 1, runsActive: 1 } },
  ]);
  assertEqual(totals.sessions, 3, 'session count');
  assertEqual(totals.orphans, 1, 'orphan count');
});
