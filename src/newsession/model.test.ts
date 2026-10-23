/**
 * New session rules (ux-ia §3.12, §5.8; contract §4.2.2, F-25, A26.2).
 */

import type { ProjectId } from '../lib/gen/ProjectId';
import type { SessionUid } from '../lib/gen/SessionUid';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { project, session, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import * as M from './model';
import { options } from './testing';


test('T18 slug: the first five words of the prompt, lowercase ASCII', () => {
  assertEqual(M.slugOfPrompt('Add rate limiting to the public order submit endpoint'), 'add-rate-limiting-to-the', 'five words');
  assertEqual(M.slugOfPrompt('  Fix   "auth" — refresh_token race!  '), 'fix-auth-refresh-token-race', 'punctuation becomes hyphens');
  assertEqual(M.slugOfPrompt('Café déjà vu'), 'cafe-deja-vu', 'accents fold');
  assertEqual(M.slugOfPrompt('로그인 버그를 고쳐 줘'), '', 'a Korean prompt gives no slug');
  assertEqual(M.slugOfPrompt(''), '', 'empty');
  const long = M.slugOfPrompt(`${'a'.repeat(40)} ${'b'.repeat(40)}`);
  assert(M.SLUG_RE.test(long) && long.length <= 63, `capped and valid: ${long}`);
  assertEqual(M.slugOfPrompt('-- --'), '', 'nothing left');
});

test('T18 slug: an empty slug falls back to <yyyymmdd>-<4 hex> (F-25), stable for the sheet', () => {
  const fb = M.fallbackSlug(new Date(2026, 9, 3), 'a1b2');
  assertEqual(fb, '20261003-a1b2', 'date and hex');
  assert(M.SLUG_RE.test(fb), 'a valid slug');
  assertEqual(M.effectiveSlug('로그인 버그', null, fb), fb, 'fallback');
  assertEqual(M.effectiveSlug('Fix login', null, fb), 'fix-login', 'prompt wins');
  assertEqual(M.effectiveSlug('Fix login', 'my-name', fb), 'my-name', 'an edit wins');
  assert(/^[0-9a-f]{4}$/.test(M.randomHex4(() => 0.999)), 'four hex digits');
});

test('T18 resolve: zero choices take every default; invalid picks fall back', () => {
  const o = options();
  const r = M.resolve(o, {});
  assertEqual(r.vendor, 'claude', 'vendor');
  assertEqual(r.accountId, 'a_claude_work', 'account');
  assertEqual(r.model, 'sonnet', 'model');
  assertEqual(r.controlMode, 'conversation', 'mode');
  assertEqual(r.permissionMode, 'default', 'permissions');
  assertDeepEqual(r.where, { kind: 'here', locationId: 'l_main' }, 'where');
  assertEqual(r.team, false, 'team off');
  assertDeepEqual(r.accounts.map((a) => a.accountId), ['a_claude_work', 'a_claude_personal'], 'only this vendor');

  const bad = M.resolve(o, { accountId: 'a_codex' as never, model: 'gpt', controlMode: 'turnBased', permissionMode: 'default' });
  assertEqual(bad.accountId, 'a_claude_work', 'an account of another vendor is not used');
  assertEqual(bad.model, 'sonnet', 'an unknown model is not used');
  assertEqual(bad.controlMode, 'conversation', 'a mode the vendor does not offer is not used');

  const none = M.resolve(options({ defaults: { ...o.defaults, location: null } }), {});
  assertDeepEqual(none.where, { kind: 'newWorktree' }, 'the last start used a worktree: a new one');
  assertEqual(M.resolve(o, { model: null }).model, null, 'the vendor default model is a choice');
});

test('T18 resolve: Team forces Terminal and exists only for claude', () => {
  const r = M.resolve(options(), { team: true, controlMode: 'conversation' });
  assertEqual(r.team, true, 'on');
  assertEqual(r.controlMode, 'terminal', 'Terminal');
  const codex = M.resolve(options({ defaults: { ...options().defaults, vendor: 'codex' } }), { vendor: 'codex', team: true });
  assertEqual(codex.team, false, 'no team for codex');
  assertDeepEqual(M.picksForVendor({ vendor: 'claude', accountId: 'x' as never, model: 'opus', team: true }, 'codex'), { vendor: 'codex', where: undefined, team: false }, 'vendor-bound picks dropped');
});

test('T18 blockers and the start request', () => {
  const o = options();
  const r = M.resolve(o, {});
  assertEqual(M.blockerOf(null, o, r, 'x', ''), 'noProject', 'no project');
  assertEqual(M.blockerOf('p' as ProjectId, null, r, 'x', ''), 'loading', 'options pending');
  assertEqual(M.blockerOf('p' as ProjectId, o, M.resolve(o, { where: { kind: 'newWorktree' } }), 'Bad Slug', ''), 'slugInvalid', 'slug rule');
  assertEqual(M.blockerOf('p' as ProjectId, o, r, 'x', 'x'.repeat(M.PROMPT_MAX_BYTES + 1)), 'promptTooLong', '256 KiB');
  assertEqual(M.blockerOf('p' as ProjectId, o, M.resolve(options({ accounts: [] }), {}), 'x', ''), 'noAccount', 'no account');
  assertEqual(M.blockerOf('p' as ProjectId, o, r, 'x', 'go'), null, 'ready');

  const req = M.buildStartReq('c1' as never, 'p' as ProjectId, M.resolve(o, { where: { kind: 'newWorktree' } }), 'fix-login', '   \n');
  assertEqual(req.prompt, null, 'whitespace = no first message');
  assertDeepEqual(req.location, { kind: 'newWorktree', slug: 'fix-login', base: null }, 'new worktree');
  assertEqual(M.buildStartReq('c1' as never, 'p' as ProjectId, r, 'x', ' keep me ').prompt, ' keep me ', 'sent as typed');
});

test('T18 notices: reset times read as a clock time; the switched line knows its undo', () => {
  const at = new Date(2026, 9, 23, 14, 10).getTime();
  const p = M.noticeParams({ key: 'accounts.allNear', params: { vendor: 'claude', time: at } }, 'en');
  assertEqual(p.time, '14:10', 'epoch ms → 14:10');
  assertEqual(M.noticeParams({ key: 'x', params: { n: 3 } }, 'en').n, 3, 'counts untouched');
  const o = options();
  assertEqual(M.switchedFrom({ key: 'accounts.switched', params: { a: 'Personal', b: 'Work', p: 91 } }, o.accounts), 'a_claude_personal', 'by label');
  assertEqual(M.switchedFrom({ key: 'accounts.switched', params: { a: 'Nope', b: 'Work', p: 91 } }, o.accounts), null, 'unknown label');
  assertEqual(M.maxUsedPct({ windows: [{ kind: 'fiveHour', label: null, usedPct: 38, resetsAt: null }, { kind: 'week', label: null, usedPct: 140, resetsAt: null }], fetchedAt: 1, stale: false, unavailable: null }), 38, 'out of range is absent');
});

test('T18 project preselect (A26.2): context, then the main view, then the first project', () => {
  const world = emptyWorld();
  const projects = new Map([
    ['p1' as ProjectId, project('p1', { order: 1 })],
    ['p2' as ProjectId, project('p2', { order: 0 })],
  ]);
  const sessions = new Map([[uid(5), session(5, { projectId: 'p1' as ProjectId })]]);
  const w = { ...world, projects, sessions };
  const ui = initialUiState();
  assertEqual(M.preselectProject(w, ui.view, null, 'p9' as ProjectId), 'p9', 'a context wins, even before its patch');
  assertEqual(M.preselectProject(w, { screen: 'session', uid: uid(5), sub: 'conversation' }, null, null), 'p1', 'the open session');
  assertEqual(M.preselectProject(w, { screen: 'split' }, uid(5) as SessionUid, undefined), 'p1', 'the focused tile');
  assertEqual(M.preselectProject(w, ui.view, null, null), 'p2', 'the first project in order');
  assertEqual(M.preselectProject(world, ui.view, null, null), null, 'none');
});
