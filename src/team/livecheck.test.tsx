/**
 * Live check on a second Mac — the four fixes G-a … G-d.
 *
 *  B1 (G-a) the Team pane opens at its top, and the preview line is an in-flow
 *           row above the one scroll container — it cannot cover a heading;
 *  B2 (G-b) the project-folder list is stable: a folder whose path is gone
 *           stays listed ("Folder not found", with Remove) until removed, the
 *           count is the number of rows shown, and a reload asked for while a
 *           read is in flight is never dropped (no stale list after Remove);
 *  B3 (G-c) the engine's known notice templates read in the user's language
 *           (variable parts kept as written), the sender `engine` reads
 *           "Orchestrator", and an unknown notice is shown as written;
 *  B4 (G-d) Korean says "멤버" for team members, never "구성원".
 */

import { renderToStaticMarkup } from 'react-dom/server';
import type { ReactElement } from 'react';

import fixture from '../../src-tauri/tests/fixtures/cco/team-snapshot.json';
import { en } from '../i18n/en';
import { ko } from '../i18n/ko';
import { translate } from '../i18n';
import type { MessageKey } from '../i18n';
import type { CcoTeamMessage, CcoTeamView, Project } from '../lib/ipc';
import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import { initialTeamState } from '../store/teamSlice';
import { cancelPersistLayout, initialUiState } from '../store/uiSlice';
import { initialWorktreeState } from '../store/worktreeSlice';
import { useAppStore } from '../store/useAppStore';
import { folderMissing, groupFolders, scopeFolder } from './grouping';
import { NOTICE_TEMPLATES, localizeNotice, noticeDisplay, senderDisplay, warningDisplay } from './notices';
import { TeamPane, reasonSentence, resetTeamScroll } from './TeamPane';
import { TeamSidebar, folderRowCount } from './TeamSidebar';
import teamCss from './team.css?raw';
import teamPaneSource from './TeamPane.tsx?raw';

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

function realView(patch: Partial<CcoTeamView> = {}): CcoTeamView {
  const raw = (fixture as unknown as { view: CcoTeamView }).view;
  const view: CcoTeamView = JSON.parse(JSON.stringify(raw));
  return { ...view, generation: 1, believed: true, ...patch };
}

function message(seq: number, from: string, kind: string, body: string): CcoTeamMessage {
  return { seq, id: `m${seq}`, ts: 1_700_000_000_000 + seq, from, to: 'lead', kind, urgent: false, cut: false, bytes: body.length, body };
}

function project(id: string, path: string, pathExists?: boolean): Project {
  return {
    id,
    path,
    canonicalPath: path,
    repoSlug: `${id}-0000`,
    name: id,
    color: '#000000', // lint-ui: allow
    defaultBaseRef: 'main',
    addedAt: 1,
    lastOpenedAt: null,
    ...(pathExists === undefined ? {} : { pathExists }),
  };
}

function seed(views: CcoTeamView[], projects: Project[] = []): void {
  cancelPersistLayout();
  useAppStore.setState({
    ui: initialUiState(),
    team: {
      ...initialTeamState(),
      watching: true,
      views: new Map(views.map((v) => [v.teamId, v])),
      selectedTeamId: views[0]?.teamId ?? null,
      list: {
        available: true,
        supported: true,
        dataRoot: '/h/.claude-code-orchestrator',
        teamMode: 'on',
        hosts: [],
        teams: [],
        generation: 1,
      },
    },
    worktree: {
      ...initialWorktreeState(),
      loaded: true,
      roster: projects.map((p) => ({ project: p, worktrees: [] })),
    },
    startTeamWatch: () => () => undefined,
  });
}

function renderNode(node: ReactElement): string {
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  return renderToStaticMarkup(node);
}

const renderPane = () =>
  renderNode(
    <TeamPane
      descriptor={{ view: 'team' }}
      tab={{ id: 't1', title: 'team', descriptor: { view: 'team' }, closable: true }}
      paneId="pane-1"
      active
    />,
  );

function textOf(html: string): string {
  return html
    .replace(/<[^>]+>/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#x27;/g, "'")
    .replace(/\s+/g, ' ');
}

const tKo = (key: MessageKey, params?: Record<string, string | number>) => translate('ko', key, params);
const tEn = (key: MessageKey, params?: Record<string, string | number>) => translate('en', key, params);

/** The CSS rule body of one selector in team.css. */
function ruleOf(css: string, selector: string): string {
  const at = css.indexOf(`${selector} {`);
  assert(at >= 0, `team.css has a ${selector} rule`);
  return css.slice(at, css.indexOf('}', at));
}

// ---------------------------------------------------------------------------
// B1 (G-a)
// ---------------------------------------------------------------------------

test('G-a: the preview line is an in-flow row above the one scroll container', () => {
  const view = realView();
  seed([view]);
  const html = renderPane();
  const hdr = html.indexOf('team-hdr');
  const banner = html.indexOf('team-preview');
  const body = html.indexOf('data-testid="team-body"');
  const messages = html.indexOf(tKo('team.section.messages', { n: view.messages.length }));
  assert(hdr >= 0 && banner > hdr, 'the header, then the preview line');
  assert(body > banner, 'the scroll container starts AFTER the preview line (a sibling below it)');
  assert(messages > body, 'the messages heading is inside the scroll container, below the banner');
  // The banner closes before the body opens: it is not a child of the scroller.
  const bannerClose = html.indexOf('</div>', banner);
  assert(bannerClose < body, 'the preview line is not inside the scroll container');
  // The roster and the board come before the messages: opening at the top shows the team header first.
  assert(html.indexOf('team-head') > body && html.indexOf('team-head') < messages, 'the team header is first in the body');
});

test('G-a: nothing in the Team pane is layered or positioned over the content (layout, not z-index)', () => {
  const css = teamCss as string;
  assert(!/position\s*:/.test(css), 'team.css positions nothing');
  assert(!/z-index/.test(css), 'team.css layers nothing');
  const team = ruleOf(css, '.team');
  assert(/display:\s*flex/.test(team) && /flex-direction:\s*column/.test(team), '.team stacks its rows');
  assert(/overflow:\s*hidden/.test(team), '.team clips: only the body can scroll');
  assert(/flex:\s*none/.test(ruleOf(css, '.team > :not(.team-body)')), 'header and banner keep their height');
  const body = ruleOf(css, '.team-body');
  assert(/min-height:\s*0/.test(body), 'the body shrinks to the space left');
  assert(/overflow-anchor:\s*none/.test(body), 'content filling in above does not drag the view down');
});

test('G-a: the pane opens at the top — the scroll offset is reset per team, and nothing scrolls it down', () => {
  const scroller = { scrollTop: 640 };
  resetTeamScroll(scroller);
  assertEqual(scroller.scrollTop, 0, 'a body left at the messages goes back to the team header');
  resetTeamScroll(null); // not mounted: no-op, no throw
  const source = teamPaneSource as string;
  assert(/useLayoutEffect\(\(\) => \{\s*resetTeamScroll\(bodyRef\.current\);\s*\}, \[shownTeamId\]\)/.test(source),
    'the reset runs before paint, every time the shown team changes (and on first show)');
  for (const banned of ['scrollIntoView', 'autoFocus', '.focus(']) {
    assert(!source.includes(banned), `TeamPane never moves the view or focus on its own (${banned})`);
  }
});

// ---------------------------------------------------------------------------
// B2 (G-b)
// ---------------------------------------------------------------------------

test('G-b: a folder whose path is gone stays listed, marked, with Remove; the count is the rows shown', () => {
  const present = project('api', '/w/api', true);
  const gone = project('ops', '/old/ops', false);
  seed([realView()], [present, gone]);
  const html = renderNode(<TeamSidebar />);
  const text = textOf(html);
  assert(text.includes('ops') && text.includes('api'), 'both folders are listed');
  assert(text.includes(tKo('team.folders.missing')), '"폴더를 찾을 수 없음" is shown');
  assertEqual((html.match(/data-missing="true"/g) ?? []).length, 1, 'only the gone one is marked');
  assert(html.includes(`aria-label="${tKo('team.folders.remove', { name: 'ops' })}"`), 'the gone folder keeps its Remove');
  assert(/data-testid="team-folders-count">2</.test(html), 'the header counts the two rows shown');
  assert(!text.includes(tKo('team.folders.none')), 'never "no folder added" next to a listed folder');
  assertEqual(tKo('team.folders.missing'), '폴더를 찾을 수 없음', 'the Korean wording');
  assertEqual(tEn('team.folders.missing'), 'Folder not found', 'the English wording');

  // The count is derived from the same groups the list renders.
  assertEqual(folderRowCount(groupFolders([present, gone], new Map())), 2, 'count = rows');
  assertEqual(folderRowCount([]), 0, 'no rows, zero');
  assert(folderMissing(gone) && !folderMissing(present), 'flag read');
  assert(!folderMissing(project('old', '/x')), 'an older backend (no flag) reads as present');
  // The automatic "This workspace" folder prefers one that exists; a chosen one stands.
  assertEqual(scopeFolder([gone, present], null)?.id, 'api', 'default skips a gone folder');
  assertEqual(scopeFolder([gone, present], 'ops')?.id, 'ops', 'the user\'s choice stands');
});

test('G-b: a reload asked for while a read is in flight is not dropped — Remove never leaves a stale list', async () => {
  // The backend: a project table, a read that answers what it saw WHEN CALLED,
  // the first read slow (the boot read racing the user's click).
  let rows: Project[] = [project('ops', '/old/ops', false)];
  let listCalls = 0;
  const g = globalThis as unknown as { window?: unknown };
  const hadWindow = 'window' in g;
  const previous = g.window;
  g.window = {
    __TAURI_INTERNALS__: {
      invoke: async (cmdName: string, args: Record<string, unknown>) => {
        if (cmdName === 'project_list') {
          listCalls += 1;
          const seen = rows.map((r) => ({ ...r }));
          await new Promise((resolve) => setTimeout(resolve, listCalls === 1 ? 40 : 1));
          return seen;
        }
        if (cmdName === 'worktree_list') return [];
        if (cmdName === 'project_remove') {
          rows = rows.filter((r) => r.id !== args.id);
          return null;
        }
        throw new Error(`unexpected command ${cmdName}`);
      },
    },
  };
  try {
    seed([]);
    useAppStore.setState({ worktree: initialWorktreeState() });
    const boot = useAppStore.getState().loadRoster(); // in flight: it saw `ops`
    await new Promise((resolve) => setTimeout(resolve, 5));
    await useAppStore.getState().removeProject('ops'); // removes, then reloads
    await boot;
    await useAppStore.getState().loadRoster(); // and a later refresh agrees
    assertDeepEqual(
      useAppStore.getState().worktree.roster.map((e) => e.project.id),
      [],
      'after Remove the list is the table as it is now, not the read that started before it',
    );
    assert(listCalls >= 2, 'the reload asked during the in-flight read did read again');
    assertEqual(useAppStore.getState().worktree.loading, false, 'nothing left loading');

    // Steady state: reads agree with each other, read after read.
    rows = [project('ops', '/old/ops', false)];
    await useAppStore.getState().loadRoster();
    const first = useAppStore.getState().worktree.roster.map((e) => `${e.project.id}:${String(e.project.pathExists)}`);
    await useAppStore.getState().loadRoster();
    const second = useAppStore.getState().worktree.roster.map((e) => `${e.project.id}:${String(e.project.pathExists)}`);
    assertDeepEqual(first, ['ops:false'], 'a gone folder is listed, flagged');
    assertDeepEqual(second, first, 'and stays so across refreshes');
  } finally {
    if (hadWindow) g.window = previous;
    else delete g.window;
  }
});

// ---------------------------------------------------------------------------
// B3 (G-c)
// ---------------------------------------------------------------------------

/** One real engine sentence per known template, as the engine prints it. */
const SAMPLES: Record<string, { text: string; params: Record<string, string> }> = {
  'team.notice.teamIdle': { text: 'the team is idle', params: {} },
  'team.notice.movedToReview': {
    text: 'alpha moved b1 "Parse the config" to review: All tests pass; see report.md',
    params: { member: 'alpha', id: 'b1', title: 'Parse the config', report: 'All tests pass; see report.md' },
  },
  'team.notice.blockedOn': {
    text: 'beta is blocked on b2 "Wire the API": waiting for the schema',
    params: { member: 'beta', id: 'b2', title: 'Wire the API', note: 'waiting for the schema' },
  },
  'team.notice.taskDone': { text: 'b3 "Docs" was marked done by the lead.', params: { id: 'b3', title: 'Docs', by: 'lead' } },
  'team.notice.taskUnassigned': {
    text: 'b3 "Docs" was unassigned from you by the user — stop working on it.',
    params: { id: 'b3', title: 'Docs', by: 'user' },
  },
  'team.notice.taskCancelled': {
    text: 'b3 "Docs" was cancelled by the lead — stop working on it.',
    params: { id: 'b3', title: 'Docs', by: 'lead' },
  },
  'team.notice.taskReassigned': {
    text: 'b4 was reassigned to gamma by the lead — stop working on it',
    params: { id: 'b4', to: 'gamma', by: 'lead' },
  },
  'team.notice.stillClaimed': {
    text: 'alpha ended a turn with b1 still claimed; its last words: almost there',
    params: { member: 'alpha', id: 'b1', words: 'almost there' },
  },
  'team.notice.blockedQuota': {
    text: 'alpha is blocked: every claude account is out of quota (earliest reset 14:30 on work).',
    params: { member: 'alpha', provider: 'claude', time: '14:30', account: 'work' },
  },
  'team.notice.blocked': {
    text: 'alpha is blocked: the CLI exited with code 1',
    params: { member: 'alpha', reason: 'the CLI exited with code 1' },
  },
  'team.notice.lostTurnFree': {
    text: "alpha's lost turn is past its time limit; alpha is free again.",
    params: { member: 'alpha', member2: 'alpha' },
  },
  'team.notice.freshNextTurn': {
    text: 'alpha starts a fresh conversation on its next turn (its context is nearly full); it picks up from a hand-off note in the same worktree.',
    params: { member: 'alpha', why: 'its context is nearly full' },
  },
  'team.notice.contextOut': {
    text: "alpha's conversation ran out of context; it continues in a fresh conversation from a hand-off note in the same worktree.",
    params: { member: 'alpha' },
  },
  'team.notice.resumeFailed': {
    text: "alpha's CLI could not resume its conversation; it continues in a fresh conversation from a hand-off note in the same worktree.",
    params: { member: 'alpha' },
  },
  'team.notice.contextLost': {
    text: "alpha's CLI did not find its conversation and started a new one; alpha was re-briefed and continues in the same worktree",
    params: { member: 'alpha', member2: 'alpha' },
  },
  'team.notice.failoverLimit': {
    text: 'alpha continues on claude/home (work is at its limit until 18:00); it picks up from a hand-off note in the same worktree.',
    params: { member: 'alpha', account: 'claude/home', old: 'work', time: '18:00' },
  },
  'team.notice.failoverLoggedOut': {
    text: 'alpha continues on claude/home (work is logged out); it picks up from a hand-off note in the same worktree.',
    params: { member: 'alpha', account: 'claude/home', old: 'work' },
  },
  'team.notice.failoverRemoved': {
    text: 'alpha continues on claude/home (work is no longer in the registry); it picks up from a hand-off note in the same worktree.',
    params: { member: 'alpha', account: 'claude/home', old: 'work' },
  },
  'team.notice.quotaWait': {
    text: 'alpha is paused: claude/work is at its 5-hour limit until 16:05; it continues then.',
    params: { member: 'alpha', account: 'claude/work', window: '5-hour', time: '16:05' },
  },
  'team.notice.budgetDay': {
    text: 'the team has used its 300 turns of today; alpha continues at 0:00.',
    params: { n: '300', member: 'alpha', time: '0:00' },
  },
  'team.notice.budgetHour': {
    text: 'alpha has used its 20 turns of this hour; alpha continues at 15:12.',
    params: { member: 'alpha', n: '20', member2: 'alpha', time: '15:12' },
  },
  'team.notice.peerWakes': {
    text: 'alpha has been woken 6 times by teammates without a message from you or board progress; teammate messages to alpha now wait for a message from you or the user.',
    params: { member: 'alpha', n: '6', member2: 'alpha' },
  },
  'team.notice.turnTimeout': {
    text: "alpha's turn 7 stopped after 45 min (turn limit); its changes are on disk",
    params: { member: 'alpha', n: '7', minutes: '45' },
  },
  'team.notice.turnCancelled': {
    text: "alpha's turn 7 was stopped from outside the team engine; it is paused — unpause it when you are ready",
    params: { member: 'alpha', n: '7' },
  },
  'team.notice.noAnswer': { text: 'alpha ended turn 7 with no answer', params: { member: 'alpha', n: '7' } },
  'team.notice.turnCrashed': {
    text: "alpha's turn 7 ended unexpectedly; it is idle and waits for its next message.",
    params: { member: 'alpha', n: '7' },
  },
  'team.notice.turnMemoryStopped': {
    text: "alpha's turn 7 was stopped at 8300 MB of memory (the limit is 8 GiB); its changes are on disk.",
    params: { member: 'alpha', n: '7', mb: '8300' },
  },
  'team.notice.turnMemory': { text: "alpha's turn 7 uses 4200 MB of memory.", params: { member: 'alpha', n: '7', mb: '4200' } },
  'team.notice.turnSilent': {
    text: "alpha's turn 7 has printed nothing for 20 minutes; it is still running.",
    params: { member: 'alpha', n: '7', minutes: '20' },
  },
  'team.notice.memberWroteUser': { text: 'alpha wrote to the user (m42)', params: { member: 'alpha', msg: 'm42' } },
  'team.notice.userWrote': {
    text: 'the user wrote to alpha (m43): please also update the README',
    params: { to: 'alpha', msg: 'm43', text: 'please also update the README' },
  },
  'team.notice.hostEnded': {
    text: 'Team gilde-live was suspended after its host ended; resume it with ~/.claude-code-orchestrator/bin/cco team_resume gilde-live',
    params: { team: 'gilde-live', cco: '~/.claude-code-orchestrator/bin/cco', team2: 'gilde-live' },
  },
  // the team's warnings (TeamState.warnings[].text)
  'team.notice.leadInbox': { text: 'lead inbox: 12 unread', params: { n: '12' } },
  'team.notice.blockedBare': { text: 'alpha is blocked', params: { member: 'alpha' } },
  'team.notice.turnsOnly': { text: 'alpha is turns-only', params: { member: 'alpha' } },
  'team.notice.turnsOnlyWhy': { text: 'alpha is turns-only (its team tools did not connect)', params: { member: 'alpha', why: 'its team tools did not connect' } },
  'team.notice.pingPong': { text: 'teammate wakes are paused for the team (ping-pong breaker)', params: {} },
  'team.notice.contextLostShort': { text: "alpha's CLI did not find its conversation and started a new one", params: { member: 'alpha' } },
  'team.notice.stashChanged': { text: "the repository's stash changed during alpha's turn", params: { member: 'alpha' } },
  'team.notice.overlap': { text: 'alpha, beta changed the same files: src/a.ts, src/b.ts', params: { members: 'alpha, beta', paths: 'src/a.ts, src/b.ts' } },
  'team.notice.attached': { text: 'alpha is taken over in a terminal since 14:05', params: { member: 'alpha', time: '14:05' } },
  'team.notice.budgetDayWarn': { text: 'alpha: the team has used its 300 turns of today', params: { member: 'alpha', n: '300' } },
  'team.notice.budgetHourWarn': { text: 'alpha: alpha has used its 20 turns of this hour', params: { member: 'alpha', member2: 'alpha', n: '20' } },
  // a member's reason (MemberState.reason.text)
  'team.notice.budgetDayReason': { text: 'the team has used its 300 turns of today', params: { n: '300' } },
  'team.notice.budgetHourReason': { text: 'alpha has used its 20 turns of this hour', params: { member: 'alpha', n: '20' } },
  'team.notice.atLimit': { text: 'claude/work is at its 5-hour limit', params: { account: 'claude/work', window: '5-hour' } },
  'team.notice.stoppedFromTaskList': { text: 'stopped from the task list', params: {} },
  'team.notice.stoppedByCancel': { text: 'stopped by cco cancel', params: { cco: 'cco' } },
  'team.notice.unknownTurn': { text: 'a turn of this member may still be running; it could not be found', params: {} },
  // why a host lists a team as it does (TeamListEntry.reason)
  'team.notice.hostGone': { text: 'host gone', params: {} },
};

test('G-c: every known engine notice template is recognized with its variable parts', () => {
  for (const template of NOTICE_TEMPLATES) {
    const sample = SAMPLES[template.key];
    assert(sample !== undefined, `a sample for ${template.key}`);
    const got = localizeNotice(sample.text);
    assert(got !== null, `${template.key} matches: ${sample.text}`);
    assertEqual(got?.key, template.key, `the right template for: ${sample.text}`);
    assertDeepEqual(got?.params, sample.params, `${template.key} keeps the variable parts exactly`);
    // Both languages have the sentence, and the Korean one is not the English one.
    assert(tKo(template.key) !== template.key && tEn(template.key) !== template.key, `${template.key} in ko and en`);
  }
  assertEqual(Object.keys(SAMPLES).length, NOTICE_TEMPLATES.length, 'one sample per template');
});

test('G-c: Korean UI shows the localized sentence; free text stays as the member wrote it', () => {
  const review = noticeDisplay(SAMPLES['team.notice.movedToReview'].text, tKo);
  assert(review.includes('검토') && review.includes('alpha') && review.includes('b1'), review);
  assert(review.includes('All tests pass; see report.md'), 'the report text is kept as written');
  assert(!review.includes('moved'), 'no raw English template words');
  assertEqual(noticeDisplay('the team is idle', tKo), '팀이 쉬고 있습니다.', 'team idle');
  const done = noticeDisplay(SAMPLES['team.notice.taskDone'].text, tKo);
  assert(done.includes('리드') && !done.includes('lead'), `"by the lead" is worded too: ${done}`);
  const fresh = noticeDisplay(SAMPLES['team.notice.freshNextTurn'].text, tKo);
  assert(fresh.includes('새 대화') && fresh.includes('its context is nearly full'), fresh);
});

test('G-c: an unknown notice falls back to the raw text, unchanged; near-misses do not match', () => {
  const unknown = 'the engine said something this build does not know';
  assertEqual(localizeNotice(unknown), null, 'not a template');
  assertEqual(noticeDisplay(unknown, tKo), unknown, 'shown as written');
  assertEqual(localizeNotice('the team is idle now'), null, 'anchored at the end');
  assertEqual(localizeNotice('note: the team is idle'), null, 'anchored at the start');
  assertEqual(localizeNotice(''), null, 'empty');
  assertEqual(localizeNotice(null), null, 'null');
  // A hostile body is still flattened (no control characters survive).
  assert(!noticeDisplay('a‮b\u0000c', tKo).includes('‮'), 'bidi controls removed');
});

test('G-c: the sender "engine" reads Orchestrator; the stored text is never changed', () => {
  assertEqual(senderDisplay('engine', tKo), '오케스트레이터', 'ko');
  assertEqual(senderDisplay('engine', tEn), 'Orchestrator', 'en');
  assertEqual(senderDisplay('alpha', tKo), 'alpha', 'members keep their names');

  const view = realView();
  const stored = [
    message(901, 'engine', 'notice', 'the team is idle'),
    message(902, 'engine', 'notice', SAMPLES['team.notice.movedToReview'].text),
    message(903, 'engine', 'notice', 'a notice nobody has a template for'),
    message(904, 'alpha', 'text', 'the team is idle'),
  ];
  view.messages = [...stored, ...view.messages];
  const before = JSON.stringify(view.messages);
  seed([view]);
  const text = textOf(renderPane());
  assert(text.includes('오케스트레이터'), 'the engine is shown as the orchestrator');
  assert(!/\bengine\b/.test(text.replace(/team engine/g, '')), 'the raw sender "engine" is not shown');
  assert(text.includes('팀이 쉬고 있습니다.'), 'the idle notice in Korean');
  assert(text.includes('All tests pass; see report.md'), 'the member\'s report as written');
  assert(text.includes('a notice nobody has a template for'), 'an unknown notice as written');
  assert(text.includes('alpha') && text.includes('the team is idle'), 'a MEMBER writing the same words is not translated');
  assertEqual(JSON.stringify(view.messages), before, 'the stored messages are untouched');
});

// ---------------------------------------------------------------------------
// B4 (G-d)
// ---------------------------------------------------------------------------

test('G-d: Korean says 멤버 for team members, never 구성원; worker accounts stay 작업자 계정', () => {
  const offenders = Object.entries(ko).filter(([, v]) => v.includes('구성원')).map(([k]) => k);
  assertDeepEqual(offenders, [], 'no 구성원 in ko.ts');
  assert(ko['team.section.members'].startsWith('멤버'), 'the roster heading');
  assertEqual(ko['team.noMembers'], '멤버가 없습니다.', 'the particle follows the new word');
  assertEqual(ko['team.selectMember'], '멤버를 선택하면 자세한 내용이 보입니다.', 'the particle follows the new word');
  assertEqual(ko['view.workers'], '작업자 계정', 'worker accounts keep their name');
  assert(Object.keys(en).length === Object.keys(ko).length, 'ko and en have the same keys');
});

// ---------------------------------------------------------------------------
// G-c follow-up: every surface that shows engine text reads it through notices.ts
// ---------------------------------------------------------------------------

/** The team header's warning lines, as text. */
function warningLines(html: string): string[] {
  const list = /<ul class="team-warnings[^"]*"[^>]*>([\s\S]*?)<\/ul>/.exec(html);
  if (list === null) return [];
  return (list[1]!.match(/<li[^>]*>[\s\S]*?<\/li>/g) ?? []).map((li) => textOf(li).trim());
}

test('G-c: the team header says "쉬는 중" for an idle team — no raw English, no repeated meaning', () => {
  const view = realView();
  view.state.warnings = [{ kind: 'idle', text: 'the team is idle' }];
  const before = JSON.stringify(view.state.warnings);
  seed([view]);
  const html = renderPane();
  const lines = warningLines(html);
  assertDeepEqual(lines, [tKo('team.warning.idle')], 'the state word alone');
  const text = textOf(html);
  assert(!text.includes('the team is idle'), 'no raw English notice in the header');
  assert(!text.includes(tKo('team.notice.teamIdle')), 'the notice does not repeat the state word');
  assertEqual(JSON.stringify(view.state.warnings), before, 'the stored warning is untouched');
  // The rule is per pair: the same sentence under another topic is still shown.
  assertEqual(warningDisplay('store', 'the team is idle', tKo), tKo('team.notice.teamIdle'), 'another topic keeps it');
  assertEqual(warningDisplay('idle', 'the team is idle', tEn), '', 'English drops the repeat too');
});

test('G-c: the header localizes known warnings and shows an unknown one as written', () => {
  const view = realView();
  view.state.warnings = [
    { kind: 'blocked', text: 'alpha is blocked: the CLI exited with code 1' },
    { kind: 'inbox', text: 'lead inbox: 12 unread' },
    { kind: 'store', text: 'the store said something this build does not know' },
    { kind: 'idle', text: 'the team went quiet in a new way' },
  ];
  seed([view]);
  const lines = warningLines(renderPane());
  assertEqual(lines.length, 4, 'four lines');
  assertEqual(lines[0], `${tKo('team.warning.blocked')} · ${tKo('team.notice.blocked', { member: 'alpha', reason: 'the CLI exited with code 1' })}`, 'blocked, in Korean; the reason as written');
  assert(!lines[0]!.includes('is blocked'), 'no raw template words');
  assertEqual(lines[1], `${tKo('team.warning.inbox')} · ${tKo('team.notice.leadInbox', { n: '12' })}`, 'the lead inbox line');
  assertEqual(lines[2], `${tKo('team.warning.store')} · the store said something this build does not know`, 'an unknown warning falls back to the raw text');
  assertEqual(lines[3], `${tKo('team.warning.idle')} · the team went quiet in a new way`, 'an unknown idle text is kept, not dropped');
});

test('G-c: a member\'s reason (roster tooltip, inspector) reads through the same localizer', () => {
  const known = reasonSentence({ code: 'quota-wait', text: 'claude/work is at its 5-hour limit' }, tKo, 'ko');
  assertEqual(known.title, tKo('team.notice.atLimit', { account: 'claude/work', window: '5-hour' }), 'the reason in Korean');
  assert(!known.title.includes('is at its'), 'no raw template words');
  const cancelled = reasonSentence({ code: 'cancelled', text: 'stopped from the task list' }, tKo, 'ko');
  assertEqual(cancelled.title, tKo('team.notice.stoppedFromTaskList'), 'a fixed reason in Korean');
  const unknown = reasonSentence({ code: 'error', text: 'team_store_failed: EACCES' }, tKo, 'ko');
  assertEqual(unknown.title, 'team_store_failed: EACCES', 'an unknown reason as written');
});
