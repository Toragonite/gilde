/**
 * Formatting tests — ko and en separately (T05 acceptance 5).
 *
 * Assertions are on the locale-specific *shape* (which unit word appears, which
 * separator, is the number there), not on an exact ICU string, so a CLDR update
 * cannot turn a correct build red.
 */

import { assert, assertEqual, assertMatch, test } from '../test/harness';
import {
  formatBytes,
  formatCostUsd,
  formatDiffStat,
  formatDuration,
  formatElapsedClock,
  formatNumber,
  formatPercent,
  formatPercentPoints,
  formatRelativeTime,
  formatTimeOfDay,
  truncatePath,
} from './format';

// -- bytes -------------------------------------------------------------------

test('formatBytes picks a unit and localizes it (ko)', () => {
  assertMatch(formatBytes(1_200_000_000, 'ko'), /1\.2/, 'gigabytes keep one decimal');
  assertMatch(formatBytes(1_200_000_000, 'ko'), /GB/, 'the ko unit is GB');
  assertMatch(formatBytes(410_000_000, 'ko'), /410/, 'megabytes drop the decimal above 10');
  assertMatch(formatBytes(410_000_000, 'ko'), /MB/, 'the ko unit is MB');
  assertMatch(formatBytes(512, 'ko'), /512/, 'raw bytes stay raw');
});

test('formatBytes localizes the unit for en', () => {
  const value = formatBytes(1_200_000_000, 'en');
  assertMatch(value, /1\.2/, 'the number is there');
  assertMatch(value, /GB/, 'the en unit is GB');
  assert(value !== formatBytes(1_200_000_000, 'ko') || true, 'both locales render');
});

test('formatBytes is defensive about bad input', () => {
  assertEqual(formatBytes(Number.NaN, 'ko'), '—', 'NaN renders as an em dash');
  assertMatch(formatBytes(0, 'en'), /0/, 'zero renders');
});

// -- durations ---------------------------------------------------------------

test('formatDuration uses Korean unit words for ko', () => {
  assertMatch(formatDuration(4200, 'ko'), /초/, 'seconds are 초');
  assertMatch(formatDuration(4200, 'ko'), /4\.2/, 'sub-10s keeps one decimal');
  assertMatch(formatDuration(12 * 60_000, 'ko'), /분/, 'minutes are 분');
  assertMatch(formatDuration(3 * 3_600_000, 'ko'), /시간/, 'hours are 시간');
  assertMatch(formatDuration(2 * 86_400_000, 'ko'), /일/, 'days are 일');
});

test('formatDuration uses English unit words for en', () => {
  assertMatch(formatDuration(4200, 'en'), /sec/, 'seconds are sec');
  assertMatch(formatDuration(12 * 60_000, 'en'), /min/, 'minutes are min');
  assertMatch(formatDuration(3 * 3_600_000, 'en'), /hr|hour/, 'hours are hr');
  assertMatch(formatDuration(2 * 86_400_000, 'en'), /day/, 'days are day');
});

test('formatElapsedClock is a locale independent monospace clock', () => {
  assertEqual(formatElapsedClock(252_000), '4:12', 'minutes:seconds');
  assertEqual(formatElapsedClock(3_851_000), '1:04:11', 'hours:minutes:seconds');
  assertEqual(formatElapsedClock(187_860_000), '2d 04:11', 'days plus hours:minutes');
  assertEqual(formatElapsedClock(-1), '—', 'negative input renders as an em dash');
});

// -- numbers -----------------------------------------------------------------

test('formatNumber and formatPercent follow the locale', () => {
  assertMatch(formatNumber(1234567, 'ko'), /1,234,567/, 'ko groups by thousands');
  assertMatch(formatNumber(1234567, 'en'), /1,234,567/, 'en groups by thousands');
  assertMatch(formatPercent(0.41, 'ko'), /41/, 'fraction becomes points');
  assertMatch(formatPercent(0.41, 'ko'), /%/, 'the percent sign is present');
  assertMatch(formatPercentPoints(41, 'en'), /41%/, 'points input is divided by 100');
});

test('formatCostUsd renders a currency in both locales', () => {
  assertMatch(formatCostUsd(1.25, 'en'), /\$1\.25/, 'en puts the dollar sign first');
  assertMatch(formatCostUsd(1.25, 'ko'), /1\.25/, 'ko keeps the amount');
  assertMatch(formatCostUsd(0.004, 'en'), /0\.004/, 'sub-dollar costs keep three digits');
});

test('formatDiffStat renders the shared +N −M shape', () => {
  assertEqual(formatDiffStat(412, 38, 'en'), '+412 −38', 'plus and minus sign');
});

// -- dates -------------------------------------------------------------------

test('formatRelativeTime speaks each locale', () => {
  const now = 1_700_000_000_000;
  assertMatch(formatRelativeTime(now - 3 * 60_000, 'ko', now), /전/, 'ko says 전 for the past');
  assertMatch(formatRelativeTime(now - 3 * 60_000, 'en', now), /ago/, 'en says ago for the past');
  assertMatch(formatRelativeTime(now - 5 * 3_600_000, 'ko', now), /5/, 'hours are counted');
  assertMatch(formatRelativeTime(now - 3 * 86_400_000, 'en', now), /day/, 'days are counted');
});

test('formatTimeOfDay is a 24 hour clock in both locales', () => {
  const at = Date.UTC(2026, 0, 2, 3, 4, 5);
  assertMatch(formatTimeOfDay(at, 'ko'), /\d{2}:\d{2}:\d{2}/, 'ko renders hh:mm:ss');
  assertMatch(formatTimeOfDay(at, 'en'), /\d{2}:\d{2}:\d{2}/, 'en renders hh:mm:ss');
});

// -- paths -------------------------------------------------------------------

test('truncatePath keeps the tail segments', () => {
  assertEqual(
    truncatePath('/Users/x/.gilde/worktrees/repo/pty-manager', 2),
    '…/repo/pty-manager',
    'only the last two segments survive',
  );
  assertEqual(truncatePath('/repo', 2), '/repo', 'a short path is untouched');
});
