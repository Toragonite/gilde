/**
 * The palette matcher is Korean-safe (ux-ia §7.4): a syllable still being
 * composed matches, initial consonants alone match, Latin ignores case, and
 * the best candidate ranks first.
 */

import { assert, assertEqual, test } from '../test/harness';
import { fuzzyBest, fuzzyMatch, units } from './match';
import { parseQuery, rankEntries } from './entries';
import type { PaletteEntry } from './entries';

test('Hangul syllables compare as jamo', () => {
  assertEqual(units('작').join(''), 'ㅈㅏㄱ', 'lead · vowel · tail');
  assertEqual(units('닭').join(''), 'ㄷㅏㄹㄱ', 'compound tails split');
  assertEqual(units('A').join(''), 'a', 'Latin lower-cased');
});

test('a syllable still being composed matches (IME mid-composition)', () => {
  assert(fuzzyMatch('작ㅇ', '작업장') !== null, '"작ㅇ" finds "작업장"');
  assert(fuzzyMatch('작어', '작업장') !== null, 'a half-built second syllable');
  assert(fuzzyMatch('작업장', '작업장') !== null, 'the whole word');
});

test('initial consonants alone match', () => {
  const m = fuzzyMatch('ㅈㅇㅈ', '작업장');
  assert(m !== null, 'choseong query');
  assertEqual(m?.positions.join(','), '0,1,2', 'one position per syllable');
});

test('Latin is case-insensitive and NFD equals NFC', () => {
  assert(fuzzyMatch('FLOOR', 'Floor') !== null, 'case');
  assert(fuzzyMatch('작업', '작업장'.normalize('NFD')) !== null, 'decomposed candidate');
  assert(fuzzyMatch('xyz', 'Floor') === null, 'no match');
});

test('a word start and adjacency rank higher', () => {
  const tight = fuzzyMatch('floor', 'Floor');
  const loose = fuzzyMatch('floor', 'Follow a team or open the room');
  assert(tight !== null && loose !== null, 'both match');
  assert((tight?.score ?? 0) > (loose?.score ?? 0), 'the exact word wins');
  assertEqual(fuzzyBest('작업장', ['Floor', '작업장'])?.positions.length, 3, 'best of both names');
});

function entry(id: string, group: PaletteEntry['group'], label: string, rank: number, haystack: string[] = []): PaletteEntry {
  return { id, group, label, detail: '', haystack, action: { kind: 'run', id }, disabledReason: null, rank };
}

test('prefixes narrow: > commands, # projects, @ accounts', () => {
  assertEqual(parseQuery('>floor').group, 'commands', '>');
  assertEqual(parseQuery('# web').text, 'web', '# text');
  assertEqual(parseQuery('@work').group, 'accounts', '@');
  assertEqual(parseQuery('plain').group, null, 'no prefix');
  const entries = [entry('p', 'projects', 'checkout-service', 2), entry('c', 'commands', 'Floor', 5, ['작업장', 'Floor']), entry('s', 'sessions', 'floor tweaks', 0)];
  assertEqual(rankEntries(entries, '>fl').map((r) => r.entry.id).join(','), 'c', 'commands only');
  assertEqual(rankEntries(entries, '작업').map((r) => r.entry.id).join(','), 'c', 'the Korean name matches an English UI entry');
  assertEqual(rankEntries(entries, '')[0]?.entry.id, 's', 'empty query: live sessions first');
});
