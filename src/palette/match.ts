/**
 * The palette's Korean-safe fuzzy matcher (ux-ia §7.4, R1 §5).
 *
 * Subsequence match with bonuses for word starts and adjacent hits, as the
 * shipped matcher did — and Hangul syllables are compared as their jamo, so:
 *  - a syllable still being composed matches ("작ㅇ" finds "작업장");
 *  - initial consonants alone match ("ㅈㅇㅈ" finds "작업장");
 *  - Latin is case-insensitive; NFC/NFD forms compare equal.
 * Positions are reported per character of the candidate, for highlighting.
 */

export interface FuzzyMatch {
  /** Higher is better; comparable only between candidates for one query. */
  score: number;
  /** Indices of the candidate's characters (code points) the query used. */
  positions: number[];
}

const SEPARATORS = new Set([' ', '-', '_', '/', '.', ':', '·', '›', '@', '#']);

const S_BASE = 0xac00;
const L_COUNT = 19;
const V_COUNT = 21;
const T_COUNT = 28;
const N_COUNT = V_COUNT * T_COUNT;
const S_COUNT = L_COUNT * N_COUNT;

// Compatibility jamo (what a keyboard produces mid-composition) for each conjoining index.
const LEADS = 'ㄱㄲㄴㄷㄸㄹㅁㅂㅃㅅㅆㅇㅈㅉㅊㅋㅌㅍㅎ';
const VOWELS = 'ㅏㅐㅑㅒㅓㅔㅕㅖㅗㅘㅙㅚㅛㅜㅝㅞㅟㅠㅡㅢㅣ';
const TAILS = ['', 'ㄱ', 'ㄲ', 'ㄳ', 'ㄴ', 'ㄵ', 'ㄶ', 'ㄷ', 'ㄹ', 'ㄺ', 'ㄻ', 'ㄼ', 'ㄽ', 'ㄾ', 'ㄿ', 'ㅀ', 'ㅁ', 'ㅂ', 'ㅄ', 'ㅅ', 'ㅆ', 'ㅇ', 'ㅈ', 'ㅊ', 'ㅋ', 'ㅌ', 'ㅍ', 'ㅎ'];
// Compound tails split into their parts, so "닭" (ㄷㅏㄺ) matches "달ㄱ".
const SPLIT_TAIL: Readonly<Record<string, string>> = {
  ㄳ: 'ㄱㅅ', ㄵ: 'ㄴㅈ', ㄶ: 'ㄴㅎ', ㄺ: 'ㄹㄱ', ㄻ: 'ㄹㅁ', ㄼ: 'ㄹㅂ', ㄽ: 'ㄹㅅ', ㄾ: 'ㄹㅌ', ㄿ: 'ㄹㅍ', ㅀ: 'ㄹㅎ', ㅄ: 'ㅂㅅ',
};

/** One character as the units the matcher compares (jamo for Hangul syllables). */
export function units(ch: string): string[] {
  const code = ch.codePointAt(0) ?? 0;
  if (code >= S_BASE && code < S_BASE + S_COUNT) {
    const i = code - S_BASE;
    const lead = LEADS[Math.floor(i / N_COUNT)] as string;
    const vowel = VOWELS[Math.floor((i % N_COUNT) / T_COUNT)] as string;
    const tail = TAILS[i % T_COUNT] as string;
    return [lead, vowel, ...[...(SPLIT_TAIL[tail] ?? tail)]];
  }
  const split = SPLIT_TAIL[ch];
  if (split) return [...split];
  return [ch.toLowerCase()];
}

interface Unit {
  u: string;
  /** Character index in the candidate. */
  at: number;
  /** First unit of its character. */
  first: boolean;
}

function explode(text: string): { list: Unit[]; chars: string[] } {
  const chars = [...text.normalize('NFC')];
  const list: Unit[] = [];
  chars.forEach((ch, at) => {
    units(ch).forEach((u, k) => list.push({ u, at, first: k === 0 }));
  });
  return { list, chars };
}

/** Match `query` against `candidate`; null when it is not a subsequence. */
export function fuzzyMatch(query: string, candidate: string): FuzzyMatch | null {
  const needles = explode(query.trim()).list.filter((n) => n.u !== ' ');
  if (needles.length === 0) return { score: 0, positions: [] };
  const { list: hay, chars } = explode(candidate);
  if (needles.length > hay.length) return null;

  const positions: number[] = [];
  let score = 0;
  let cursor = 0;
  let previousChar = -2;
  let previousUnit = -2;
  for (const needle of needles) {
    let found = -1;
    for (let i = cursor; i < hay.length; i++) {
      if ((hay[i] as Unit).u === needle.u) {
        found = i;
        break;
      }
    }
    if (found < 0) return null;
    const unit = hay[found] as Unit;
    if (found === previousUnit + 1) score += 10;
    if (unit.first) {
      if (unit.at === 0) score += 12;
      else if (SEPARATORS.has(chars[unit.at - 1] ?? '')) score += 8;
      else if (unit.at !== previousChar) score += 2;
    }
    score -= Math.min(Math.max(0, unit.at - previousChar - 1), 6);
    if (unit.at !== previousChar) positions.push(unit.at);
    previousChar = unit.at;
    previousUnit = found;
    cursor = found + 1;
  }
  score -= Math.min(chars.length / 8, 8);
  return { score, positions };
}

/** The best match across several haystacks (Korean and English names, keywords). */
export function fuzzyBest(query: string, candidates: readonly string[]): FuzzyMatch | null {
  let best: FuzzyMatch | null = null;
  for (const candidate of candidates) {
    const m = fuzzyMatch(query, candidate);
    if (m && (!best || m.score > best.score)) best = m;
  }
  return best;
}
