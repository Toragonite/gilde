/**
 * Status evidence and the status pill — BACKLOG M1.5 #4 and #12.
 *
 * Both are "what the user is looking at" bugs, so both are pinned by rendering
 * the real components to static markup rather than by asserting on a helper:
 *
 *  - #4 the inspector claimed there was no evidence while the pill's popover was
 *    listing it. The two surfaces now read the same `StatusEvidence[]`, and the
 *    test renders both and compares the sentences they produced.
 *  - #12 the 유휴 pill drew a confidence arc, which reads as a loading spinner.
 *    The ring belongs to the in-flight statuses; everything at rest gets the dot
 *    the mockup draws.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { assert, assertEqual } from '../test/harness';
import { laneTest as test, laneUiState } from '../test/lanes';
import { emptyGraph } from '../store/graphSlice';
import { useAppStore } from '../store/useAppStore';
import type { AppStore } from '../store/useAppStore';
import { viewOfInfo, withStatus } from '../store/sessionSlice';
import type { SessionView } from '../store/sessionSlice';
import type { GraphNode, PtySessionInfo, SessionStatus, StatusEvidence } from '../lib/ipc';
import { translate } from '../i18n';
import {
  Inspector,
  evidenceSentence,
  showsEvidenceSection,
  statusEvidenceOf,
} from '../shell/Inspector';
import { EvidencePopover } from './EvidencePopover';
import { SessionStatusPill, showsConfidenceRing } from './SessionStatusPill';

// ---------------------------------------------------------------------------
// Fixtures — the exact shapes Rust sends
// ---------------------------------------------------------------------------

const EVIDENCE: StatusEvidence[] = [
  { kind: 'outputQuiet', ms: 4200 },
  { kind: 'foregroundGroup', pgid: 41822, cpuPct: 0.3, rssBytes: 412_000_000 },
  {
    kind: 'patternMatch',
    signatureId: 'claude.waiting.6',
    class: 'waiting',
    line: 'Enter to select · ↑/↓ to navigate · Esc to cancel',
  },
];

function sessionInfo(): PtySessionInfo {
  return {
    id: 'session-1',
    spawnId: 'spawn-1',
    projectId: 'project-1',
    worktreeId: 'worktree-1',
    agent: 'claude',
    accountId: null,
    title: 'claude · pty-manager',
    cwd: '/tmp/wt',
    cols: 80,
    rows: 24,
    pid: 41822,
    pgid: 41822,
    openedAt: 1000,
    detached: false,
    firstRetainedSeq: 0,
    nextSeq: 0,
  };
}

/** The session as the terminal lane knows it, after one status event. */
function sessionView(): SessionView {
  return withStatus(viewOfInfo(sessionInfo()), {
    sessionId: 'session-1',
    status: { kind: 'waitingForInput', prompt: 'Enter to select' },
    confidence: 0.85,
    evidence: EVIDENCE,
    since: 2000,
    at: 2001,
  });
}

/**
 * The graph's copy of that session — **without** evidence, which is the state
 * the inspector was actually rendering: the projection only attaches evidence on
 * a tick where it changed, so a node that has been on screen for a while carries
 * none.
 */
function sessionNode(evidence?: StatusEvidence[]): GraphNode {
  return {
    id: 'session:session-1',
    kind: 'session',
    parentId: 'worktree:worktree-1',
    label: '메뉴에서 선택 대기',
    status: 'waiting',
    confidence: 0.85,
    ...(evidence ? { evidence } : {}),
    badges: [],
    metrics: {},
    meta: { vendor: 'claude', pid: 41822, pgid: 41822 },
    updatedAt: 2001,
  };
}

function seed(patch: Partial<AppStore>): void {
  useAppStore.setState({
    graph: emptyGraph(),
    ui: laneUiState(),
    toasts: [],
    sessions: new Map(),
    ...patch,
  });
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
}

function seedSelectedSession(node: GraphNode, view: SessionView | null): void {
  const graph = emptyGraph();
  graph.nodes.set(node.id, node);
  seed({
    graph,
    ui: { ...laneUiState(), selectedNodeId: node.id },
    sessions: view ? new Map([[view.id, view]]) : new Map(),
  });
}

const t = (key: Parameters<typeof translate>[1], params?: Parameters<typeof translate>[2]) =>
  translate('ko', key, params);

// ---------------------------------------------------------------------------
// BACKLOG M1.5 #4 — one evidence list, two surfaces
// ---------------------------------------------------------------------------

test('the inspector and the pill popover render the same evidence for a session', () => {
  const view = sessionView();
  // The reported case exactly: the live evidence is on the session view and the
  // graph node has none.
  seedSelectedSession(sessionNode(), view);

  const inspector = renderToStaticMarkup(<Inspector />);
  const popover = renderToStaticMarkup(
    <EvidencePopover
      status={view.status}
      confidence={view.confidence}
      evidence={view.evidence}
      onClose={() => {}}
    />,
  );

  assert(
    !inspector.includes(t('inspector.noEvidence')),
    'the inspector no longer claims there is no evidence while the popover shows some',
  );
  for (const item of EVIDENCE) {
    const sentence = evidenceSentence(item, t, 'ko');
    assert(inspector.includes(sentence), `the inspector shows: ${sentence}`);
    assert(popover.includes(sentence), `and the popover shows the same words: ${sentence}`);
  }
  assertEqual(
    statusEvidenceOf(sessionNode(), useAppStore.getState().sessions),
    view.evidence,
    'both surfaces read one array, not two copies of it',
  );
});

test('a session the terminal lane does not know falls back to the projection copy', () => {
  const node = sessionNode(EVIDENCE);
  seedSelectedSession(node, null);

  const html = renderToStaticMarkup(<Inspector />);
  assert(!html.includes(t('inspector.noEvidence')), 'a detached session still shows its evidence');
  assert(
    html.includes(evidenceSentence(EVIDENCE[0], t, 'ko')),
    'from the node, because there is no session view to read',
  );
  assertEqual(
    statusEvidenceOf(node, new Map()),
    node.evidence,
    'the node is the fallback source, never a second live source',
  );
});

test('a session view with no status event yet keeps the projection copy', () => {
  // `session_list` builds the view before the first `gilde://session/status`
  // arrives; its evidence list is empty because nothing has been decided, which
  // must not blank out what the projection already knows.
  const fresh = viewOfInfo(sessionInfo());
  assertEqual(fresh.evidence.length, 0, 'a fresh view starts with nothing');
  const node = sessionNode(EVIDENCE);
  seedSelectedSession(node, fresh);

  assert(
    renderToStaticMarkup(<Inspector />).includes(evidenceSentence(EVIDENCE[0], t, 'ko')),
    'the inspector shows what there is instead of an empty state',
  );
});

// ---------------------------------------------------------------------------
// BACKLOG 25 — the 상태 근거 section belongs to sessions
// ---------------------------------------------------------------------------

function worktreeNode(): GraphNode {
  return {
    id: 'worktree:worktree-1',
    kind: 'worktree',
    parentId: null,
    label: 'pty-manager',
    status: 'clean',
    badges: [],
    metrics: {},
    meta: {},
    updatedAt: 1,
  };
}

test('a selected worktree has no status-evidence section at all', () => {
  // The 2026-09-09 re-verification: selecting a worktree drew
  // `상태 근거 · 기록된 근거가 없습니다`, which claims we looked. Nothing looks —
  // the §6-4 engine runs per pty session, and a worktree's status comes from git.
  const worktree = worktreeNode();
  seedSelectedSession(worktree, null);

  const html = renderToStaticMarkup(<Inspector />);
  assert(
    !html.includes(t('inspector.section.evidence')),
    'the section header is not drawn for a worktree',
  );
  assert(
    !html.includes(t('inspector.noEvidence')),
    'and neither is the empty state under it',
  );
  assert(html.includes(t('inspector.section.supervision')), 'the rest of the panel is untouched');
  assertEqual(showsEvidenceSection(worktree, []), false, 'and the rule says so directly');
  assertEqual(statusEvidenceOf(worktree, new Map()).length, 0, 'there really was nothing to show');
});

test('a session with no status event yet still gets the honest empty state', () => {
  // The other half: a session is a thing the engine reports on, so "no event
  // yet" is news and stays visible (BACKLOG M1.5 #4).
  seedSelectedSession(sessionNode(), null);
  const html = renderToStaticMarkup(<Inspector />);
  assert(html.includes(t('inspector.section.evidence')), 'a session keeps the section');
  assert(html.includes(t('inspector.noEvidence')), 'with the empty state inside it');
  assertEqual(showsEvidenceSection(sessionNode(), []), true, 'and the rule says so');
});

test('a non-session node that does carry evidence still shows it', () => {
  // A fact that exists is never hidden — the rule is about the empty state.
  const worktree = { ...worktreeNode(), evidence: EVIDENCE };
  seedSelectedSession(worktree, null);
  assert(
    renderToStaticMarkup(<Inspector />).includes(evidenceSentence(EVIDENCE[0], t, 'ko')),
    'the section comes back when there is something in it',
  );
  assertEqual(showsEvidenceSection(worktree, EVIDENCE), true, 'a fact is never hidden');
});

test('the screen-stability observation renders as a sentence in both locales', () => {
  // BACKLOG 24's new variant. The inspector switches on `kind`, so a variant it
  // does not know renders as nothing at all.
  const stable: StatusEvidence = { kind: 'screenStable', ms: 3_200 };
  const ko = evidenceSentence(stable, t, 'ko');
  const en = evidenceSentence(
    stable,
    (key, params) => translate('en', key, params),
    'en',
  );
  assert(ko.includes('화면 내용'), `ko sentence: ${ko}`);
  assert(en.toLowerCase().includes('screen unchanged'), `en sentence: ${en}`);

  seedSelectedSession(sessionNode([stable]), null);
  assert(
    renderToStaticMarkup(<Inspector />).includes(ko),
    'and the inspector prints it like every other observation',
  );
});

// ---------------------------------------------------------------------------
// BACKLOG M1.5 #12 — no spinner on a resting session
// ---------------------------------------------------------------------------

test('the confidence ring is drawn only while something is in flight', () => {
  const ringed: SessionStatus[] = [
    { kind: 'working', since: 1 },
    { kind: 'waitingForInput', prompt: '?' },
    { kind: 'stalled', quietMs: 90_000 },
  ];
  for (const status of ringed) {
    assertEqual(showsConfidenceRing(status), true, `${status.kind} keeps the ring`);
  }

  const resting: SessionStatus[] = [
    { kind: 'starting' },
    { kind: 'idle' },
    { kind: 'exited', code: 0 },
    { kind: 'killed' },
    { kind: 'detached' },
    { kind: 'failed', reason: 'spawn' },
  ];
  for (const status of resting) {
    assertEqual(showsConfidenceRing(status), false, `${status.kind} does not`);
  }
});

test('the idle pill shows a status dot, not an arc that reads as a spinner', () => {
  seed({});
  const idle = renderToStaticMarkup(
    <SessionStatusPill status={{ kind: 'idle' }} confidence={0.35} />,
  );
  assert(idle.includes(t('session.status.idle')), 'the pill still names the status');
  assert(!idle.includes('<svg'), 'no ring, and therefore no arc to mistake for a spinner');
  assert(idle.includes('class="dot"'), 'the mockup draws a dot for a resting session');
  assert(
    idle.includes('data-hollow="true"'),
    'and the low confidence is still shown, on the dot instead of the ring',
  );
  assert(!idle.includes('data-pulse'), 'nothing animates on an idle session');

  const working = renderToStaticMarkup(
    <SessionStatusPill status={{ kind: 'working', since: 1 }} confidence={0.9} />,
  );
  assert(working.includes('<svg'), 'a session with work in flight keeps the confidence ring');
  assert(!working.includes('class="dot"'), 'and does not draw both');
});
