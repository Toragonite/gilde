/**
 * Korean particles after placeholders (ux-ia §9.1, contract §13.3 "KO plurals
 * and particles").
 *
 * A Korean string writes the particle in its two-form spelling right after a
 * placeholder — `{agent}(이)가`, `{owner}(으)로`, `{account}(을)를` — and the
 * interpolator replaces it with the form the substituted word takes:
 *
 *   {agent}(이)가  with agent = "claude"   → "claude가"
 *   {owner}(으)로  with owner = "터미널"    → "터미널로"
 *   {owner}(으)로  with owner = "VS Code"  → "VS Code로"
 *   {n}(이)가      with n = 3              → "3이"
 *
 * The choice depends on whether the word's last sound has a final consonant
 * (받침), and for (으)로 whether that consonant is ㄹ. Hangul, digits and
 * acronyms are decided exactly; a Latin word only when its ending is clear.
 * When the ending is not clear the two-form spelling is kept as written, which
 * every Korean reader understands, rather than guessing wrong.
 */

/** The final consonant of a word's last sound. */
export type Batchim = 'none' | 'rieul' | 'other' | 'unknown';

/** Two-form particle spellings → [form after a final consonant, form after a vowel]. */
const PATTERNS: Record<string, readonly [string, string]> = {
  '(이)가': ['이', '가'],
  '(을)를': ['을', '를'],
  '(은)는': ['은', '는'],
  '(과)와': ['과', '와'],
  '(와)과': ['과', '와'],
  '(이)나': ['이나', '나'],
  '(이)라': ['이라', '라'],
  '(이)랑': ['이랑', '랑'],
  '(아)야': ['아', '야'],
  '(으)로': ['으로', '로'],
  '(으)로서': ['으로서', '로서'],
  '(으)로써': ['으로써', '로써'],
};

/** Every supported spelling, longest first (so `(으)로서` wins over `(으)로`). */
export const JOSA_SPELLINGS: readonly string[] = Object.keys(PATTERNS).sort((a, b) => b.length - a.length);

const HANGUL_FIRST = 0xac00;
const HANGUL_LAST = 0xd7a3;
const RIEUL = 8;

/** Final sound of the digit names 0–9 (영 일 이 삼 사 오 육 칠 팔 구). */
const DIGIT_BATCHIM: readonly Batchim[] = [
  'other', 'rieul', 'none', 'other', 'none', 'none', 'other', 'rieul', 'rieul', 'none',
];

/** Final sound of the English letter names, for acronyms (엘, 알 → ㄹ; 엠, 엔 → other). */
function letterNameBatchim(letter: string): Batchim {
  if (letter === 'L' || letter === 'R') return 'rieul';
  if (letter === 'M' || letter === 'N') return 'other';
  return 'none';
}

/** Final sound of a lowercase Latin word, only where the reading is clear. */
function latinWordBatchim(word: string): Batchim {
  const w = word.toLowerCase();
  if (w.endsWith('ng') || w.endsWith('m') || w.endsWith('n')) return 'other';
  if (w.endsWith('l')) return 'rieul';
  // Stop consonants are read either way in loanwords (맥 / 테스트): not clear.
  if (/[bcdgkpqtj]$/.test(w)) return 'unknown';
  // Vowels and the endings Korean writes with a vowel: -e (claude 클로드),
  // -r (server 서버), -s (agents 에이전츠), -x (codex 코덱스), -y, -f, -h, -v, -w, -z.
  return 'none';
}

/** Numbers: the last spoken unit decides (10 십, 100 백, 1000 천, 10000 만). */
function numberBatchim(digits: string): Batchim {
  const last = digits.charCodeAt(digits.length - 1) - 48;
  if (last !== 0 || /^0+$/.test(digits)) return DIGIT_BATCHIM[last] ?? 'unknown';
  const zeros = digits.length - digits.replace(/0+$/, '').length;
  if (zeros === 1) return 'other'; // 십
  if (zeros === 2) return 'other'; // 백
  return 'other'; // 천, 만 and above
}

/** The final consonant of `word`'s last sound, ignoring closing punctuation. */
export function batchimOf(word: string): Batchim {
  const trimmed = word.replace(/[\s)\]}"'’”」』>.,!?…·:;-]+$/u, '');
  if (trimmed === '') return 'unknown';
  const last = trimmed.codePointAt(trimmed.length - 1) ?? 0;
  if (last >= HANGUL_FIRST && last <= HANGUL_LAST) {
    const jong = (last - HANGUL_FIRST) % 28;
    if (jong === 0) return 'none';
    return jong === RIEUL ? 'rieul' : 'other';
  }
  const digits = /[0-9]+$/.exec(trimmed);
  if (digits) return numberBatchim(digits[0]);
  const acronym = /(?:^|[^A-Za-z])([A-Z]{2,})$/.exec(trimmed) ?? /^([A-Z])$/.exec(trimmed);
  if (acronym) {
    const letters = acronym[1] ?? '';
    return letterNameBatchim(letters[letters.length - 1] ?? '');
  }
  const latin = /[A-Za-z]+$/.exec(trimmed);
  if (latin) return latinWordBatchim(latin[0]);
  return 'unknown';
}

/**
 * The particle for `word` in the given two-form spelling, or the spelling
 * itself when the word's ending is not clear (or the spelling is not known).
 */
export function josa(word: string, spelling: string): string {
  const forms = PATTERNS[spelling];
  if (!forms) return spelling;
  const b = batchimOf(word);
  if (b === 'unknown') return spelling;
  const isRo = spelling.startsWith('(으)로');
  if (b === 'none' || (isRo && b === 'rieul')) return forms[1];
  return forms[0];
}

/** `word` followed by its particle: `withJosa("claude", "(이)가")` → `"claude가"`. */
export function withJosa(word: string, spelling: string): string {
  return word + josa(word, spelling);
}

/** If `text` starts with a supported spelling, that spelling; else null. */
export function josaAt(text: string, index: number): string | null {
  if (text.charCodeAt(index) !== 0x28 /* ( */) return null;
  for (const spelling of JOSA_SPELLINGS) {
    if (text.startsWith(spelling, index)) return spelling;
  }
  return null;
}
