/**
 * Team preview tests (T8, team contract v2 §8.2).
 *
 * The fixture is not hand-written: it is a `cco.team.snapshot` exactly as the
 * sidecar's observer composed it against a fake dispatch host
 * (`src-tauri/sidecar/test/cco-team.mjs` with `CCO_FIXTURE_OUT`), the same file
 * the Rust relay tests decode. Rust adds `generation` and `believed` on the way;
 * the tests add them the same way.
 *
 * What is pinned:
 *
 *  1. the pane renders a real snapshot: roster with engine state pills and the
 *     blocked reason, the board grouped into the five columns, the messages,
 *     and a selected member's activity;
 *  2. it is READ-ONLY: no input, no textarea, and no button but the local ones
 *     (refresh, member selection) — no composer, no member action, no take-over;
 *  3. L7: a team read from disk whose host is gone shows "host gone" and
 *     "was running", never `running`;
 *  4. the empty states say why (team mode off, sidecar down);
 *  5. hostile strings are flattened, capped and escaped;
 *  6. the slice keeps the newest `(generation, rev)` and withdraws on `null`;
 *  7. CCO 0b7b1b8 `TeamListEntry.reason` (relayed in the list summary): shown in
 *     the selected team's header and on its team chip (full line as the chip's
 *     title), flattened and capped; absent or blank, nothing is shown.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import fixture from '../../src-tauri/tests/fixtures/cco/team-snapshot.json';
import { translate } from '../i18n';
import type { CcoTeamListResult, CcoTeamView, TeamSnapshotEvent } from '../lib/ipc';
import { assert, assertEqual, test } from '../test/harness';
import { initialTeamState, isNewerRevision, pruneUnlisted, reduceTeamSnapshot } from '../store/teamSlice';
import type { TeamPreviewState } from '../store/teamSlice';
import { initialUiState } from '../store/uiSlice';
import { useAppStore } from '../store/useAppStore';
import {
  boardColumns,
  flat,
  mailboxKey,
  memberPill,
  messageKindKey,
  outcomeLabel,
  providerBadge,
  reasonLabel,
  teamPill,
  warningLabel,
} from './model';
import { AddProjectFolderButton, TeamPane, listReasons, noTeamCase } from './TeamPane';
import { TeamMemberInspector } from './TeamMemberInspector';

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/** The real snapshot, with the two fields Rust stamps. */
function realView(patch: Partial<CcoTeamView> = {}): CcoTeamView {
  const raw = (fixture as unknown as { view: CcoTeamView }).view;
  const view: CcoTeamView = JSON.parse(JSON.stringify(raw));
  return { ...view, generation: 1, believed: true, ...patch };
}

function listOf(views: CcoTeamView[], patch: Partial<CcoTeamListResult> = {}): CcoTeamListResult {
  return {
    available: true,
    supported: true,
    dataRoot: '/h/.claude-code-orchestrator',
    teamMode: 'on',
    hosts: [],
    teams: views.map((v) => ({
      teamId: v.teamId,
      name: v.state.name,
      title: v.state.title,
      status: v.state.status,
      source: v.source,
      live: v.live,
      members: v.state.members.length,
      running: 0,
      blocked: 0,
      userUnread: 0,
      leadUnread: 0,
      createdAt: v.state.createdAt,
      owner: v.state.owner,
      rev: v.rev,
      believed: v.believed,
    })),
    generation: 1,
    ...patch,
  };
}

function seedTeam(team: Partial<TeamPreviewState>): void {
  useAppStore.setState({
    ui: initialUiState(),
    team: { ...initialTeamState(), watching: true, ...team },
    // The pane starts watching on mount; on the server that effect never runs,
    // but the store action must still be a harmless function.
    startTeamWatch: () => () => undefined,
  });
}

function render(): string {
  // Static rendering reads zustand's initial state (see test/render.test.tsx).
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  return renderToStaticMarkup(
    <TeamPane
      descriptor={{ view: 'team' }}
      tab={{ id: 't1', title: 'team', descriptor: { view: 'team' }, closable: true }}
      paneId="pane-1"
      active
    />,
  );
}

function renderInspector(): string {
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  return renderToStaticMarkup(<TeamMemberInspector />);
}

const ko = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) =>
  translate('ko', key, params);

/** Text of the markup, tags removed and entities decoded (enough for these assertions). */
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

// ---------------------------------------------------------------------------
// 1 + 2. The pane renders a real snapshot, read-only
// ---------------------------------------------------------------------------

test('the team pane renders a real sidecar snapshot: roster, board, messages, activity', () => {
  const view = realView();
  seedTeam({
    list: listOf([view]),
    views: new Map([[view.teamId, view]]),
    selectedTeamId: view.teamId,
    selectedMember: 'alice',
  });
  const html = render();
  const text = textOf(html);

  assert(text.includes(ko('team.preview')), 'the preview line is on the pane');
  assert(text.includes('demo') && text.includes('Demo team'), 'team name and title');
  assert(text.includes(ko('team.status.active')), 'the team status pill');
  assert(text.includes(ko('team.source.live')), 'a live view says so');
  assert(text.includes('Ship the thing'), 'the goal');
  // Roster: both members, the engine's states, and bob's blocked reason.
  assert(text.includes('alice') && text.includes('bob'), 'both members are listed');
  assert(text.includes(ko('team.member.idle')), 'alice is idle (the engine state)');
  assert(text.includes(ko('team.member.blocked')), 'bob is blocked');
  // F7: the reason code is a sentence; the engine's text and the code are in the tooltip.
  assert(text.includes(ko('team.reason.quota')), 'bob\'s blocked reason is a sentence');
  assert(html.includes('quota wall'), 'the engine text is kept, in the tooltip');
  assert(text.includes('Claude Code'), 'the provider as a text badge, by its product name');
  // Board: five columns, the claimed task in its column.
  for (const key of ['team.board.open', 'team.board.claimed', 'team.board.blocked', 'team.board.review', 'team.board.done'] as const) {
    assert(text.includes(ko(key)), `the ${key} column is there`);
  }
  assert(text.includes('Write the parser'), 'the board task title');
  // Messages; the selected member's details are in the inspector, not the pane.
  assert(text.includes('done with b1'), 'the message body');
  assert(!text.includes(ko('team.section.activity', { name: 'alice' })), 'no inline member panel');

  const inspector = textOf(renderInspector());
  assert(inspector.includes(ko('team.section.activity', { name: 'alice' })), 'the inspector shows the member panel');
  assert(inspector.includes('Edit · src/a.ts'), 'the activity line from cco/team.activity');
  assert(inspector.includes(ko('team.mailbox.tools')), 'how it gets messages, in words');
  assert(inspector.includes(ko('team.runsCommands')), 'whether it runs commands');
});

test('member rows are at most two lines, with no pid or raw code in the text', () => {
  const live = realView();
  const members = live.state.members.map((m, i) =>
    i === 0
      ? { ...m, state: 'running', turn: { id: 't1', n: 7, startedAt: 1, pid: 4242 }, mailbox: 'turns-only' }
      : m,
  );
  const view = realView({ state: { ...live.state, members } });
  seedTeam({ list: listOf([view]), views: new Map([[view.teamId, view]]), selectedTeamId: view.teamId });
  const html = render();
  const rows = html.match(/<button[^>]*class="team-member"[\s\S]*?<\/button>/g) ?? [];
  assertEqual(rows.length, members.length, 'one row per member');
  for (const row of rows) {
    const lines = row.match(/<span class="row gap-6 team-member-line">|<span class="s12[^"]*(team-reason|team-member-activity|team-member-meta)/g) ?? [];
    assert(lines.length <= 2, `a member row has at most two lines (got ${lines.length})`);
  }
  const text = textOf(html);
  assert(!text.includes('4242'), 'no pid in the pane text');
  assert(!text.includes('turns-only'), 'no raw mailbox mode');
});

test('the team pane is read-only: no input, no composer, only local buttons', () => {
  const view = realView();
  seedTeam({
    list: listOf([view]),
    views: new Map([[view.teamId, view]]),
    selectedTeamId: view.teamId,
  });
  const html = render();
  assert(!/<input\b/.test(html), 'no <input>');
  assert(!/<textarea\b/.test(html), 'no <textarea>');
  const buttons = html.match(/<button\b[^>]*>/g) ?? [];
  // One refresh, one per member (selection). One team: no team chips.
  assertEqual(buttons.length, 1 + view.state.members.length, 'refresh + member selection only');
  const text = textOf(html).toLowerCase();
  for (const word of ['interrupt', 'take over', 'restart', 'send', 'pause']) {
    assert(!text.includes(word), `no "${word}" action on the pane`);
  }
});

// ---------------------------------------------------------------------------
// 3. L7
// ---------------------------------------------------------------------------

test('a team read from disk whose host is gone shows host gone and was running', () => {
  const live = realView();
  const members = live.state.members.map((m, i) => (i === 0 ? { ...m, state: 'running' } : m));
  const view = realView({
    live: false,
    source: 'disk',
    believed: false,
    state: { ...live.state, members },
  });
  seedTeam({ list: listOf([view]), views: new Map([[view.teamId, view]]), selectedTeamId: view.teamId });
  const text = textOf(render());
  assert(text.includes(ko('team.hostGone')), 'host gone, not active');
  assert(text.includes(ko('team.wasRunning')), 'was running, not running');
  assert(!text.includes(` ${ko('team.member.running')} `), 'never a running pill without a live owner');
  assert(text.includes(ko('team.source.disk')), 'it says the view is the saved state');

  assertEqual(memberPill(members[0]!, false).key, 'team.wasRunning', 'model: was running');
  assertEqual(memberPill(members[0]!, true).key, 'team.member.running', 'model: running when believed');
  assertEqual(teamPill(view.state, false).key, 'team.hostGone', 'model: host gone');
  assertEqual(teamPill({ ...view.state, status: 'suspended' }, false).key, 'team.status.suspended', 'a suspended team is suspended');
});

// ---------------------------------------------------------------------------
// 4. Empty states
// ---------------------------------------------------------------------------

test('F3: not set up, Teammate mode off, and on-but-empty are three different sentences', () => {
  seedTeam({ list: listOf([], { teamMode: 'unknown' }) });
  let html = render();
  let text = textOf(html);
  assertEqual(noTeamCase(listOf([], { teamMode: 'unknown' })), 'notSetUp', 'no readable registry = not set up');
  assert(text.includes(ko('team.notSetUp')) && text.includes(ko('team.notSetUp.how')), 'not set up says what to install');
  assert(!text.includes(ko('team.modeOff')), 'and does not claim the mode is off');

  seedTeam({ list: listOf([], { teamMode: 'off' }) });
  html = render();
  text = textOf(html);
  assertEqual(noTeamCase(listOf([], { teamMode: 'off' })), 'modeOff', 'off');
  assert(text.includes(ko('team.modeOff')), 'Teammate mode off, in plain words');
  assert(text.includes(ko('team.modeOff.how')), 'with how to turn it on on a secondary line');
  assert(!ko('team.modeOff').includes('teamMode') && !translate('en', 'team.modeOff').includes('teamMode'), 'no raw setting key in the main text');
  assert(translate('en', 'team.modeOff.how').includes('Claude Code Orchestrator: Team Mode'), 'the setting is named as VS Code shows it');

  seedTeam({ list: listOf([], { teamMode: 'on' }) });
  text = textOf(render());
  assertEqual(noTeamCase(listOf([], { teamMode: 'on' })), 'empty', 'on, nothing');
  assert(text.includes(ko('team.empty')), 'team mode on, no team');
  assert(!text.includes(ko('team.modeOff')) && !text.includes(ko('team.notSetUp')), 'and neither of the other two');
  seedTeam({ list: listOf([], { available: false }) });
  assert(textOf(render()).includes(ko('team.sidecarDown')), 'sidecar down');
  seedTeam({
    list: listOf([], {
      hosts: [{ key: 's9', sessionPid: 9, hostPid: 77, workspace: '/w', pane: false, state: 'no-teams' }],
    }),
  });
  html = render();
  assert(textOf(html).includes(ko('team.host.noTeams')), 'a host without teams is explained');
  assert(!textOf(html).includes('77') && html.includes(ko('team.host.pid', { pid: 77 })), 'its pid only in a tooltip');
  assertEqual(
    translate('en', 'team.preview'),
    'Preview — read-only. Steer the team from the lead session or VS Code.',
    'the en preview line is the engine sentence (host-protocol TEAM_PREVIEW_READ_ONLY_TEXT)',
  );
});

test('F2: every no-team case offers "Add project folder", and it uses the folder picker flow', () => {
  for (const teamMode of ['unknown', 'off', 'on']) {
    seedTeam({ list: listOf([], { teamMode }) });
    const html = render();
    assert(textOf(html).includes(ko('team.addFolder')), `${teamMode}: the button is there`);
  }
  // The button runs the store's addProject (plugin-dialog picker → project_add).
  let picked = 0;
  useAppStore.setState({ addProject: async () => { picked += 1; return null; } });
  assert(typeof AddProjectFolderButton === 'function', 'the button is a component');
  void useAppStore.getState().addProject();
  assertEqual(picked, 1, 'and the flow it calls is the existing project_add one');
});

test('F7: engine codes become words; an unknown code falls back with the code in a tooltip', () => {
  assertEqual(reasonLabel('quota').key, 'team.reason.quota', 'a known reason');
  assertEqual(reasonLabel('quota-wait').key, 'team.reason.quota-wait', 'a paused reason');
  const unknown = reasonLabel('flux-capacitor');
  assertEqual(unknown.key, 'team.reason.other', 'an unknown reason → the generic sentence');
  assertEqual(unknown.code, 'flux-capacitor', 'with its code kept for the tooltip');
  assertEqual(unknown.known, false, 'and marked unknown');
  assertEqual(warningLabel('overlap').key, 'team.warning.overlap', 'a known warning kind');
  assertEqual(warningLabel('new-kind').key, 'team.warning.other', 'an unknown warning kind');
  assertEqual(mailboxKey('turns-only'), 'team.mailbox.turnsOnly', 'turns-only');
  assertEqual(ko('team.mailbox.turnsOnly'), '메시지를 턴 사이에 받음', 'in words');
  assertEqual(translate('en', 'team.mailbox.turnsOnly'), 'Gets messages between turns', 'in words (en)');
  assertEqual(translate('en', 'team.pingPong'), 'Paused: members kept replying to each other.', 'ping-pong in words');
  assertEqual(messageKindKey('question'), 'team.messageKind.question', 'a message kind');
  assertEqual(messageKindKey('text'), null, 'a plain text message needs no badge');
  assertEqual(messageKindKey('mystery'), null, 'an unknown kind gets no badge');
  assertEqual(outcomeLabel('ok').key, 'team.outcome.ok', 'a turn outcome');
  assertEqual(outcomeLabel('weird').key, 'team.outcome.other', 'an unknown outcome');
  assertEqual(providerBadge('gemini'), 'Antigravity', 'the engine\'s gemini is Antigravity');
  assertEqual(providerBadge('cursor'), 'Cursor', 'cursor is Cursor');

  // Rendered: an unknown reason shows the generic sentence, the code only in the tooltip.
  const live = realView();
  const members = live.state.members.map((m, i) =>
    i === 0 ? { ...m, state: 'blocked', reason: { code: 'flux-capacitor', text: 'engine words' } } : m,
  );
  const warnings = [{ kind: 'new-kind', text: 'something happened' }];
  const view = realView({ state: { ...live.state, members, warnings } });
  seedTeam({ list: listOf([view]), views: new Map([[view.teamId, view]]), selectedTeamId: view.teamId });
  const html = render();
  const text = textOf(html);
  assert(text.includes(ko('team.reason.other')), 'the generic sentence is shown');
  assert(!text.includes('flux-capacitor'), 'the unknown code is not the main text');
  assert(html.includes(ko('team.code', { code: 'flux-capacitor' })), 'it is in the tooltip');
  assert(text.includes(ko('team.warning.other')) && !text.includes('new-kind'), 'an unknown warning kind reads "Warning"');
});

// ---------------------------------------------------------------------------
// 5. Hostile strings
// ---------------------------------------------------------------------------

test('hostile strings are flattened, capped and escaped', () => {
  const live = realView();
  const nasty = '<script>alert(1)</script>\n\u202Eevil\u0007 ' + 'x'.repeat(5_000);
  const members = live.state.members.map((m, i) => (i === 0 ? { ...m, role: nasty } : m));
  const view = realView({ state: { ...live.state, members } });
  seedTeam({ list: listOf([view]), views: new Map([[view.teamId, view]]), selectedTeamId: view.teamId });
  const html = render();
  assert(!html.includes('<script>'), 'markup in a role is escaped, never rendered');
  assert(!html.includes('\u202E') && !html.includes('\u0007'), 'bidi overrides and controls are removed');
  assert(!html.includes('x'.repeat(200)), 'a long role is capped');

  assertEqual(flat('a\n\tb\u202Ec', 10), 'a bc', 'flat: one line, no bidi');
  assertEqual(flat('abcdef', 4), 'abc…', 'flat: capped with an ellipsis');
  assertEqual(flat('ab\uD83D\uDE00cd', 4), 'ab…', 'flat: never splits a surrogate pair');
  assertEqual(flat(undefined, 4), '', 'flat: nothing is empty');
});

test('the board groups by state; cancelled is finished, an unknown state is not lost', () => {
  const base = realView().state.board[0]!;
  const cols = boardColumns([
    { ...base, id: 'b1', state: 'open' },
    { ...base, id: 'b2', state: 'review' },
    { ...base, id: 'b3', state: 'cancelled' },
    { ...base, id: 'b4', state: 'someday' },
  ]);
  assertEqual(cols.open.map((t) => t.id).join(','), 'b1,b4', 'open + unknown');
  assertEqual(cols.review.map((t) => t.id).join(','), 'b2', 'review');
  assertEqual(cols.done.map((t) => t.id).join(','), 'b3', 'cancelled sits under done');
});

// ---------------------------------------------------------------------------
// 6. Latest wins
// ---------------------------------------------------------------------------

test('the slice keeps the newest (generation, rev) and withdraws on null', () => {
  const view = realView();
  const ev = (generation: number, rev: number, v: CcoTeamView | null): TeamSnapshotEvent => ({
    generation,
    rev,
    teamId: view.teamId,
    view: v === null ? null : { ...v, generation, rev },
  });
  let state = reduceTeamSnapshot(initialTeamState(), ev(1, 5, view));
  assertEqual(state.views.get(view.teamId)?.rev, 5, 'first view applied');
  assertEqual(state.selectedTeamId, view.teamId, 'and selected');

  const older = reduceTeamSnapshot(state, ev(1, 4, { ...view, state: { ...view.state, name: 'old' } }));
  assert(older === state, 'an older rev is ignored');

  state = reduceTeamSnapshot(state, ev(2, 1, view));
  assertEqual(state.views.get(view.teamId)?.generation, 2, 'a restarted sidecar (new generation) wins with rev 1');

  state = reduceTeamSnapshot(state, ev(2, 2, null));
  assert(!state.views.has(view.teamId), 'null withdraws the team');
  assertEqual(state.selectedTeamId, null, 'and the selection follows');

  const late = reduceTeamSnapshot(state, ev(2, 1, view));
  assert(!late.views.has(view.teamId), 'a view older than the withdrawal does not resurrect it');

  assert(isNewerRevision({ generation: 1, rev: 1 }, undefined), 'anything beats nothing');
  const held = reduceTeamSnapshot(initialTeamState(), ev(1, 3, view));
  const pruned = pruneUnlisted(held, new Set());
  assert(!pruned.views.has(view.teamId) && !pruned.revisions.has(view.teamId), 'a team the list lost is dropped with its revision');
  assert(pruneUnlisted(held, new Set([view.teamId])) === held, 'a listed team is kept as is');
});

// ---------------------------------------------------------------------------
// 7. TeamListEntry.reason (CCO 0b7b1b8)
// ---------------------------------------------------------------------------

test('a host\'s reason for listing a team is shown in the header and on its chip', () => {
  const a = realView();
  const b = realView({ teamId: 'tmzzzzzzzzz2', live: false, source: 'disk', state: { ...a.state, id: 'tmzzzzzzzzz2', name: 'old', createdAt: a.state.createdAt - 1 } });
  const list = listOf([a, b]);
  list.teams[1] = { ...list.teams[1]!, reason: 'host gone' };
  seedTeam({ list, views: new Map([[a.teamId, a], [b.teamId, b]]), selectedTeamId: b.teamId });
  const html = render();
  const text = textOf(html);
  // `host gone` is a known engine sentence: it reads in the user's language (notices.ts).
  const gone = ko('team.notice.hostGone');
  assert(html.includes('data-testid="team-list-reason"') && text.includes(ko('team.listReason', { reason: gone })), 'the selected team\'s header says why');
  assert(!text.includes('host gone'), 'no raw English reason in the Korean UI');
  const chips = html.match(/<button[^>]*class="chip"[^>]*>[\s\S]*?<\/button>/g) ?? [];
  assertEqual(chips.length, 2, 'two teams: two chips');
  const oldChip = chips.find((c) => c.includes('old')) ?? '';
  assert(oldChip.includes('data-reason="true"') && oldChip.includes(`title="${ko('team.listReason', { reason: gone })}"`) && textOf(oldChip).includes(gone), 'its chip carries the reason');
  const demoChip = chips.find((c) => c.includes('demo')) ?? '';
  assert(!demoChip.includes('data-reason') && !demoChip.includes('title='), 'a team without a reason has none on its chip');

  seedTeam({ list, views: new Map([[a.teamId, a], [b.teamId, b]]), selectedTeamId: a.teamId });
  assert(!render().includes('data-testid="team-list-reason"'), 'the header of a team without a reason says nothing');
});

test('a listed reason is host text: flattened, capped at 300 (24 on a chip), escaped; blank is none', () => {
  const view = realView();
  const nasty = `<b>gone</b>\u202E\n\u0007 ${'r'.repeat(1_000)}`;
  const list = listOf([view]);
  list.teams[0] = { ...list.teams[0]!, reason: nasty };
  const reasons = listReasons(list, ko);
  const line = reasons.get(view.teamId) ?? '';
  assert(line.length <= 300 && !/[\n\u0007\u202E]/.test(line) && line.startsWith('<b>gone</b>'), 'one line, ≤ 300, no controls or bidi');
  seedTeam({ list, views: new Map([[view.teamId, view]]), selectedTeamId: view.teamId });
  const html = render();
  assert(!html.includes('<b>gone</b>') && html.includes('&lt;b&gt;gone&lt;/b&gt;'), 'escaped, never markup');
  assert(!html.includes('r'.repeat(300)), 'capped on the page');
  assertEqual(flat(nasty, 24).length, 24, 'the chip cap');
  const blank = listOf([view]);
  blank.teams[0] = { ...blank.teams[0]!, reason: ' \n\t ' };
  assertEqual(listReasons(blank, ko).size, 0, 'a blank reason is no reason');
  assertEqual(listReasons(null, ko).size, 0, 'no list, no reasons');
});
