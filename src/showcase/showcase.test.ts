/**
 * Tests for the showcase (contract §14.1 store fixtures, §15.2 T19): the world
 * is deterministic, well-formed and internally consistent; the patch script
 * chains; the backend answers the boot path through the real command wrappers
 * and the real worldSlice, and refuses everything it does not play.
 */

import { commands, installShowcaseBackend } from '../lib/commands';
import { COMMAND_NAMES } from '../lib/gen/commands';
import type { CommandName } from '../lib/gen/commands';
import type { ConvoEnvelope } from '../lib/gen/ConvoEnvelope';
import type { PtyChunk } from '../lib/gen/PtyChunk';
import type { Session } from '../lib/gen/Session';
import type { StorePatch } from '../lib/gen/StorePatch';
import type { ViewId } from '../lib/gen/ViewId';
import type { IpcError } from '../lib/ipcError';
import { setShowcaseModeForTests } from '../lib/mode';
import { parseView } from '../store/uiSlice';
import { createAppStore } from '../store/useAppStore';
import { assert, assertDeepEqual, assertEqual, assertRejects, sleep, test } from '../test/harness';
import { SHOWCASE_MARKER, createShowcaseBackend } from './backend';
import { SCENES, SCENE_NAMES, parseShowcaseQuery } from './scenes';
import type { SceneName } from './scenes';
import { readSnapshotHeader } from '../terminal/terminalPool';
import { seedShowcase, whenReady } from './showcase';
import { SAMPLE_TIMES_S, buildTimeline, foldSnapshot, snapshotAt } from './timeline';
import { bandOf, buildWorld, teamSizes } from './world';
import type { ShowcaseWorld, WorldOptions } from './world';

const NOW = Date.UTC(2026, 9, 22, 9, 0, 0);
const BASE: WorldOptions = { agents: 12, teams: ['native'], pending: true, leftovers: 2, history: 40 };

function world(options: Partial<WorldOptions> = {}): ShowcaseWorld {
  return buildWorld({ ...BASE, ...options }, NOW);
}

/** Run `body` with showcase mode on, restoring the boundary afterwards. */
async function inShowcase(body: () => Promise<void>): Promise<void> {
  setShowcaseModeForTests(true);
  try {
    await body();
  } finally {
    installShowcaseBackend(null);
    setShowcaseModeForTests(null);
  }
}

test('showcase world: deterministic for the same options and time', () => {
  assertEqual(JSON.stringify(world()), JSON.stringify(world()), 'two builds are equal');
  assert(JSON.stringify(world()) !== JSON.stringify(buildWorld(BASE, NOW + 60_000)), 'another time gives other timestamps');
});

test('showcase world: every id has its contract shape (§3.2)', () => {
  const w = world({ agents: 31, teams: ['native', 'nativeInferred', 'orchestrator'] });
  const s = w.snapshot;
  const check = (values: readonly string[], re: RegExp, what: string) => {
    for (const v of values) assert(re.test(v), `${what} ${v} matches ${re}`);
  };
  check(s.sessions.map((x) => x.uid), /^s_[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/, 'SessionUid');
  check(s.projects.map((x) => x.id), /^p_[0-9a-f]{16}$/, 'ProjectId');
  check(s.locations.map((x) => x.id), /^l_[0-9a-f]{16}$/, 'LocationId');
  check(s.teams.map((x) => x.key), /^t_[0-9a-f]{16}$/, 'TeamKey');
  check(s.members.map((x) => x.key), /^m_[0-9a-f]{16}$/, 'MemberKey');
  check(s.tasks.map((x) => x.key), /^k_[0-9a-f]{16}$/, 'TaskKey');
  check(s.messages.map((x) => x.key), /^g_[0-9a-f]{16}$/, 'MessageKey');
  check(s.accounts.map((x) => x.id), /^a_[0-9a-f]{16}$/, 'AccountId');
  check(s.leftovers.map((x) => x.key), /^x_[0-9]{1,10}_[0-9]{1,12}$/, 'ProcessKey');
  check(s.members.flatMap((m) => (m.pane ? [m.pane] : [])), /^%[0-9]{1,6}$/, 'PaneId');
  for (const kind of ['sessions', 'projects', 'teams', 'members', 'tasks', 'messages', 'accounts'] as const) {
    const ids = (s[kind] as readonly { uid?: string; id?: string; key?: string }[]).map((x) => x.uid ?? x.id ?? x.key);
    assertEqual(new Set(ids).size, ids.length, `${kind} ids are unique`);
  }
});

test('showcase world: references resolve (members, sessions, tasks, messages, projects, locations)', () => {
  const w = world({ agents: 31, teams: ['native', 'nativeInferred', 'orchestrator'] });
  const s = w.snapshot;
  const sessions = new Set(s.sessions.map((x) => x.uid));
  const members = new Map(s.members.map((m) => [m.key, m]));
  const teams = new Set(s.teams.map((t) => t.key));
  const tasks = new Map(s.tasks.map((t) => [t.key, t]));
  const projects = new Set(s.projects.map((p) => p.id));
  const locations = new Set(s.locations.map((l) => l.id));
  for (const m of s.members) {
    assert(teams.has(m.teamKey), `member ${m.name} has a team`);
    assert(m.sessionUid === null || sessions.has(m.sessionUid), `member ${m.name} session exists`);
  }
  for (const x of s.sessions) {
    if (x.team) {
      assert(teams.has(x.team.teamKey) && members.has(x.team.memberKey), `session ${x.uid} team role resolves`);
      assertEqual(members.get(x.team.memberKey)?.sessionUid, x.uid, 'the member points back at its session');
    }
    assert(x.projectId === null || projects.has(x.projectId), 'session project exists');
    assert(x.locationId === null || locations.has(x.locationId), 'session location exists');
    for (const c of x.pending) {
      assert(c.answerIn === null || sessions.has(c.answerIn), 'answerIn session exists');
      assert(c.onBehalfOf === null || members.has(c.onBehalfOf), 'onBehalfOf member exists');
    }
  }
  for (const t of s.teams) {
    assert(t.lead !== null && members.get(t.lead)?.role === 'lead', `team ${t.name} lead is a lead`);
    assert(s.members.filter((m) => m.teamKey === t.key).length >= 2, 'a team row has a member besides the lead');
  }
  for (const t of s.tasks) {
    assert(t.owner === null || members.has(t.owner), 'task owner exists');
    for (const b of [...t.blockedBy, ...t.blocks]) assertEqual(tasks.get(b)?.teamKey, t.teamKey, 'dependencies stay inside the team');
    if (t.inferred) assert(t.title === null && t.blockedBy.length === 0, 'inferred items have no title and no blockedBy');
  }
  for (const msg of s.messages) {
    for (const p of [msg.from, msg.to]) if (p.kind === 'member') assert(members.has(p.member), 'message party exists');
  }
});

test('showcase world: the Floor sizes are exact and carry native-inferred and orchestrator teams', () => {
  for (const [agents, teams] of [[1, []], [8, ['native']], [20, ['nativeInferred', 'orchestrator']], [31, ['native', 'nativeInferred', 'orchestrator']]] as const) {
    const w = world({ agents, teams: [...teams] });
    assertEqual(w.snapshot.sessions.length, agents, `${agents} agents`);
    assertDeepEqual(w.snapshot.teams.map((t) => (t.source === 'orchestrator' ? 'orchestrator' : t.caps.observeTasks === 'inferred' ? 'nativeInferred' : 'native')), [...teams], `${agents}: team kinds`);
  }
  assertDeepEqual(teamSizes(2, ['native']), [], 'too few agents for a team');
  assertEqual(world({ agents: 0, teams: [] }).snapshot.sessions.length, 0, 'the empty world');
  const floor20 = parseShowcaseQuery('?scene=floor-20');
  assertEqual(floor20.world.agents, 20, 'floor-20 scene size');
});

test('showcase world: the queue follows Q1–Q8 and its counts match the sessions', () => {
  for (const options of [BASE, { ...BASE, agents: 31, teams: ['native', 'nativeInferred', 'orchestrator'] as const }]) {
    const w = world({ ...options, teams: [...options.teams] });
    const q = w.snapshot.queue;
    const keys = q.items.map((i) => i.key);
    assertEqual(new Set(keys).size, keys.length, 'Q2: one item per session');
    for (const s of w.snapshot.sessions) {
      const band = bandOf(s);
      const item = q.items.find((i) => i.subject.kind === 'session' && i.subject.uid === s.uid);
      assertEqual(item?.band ?? null, band, `Q1: ${s.uid} band`);
    }
    for (let i = 1; i < q.items.length; i++) {
      const a = q.items[i - 1];
      const b = q.items[i];
      if (!a || !b) continue;
      assert(a.band <= b.band, 'Q4: bands ascending');
      if (a.band === b.band && (a.band === 1 || a.band === 2 || a.band === 4)) assert(a.since <= b.since, 'Q4: oldest first in bands 1, 2, 4');
      if (a.band === b.band && (a.band === 3 || a.band === 5)) assert(a.since >= b.since, 'Q4: newest first in bands 3, 5');
    }
    assertEqual(q.needYou, q.items.filter((i) => i.band <= 2).length, 'Q7: needYou = bands 1–2');
    assertEqual(q.also, q.items.filter((i) => i.band >= 3).length, 'Q7: also = bands 3–6');
    const leftover = q.items.filter((i) => i.band === 6);
    assertEqual(leftover.length, w.snapshot.leftovers.length > 0 ? 1 : 0, 'Q8: one aggregated leftover item');
  }
  const hosted = world().snapshot.queue.items.find((i) => i.subject.kind === 'session' && i.subject.answerIn !== null);
  assert(hosted !== undefined, 'Q3: a hosted teammate request answers in the lead');
});

test('showcase world: project live counts count a team once', () => {
  const w = world({ agents: 20, teams: ['native'] });
  const p = w.snapshot.projects.find((x) => x.name === 'checkout-service');
  const solo = w.snapshot.sessions.filter((s) => !s.team && s.projectId === p?.id && s.control.kind !== 'notRunning').length;
  const teams = new Set(w.snapshot.sessions.filter((s) => s.team && s.projectId === p?.id).map((s) => s.team?.teamKey)).size;
  assertEqual(p?.liveSessions, solo + teams, 'liveSessions = solo sessions + one per team');
});

test('showcase world: tool-approval cards are shaped as the producer sends them (§3.4.5, A25.4)', () => {
  const w = world({ agents: 31, teams: ['native', 'nativeInferred', 'orchestrator'] });
  const cards = w.snapshot.sessions.flatMap((x) => x.pending).filter((p) => p.kind === 'toolApproval');
  assert(cards.some((c) => c.alwaysLabel !== null), 'a card with button 2');
  for (const c of cards) {
    const keys = c.options.map((o) => o.key);
    assertDeepEqual(keys, c.alwaysLabel === null ? ['1', '3'] : ['1', '2', '3'], `${c.requestId}: keys 1, 2 iff alwaysLabel, 3`);
    if (c.alwaysLabel !== null) {
      assertEqual(c.options[1]?.label, c.alwaysLabel, 'button 2 carries the rule');
      assert(!/^(yes|always|allow)\b/i.test(c.alwaysLabel), `alwaysLabel is the rule prefix, not a sentence: ${c.alwaysLabel}`);
    }
  }
});

test('showcase timeline: steps sit on the gate sample times and chain by rev', () => {
  const w = world();
  const steps = buildTimeline(w);
  assertDeepEqual(steps.map((s) => s.at / 1000), [5, 14, 15, 27, 30], 'step times');
  for (const s of steps) assert((SAMPLE_TIMES_S as readonly number[]).includes(s.at / 1000), 'on a sample time');
  let snap = w.snapshot;
  for (const step of steps) {
    const ops = step.make(snap, w.now + step.at);
    assert(ops.length > 0, `${step.label} has ops`);
    const next = foldSnapshot(snap, { fromRev: snap.rev, toRev: snap.rev + 1, ops });
    assertEqual(next.rev, snap.rev + 1, 'rev advances by one');
    snap = next;
  }
  const at15 = snapshotAt(w, steps, 15000);
  assertEqual(at15.applied, 3, 'three steps by 15 s');
  const asker = at15.snapshot.sessions.find((s) => s.pending.some((c) => c.requestId.endsWith('_9')));
  assertEqual(asker, undefined, 'the 5 s card was answered by 15 s');
  const at5 = snapshotAt(w, steps, 5000).snapshot;
  assert(at5.sessions.some((s) => s.pending.some((c) => c.requestId.endsWith('_9'))), 'the 5 s card is up at 5 s');
  assertEqual(at5.queue.needYou, world().snapshot.queue.needYou + 1, 'the queue counts it');
});

test('showcase scenes: every scene restores a view the app accepts', () => {
  for (const name of SCENE_NAMES) {
    const options = parseShowcaseQuery(`?scene=${name}&live=0`);
    const w = buildWorld(options.world, NOW);
    const view = parseView(SCENES[name].view(w.roles));
    const raw = SCENES[name].view(w.roles) as { screen: string };
    assertEqual(view.screen, raw.screen, `${name}: parseView keeps ${raw.screen}`);
  }
  const s = parseView(SCENES.session.view(world().roles));
  assert(s.screen === 'session' && s.uid === world().roles.approval, 'the session scene opens the approval session');
});

test('showcase query: parses and falls back, never throws', () => {
  const o = parseShowcaseQuery('?scene=team&lang=ko&theme=dark&t=14&live=0&agents=9&teams=orchestrator,bogus');
  assertEqual(o.scene, 'team', 'scene');
  assertEqual(o.locale, 'ko', 'lang');
  assertEqual(o.theme, 'dark', 'theme');
  assertEqual(o.t, 14, 't');
  assertEqual(o.live, false, 'live');
  assertEqual(o.world.agents, 9, 'agents');
  assertDeepEqual(o.world.teams, ['orchestrator'], 'unknown team kinds are dropped');
  const d = parseShowcaseQuery('?scene=nope&lang=fr&theme=x&t=-3&agents=1e9');
  assertEqual(d.scene, 'home', 'unknown scene → home');
  assertEqual(d.locale, 'en', 'unknown lang → en');
  assertEqual(d.theme, 'system', 'unknown theme → system');
  assertEqual(d.t, 0, 'negative t → 0');
  assertEqual(d.world.agents, SCENES.home.world.agents, 'absurd agents ignored');
  assertEqual(parseShowcaseQuery('%%%').scene, 'home', 'garbage query');
});

test('showcase backend: the app boot path through the real wrappers and worldSlice', () =>
  inShowcase(async () => {
    const options = parseShowcaseQuery('?scene=home&live=0');
    const seeded = seedShowcase(options, NOW);
    const store = createAppStore();
    const boot = await commands.app_bootstrap({});
    store.getState().hydrateFromBootstrap(boot);
    await store.getState().subscribeWorld();
    assertEqual(store.getState().world.status, 'live', 'the world is live');
    assertEqual(store.getState().world.sessions.size, seeded.world.snapshot.sessions.length, 'every session mirrored');
    assertEqual(store.getState().world.queue.needYou, seeded.world.snapshot.queue.needYou, 'queue counts mirrored');
    let ready = false;
    whenReady(seeded, store, () => {
      ready = true;
    });
    assert(ready, 'ready once hydrated and live');

    // The whole script arrives as gap-free patches and the mirror ends where the backend is.
    seeded.handle.advanceTo(30_000);
    const mirror = store.getState().world;
    const backend = seeded.handle.current();
    assertEqual(mirror.rev, backend.rev, 'mirror rev = backend rev (no gap, no resync)');
    assertEqual(mirror.resyncCount, 0, 'no resync was needed');
    for (const s of backend.sessions) assertDeepEqual(mirror.sessions.get(s.uid), s, `session ${s.uid} equal`);
    assertDeepEqual(mirror.queue, backend.queue, 'queue equal');
    store.getState().unsubscribeWorld();
    seeded.handle.dispose();
  }));

test('showcase backend: answering a card removes it, updates the queue, and repeats are alreadyAnswered', () =>
  inShowcase(async () => {
    const w = world();
    const handle = createShowcaseBackend(w, buildTimeline(w), { locale: 'en', theme: 'system', startAt: 0, live: false, uiState: {}, launch: { kind: 'restore' }, buildId: 'x' });
    installShowcaseBackend(handle.backend);
    const patches: StorePatch[] = [];
    await commands.store_subscribe({}, { onmessage: (p: StorePatch) => patches.push(p) } as never);
    const uid = w.roles.approval;
    assert(uid !== null, 'an approval session exists');
    const card = w.snapshot.sessions.find((s) => s.uid === uid)?.pending[0];
    assert(card !== undefined, 'it has a card');
    const res = await commands.interaction_answer({ uid, requestId: card.requestId, decision: { kind: 'allowOnce' }, via: 'digit' });
    assertEqual(res.outcome, 'answered', 'answered');
    const after = handle.current().sessions.find((s) => s.uid === uid) as Session;
    assertEqual(after.pending.length, 0, 'the card left');
    assertEqual(after.activity.state.kind, 'working', 'the session works');
    assert(patches.length === 1 && patches[0]?.fromRev === w.snapshot.rev, 'one patch, chained to the snapshot');
    assertEqual(handle.current().queue.needYou, w.snapshot.queue.needYou - 1, 'Q6: the item left in the same patch');
    const again = await commands.interaction_answer({ uid, requestId: card.requestId, decision: { kind: 'deny' }, via: 'button' });
    assertDeepEqual(again, { outcome: 'alreadyAnswered', previous: 'allowOnce' }, 'a repeat reports the first decision');
    const missing = (await assertRejects(() => commands.interaction_answer({ uid, requestId: 'r_nope_1' as never, decision: { kind: 'deny' }, via: 'button' }), 'unknown request')) as IpcError;
    assertEqual(missing.code, 'NOT_FOUND', 'unknown request → NOT_FOUND');
    handle.dispose();
  }));

test('showcase backend: conversations, PTY screens, History and on-demand texts', () =>
  inShowcase(async () => {
    const w = world();
    const handle = createShowcaseBackend(w, [], { locale: 'ko', theme: 'dark', startAt: 0, live: false, uiState: {}, launch: { kind: 'restore' }, buildId: 'x' });
    installShowcaseBackend(handle.backend);
    const uid = w.roles.approval as Session['uid'];
    const events: ConvoEnvelope[] = [];
    const sub = await commands.convo_subscribe({ uid, viewId: 'v-1' as ViewId, tailBlocks: null }, { onmessage: (e: ConvoEnvelope) => events.push(e) } as never);
    assertEqual(sub.source, 'stream', 'a Conversation session streams');
    await sleep(5);
    assertEqual(events[0]?.event.kind, 'reset', 'the first event is a reset');
    assertEqual(events[0]?.seq, 1, 'seq starts at 1');
    const terminal = w.snapshot.sessions.find((s) => s.uid === w.roles.terminal);
    assert(terminal && terminal.control.kind === 'inGilde' && terminal.control.pty !== null, 'a terminal session with a PTY');
    const chunks: PtyChunk[] = [];
    const info = await commands.pty_attach({ ptyId: terminal.control.pty, viewId: 'v-2' as ViewId }, { onmessage: (c: PtyChunk) => chunks.push(c) } as never);
    await sleep(5);
    assertEqual(chunks[0]?.kind, 'data', 'PTY data arrives');
    assertEqual(info.cols, 100, 'PTY size');
    // A25.4: the snapshot carries the core's GLDP header, which the terminal's own reader accepts.
    const snap = new Uint8Array(await commands.pty_snapshot({ ptyId: terminal.control.pty }));
    assertDeepEqual([...snap.slice(0, 8)], [0x47, 0x4c, 0x44, 0x50, 1, 0, 0, 0], 'tag bytes, version 1, reserved zeros');
    const header = readSnapshotHeader(snap);
    assertEqual(header.nextSeq, snap.length - 16, 'next_seq = the ring bytes after the header');
    assertEqual(header.nextSeq, info.offset, 'the snapshot ends where attach said the stream is');
    const page = await commands.history_query({ queryId: 'q1' as never, text: '', filters: { projectId: null, vendor: null, from: null, to: null, accountId: null, origin: null }, cursor: null, limit: 25 });
    assertEqual(page.rows.length, 25, 'first History page');
    assert(page.nextCursor !== null, 'more pages');
    const next = await commands.history_query({ queryId: 'q2' as never, text: '', filters: { projectId: null, vendor: null, from: null, to: null, accountId: null, origin: null }, cursor: page.nextCursor, limit: 25 });
    assertEqual(next.rows.length, 15, 'the rest');
    assertEqual(next.nextCursor, null, 'last page');
    const msg = w.snapshot.messages[0];
    assert(msg !== undefined, 'a team message');
    const body = await commands.team_message_body({ messageKey: msg.key });
    assert(typeof body.text === 'string' && body.text.length > 0, 'a message body on demand');
    const settings = await commands.settings_set({ patch: { locale: null, theme: 'light', attention: null, notify: { watchedSessions: null, sound: true, showDetails: null }, ide: null } });
    assertEqual(settings.theme, 'light', 'settings patch applies');
    assertEqual(settings.notify.sound, true, 'nested patch applies');
    assertEqual(settings.locale, 'ko', 'untouched keys stay');
    handle.dispose();
  }));

test('showcase backend: unknown commands are refused as UNSUPPORTED{showcase} with the marker', () =>
  inShowcase(async () => {
    const w = world();
    const handle = createShowcaseBackend(w, [], { locale: 'en', theme: 'system', startAt: 0, live: false, uiState: {}, launch: { kind: 'restore' }, buildId: 'x' });
    installShowcaseBackend(handle.backend);
    const err = (await assertRejects(() => commands.session_start({} as never), 'session_start is not played')) as IpcError;
    assertEqual(err.code, 'UNSUPPORTED', 'code');
    assertEqual(err.params.cap, 'showcase', 'cap');
    assert(typeof err.detail === 'string' && err.detail.startsWith(SHOWCASE_MARKER), 'marker in the detail');
    // Every command either has an answer or this refusal; none throws anything else.
    const answered = new Set<CommandName>();
    for (const name of COMMAND_NAMES) {
      try {
        await handle.backend(name, {}, { onmessage: () => {} });
        answered.add(name);
      } catch (e) {
        const code = (e as IpcError).code;
        assert(code === 'UNSUPPORTED' || code === 'NOT_FOUND' || e instanceof TypeError, `${name}: ${String(code ?? e)}`);
      }
    }
    for (const name of ['app_bootstrap', 'store_subscribe', 'settings_get', 'health_processes', 'app_quit_preview'] as const) assert(answered.has(name), `${name} answers`);
    handle.dispose();
  }));

test('showcase backend: live mode delivers the script on timers relative to the start time', () =>
  inShowcase(async () => {
    const w = world();
    const steps = buildTimeline(w).map((s) => ({ ...s, at: s.at / 1000 }));
    const handle = createShowcaseBackend(w, steps, { locale: 'en', theme: 'system', startAt: 0, live: true, uiState: {}, launch: { kind: 'restore' }, buildId: 'x' });
    installShowcaseBackend(handle.backend);
    const patches: StorePatch[] = [];
    await commands.store_subscribe({}, { onmessage: (p: StorePatch) => patches.push(p) } as never);
    await sleep(80);
    assertEqual(patches.length, steps.length, 'every step arrived on its timer');
    for (let i = 1; i < patches.length; i++) assertEqual(patches[i]?.fromRev, patches[i - 1]?.toRev, 'gap-free');
    handle.dispose();
  }));

test('showcase data: neutral sample data — no emoji, no real product names, no filler text', () => {
  const text = JSON.stringify(world({ agents: 31, teams: ['native', 'nativeInferred', 'orchestrator'] }));
  assert(!/[\u{1F300}-\u{1FAFF}\u{2600}-\u{27BF}]/u.test(text), 'no emoji');
  // The filler words are spelled in pieces so the UI lint (ui/no-sample-data) does not flag this check itself.
  const filler = new RegExp(['lo' + 'rem', 'ip' + 'sum', 'acme'].join('|'), 'i');
  assert(!filler.test(text), 'no filler names');
  const scenes: SceneName[] = [...SCENE_NAMES];
  assertEqual(new Set(scenes).size, scenes.length, 'scene names unique');
});

test('home-notice (A26.6): Home after a quit that stopped sessions, with one account near its limit', () =>
  inShowcase(async () => {
    const seeded = seedShowcase(parseShowcaseQuery('?scene=home-notice&live=0'), NOW);
    const boot = await commands.app_bootstrap({});
    assertEqual(boot.launch.kind, 'afterQuit', 'launch after a quit');
    assertEqual(boot.launch.kind === 'afterQuit' ? boot.launch.count : 0, 2, 'two sessions stopped');
    const hot = seeded.world.snapshot.accounts.flatMap((a) => a.quota?.windows ?? []).filter((w) => (w.usedPct ?? 0) >= 90);
    assertEqual(hot.length, 1, 'one quota window at ≥ 90 %');
    const plain = seedShowcase(parseShowcaseQuery('?scene=home&live=0'), NOW);
    assertEqual((await commands.app_bootstrap({})).launch.kind, 'restore', 'the plain Home restores');
    assertEqual(plain.world.snapshot.accounts.flatMap((a) => a.quota?.windows ?? []).filter((w) => (w.usedPct ?? 0) >= 90).length, 0, 'and has no warning');
    seeded.handle.dispose();
    plain.handle.dispose();
  }));

test('showcase team-panes scene (A30.1): one starting, one failed and one running unknown pane; the backend serves its terminal', () =>
  inShowcase(async () => {
    const options = parseShowcaseQuery('?scene=team-panes&live=0');
    const w = buildWorld(options.world, NOW);
    const team = w.snapshot.teams.find((t) => t.hosted);
    assert(team !== undefined, 'a hosted team');
    const kinds = team.paneTiles.map((p) => (p.state.kind === 'starting' ? (p.state.spawnFailed ? 'failed' : 'starting') : p.state.running ? 'running' : 'exited'));
    assertDeepEqual(kinds, ['starting', 'failed', 'running'], 'the three tiles, in creation order');
    const running = team.paneTiles[2];
    assert(running?.pty !== null && running?.pty !== undefined, 'only the running unknown pane has a terminal');
    assert(team.paneTiles.slice(0, 2).every((p) => p.pty === null), 'starting panes have none');
    const members = w.snapshot.members.filter((m) => m.teamKey === team.key && m.role !== 'lead');
    assert(members.length + team.paneTiles.length <= 4, 'everything fits next to the lead (no strip)');
    const handle = createShowcaseBackend(w, buildTimeline(w), { locale: 'en', theme: 'system', startAt: 0, live: false, uiState: {}, launch: { kind: 'restore' }, buildId: 'x' });
    installShowcaseBackend(handle.backend);
    const attached = await commands.pty_attach({ ptyId: running.pty, viewId: 'pane-tile' as ViewId }, { onmessage: () => {} } as never);
    assertEqual(attached.title, 'dev-server', 'the pane terminal attaches');
  }));
