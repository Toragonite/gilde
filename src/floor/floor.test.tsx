/**
 * The Floor in the DOM (pixel-office §6–§7, ux-ia §10): the twin is a named
 * tree with one Tab stop and arrow keys, selection is focus, Enter opens,
 * `⌘⏎` approves nothing; the counter shows the queue's own counts in queue
 * order; Show as list persists; reduced motion says so; axe finds nothing
 * serious.
 */

import { act, fireEvent, render } from '@testing-library/react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { useAppStore } from '../store/useAppStore';
import { assert, assertEqual, domTest, sleep } from '../test/harness';
import { FloorScreen, TeamFloor } from './Floor';
import { floorEngine } from './engine';
import { parsePrefs } from './prefs';
import { showcaseSnapshot } from './testing';

const LAYOUT_RULES = ['color-contrast', 'target-size', 'scrollable-region-focusable', 'region'];

async function axeViolations(root: Element): Promise<string[]> {
  const axe = (await import('axe-core')).default;
  const result = await axe.run(root, { rules: Object.fromEntries(LAYOUT_RULES.map((id) => [id, { enabled: false }])), resultTypes: ['violations'] });
  return result.violations.filter((v) => v.impact === 'serious' || v.impact === 'critical').map((v) => `${v.id}: ${v.nodes.map((n) => n.target.join(' ')).join(', ')}`);
}

/** Let the engine's first frame (requestAnimationFrame) land inside act. */
async function settle(): Promise<void> {
  await act(async () => {
    await sleep(30);
  });
}

function seed(agents = 20, teams: ('native' | 'nativeInferred' | 'orchestrator')[] = ['nativeInferred', 'orchestrator'], prefs: Record<string, boolean> | null = null) {
  resetHotkeysForTests();
  floorEngine.resetForTests();
  const s = useAppStore.getState();
  s.setLocale('ko');
  s.setFloorPrefs(prefs);
  s.navigate({ screen: 'floor' });
  s.applyStoreSnapshot(showcaseSnapshot(agents, teams, 0, 1));
}

domTest('T-floor-twin: a tree named "작업장" with levels, names and one Tab stop', async () => {
  seed();
  const { container } = render(<FloorScreen />);
  await settle();
  const tree = container.querySelector('[role="tree"]');
  assert(tree, 'a tree');
  assertEqual(tree.getAttribute('aria-label'), '작업장', 'named after the Floor (F7)');
  const items = [...container.querySelectorAll('[role="treeitem"]')];
  assert(items.length > 20, `members, tasks and sessions are listed (${items.length})`);
  for (const it of items) {
    assert(it.getAttribute('aria-level'), 'every item has a level');
    assert((it.textContent ?? '').length > 0, 'every item is named by its own text');
    assert(!it.querySelector('[role="treeitem"]'), 'no item holds another (a flat tree: the name is never a whole subtree)');
  }
  assertEqual(items.filter((it) => it.getAttribute('tabindex') === '0').length, 1, 'one Tab stop (roving)');
  const team = items.find((it) => (it.textContent ?? '').startsWith('팀 docs-refresh'));
  assert(team, 'the orchestrator team is an item');
  assertEqual(team.textContent, '팀 docs-refresh: 작업 2/6 완료, 막힘 1, 전달 중 메시지 1', 'progress, blocked and in flight in words');
  const member = items.find((it) => /^coordinator, claude, /.test(it.textContent ?? ''));
  assert(member, 'members carry name, vendor, state and ownership');
  assert((member.textContent ?? '').includes('지켜보는 중'), 'ownership is said in words, never a dot (F8)');
  const inferred = items.find((it) => /^지시 \d+ \(추정\)/.test(it.textContent ?? ''));
  assert(inferred, 'inferred items say "추정"');
  const v = await axeViolations(container);
  assertEqual(v.length, 0, `axe: ${v.join(' | ')}`);
});

domTest('T-floor-twin: Show as list makes the twin visible, persists, and arrows move focus', async () => {
  seed();
  const { container, getByRole } = render(<FloorScreen />);
  await settle();
  assert(container.querySelector('canvas'), 'picture mode draws a canvas');
  assert(container.querySelector('[data-fl-twin]')?.classList.contains('fl-sr-only'), 'the twin is in the accessibility tree, visually hidden');
  act(() => {
    fireEvent.click(getByRole('button', { name: '목록으로 보기' }));
  });
  assert(!container.querySelector('canvas'), 'no canvas in list mode');
  assert(!container.querySelector('[data-fl-twin]')?.classList.contains('fl-sr-only'), 'the twin is the list');
  assertEqual(parsePrefs(useAppStore.getState().ui.floorPrefs).list, true, 'persisted in floorPrefs');
  const items = [...container.querySelectorAll<HTMLElement>('[role="treeitem"]')];
  act(() => (items[0] as HTMLElement).focus());
  act(() => {
    fireEvent.keyDown(items[0] as HTMLElement, { key: 'ArrowDown', code: 'ArrowDown' });
  });
  assertEqual(document.activeElement, items[1], 'ArrowDown moves to the next item');
  assertEqual(items[1]?.getAttribute('tabindex'), '0', 'the Tab stop moved with it');
  const v = await axeViolations(container);
  assertEqual(v.length, 0, `axe in list mode: ${v.join(' | ')}`);
});

domTest('T-floor-twin: Enter on a session opens it; ⌘⏎ approves nothing; selection is focus', async () => {
  seed(8, []);
  const { container } = render(<FloorScreen />);
  await settle();
  const seat = container.querySelector<HTMLElement>('[role="treeitem"][data-flsel]');
  assert(seat, 'a session item');
  act(() => seat.focus());
  assertEqual(floorEngine.sel, seat.getAttribute('data-flsel'), 'focusing the item selects the character');
  act(() => {
    fireEvent.keyDown(seat, { key: 'Enter', code: 'Enter', metaKey: true });
  });
  assertEqual(useAppStore.getState().ui.view.screen, 'floor', '⌘⏎ does nothing on the Floor (F6)');
  act(() => {
    fireEvent.keyDown(seat, { key: 'Enter', code: 'Enter' });
  });
  const view = useAppStore.getState().ui.view;
  assertEqual(view.screen, 'session', 'Enter opens the session');
  assertEqual(view.screen === 'session' ? view.uid : null, seat.getAttribute('data-flsel'), 'the selected one');
});

domTest('T-floor-counter: counts are the queue’s, tickets in queue order, a ticket opens its session', async () => {
  seed(31, ['native', 'nativeInferred', 'orchestrator']);
  const { container } = render(<FloorScreen />);
  await settle();
  const q = useAppStore.getState().world.queue;
  const label = container.querySelector('.fl-cnt-l')?.textContent ?? '';
  assert(label.startsWith(`나를 기다림 · ${q.needYou}`), `the counter says the queue count (${label})`);
  if (q.also) assert(label.includes(`+${q.also}`), 'and bands 3–6');
  const tickets = [...container.querySelectorAll<HTMLElement>('[data-fl-ticket]')];
  assertEqual(tickets.length, q.items.length, 'one ticket per queue item');
  const first = q.items[0];
  assert(first && first.subject.kind === 'session', 'a session first');
  assertEqual(tickets[0]?.getAttribute('data-flseat'), first.subject.uid, 'queue order');
  act(() => {
    fireEvent.click(tickets[0] as HTMLElement);
  });
  const view = useAppStore.getState().ui.view;
  assertEqual(view.screen === 'session' ? view.uid : null, first.subject.uid, 'the ticket opens its session');
  assertEqual(useAppStore.getState().ui.focusCard?.uid, first.subject.uid, 'with its card focused, like ⌘J');
});

domTest('T-floor-motion: reduced motion and Pause say the floor shows the latest state', async () => {
  seed(8, ['native']);
  const g = globalThis as unknown as { matchMedia?: unknown };
  const was = g.matchMedia;
  g.matchMedia = (q: string) => ({ matches: q.includes('reduce'), media: q, addEventListener() {}, removeEventListener() {} });
  try {
    const { container, unmount } = render(<FloorScreen />);
  await settle();
    assert((container.textContent ?? '').includes('움직임을 꺼서 작업장은 최신 상태만 보여 줍니다.'), 'the reduced-motion line (ux-ia row 126)');
    unmount();
  } finally {
    g.matchMedia = was;
  }
  const { container, getByRole } = render(<FloorScreen />);
  await settle();
  assert(!(container.textContent ?? '').includes('움직임을 꺼서'), 'no line with motion on');
  act(() => {
    fireEvent.click(getByRole('button', { name: '애니메이션 정지' }));
  });
  assertEqual(floorEngine.paused, true, 'Pause');
  assert((container.textContent ?? '').includes('움직임을 꺼서'), 'Pause says it too');
  assertEqual(getByRole('button', { name: '애니메이션 정지' }).getAttribute('aria-pressed'), 'true', 'the button is pressed');
});

domTest('T-floor-team: Team › Floor lists only its team and no standalone sessions', async () => {
  seed(12, ['native']);
  const key = [...useAppStore.getState().world.teams.keys()][0];
  assert(key, 'a team');
  const { container } = render(<TeamFloor teamKey={key} />);
  await settle();
  const labels = [...container.querySelectorAll('[role="treeitem"]')].map((it) => it.textContent ?? '');
  assert(labels.some((l) => l.startsWith('팀 auth-split')), 'its team');
  assert(!labels.includes('단독 세션'), 'no standalone sessions');
  assert(!container.querySelector('h1'), 'no screen heading (the Team screen has its own)');
  await sleep(0);
});

domTest('T-floor-empty: an empty floor says so in one sentence with one action', async () => {
  resetHotkeysForTests();
  floorEngine.resetForTests();
  const s = useAppStore.getState();
  s.setLocale('ko');
  s.applyStoreSnapshot(showcaseSnapshot(0, [], 0));
  const { container, getByRole } = render(<FloorScreen />);
  await settle();
  assert((container.textContent ?? '').includes('실행 중인 세션이 없습니다.'), 'the empty line');
  act(() => {
    fireEvent.click(getByRole('button', { name: '새 세션' }));
  });
  assertEqual(useAppStore.getState().ui.sheet, 'newSession', 'New session');
});
