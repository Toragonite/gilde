/**
 * i18n tests (contract §14.2 T-i18n-*, §13.3, A7.11).
 *
 * The types already force `en` to carry exactly the keys of `ko` per area.
 * These tests lock what types cannot: no key in two areas, the same parameters
 * on both sides, every contract and ux-ia key present (and the retired ones
 * absent), every error code's key, English plurals, Korean particles and the
 * Korean register.
 */

import { ERROR_CODES } from '../lib/ipcError';
import { assert, assertEqual, test } from '../test/harness';
import { ERROR_KEYS } from './areas/errors';
import {
  en,
  formatMessage,
  isMessageKey,
  ko,
  translate,
  translateDynamic,
} from './index';
import type { MessageKey } from './index';
import { batchimOf, josa, withJosa } from './josa';
import { parsePluralOptions } from './plural';
import { AREAS } from './ko';

// ---------------------------------------------------------------------------
// Key lists the contract fixes
// ---------------------------------------------------------------------------

/** contract §9.12 (78 rows). */
const CONTRACT_9_12_KEYS = [
  'action.signIn', 'action.showLog', 'action.retry', 'action.startAsTerminal', 'action.openAsTerminal',
  'action.closeTile', 'action.stopDiscardQueued', 'action.continueCopyOn', 'card.stackPosition',
  'approval.allowWithNote', 'turn.interrupted', 'team.neverRead', 'team.messagesUnknown', 'composer.queuedChip',
  'turn.running', 'team.answeredAtLead', 'team.locationUnverified', 'team.ended', 'team.placeholder',
  'team.didNotStart', 'team.prefill.tell', 'team.prefill.tellAll', 'team.prefill.spawn', 'team.prefill.askStop',
  'team.prefill.addTask', 'account.default', 'account.unknown', 'history.vendorDate', 'prompt.questionNotShown',
  'notify.stillWaiting', 'notify.body', 'notify.showDetails', 'details.connectionHelper',
  'details.unverifiedVersion', 'details.teamFormatUnverified', 'details.teammateVersionDiffers',
  'details.followNote', 'compat.unreadableRequest', 'notice.conversationUnavailable',
  'notice.conversationMinVersion', 'error.compatFailed', 'notice.cantReadFolders', 'evidence.cantRead',
  'health.folderDenied', 'notice.memoryOnly', 'error.diskFull', 'update.restart', 'card.abandonedOnResume',
  'home.continueAllWaiting', 'home.stoppingPrevious', 'approval.composerAnswerHint', 'plan.composerHint',
  'team.unknownPane', 'team.unattributedMember', 'team.backgroundHint', 'team.backgroundedLead',
  'origin.otherGilde', 'screen.noStopsTurn', 'link.localConfirm', 'card.trustContinue', 'card.continueInTerminal',
  'team.bypassMayPrompt', 'card.needsSetup', 'composer.sendNowRestarts', 'team.closePaneUnread',
  'sheet.quit.pendingLost', 'card.degraded', 'card.sessionEnded', 'convo.unanswerable', 'convo.notReading',
  'convo.noPromptSeen', 'turn.newConversation', 'turn.earlierNotShown', 'turn.helperRestarted',
  'home.stoppedOnCrash', 'history.searching', 'history.notIndexed', 'newsession.findingAgents',
] as const;

/** ux-ia §9.2 (128 rows; the wildcard rows expanded; #79 retired). */
const UX_IA_9_2_KEYS = [
  // 1–18
  'state.starting', 'state.working', 'state.needsApproval', 'state.needsAnswer', 'state.error',
  'state.error.usageLimit', 'state.error.signIn', 'state.error.didntStart', 'state.error.agent', 'state.crashed',
  'state.noProgress', 'state.newReply', 'state.ready', 'state.ended', 'state.stopped', 'state.unknown',
  'state.leftover', 'state.likely',
  // 19–25
  'ownership.inGilde', 'ownership.watching', 'ownership.runsIn',
  'origin.terminal', 'origin.vscode', 'origin.itermTeam', 'origin.background', 'origin.headless',
  'origin.orchestrator',
  'mode.conversation', 'mode.terminal', 'mode.turnBased',
  'session.tab.conversation', 'session.tab.changes', 'session.tab.log',
  'team.mode.panes', 'team.mode.board', 'team.mode.floor',
  // 26–37
  'nav.home', 'nav.history', 'nav.accountsHealth', 'nav.needsYou', 'nav.earlierProjects', 'sidebar.endedToday',
  'home.alsoWaiting', 'home.pipelines', 'home.running', 'home.resources', 'home.accounts', 'queue.next',
  // 38–57
  'action.newSession', 'action.send', 'action.stop', 'action.stopSession', 'action.forceStop', 'action.later',
  'action.goToOwner', 'action.continue', 'action.continueCopy', 'action.continueAll', 'action.continueOn',
  'action.openInGilde', 'action.askLead', 'action.openInIde', 'action.diffInIde', 'action.review',
  'action.askCommit', 'action.cleanUp', 'action.showAsList', 'action.openFloor',
  // 58–72
  'approval.allowOnce', 'approval.always', 'approval.deny', 'approval.denyWhy',
  'plan.ready', 'plan.approve', 'plan.keepPlanning',
  'observed.whyLink', 'observed.waitingApproval', 'composer.placeholder', 'model.nextMessage',
  'noProgress.evidence',
  'floor.waitsOnYou', 'floor.blockedBy', 'floor.bottleneck', 'floor.noTaskList', 'floor.pauseAnimation',
  'floor.tidy', 'floor.counter',
  'notify.paused',
  'history.titleOnly', 'history.scope.titles', 'history.scope.titlesAndPrompts', 'history.scope.everything',
  'changes.unknownSource', 'tile.crashed',
  // 73–128 (79 retired)
  'approval.command', 'approval.edit', 'approval.composerHint', 'approval.defaultNo', 'approval.answeredElsewhere',
  'approval.useOwnKeys', 'observed.banner', 'observed.waitingEdit', 'observed.why', 'observed.unreadable',
  'observed.headless', 'observed.background', 'ended.banner', 'ended.notResumable', 'composer.queued',
  'composer.stopping', 'composer.stillStopping', 'turn.stopped', 'model.terminal', 'empty.needsYou', 'empty.home',
  'firstRun.looking', 'firstRun.found', 'firstRun.notify', 'error.usageLimit', 'error.didntStart',
  'team.external', 'team.noTasks', 'team.paneHint', 'team.inProcess', 'team.outsideGilde', 'team.closePane',
  'team.resumedLead', 'changes.sharedFolder', 'changes.ownerWriting', 'sheet.quit', 'sheet.quit.elsewhere',
  'home.stoppedOnQuit', 'sheet.cleanup', 'sheet.stopWatched', 'health.never', 'health.memHigh',
  'accounts.switched', 'accounts.allNear', 'accounts.noLimits', 'orchestrator.absent', 'orchestrator.notRunning',
  'orchestrator.messageFromLead', 'notify.many', 'notify.repeat', 'history.retention', 'split.max',
  'floor.reducedMotion', 'floor.listHint', 'floor.activityNotShown',
] as const;

/** Retired by the lead (ux-ia #79) or removed from §9.12 (Appendix B A3, F-31). */
const RETIRED_KEYS = [
  'approval.answerFirst',
  'approval.alwaysEdits',
  'card.mayHaveExpired',
  'convo.reconnecting',
  'convo.mayWaitInTerminal',
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function closingBrace(s: string, open: number): number {
  let depth = 0;
  for (let i = open; i < s.length; i++) {
    if (s[i] === '{') depth++;
    else if (s[i] === '}') {
      depth--;
      if (depth === 0) return i;
    }
  }
  return -1;
}

interface Placeholder {
  name: string;
  /** Inside a plural block's text, or the plural selector itself. */
  plural: boolean;
}

/** Every placeholder of a template, walking into plural blocks the way `formatMessage` does. */
function placeholders(template: string, inPlural = false): Placeholder[] {
  const out: Placeholder[] = [];
  let i = 0;
  while (i < template.length) {
    if (template[i] !== '{') {
      i++;
      continue;
    }
    const close = closingBrace(template, i);
    if (close < 0) break;
    const inner = template.slice(i + 1, close);
    i = close + 1;
    const simple = /^\s*(\w+)\s*$/.exec(inner);
    if (simple) {
      out.push({ name: simple[1] as string, plural: inPlural });
      continue;
    }
    const plural = /^\s*(\w+)\s*,\s*plural\s*,([\s\S]*)$/.exec(inner);
    if (plural) {
      const name = plural[1] as string;
      out.push({ name, plural: true });
      const block = parsePluralOptions(name, plural[2] ?? '');
      assert(block !== null, `malformed plural block in "${template}"`);
      for (const text of [...block.exact.values(), ...block.categories.values()]) {
        out.push(...placeholders(text, true));
      }
    }
  }
  return out;
}

function paramNames(template: string): string[] {
  return [...new Set(placeholders(template).map((p) => p.name))].sort();
}

const koTable = ko as Readonly<Record<string, string>>;
const enTable = en as Readonly<Record<string, string>>;
const allKeys = Object.keys(ko) as MessageKey[];

// ---------------------------------------------------------------------------
// T-i18n parity
// ---------------------------------------------------------------------------

test('T-i18n parity: every area has the same keys in ko and en, and no key is in two areas', () => {
  const seen = new Map<string, string>();
  for (const [area, mod] of Object.entries(AREAS)) {
    const koKeys = Object.keys(mod.ko).sort();
    const enKeys = Object.keys(mod.en).sort();
    assertEqual(enKeys.join('\n'), koKeys.join('\n'), `area ${area}: en and ko have the same keys`);
    for (const key of koKeys) {
      const other = seen.get(key);
      assert(other === undefined, `key ${key} is in both ${other} and ${area}`);
      seen.set(key, area);
    }
  }
  assertEqual(seen.size, allKeys.length, 'the merged table holds every area key exactly once');
  assertEqual(Object.keys(en).length, allKeys.length, 'en has as many keys as ko');
});

test('T-i18n parity: no message is empty and both sides use the same parameters', () => {
  for (const key of allKeys) {
    const k = koTable[key] ?? '';
    const e = enTable[key] ?? '';
    assert(k.trim().length > 0, `ko.${key} is not empty`);
    assert(e.trim().length > 0, `en.${key} is not empty`);
    assertEqual(paramNames(e).join(','), paramNames(k).join(','), `${key} uses the same parameters in ko and en`);
  }
});

test('T-i18n parity: every message renders completely when given its parameters', () => {
  for (const key of allKeys) {
    for (const locale of ['ko', 'en'] as const) {
      const template = (locale === 'ko' ? koTable : enTable)[key] ?? '';
      const params: Record<string, string | number> = {};
      for (const p of placeholders(template)) params[p.name] = p.plural ? 2 : 'x';
      const out = translate(locale, key, params);
      assert(!/[{}]/.test(out), `${locale}.${key} leaves no brace: ${out}`);
      assert(!/\((이|을|은|과|와|아|으)\)/.test(out), `${locale}.${key} resolves every particle: ${out}`);
    }
  }
});

test('T-i18n parity: every contract §9.12 key and every ux-ia §9.2 key exists; retired keys do not', () => {
  assertEqual(CONTRACT_9_12_KEYS.length, 78, 'the §9.12 list has its 78 rows');
  for (const key of [...CONTRACT_9_12_KEYS, ...UX_IA_9_2_KEYS]) {
    assert(isMessageKey(key), `${key} exists in ko`);
    assert(Object.prototype.hasOwnProperty.call(en, key), `${key} exists in en`);
  }
  for (const key of RETIRED_KEYS) assert(!isMessageKey(key), `${key} is retired`);
  for (const key of allKeys) assert(!key.startsWith('notify.title'), `${key}: notify.title.* is replaced by notify.body`);
});

test('T-i18n parity: every error code has its default key in ko and en (A7.11)', () => {
  assertEqual(Object.keys(ERROR_KEYS).length, ERROR_CODES.length, 'one key per error code');
  for (const code of ERROR_CODES) {
    const key = ERROR_KEYS[code];
    assert(key !== undefined, `${code} has a key`);
    assert(isMessageKey(key), `${code} → ${key} exists in ko`);
    assert(Object.prototype.hasOwnProperty.call(en, key), `${code} → ${key} exists in en`);
    const expected =
      code === 'VERSION_MISMATCH'
        ? 'update.restart'
        : 'error.' + code.toLowerCase().replace(/_([a-z])/g, (_, c: string) => c.toUpperCase());
    assertEqual(key, expected, `${code} uses the error.<camelCase code> key`);
  }
  const errorKeys = allKeys.filter((k) => /^error\.[a-z]+[A-Za-z]*$/.test(k) && k !== 'error.reference');
  assert(errorKeys.length >= 17, 'at least the 17 code keys exist');
});

// ---------------------------------------------------------------------------
// T-i18n-plural
// ---------------------------------------------------------------------------

/** English strings that use a count without a plural block, each with its reason. */
const BARE_COUNT_ALLOWED: Readonly<Record<string, string>> = {
  'card.stackPosition': 'a position, "{i} of {n}": no counted noun',
  'composer.queuedChip': 'a chip, "{n} queued": no counted noun',
  'sidebar.endedToday': '"{n} ended today": no counted noun',
  'history.searching': 'a ratio, "{loaded} of {total} loaded"',
  'noProgress.evidence': '"{n} min": an invariant unit abbreviation',
  'orchestrator.notRunning': '"{n} min ago": an invariant unit abbreviation',
  'sheet.quit.elsewhere': '"({n})": a parenthetical count after a plural noun',
};

const COUNT_LIKE = /^(n|i|count|total|loaded|num|\w+Count)$/;

test('T-i18n-plural: en uses a plural block for every counted noun', () => {
  for (const key of Object.keys(BARE_COUNT_ALLOWED)) {
    assert(Object.prototype.hasOwnProperty.call(en, key), `allowlist entry ${key} still exists`);
  }
  for (const key of allKeys) {
    const bare = placeholders(enTable[key] ?? '').filter((p) => !p.plural && COUNT_LIKE.test(p.name));
    if (bare.length === 0) continue;
    assert(
      Object.prototype.hasOwnProperty.call(BARE_COUNT_ALLOWED, key),
      `en.${key} uses a bare count (${bare.map((p) => p.name).join(', ')}): use {n, plural, one {…} other {…}}`,
    );
  }
});

test('T-i18n-plural: one vs other, exact matches and grouped numbers', () => {
  assertEqual(translate('en', 'home.stoppedOnQuit', { n: 1 }), '1 session stopped when Gilde quit.', 'one');
  assertEqual(translate('en', 'home.stoppedOnQuit', { n: 2 }), '2 sessions stopped when Gilde quit.', 'other');
  assertEqual(translate('en', 'home.stoppedOnQuit', { n: 1234 }), '1,234 sessions stopped when Gilde quit.', '# is grouped');
  assertEqual(translate('en', 'notify.many', { n: 1 }), '1 session is waiting for you.', 'verb agrees (one)');
  assertEqual(translate('en', 'notify.many', { n: 3 }), '3 sessions are waiting for you.', 'verb agrees (other)');
  assertEqual(
    translate('en', 'sheet.cleanup', { n: 1, memory: '1.2 GB' }),
    'Stop 1 leftover process and free 1.2 GB. Conversations stay in History.',
    'plural block with another parameter',
  );
  assertEqual(translate('ko', 'home.stoppedOnQuit', { n: 1234 }), 'Gilde를 종료할 때 세션 1,234개가 멈췄습니다.', 'ko has no plural forms');
  const exact = '{n, plural, =0 {nothing} one {# item} other {# items}}';
  assertEqual(formatMessage('en', exact, { n: 0 }), 'nothing', '=0 wins over the category');
  assertEqual(formatMessage('en', exact, { n: 1 }), '1 item', 'one');
  assertEqual(formatMessage('en', exact, { n: 5 }), '5 items', 'other');
  assertEqual(formatMessage('en', '{n, plural, one {# item}}', { n: 2 }), '{n, plural, one {# item}}', 'a block without other stays visible');
  assertEqual(formatMessage('en', exact, { n: 'many' }), exact, 'a non-number count stays visible');
  assertEqual(formatMessage('en', 'a # b', {}), 'a # b', '# outside a plural block is literal');
});

// ---------------------------------------------------------------------------
// T-i18n-josa
// ---------------------------------------------------------------------------

test('T-i18n-josa: the final sound of words', () => {
  const cases: [string, ReturnType<typeof batchimOf>][] = [
    ['세션', 'other'],
    ['터미', 'none'],
    ['서울', 'rieul'],
    ['1', 'rieul'],
    ['2', 'none'],
    ['3', 'other'],
    ['10', 'other'],
    ['100', 'other'],
    ['0', 'other'],
    ['1,000', 'other'],
    ['CPU', 'none'],
    ['URL', 'rieul'],
    ['VM', 'other'],
    ['A', 'none'],
    ['claude', 'none'],
    ['server', 'none'],
    ['codex', 'none'],
    ['terminal', 'rieul'],
    ['VS Code', 'none'],
    ['iTerm2', 'none'],
    ['Mac', 'unknown'],
    ['api-refactor)', 'none'],
    ['', 'unknown'],
    ['...', 'unknown'],
  ];
  for (const [word, expected] of cases) assertEqual(batchimOf(word), expected, `batchimOf(${JSON.stringify(word)})`);
});

test('T-i18n-josa: particles', () => {
  const cases: [string, string, string][] = [
    ['세션', '(이)가', '세션이'],
    ['터미', '(이)가', '터미가'],
    ['서울', '(으)로', '서울로'],
    ['서울', '(을)를', '서울을'],
    ['세션', '(으)로', '세션으로'],
    ['터미널', '(으)로', '터미널로'],
    ['1', '(으)로', '1로'],
    ['3', '(으)로', '3으로'],
    ['10', '(으)로', '10으로'],
    ['0', '(으)로', '0으로'],
    ['CPU', '(이)가', 'CPU가'],
    ['URL', '(으)로', 'URL로'],
    ['VM', '(을)를', 'VM을'],
    ['claude', '(이)가', 'claude가'],
    ['server', '(을)를', 'server를'],
    ['codex', '(으)로', 'codex로'],
    ['terminal', '(으)로', 'terminal로'],
    ['VS Code', '(으)로', 'VS Code로'],
    ['세션', '(은)는', '세션은'],
    ['터미', '(와)과', '터미와'],
    ['세션', '(과)와', '세션과'],
    ['세션', '(이)나', '세션이나'],
    ['서울', '(으)로서', '서울로서'],
    ['Mac', '(이)가', 'Mac(이)가'],
    ['Mac', '(으)로', 'Mac(으)로'],
  ];
  for (const [word, spelling, expected] of cases) {
    assertEqual(withJosa(word, spelling), expected, `${word} + ${spelling}`);
  }
  assertEqual(josa('세션', '(이)야'), '(이)야', 'an unknown spelling is kept as written');
});

test('T-i18n-josa: translate() picks the particle after a parameter', () => {
  assertEqual(
    translate('ko', 'approval.command', { agent: 'claude', dir: '~/x' }),
    'claude가 ~/x에서 명령 실행을 요청합니다.',
    'claude가',
  );
  assertEqual(translate('ko', 'action.goToOwner', { owner: '터미널' }), '터미널로 이동', '터미널로');
  assertEqual(translate('ko', 'action.goToOwner', { owner: 'VS Code' }), 'VS Code로 이동', 'VS Code로');
  assertEqual(translate('ko', 'action.goToOwner', { owner: '세션' }), '세션으로 이동', '세션으로');
  assert(
    translate('ko', 'health.folderDenied', { category: '문서' }).startsWith('Gilde가 문서를 읽을 수 없습니다.'),
    '문서를',
  );
  const tricky = translate('ko', 'approval.command', { agent: 'a(이)가', dir: '{x}' });
  assert(tricky.startsWith('a(이)가'), `a value is shown as it is: ${tricky}`);
  assert(tricky.includes('{x}에서'), `a value is never interpolated again: ${tricky}`);
  assertEqual(
    translate('ko', 'approval.command', { dir: '~' }),
    '{agent}(이)가 ~에서 명령 실행을 요청합니다.',
    'a missing parameter stays visible, with its particle spelling',
  );
});

test('T-i18n-josa: no Korean string writes a fixed particle right after a parameter', () => {
  const fixed = /\}(이|가|을|를|은|는|와|과|로|으로)(?=[\s.,·:)]|$)/;
  for (const key of allKeys) {
    const m = fixed.exec(koTable[key] ?? '');
    assert(m === null, `ko.${key}: use the two-form spelling after a parameter (found "}${m?.[1]}")`);
  }
});

// ---------------------------------------------------------------------------
// T-i18n-register
// ---------------------------------------------------------------------------

const EMOJI = /\p{Extended_Pictographic}/u;

test('T-i18n-register: Korean sentences end in -습니다, never -세요, no exclamation marks or emoji', () => {
  for (const key of allKeys) {
    const text = koTable[key] ?? '';
    assert(!text.includes('세요'), `ko.${key} has no -세요`);
    assert(!text.includes('하십시오'), `ko.${key} has no -하십시오`);
    assert(!/[!！]/.test(text), `ko.${key} has no exclamation mark`);
    assert(!EMOJI.test(text), `ko.${key} has no emoji`);
    const sentences = text.split(/(?<=[.?…])\s+/);
    for (const raw of sentences) {
      const sentence = raw.trim().replace(/[.?…]+$/, '');
      if (sentence.endsWith('다')) assert(sentence.endsWith('니다'), `ko.${key}: "${raw}" ends in -니다`);
      assert(!/(어요|아요|해요|에요|예요|여요|워요|와요)$/.test(sentence), `ko.${key}: "${raw}" is not 해요체`);
    }
  }
});

test('T-i18n-register: English has no exclamation marks or emoji', () => {
  for (const key of allKeys) {
    const text = enTable[key] ?? '';
    assert(!/[!！]/.test(text), `en.${key} has no exclamation mark`);
    assert(!EMOJI.test(text), `en.${key} has no emoji`);
  }
});

// ---------------------------------------------------------------------------
// Keys from Rust
// ---------------------------------------------------------------------------

test('translateDynamic renders backend keys and degrades safely', () => {
  assertEqual(
    translateDynamic('en', 'error.busy', { retryAfterMs: 200 }),
    translate('en', 'error.busy'),
    'a known key renders like a static one',
  );
  assertEqual(translateDynamic('en', 'error.does.not.exist', {}), translate('en', 'error.internal'), 'unknown → error.internal');
  assertEqual(translateDynamic('ko', 'nope', null), translate('ko', 'error.internal'), 'null params');
  assertEqual(
    translateDynamic('en', 'notice.memoryOnly', { reason: { nested: true } as unknown as string }),
    'Gilde cannot save its state: {reason}.',
    'a non-primitive parameter is dropped, never stringified',
  );
  assertEqual(
    translateDynamic('ko', 'notice.memoryOnly', { reason: 'ENOSPC' }),
    'Gilde가 상태를 저장할 수 없습니다: ENOSPC.',
    'a string parameter renders',
  );
});
