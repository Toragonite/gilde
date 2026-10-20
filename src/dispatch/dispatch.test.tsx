/**
 * Dispatch lane unit tests (T24).
 *
 * These pin the claims a type check cannot see:
 *
 *  1. the task slug the composer validates is the one `naming.rs::sanitize`
 *     produces — it is a promise about what Rust will create;
 *  2. quota is advisory: unknown reads "알 수 없음", never "충분함", and no
 *     code path gates a dispatch on it;
 *  3. `interrupted` is not `error` — different colour, different copy,
 *     different choices;
 *  4. a non-streaming vendor walks the *same* fold as a streaming one;
 *  5. streamed text is merged, not written per chunk.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { assert, assertDeepEqual, assertEqual } from '../test/harness';
import { laneTest as test, laneUiState } from '../test/lanes';
import { emptyGraph } from '../store/graphSlice';
import { initialWorktreeState, slugBody } from '../store/worktreeSlice';
import { useAppStore } from '../store/useAppStore';
import type { AppStore } from '../store/useAppStore';
import {
  DEFAULT_PERMISSION_MODE,
  PERMISSION_MODES,
  QUOTA_STALE_MS,
  TextCoalescer,
  appendStream,
  applyTaskDetail,
  elapsedMs,
  foldRunEvent,
  initialComposerState,
  initialDispatchState,
  isResumable,
  isTerminalRun,
  quotaStale,
  quotaTone,
  runsOfTask,
  sanitizeSlug,
  setClipboardWriter,
  slugIssue,
  tailLines,
  taskProgress,
  titleOfPrompt,
  viewOfRun,
  worstWindow,
} from '../store/dispatchSlice';
import type { DispatchState, QuotaEntry, RunView } from '../store/dispatchSlice';
import type {
  AdapterInfo,
  DispatchEventPayload,
  DispatchRun,
  DispatchTask,
  GraphNode,
  QuotaSnapshot,
  RunStatus,
} from '../lib/ipc';
import { AgentPicker, QuotaChip } from './AgentPicker';
import { ResultCard, runNodeStatus } from './ResultCard';
import { ResultCardGrid, shortTaskId, taskNodeStatus } from './ResultCardGrid';
import { DispatchComposerActions, DispatchComposerForm } from './DispatchComposer';

// ---------------------------------------------------------------------------
// Fixtures — shaped exactly like what Rust sends, never like demo data
// ---------------------------------------------------------------------------

function run(id: string, vendor: string, status: RunStatus, patch: Partial<DispatchRun> = {}): DispatchRun {
  return {
    id,
    taskId: 't1',
    vendor: vendor as DispatchRun['vendor'],
    modelRequested: null,
    modelActual: null,
    accountId: null,
    worktreeId: 'w1',
    spawnId: null,
    conversationId: null,
    status,
    note: null,
    routeNote: null,
    text: '',
    textPath: null,
    usage: null,
    costUsd: null,
    startedAt: 1_000,
    finishedAt: null,
    error: null,
    ...patch,
  };
}

function task(patch: Partial<DispatchTask> = {}): DispatchTask {
  return {
    id: 't1',
    projectId: 'p1',
    title: 'PTY reader를 ring buffer로 교체',
    prompt: 'PTY reader를 ring buffer로 교체',
    taskSlug: 'pty-ring',
    baseRef: 'main',
    worktreeStrategy: { strategy: 'perTarget' },
    permissionMode: 'default',
    timeoutMs: 45 * 60_000,
    createdAt: 1_000,
    finishedAt: null,
    status: 'running',
    ...patch,
  };
}

function event(kind: DispatchEventPayload['kind'], payload: unknown): DispatchEventPayload {
  return {
    taskId: 't1',
    runId: 'r1',
    vendor: 'claude',
    kind,
    payload: payload as DispatchEventPayload['payload'],
  };
}

function adapter(vendor: string, installed: boolean, steer: 'mid-run' | 'turn' | 'none'): AdapterInfo {
  return {
    vendor: vendor as AdapterInfo['vendor'],
    capabilities: { resume: true, steer, peerMesh: false, quota: true, stream: true },
    binary: vendor,
    binaryPath: installed ? `/usr/local/bin/${vendor}` : null,
    installed,
  };
}

function worktreeNode(): GraphNode {
  return {
    id: 'worktree:w1',
    kind: 'worktree',
    parentId: 'project:p1',
    label: 'pty-manager',
    status: 'dirty',
    badges: [],
    metrics: {},
    meta: { branch: 'gilde/pty-ring-claude', cwd: '/wt/pty-ring-claude' },
    updatedAt: 1,
  };
}

function dispatchWith(patch: Partial<DispatchState>): DispatchState {
  return { ...initialDispatchState(), ...patch };
}

function stateWithRuns(runs: DispatchRun[], patch: Partial<DispatchState> = {}): DispatchState {
  return applyTaskDetail(dispatchWith(patch), { task: task(), runs });
}

function seedStore(patch: Partial<AppStore>): void {
  useAppStore.setState({
    graph: emptyGraph(),
    ui: laneUiState(),
    toasts: [],
    agents: [],
    sidecar: null,
    sessions: new Map(),
    worktree: initialWorktreeState(),
    dispatch: initialDispatchState(),
    ...patch,
  });
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
}

// ---------------------------------------------------------------------------
// 1. The slug the composer promises is the slug Rust makes (acceptance 1)
// ---------------------------------------------------------------------------

test('sanitizeSlug runs the same [a-z0-9] pipeline as naming.rs::sanitize', () => {
  assertEqual(sanitizeSlug('PTY reader → ring buffer'), 'pty-reader-ring-buffer', 'collapsed');
  assertEqual(sanitizeSlug('trailing dashes ---'), 'trailing-dashes', 'no dash edges');
  assertEqual(sanitizeSlug('café'), 'cafe', 'compatibility fold');
  assertEqual(sanitizeSlug('한글 태스크'), '', 'Hangul drops out, exactly as Rust does');
  assertEqual(sanitizeSlug('Ａ1'), 'a1', 'fullwidth folds');
});

test('the composer refuses exactly what dispatch_create refuses (emptySlug)', () => {
  assertEqual(slugIssue(''), 'empty', 'an empty name is empty');
  assertEqual(slugIssue('   '), 'empty', 'whitespace is empty');
  assertEqual(slugIssue('@@@'), 'unsupported', 'nothing survives the filter');
  assertEqual(slugIssue('한글'), 'unsupported', 'Hangul alone sanitizes away');
  assertEqual(slugIssue('fix login'), null, 'a usable name passes');
});

test('the dispatch slug agrees with the worktree dialog for names it can hold', () => {
  for (const name of ['fix login', 'PTY reader', 'a-b-c 12', 'Ünicode näme']) {
    assertEqual(sanitizeSlug(name), slugBody(name), `${name} sanitizes identically`);
  }
});

test('the card title is the first line of the prompt, like Rust', () => {
  assertEqual(titleOfPrompt('첫 줄\n둘째 줄'), '첫 줄', 'the first line wins');
  assertEqual(titleOfPrompt(''), '', 'an empty prompt has no title');
});

// ---------------------------------------------------------------------------
// 2. Quota is advisory (acceptance 2)
// ---------------------------------------------------------------------------

function snapshot(percent: number, capturedAt?: string): QuotaSnapshot {
  return {
    vendor: 'claude',
    windows: [
      { label: '5h', usedPercent: percent / 2 },
      { label: 'week', usedPercent: percent },
    ],
    allowed: true,
    ...(capturedAt ? { capturedAt } : {}),
  };
}

test('the quota chip reports the worst window and turns amber at 80', () => {
  const low: QuotaEntry = { snapshot: snapshot(40), fetchedAt: 1_000, loading: false, unavailable: false };
  const high: QuotaEntry = { snapshot: snapshot(84), fetchedAt: 1_000, loading: false, unavailable: false };
  assertEqual(worstWindow(low.snapshot)?.label, 'week', 'the worst window is the reported one');
  assertEqual(quotaTone(low), 'neutral', 'below the threshold stays neutral');
  assertEqual(quotaTone(high), 'warn', 'at or above 80 % is amber');
});

test('a snapshot older than 30 minutes is stale, measured from capturedAt', () => {
  const now = 10_000_000;
  const fresh: QuotaEntry = {
    snapshot: snapshot(10, new Date(now - 60_000).toISOString()),
    fetchedAt: now,
    loading: false,
    unavailable: false,
  };
  const old: QuotaEntry = {
    snapshot: snapshot(10, new Date(now - QUOTA_STALE_MS - 1_000).toISOString()),
    fetchedAt: now,
    loading: false,
    unavailable: false,
  };
  assertEqual(quotaStale(fresh, now), false, 'a minute-old capture is fresh');
  assertEqual(quotaStale(old, now), true, 'a capture past 30 minutes is stale');
  assertEqual(quotaStale(undefined, now), false, 'nothing known is not "stale", it is unknown');
});

test('an unknown quota says so and never claims there is room', () => {
  seedStore({});
  const html = renderToStaticMarkup(<QuotaChip entry={undefined} now={1} />);
  assert(html.includes('쿼터 알 수 없음'), 'unknown is stated as unknown');
  assert(!html.includes('충분'), 'it never claims the quota is sufficient');
});

test('the picker offers no agent it cannot run, and says why', () => {
  seedStore({
    dispatch: dispatchWith({
      adapters: [adapter('claude', true, 'mid-run'), adapter('codex', false, 'none')],
    }),
  });
  const html = renderToStaticMarkup(<AgentPicker prompt="" />);
  assert(html.includes('이 머신에 설치되어 있지 않아 고를 수 없습니다.'), 'the reason is rendered');
  assertEqual(
    (html.match(/disabled=""/g) ?? []).length >= 2,
    true,
    'the missing adapter’s chip and model select are both disabled',
  );
});

// ---------------------------------------------------------------------------
// 3. Auto (route) shows its decision before anything runs (acceptance 3)
// ---------------------------------------------------------------------------

test('the Auto target renders the fleet decision and its note before the run', () => {
  seedStore({
    dispatch: dispatchWith({
      adapters: [adapter('claude', true, 'mid-run')],
      composer: {
        ...initialComposerState(),
        open: true,
        targets: { ...initialComposerState().targets, auto: { selected: true, model: '' } },
      },
      route: {
        loading: false,
        decision: { vendor: 'claude', note: 'codex is over its weekly window' },
        error: null,
        forPrompt: 'p',
      },
    }),
  });
  const html = renderToStaticMarkup(<AgentPicker prompt="p" />);
  assert(html.includes('claude(으)로 라우팅됩니다'), 'the decision is shown');
  assert(html.includes('codex is over its weekly window'), 'the note is shown verbatim');
});

// ---------------------------------------------------------------------------
// 4. The card shows the model that actually ran, and the adapter's own words
// ---------------------------------------------------------------------------

test('a finished card shows model_actual, the account, the branch and the note verbatim', () => {
  const finished = run('r1', 'claude', 'done', {
    modelRequested: 'claude-sonnet-4-5',
    modelActual: 'claude-opus-4-1',
    conversationId: 'conv-abc',
    note: 'permission downgraded to acceptEdits',
    finishedAt: 135_000,
    startedAt: 1_000,
    usage: { inputTokens: 3100, outputTokens: 900, costUsd: 0.12 },
    costUsd: 0.12,
    text: 'reader thread를 ring buffer로 교체했습니다.',
  });
  seedStore({ dispatch: stateWithRuns([finished]) });
  const view = viewOfRun(finished);
  const html = renderToStaticMarkup(
    <ResultCard
      view={view}
      focused
      expanded={false}
      compare={false}
      worktree={worktreeNode()}
      onFocus={() => {}}
    />,
  );

  assert(html.includes('claude-opus-4-1'), 'the model that actually ran is the one shown');
  assert(html.includes('ambient login'), 'the account slot reads ambient until M2');
  assert(html.includes('gilde/pty-ring-claude'), 'the worktree branch chip is there');
  assert(html.includes('conv-abc'), 'the conversation id is copyable');
  assert(
    html.includes('permission downgraded to acceptEdits'),
    'the adapter note is passed through unchanged',
  );
  assert(html.includes('2:14'), 'the elapsed clock counts from startedAt to finishedAt');
  assert(html.includes('완료'), 'the status pill carries a label, not only a colour');
});

test('elapsed stops at finishedAt and keeps running before it', () => {
  assertEqual(elapsedMs(run('r1', 'claude', 'done', { finishedAt: 5_000 }), 9_000), 4_000, 'frozen');
  assertEqual(elapsedMs(run('r1', 'claude', 'running'), 9_000), 8_000, 'still counting');
});

// ---------------------------------------------------------------------------
// 5. `interrupted` is not `error` (acceptance 5)
// ---------------------------------------------------------------------------

test('interrupted and error map to different status tokens', () => {
  assertEqual(runNodeStatus('error'), 'error', 'error is the red one');
  assertEqual(runNodeStatus('interrupted'), 'stalled', 'interrupted is amber, not red');
  assertEqual(runNodeStatus('running'), 'working', 'running pulses');
  assertEqual(taskNodeStatus('partial'), 'stalled', 'a partly finished fan-out is amber');
});

test('an interrupted card explains the restart and offers both ways forward', () => {
  const interrupted = run('r1', 'claude', 'interrupted', {
    conversationId: 'conv-xyz',
    finishedAt: 4_000,
  });
  seedStore({ dispatch: stateWithRuns([interrupted]) });
  const html = renderToStaticMarkup(
    <ResultCard
      view={viewOfRun(interrupted)}
      focused={false}
      expanded={false}
      compare={false}
      onFocus={() => {}}
    />,
  );
  assert(html.includes('사이드카가 재시작되어 중단되었습니다'), 'the cause is named');
  assert(html.includes('대화 이어가기'), 'continue the conversation is offered');
  assert(html.includes('다시 실행'), 'run it again is offered');
  assert(html.includes('dsp-interrupted'), 'it uses the amber treatment, not the error one');
  assert(!html.includes('dsp-error'), 'and nothing on it is rendered as an error');
});

test('resume is only offered when the vendor handed back a conversation id', () => {
  assertEqual(isResumable(run('r1', 'codex', 'done', { conversationId: '' })), false, 'empty is no');
  assertEqual(isResumable(run('r1', 'codex', 'done', { conversationId: 'c' })), true, 'set is yes');

  const noId = run('r1', 'codex', 'done', { conversationId: '' });
  seedStore({ dispatch: stateWithRuns([noId]) });
  const html = renderToStaticMarkup(
    <ResultCard view={viewOfRun(noId)} focused={false} expanded={false} compare={false} onFocus={() => {}} />,
  );
  assert(!html.includes('대화 이어가기'), 'a run with no conversation id is not offered a resume');

  const withId = run('r2', 'claude', 'done', { conversationId: 'c1' });
  seedStore({ dispatch: stateWithRuns([withId]) });
  const resumable = renderToStaticMarkup(
    <ResultCard view={viewOfRun(withId)} focused={false} expanded={false} compare={false} onFocus={() => {}} />,
  );
  assert(resumable.includes('대화 이어가기'), 'a resumable run is');

  // The one exception: an interrupted run must offer the choice either way,
  // so there it is disabled with the reason instead of hidden (acceptance 5).
  const cut = run('r3', 'codex', 'interrupted', { conversationId: '' });
  seedStore({ dispatch: stateWithRuns([cut]) });
  const cutHtml = renderToStaticMarkup(
    <ResultCard view={viewOfRun(cut)} focused={false} expanded={false} compare={false} onFocus={() => {}} />,
  );
  assert(cutHtml.includes('대화 ID가 없어 이어갈 수 없습니다'), 'the reason is on the disabled button');
});

// ---------------------------------------------------------------------------
// 6. No Steer button: there is no gateway to steer a run (removed for the preview)
// ---------------------------------------------------------------------------

test('a running card offers no Steer, even for a mid-run adapter', () => {
  const adapters = [adapter('claude', true, 'mid-run'), adapter('codex', true, 'turn')];
  const claude = run('r1', 'claude', 'running');
  const codex = run('r2', 'codex', 'running');
  seedStore({ dispatch: stateWithRuns([claude, codex], { adapters }) });

  const claudeHtml = renderToStaticMarkup(
    <ResultCard view={viewOfRun(claude)} focused={false} expanded={false} compare={false} onFocus={() => {}} />,
  );
  const codexHtml = renderToStaticMarkup(
    <ResultCard view={viewOfRun(codex)} focused={false} expanded={false} compare={false} onFocus={() => {}} />,
  );
  assert(!claudeHtml.includes('Steer') && !codexHtml.includes('Steer'), 'no card renders a Steer button');
  assert(!('steerRun' in useAppStore.getState()), 'and the store has no steer action');
  assert(claudeHtml.includes('터미널 열기'), 'both lanes are bridged from the card');
  assert(codexHtml.includes('worktree 열기'), 'and the worktree is reachable');
  assert(codexHtml.includes('결과 복사'), 'the result can be copied');
});

// ---------------------------------------------------------------------------
// 7. A non-streaming vendor uses the same event shape (acceptance 7)
// ---------------------------------------------------------------------------

test('a codex run with only a done event lands on the same fold a stream walks', () => {
  const base = viewOfRun(run('r1', 'codex', 'running'));

  // Streaming vendor: many text chunks, then done.
  let streamed = base;
  for (const chunk of ['reader ', 'thread를 ', '교체했습니다.']) {
    streamed = foldRunEvent(streamed, event('text', { type: 'text', text: chunk }), 2_000);
  }
  streamed = foldRunEvent(
    streamed,
    event('done', {
      type: 'done',
      result: {
        vendor: 'claude',
        status: 'ok',
        conversationId: 'c1',
        model: 'm1',
        text: 'reader thread를 교체했습니다.',
      },
    }),
    3_000,
  );

  // Non-streaming vendor: exactly one done, same payload shape.
  const oneShot = foldRunEvent(
    base,
    event('done', {
      type: 'done',
      result: {
        vendor: 'codex',
        status: 'ok',
        conversationId: 'c1',
        model: 'm1',
        text: 'reader thread를 교체했습니다.',
      },
    }),
    3_000,
  );

  assertEqual(streamed.stream, oneShot.stream, 'both end with the verbatim result');
  assertEqual(streamed.run.status, oneShot.run.status, 'both end done');
  assertEqual(oneShot.run.modelActual, 'm1', 'the model that ran is recorded');
  assertEqual(oneShot.run.conversationId, 'c1', 'so is the conversation id');
});

test('the fold maps every terminal kind the way Rust does', () => {
  const base = viewOfRun(run('r1', 'claude', 'running'));

  const incomplete = foldRunEvent(
    base,
    event('done', { type: 'done', result: { vendor: 'claude', status: 'incomplete', text: '' } }),
    9,
  );
  assertEqual(incomplete.run.status, 'incomplete', 'incomplete is not "done"');

  const errored = foldRunEvent(base, event('error', { type: 'error', message: 'boom' }), 9);
  assertEqual(errored.run.status, 'error', 'an adapter error is an error');
  assertEqual(errored.run.error, 'boom', 'with the adapter message');

  const cut = foldRunEvent(base, event('interrupted', { reason: 'sidecar generation 3 died' }), 9);
  assertEqual(cut.run.status, 'interrupted', 'the host verdict never becomes "error"');

  const cancelled = foldRunEvent(base, event('cancelled', { reason: 'cancelled' }), 9);
  assertEqual(cancelled.run.status, 'cancelled', 'cancel is its own outcome');

  const routed = foldRunEvent(base, event('routed', { vendor: 'claude', note: 'n' }), 9);
  assertEqual(routed.run.routeNote, 'n', 'the route note is persisted on the run');

  const tool = foldRunEvent(base, event('tool', { type: 'tool', name: 'Edit' }), 9);
  assertEqual(tool.tool, 'Edit', 'the ticker follows the adapter');

  const usage = foldRunEvent(
    base,
    event('usage', { type: 'usage', usage: { inputTokens: 10, costUsd: 0.5 } }),
    9,
  );
  assertEqual(usage.costUsd, 0.5, 'cost comes off the usage event');
});

// ---------------------------------------------------------------------------
// 8. Keyboard and compare mode (acceptance 8)
// ---------------------------------------------------------------------------

test('j/k walk the cards of the selected task and stop at the ends', () => {
  seedStore({
    dispatch: stateWithRuns([
      run('r1', 'claude', 'running'),
      run('r2', 'codex', 'running'),
      run('r3', 'cursor', 'running'),
    ]),
  });
  const store = useAppStore.getState();
  store.selectTask('t1');
  store.focusRun('r1');
  store.moveRunFocus(1);
  assertEqual(useAppStore.getState().dispatch.focusedRunId, 'r2', 'j moves down');
  store.moveRunFocus(1);
  store.moveRunFocus(1);
  assertEqual(useAppStore.getState().dispatch.focusedRunId, 'r3', 'and stops at the last card');
  store.moveRunFocus(-1);
  assertEqual(useAppStore.getState().dispatch.focusedRunId, 'r2', 'k moves up');
});

test('↵ toggles expansion and ⌘⇧C toggles compare mode', () => {
  seedStore({ dispatch: stateWithRuns([run('r1', 'claude', 'done')]) });
  useAppStore.getState().toggleRunExpanded('r1');
  assertEqual(useAppStore.getState().dispatch.expanded.r1, true, 'expanded');
  useAppStore.getState().toggleRunExpanded('r1');
  assertEqual(useAppStore.getState().dispatch.expanded.r1, undefined, 'collapsed again');
  useAppStore.getState().setCompare(true);
  assertEqual(useAppStore.getState().dispatch.compare, true, 'compare mode is store state');
});

test('compare mode pins every card body to one height', () => {
  const finished = run('r1', 'claude', 'done', { text: 'line\nline' });
  seedStore({ dispatch: stateWithRuns([finished], { compare: true }) });
  const html = renderToStaticMarkup(
    <ResultCard view={viewOfRun(finished)} focused={false} expanded={false} compare onFocus={() => {}} />,
  );
  assert(html.includes('data-compare="true"'), 'the body opts into the compare height');
});

// ---------------------------------------------------------------------------
// 9. Streamed text is merged, and the tail is bounded (acceptance 4, 9)
// ---------------------------------------------------------------------------

test('TextCoalescer batches a burst into one write', () => {
  const calls: Map<string, string>[] = [];
  const coalescer = new TextCoalescer((batch) => calls.push(batch), 100);
  for (let i = 0; i < 500; i++) coalescer.push('r1', `${i} `);
  coalescer.push('r2', 'x');
  assertEqual(calls.length, 0, 'nothing is written synchronously');
  assertEqual(coalescer.pendingSize, 2, 'two runs are pending');
  coalescer.flushNow();
  assertEqual(calls.length, 1, '500 chunks became one store write');
  assertEqual(calls[0].size, 2, 'both runs are in the same batch');
  assert(calls[0].get('r1')?.startsWith('0 1 2 '), 'the chunks were concatenated in order');
  assertEqual(calls[0].get('r2'), 'x', 'and the other run kept its own text');
  coalescer.stop();
});

test('the collapsed body shows the last 40 lines and the store keeps a bounded tail', () => {
  const text = Array.from({ length: 100 }, (_, i) => `line ${i}`).join('\n');
  const visible = tailLines(text, 40).split('\n');
  assertEqual(visible.length, 40, 'exactly 40 lines');
  assertEqual(visible[39], 'line 99', 'and they are the last ones');

  const big = appendStream('', 'x'.repeat(200_000));
  assertEqual(big.length, 64 * 1024, 'the in-memory tail is capped; the file keeps the whole answer');
});

// ---------------------------------------------------------------------------
// 10. Restoring a board is dispatch_list + dispatch_get (acceptance 10)
// ---------------------------------------------------------------------------

test('applyTaskDetail rebuilds the cards from the durable rows', () => {
  const detail = {
    task: task({ status: 'partial' as const, finishedAt: 9_000 }),
    runs: [
      run('r1', 'claude', 'done', { text: '결과 A', finishedAt: 4_000 }),
      run('r2', 'codex', 'error', { error: 'timed out', finishedAt: 5_000 }),
    ],
  };
  const state = applyTaskDetail(initialDispatchState(), detail);
  assertDeepEqual(state.runsByTask.t1, ['r1', 'r2'], 'run order survives');
  assertEqual(state.runs.get('r1')?.stream, '결과 A', 'the persisted tail is what the card shows');
  assertEqual(runsOfTask(state, 't1').length, 2, 'both cards come back');
  assertDeepEqual(taskProgress(runsOfTask(state, 't1')), { done: 2, total: 2 }, 'progress folds');
  assertEqual(isTerminalRun('running'), false, 'a running run is not finished');
});

test('a restored board renders both cards and the task row', () => {
  seedStore({
    dispatch: stateWithRuns([
      run('r1', 'claude', 'done', { text: '결과 A', finishedAt: 4_000 }),
      run('r2', 'codex', 'running'),
    ]),
  });
  const html = renderToStaticMarkup(
    <ResultCardGrid
      descriptor={{ view: 'dispatch', taskId: 't1' }}
      tab={{ id: 'tab-1', title: '디스패치', descriptor: { view: 'dispatch' }, closable: true }}
      paneId="pane-1"
      active
    />,
  );
  assert(html.includes('PTY reader를 ring buffer로 교체'), 'the task title is on the board');
  assert(html.includes('결과 A'), 'the finished card shows its restored text');
  assert(html.includes('실행 중…'), 'the running card says so instead of showing a spinner');
  assert(html.includes(shortTaskId('t1')), 'the task handle is rendered');
  assert(!html.includes('undefined'), 'nothing rendered the string "undefined"');
});

test('an empty board offers one sentence and one action', () => {
  seedStore({ dispatch: dispatchWith({ loaded: true }) });
  const html = renderToStaticMarkup(
    <ResultCardGrid
      descriptor={{ view: 'dispatch' }}
      tab={{ id: 'tab-1', title: '디스패치', descriptor: { view: 'dispatch' }, closable: true }}
      paneId="pane-1"
      active={false}
    />,
  );
  assert(html.includes('디스패치가 없습니다.'), 'the empty state is one sentence');
  assert(html.includes('새 디스패치'), 'with exactly one action');
});

// ---------------------------------------------------------------------------
// 11. The composer collects everything DispatchSpec needs (acceptance 1)
// ---------------------------------------------------------------------------

test('the composer renders every field the spec needs, and blocks on a bad slug', () => {
  seedStore({
    dispatch: dispatchWith({
      adapters: [adapter('claude', true, 'mid-run')],
      composer: { ...initialComposerState(), open: true, taskSlug: '한글' },
    }),
  });
  const html = renderToStaticMarkup(<DispatchComposerForm />);
  for (const label of [
    '프롬프트',
    '프로젝트',
    'base ref',
    '태스크 이름',
    'worktree 전략',
    '타임아웃(분)',
    '권한 모드',
  ]) {
    assert(html.includes(label), `the composer collects ${label}`);
  }
  assert(html.includes('영문 소문자나 숫자를 하나 이상'), 'a slug that sanitizes away is refused');
  assert(html.includes('⌘↵'), 'the submit chord is discoverable');
  assert(!html.includes('M2'), 'no always-disabled "arrives in M2" account control (R10)');
  assert(html.includes('Auto (route)'), 'the routing target is offered explicitly');
});

// ---------------------------------------------------------------------------
// M1.5-A-dispatch — BACKLOG 1: a new dispatch may edit its own worktree
// ---------------------------------------------------------------------------

test('a new dispatch starts on the edit rung, not on "ask a human"', () => {
  // `default` means "ask", and a headless run has nobody to ask: claude came
  // back with `write permission not granted` and codex with `workspace is
  // read-only`, so every dispatch finished having changed nothing.
  assertEqual(DEFAULT_PERMISSION_MODE, 'acceptEdits', 'the wire word is claude’s');
  assertEqual(
    initialComposerState().permissionMode,
    DEFAULT_PERMISSION_MODE,
    'the composer opens on it, so submitComposer sends it in the DispatchSpec',
  );
  assertEqual(
    PERMISSION_MODES[0],
    DEFAULT_PERMISSION_MODE,
    'and the select offers it first',
  );
  for (const mode of ['default', 'plan', 'bypassPermissions']) {
    assert(PERMISSION_MODES.includes(mode), `${mode} stays an explicit choice`);
  }
});

test('the permission select explains, in one line, what the run may touch', () => {
  const open = { ...initialComposerState(), open: true };
  seedStore({ dispatch: dispatchWith({ composer: open }) });
  const chosen = renderToStaticMarkup(<DispatchComposerForm />);
  assert(chosen.includes('편집 허용 (acceptEdits)'), 'the option keeps the wire token');
  assert(
    chosen.includes('격리된 worktree 안에서만 편집을 허용합니다'),
    'the isolated worktree is named as the safeguard (UI-GUIDELINES §2-4)',
  );
  assert(chosen.includes('codex는 workspace-write'), 'the per-vendor translation is stated');

  // Picking `default` back is allowed, and says what it costs.
  seedStore({ dispatch: dispatchWith({ composer: { ...open, permissionMode: 'default' } }) });
  const asked = renderToStaticMarkup(<DispatchComposerForm />);
  assert(asked.includes('매번 확인 (default)'), 'the ask rung is still selectable');
  assert(asked.includes('헤드리스 실행에는 물을 사람이 없어'), 'and says why it produces no edit');
});

// ---------------------------------------------------------------------------
// M1.5-A-dispatch — BACKLOG 10: the footer survives a 900 px window
// ---------------------------------------------------------------------------

/**
 * The `outerHTML` of the first `<div>` carrying `cls`, found by counting `div`
 * tags — enough of a parser for markup this file produced itself, and the only
 * way to ask "is X inside Y?" without a DOM.
 */
function divWithClass(html: string, cls: string): string {
  const start = html.search(new RegExp(`<div[^>]*class="[^"]*(?<![\\w-])${cls}(?![\\w-])`));
  if (start < 0) throw new Error(`no <div> with class ${cls}`);
  const tags = /<div\b|<\/div>/g;
  tags.lastIndex = start;
  let depth = 0;
  let match: RegExpExecArray | null;
  while ((match = tags.exec(html)) !== null) {
    depth += match[0] === '</div>' ? -1 : 1;
    if (depth === 0) return html.slice(start, match.index + match[0].length);
  }
  throw new Error(`unbalanced <div> for ${cls}`);
}

test('the composer footer is a sibling of the scrolling body, never inside it', () => {
  seedStore({
    dispatch: dispatchWith({
      adapters: [adapter('claude', true, 'mid-run')],
      composer: { ...initialComposerState(), open: true },
    }),
  });

  // Standalone: the fields scroll, the action row does not move with them.
  const html = renderToStaticMarkup(<DispatchComposerForm />);
  const shell = divWithClass(html, 'dsp-composer');
  const body = divWithClass(html, 'dsp-composer-body');
  assert(body.includes('dsp-prompt'), 'the fields are inside the scroller');
  assert(body.includes('dsp-picker'), 'and so is the agent picker');
  assert(
    !body.includes('dsp-composer-actions'),
    '취소/실행 are outside the element that scrolls (BACKLOG 10)',
  );
  assert(shell.includes('dsp-composer-actions'), 'while still belonging to the composer');

  // In the sheet the row is hoisted one level further, into `.sheet-actions` —
  // `.sheet-body`'s sibling — so it cannot be scrolled away either.
  const hoisted = renderToStaticMarkup(<DispatchComposerForm inlineActions={false} />);
  assert(!hoisted.includes('dsp-composer-actions'), 'the form renders no footer of its own');
  const actions = renderToStaticMarkup(<DispatchComposerActions />);
  assert(actions.includes('취소'), 'the hoisted row still cancels');
  assert(actions.includes('⌘↵'), 'and still shows the submit chord');
});

test('copying the result goes through the injected writer', async () => {
  const finished = run('r1', 'claude', 'done', { text: '결과' });
  seedStore({ dispatch: stateWithRuns([finished]) });
  const written: string[] = [];
  setClipboardWriter(async (text) => {
    written.push(text);
  });
  try {
    const ok = await useAppStore.getState().copyRunResult('r1');
    assertEqual(ok, true, 'the copy succeeded');
    assertDeepEqual(written, ['결과'], 'the verbatim result was written');
  } finally {
    setClipboardWriter(null);
  }
});

// A `RunView` is only ever built by `viewOfRun`/`foldRunEvent`; this keeps the
// type referenced so a shape change here is a compile error, not a silent drift.
const _shape: RunView = viewOfRun(run('r0', 'claude', 'queued'));
void _shape;
