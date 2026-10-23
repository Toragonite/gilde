/**
 * The keyboard layer (ux-ia §7, contract §8.6).
 *
 * One capture-phase `keydown` listener on `window` serves every binding, so a
 * ⌘ chord is seen before a terminal swallows it while every other key reaches
 * the terminal untouched. Rules, each one tested:
 *
 *  - Bindings match `event.code` (the physical key), not `event.key`, so they
 *    work with ⌥ held and on the Korean layout.
 *  - `mod` is ⌘ only. Ctrl is never claimed: agent TUIs keep every Ctrl key.
 *  - During IME composition (`isComposing`, keyCode 229) nothing fires: `⏎`
 *    finishes a Korean syllable and never sends, digits never answer (T-ui-ime).
 *  - `event.repeat` never fires a binding unless it opts in (navigation only;
 *    answering never does).
 *  - In a text entry only ⌘ chords, Escape and function keys fire; plain
 *    letters and digits type (ux-ia §7.1).
 *
 * Scopes (ux-ia §7.2): G global, S session, T split/team, L lists, C a focused
 * card, N the New-session sheet, H history, X any sheet, F the Floor, plus A
 * for the narrow Accounts & health screen. A registration of L, C or F (or any
 * registration with `requireFocus`) fires only while focus is inside its
 * element: digits answer a card only while that card has focus [M7]. Other
 * registrations are live while mounted. While a sheet is open (`useHotkeyScope('X', …)`,
 * pushed by `Sheet`) only bindings registered inside the top sheet fire — the
 * sheet traps focus and keys alike.
 */

import { useEffect, useRef } from 'react';
import type { RefObject } from 'react';

// ---------------------------------------------------------------------------
// Combos
// ---------------------------------------------------------------------------

export type HotkeyScope = 'G' | 'S' | 'T' | 'L' | 'C' | 'N' | 'H' | 'X' | 'F' | 'A';

export interface ParsedCombo {
  mod: boolean;
  alt: boolean;
  shift: boolean;
  /** A `KeyboardEvent.code`, e.g. `KeyJ`, `Digit1`, `Enter`, `F6`. */
  code: string;
  /** True when the combo names Ctrl, which this app never claims (always unmatched). */
  ctrl: boolean;
}

const CODE_ALIASES: Readonly<Record<string, string>> = {
  backslash: 'Backslash',
  slash: 'Slash',
  comma: 'Comma',
  period: 'Period',
  semicolon: 'Semicolon',
  quote: 'Quote',
  backquote: 'Backquote',
  bracketleft: 'BracketLeft',
  bracketright: 'BracketRight',
  equal: 'Equal',
  minus: 'Minus',
  escape: 'Escape',
  esc: 'Escape',
  enter: 'Enter',
  return: 'Enter',
  space: 'Space',
  tab: 'Tab',
  backspace: 'Backspace',
  delete: 'Delete',
  home: 'Home',
  end: 'End',
  pageup: 'PageUp',
  pagedown: 'PageDown',
  arrowleft: 'ArrowLeft',
  arrowright: 'ArrowRight',
  arrowup: 'ArrowUp',
  arrowdown: 'ArrowDown',
  left: 'ArrowLeft',
  right: 'ArrowRight',
  up: 'ArrowUp',
  down: 'ArrowDown',
};

/** A display key name (`j`, `1`, `enter`, `f6`) → its `KeyboardEvent.code`. */
export function codeFor(key: string): string {
  const lower = key.toLowerCase();
  const alias = CODE_ALIASES[lower];
  if (alias) return alias;
  if (/^[a-z]$/.test(lower)) return `Key${lower.toUpperCase()}`;
  if (/^[0-9]$/.test(lower)) return `Digit${lower}`;
  if (/^f([1-9]|1[0-9])$/.test(lower)) return lower.toUpperCase();
  return key;
}

/** `mod+shift+j` → modifiers and a code. Unknown words are kept as a code. */
export function parseCombo(combo: string): ParsedCombo {
  const parts = combo.split('+').map((p) => p.trim().toLowerCase()).filter((p) => p !== '');
  const parsed: ParsedCombo = { mod: false, alt: false, shift: false, code: '', ctrl: false };
  for (const part of parts) {
    if (part === 'mod' || part === 'cmd' || part === 'meta') parsed.mod = true;
    else if (part === 'alt' || part === 'opt' || part === 'option') parsed.alt = true;
    else if (part === 'shift') parsed.shift = true;
    else if (part === 'ctrl' || part === 'control') parsed.ctrl = true;
    else parsed.code = codeFor(part);
  }
  return parsed;
}

/** Exact match on code and modifiers. Any event with Ctrl held never matches. */
export function matchesCombo(event: Pick<KeyboardEvent, 'code' | 'metaKey' | 'ctrlKey' | 'altKey' | 'shiftKey'>, combo: string | ParsedCombo): boolean {
  const want = typeof combo === 'string' ? parseCombo(combo) : combo;
  if (want.ctrl || want.code === '') return false;
  if (event.ctrlKey) return false;
  if (event.code !== want.code) return false;
  if (event.metaKey !== want.mod) return false;
  if (event.altKey !== want.alt) return false;
  if (event.shiftKey !== want.shift) return false;
  return true;
}

const KEY_SYMBOLS: Readonly<Record<string, string>> = {
  Enter: '⏎',
  Backspace: '⌫',
  Delete: '⌦',
  Escape: 'Esc',
  Tab: '⇥',
  Space: 'Space',
  ArrowUp: '↑',
  ArrowDown: '↓',
  ArrowLeft: '←',
  ArrowRight: '→',
  Period: '.',
  Comma: ',',
  Slash: '/',
  Backslash: '\\',
  BracketLeft: '[',
  BracketRight: ']',
  Equal: '+',
  Minus: '−',
  Semicolon: ';',
  Quote: "'",
  Backquote: '`',
  Home: 'Home',
  End: 'End',
  PageUp: 'PgUp',
  PageDown: 'PgDn',
};

function keyLabel(code: string): string {
  if (KEY_SYMBOLS[code]) return KEY_SYMBOLS[code] as string;
  if (code.startsWith('Key')) return code.slice(3);
  if (code.startsWith('Digit')) return code.slice(5);
  return code;
}

/** The macOS spelling of a combo for menus, hints and the help sheet: `⇧⌘J`, `⌥⌘1`, `⌘⏎`. */
export function formatCombo(combo: string): string {
  const p = parseCombo(combo);
  return `${p.alt ? '⌥' : ''}${p.shift ? '⇧' : ''}${p.mod ? '⌘' : ''}${keyLabel(p.code)}`;
}

const ARIA_KEYS: Readonly<Record<string, string>> = {
  Space: 'Space',
  Period: '.',
  Comma: ',',
  Slash: '/',
  Backslash: '\\',
  BracketLeft: '[',
  BracketRight: ']',
  Equal: '=',
  Minus: '-',
};

/** The `aria-keyshortcuts` spelling: `Meta+Shift+J`, `Meta+Enter`, `1`. */
export function ariaKeyshortcuts(combo: string): string {
  const p = parseCombo(combo);
  let key = ARIA_KEYS[p.code] ?? p.code;
  if (p.code.startsWith('Key')) key = p.code.slice(3);
  else if (p.code.startsWith('Digit')) key = p.code.slice(5);
  return [p.alt ? 'Alt' : '', p.shift ? 'Shift' : '', p.mod ? 'Meta' : '', key].filter(Boolean).join('+');
}

// ---------------------------------------------------------------------------
// Event guards
// ---------------------------------------------------------------------------

/** True while an input method is composing (Korean syllables): nothing may fire. */
export function isImeComposing(event: Pick<KeyboardEvent, 'isComposing' | 'keyCode'>): boolean {
  return event.isComposing === true || event.keyCode === 229;
}

/**
 * Whether the key goes into a text entry (input, textarea, select,
 * contenteditable). A terminal host marks its container
 * `data-gilde-hotkeys="allow"`: xterm's helper textarea is not treated as one.
 */
export function isTextEntryTarget(target: EventTarget | null): boolean {
  if (typeof HTMLElement === 'undefined' || !(target instanceof HTMLElement)) return false;
  if (target.closest('[data-gilde-hotkeys="allow"]')) return false;
  if (target.isContentEditable || target.getAttribute('contenteditable') === 'true') return true;
  const tag = target.tagName;
  if (tag === 'TEXTAREA' || tag === 'SELECT') return true;
  if (tag === 'INPUT') {
    const type = (target as HTMLInputElement).type;
    return !['button', 'checkbox', 'radio', 'range', 'submit', 'reset', 'color', 'file'].includes(type);
  }
  return false;
}

// ---------------------------------------------------------------------------
// Bindings and the registry
// ---------------------------------------------------------------------------

export interface HotkeyBinding {
  id: string;
  /** One combo or several (`['mod+equal', 'mod+shift+equal']`). */
  combo: string | readonly string[];
  run: (event: KeyboardEvent) => void;
  /** Fire on auto-repeat too (navigation only; answering never). */
  allowRepeat?: boolean;
  /**
   * Fire inside a text entry. Default: ⌘ chords, Escape and function keys do,
   * everything else types.
   */
  allowInTextEntry?: boolean;
  /** Extra condition; a false predicate leaves the key to whatever has focus. */
  when?: (event: KeyboardEvent) => boolean;
}

export interface HotkeyOptions {
  /** Default `G`. */
  scope?: HotkeyScope;
  /** The element the bindings belong to (a list, a card, a sheet, the Floor). */
  element?: RefObject<HTMLElement | null>;
  /** Fire only while focus is inside `element`. Default true for L, C and F. */
  requireFocus?: boolean;
  /** Default true. */
  enabled?: boolean;
}

interface Registration {
  seq: number;
  scope: HotkeyScope;
  bindings: () => readonly HotkeyBinding[];
  element: () => HTMLElement | null;
  requireFocus: boolean;
}

interface ScopeEntry {
  seq: number;
  scope: HotkeyScope;
  element: () => HTMLElement | null;
}

const registrations = new Set<Registration>();
const scopeStack: ScopeEntry[] = [];
let seqCounter = 0;
let listening = false;

const FOCUS_SCOPES: ReadonlySet<HotkeyScope> = new Set<HotkeyScope>(['L', 'C', 'F']);

/** Higher runs first among registrations that can take the key. */
const SCOPE_RANK: Readonly<Record<HotkeyScope, number>> = {
  X: 6,
  N: 5,
  C: 4,
  L: 3,
  F: 3,
  S: 2,
  T: 2,
  H: 2,
  A: 2,
  G: 0,
};

/** Whether a scope is pushed right now (e.g. a binding of G that must stay off in T). */
export function isScopeActive(scope: HotkeyScope): boolean {
  return scopeStack.some((e) => e.scope === scope);
}

/** The element of the top open sheet, or null when no sheet is open. */
function topSheet(): ScopeEntry | null {
  for (let i = scopeStack.length - 1; i >= 0; i--) {
    const entry = scopeStack[i] as ScopeEntry;
    if (entry.scope === 'X') return entry;
  }
  return null;
}

function depth(el: Node): number {
  let d = 0;
  for (let n: Node | null = el; n; n = n.parentNode) d++;
  return d;
}

function contains(container: HTMLElement | null, node: EventTarget | null): boolean {
  return container !== null && node instanceof Node && container.contains(node);
}

function liveRegistrations(target: EventTarget | null): Registration[] {
  const sheet = topSheet();
  const sheetEl = sheet ? sheet.element() : null;
  const live: { reg: Registration; rank: number; depth: number }[] = [];
  for (const reg of registrations) {
    const el = reg.element();
    if (reg.requireFocus && !contains(el, target)) continue;
    if (sheet) {
      // A sheet traps keys: only what lives inside the top sheet may fire.
      const inside = el !== null && sheetEl !== null && (sheetEl === el || sheetEl.contains(el));
      if (!inside && !(reg.scope === 'X' && el === null)) continue;
    }
    live.push({ reg, rank: SCOPE_RANK[reg.scope], depth: el && contains(el, target) ? depth(el) : 0 });
  }
  // Focus-owning elements first (deepest wins), then scope rank, then the newest.
  live.sort((a, b) => b.depth - a.depth || b.rank - a.rank || b.reg.seq - a.reg.seq);
  return live.map((x) => x.reg);
}

function defaultAllowedInText(p: ParsedCombo): boolean {
  return p.mod || p.code === 'Escape' || /^F\d+$/.test(p.code);
}

/** The one keydown handler (exported for tests that drive it directly). */
export function handleHotkey(event: KeyboardEvent): boolean {
  if (isImeComposing(event)) return false;
  if (event.ctrlKey) return false;
  const inText = isTextEntryTarget(event.target);
  for (const reg of liveRegistrations(event.target)) {
    for (const binding of reg.bindings()) {
      const combos = typeof binding.combo === 'string' ? [binding.combo] : binding.combo;
      for (const combo of combos) {
        const parsed = parseCombo(combo);
        if (!matchesCombo(event, parsed)) continue;
        if (event.repeat && !binding.allowRepeat) continue;
        if (inText && !(binding.allowInTextEntry ?? defaultAllowedInText(parsed))) continue;
        if (binding.when && !binding.when(event)) continue;
        event.preventDefault();
        event.stopPropagation();
        binding.run(event);
        return true;
      }
    }
  }
  return false;
}

function onWindowKeyDown(event: KeyboardEvent): void {
  handleHotkey(event);
}

function ensureListening(): void {
  if (listening || typeof window === 'undefined') return;
  window.addEventListener('keydown', onWindowKeyDown, { capture: true });
  listening = true;
}

function stopListeningIfIdle(): void {
  if (!listening || registrations.size > 0 || scopeStack.length > 0) return;
  window.removeEventListener('keydown', onWindowKeyDown, { capture: true });
  listening = false;
}

/** Register bindings outside React (tests, imperative islands). Returns the unregister function. */
export function registerHotkeys(bindings: () => readonly HotkeyBinding[], options: HotkeyOptions = {}): () => void {
  const scope = options.scope ?? 'G';
  const elementRef = options.element;
  const reg: Registration = {
    seq: ++seqCounter,
    scope,
    bindings,
    element: () => elementRef?.current ?? null,
    requireFocus: options.requireFocus ?? FOCUS_SCOPES.has(scope),
  };
  registrations.add(reg);
  ensureListening();
  return () => {
    registrations.delete(reg);
    stopListeningIfIdle();
  };
}

/**
 * Bind keys for as long as the component is mounted. The latest `bindings`
 * array is read at key time, so it may be rebuilt on every render.
 */
export function useHotkeys(bindings: readonly HotkeyBinding[], options: HotkeyOptions = {}): void {
  const ref = useRef(bindings);
  ref.current = bindings;
  const { scope = 'G', element, requireFocus, enabled = true } = options;
  useEffect(() => {
    if (!enabled) return;
    return registerHotkeys(() => ref.current, { scope, element, requireFocus });
  }, [enabled, scope, element, requireFocus]);
}

/** Push a scope while `active` (a sheet pushes `X` with its element). Returns the pop function. */
export function pushHotkeyScope(scope: HotkeyScope, element?: RefObject<HTMLElement | null>): () => void {
  const entry: ScopeEntry = { seq: ++seqCounter, scope, element: () => element?.current ?? null };
  scopeStack.push(entry);
  ensureListening();
  return () => {
    const i = scopeStack.indexOf(entry);
    if (i >= 0) scopeStack.splice(i, 1);
    stopListeningIfIdle();
  };
}

export function useHotkeyScope(scope: HotkeyScope, active: boolean, element?: RefObject<HTMLElement | null>): void {
  useEffect(() => {
    if (!active) return;
    return pushHotkeyScope(scope, element);
  }, [scope, active, element]);
}

/** Tests only: drop every registration and scope. */
export function resetHotkeysForTests(): void {
  registrations.clear();
  scopeStack.length = 0;
  stopListeningIfIdle();
}

// ---------------------------------------------------------------------------
// The keymap (ux-ia §7.2) — the data the generated help sheet (⌘/) renders
// ---------------------------------------------------------------------------

export interface KeymapEntry {
  /** Stable id; the help sheet maps it to its label. */
  id: string;
  combos: readonly string[];
  scopes: readonly HotkeyScope[];
  /**
   * Where inside the scope the binding lives when it is narrower than the
   * scope ("composer", "question", "queueRows", …). Two entries collide only
   * when they share a scope and a context.
   */
  context?: string;
  /** Scopes in which this binding is deliberately off (G ⌥⌘↑/↓ is off in T). */
  notIn?: readonly HotkeyScope[];
}

const digits = (from: number, to: number, prefix = ''): string[] => {
  const out: string[] = [];
  for (let n = from; n <= to; n++) out.push(`${prefix}${n}`);
  return out;
};

export const KEYMAP: readonly KeymapEntry[] = [
  // G — global
  { id: 'palette', combos: ['mod+k'], scopes: ['G'] },
  { id: 'needsYou.next', combos: ['mod+j'], scopes: ['G'] },
  { id: 'needsYou.previous', combos: ['mod+shift+j'], scopes: ['G'] },
  { id: 'nav.home', combos: ['mod+0'], scopes: ['G'] },
  { id: 'nav.history', combos: ['mod+shift+f'], scopes: ['G'] },
  { id: 'nav.accountsHealth', combos: ['mod+shift+a'], scopes: ['G'] },
  { id: 'newSession', combos: ['mod+n'], scopes: ['G'] },
  { id: 'sidebar.toggle', combos: ['mod+b'], scopes: ['G'] },
  { id: 'nav.back', combos: ['mod+bracketleft'], scopes: ['G'] },
  { id: 'nav.forward', combos: ['mod+bracketright'], scopes: ['G'] },
  { id: 'session.previous', combos: ['mod+alt+arrowup'], scopes: ['G'], notIn: ['T'] },
  { id: 'session.next', combos: ['mod+alt+arrowdown'], scopes: ['G'], notIn: ['T'] },
  { id: 'region.next', combos: ['f6'], scopes: ['G'] },
  { id: 'region.previous', combos: ['shift+f6'], scopes: ['G'] },
  { id: 'close', combos: ['mod+w'], scopes: ['G'] },
  { id: 'stop', combos: ['mod+shift+backspace'], scopes: ['G'] },
  { id: 'settings', combos: ['mod+comma'], scopes: ['G'] },
  { id: 'help', combos: ['mod+slash'], scopes: ['G'] },
  { id: 'zoom.in', combos: ['mod+equal', 'mod+shift+equal'], scopes: ['G'] },
  { id: 'zoom.out', combos: ['mod+minus'], scopes: ['G'] },
  { id: 'zoom.reset', combos: ['mod+alt+0'], scopes: ['G'] },
  { id: 'contextMenu', combos: ['shift+f10'], scopes: ['G'] },
  { id: 'quit', combos: ['mod+q'], scopes: ['G'] },
  // S — Session screen and focused tile
  { id: 'session.view.conversation', combos: ['mod+alt+1'], scopes: ['S'] },
  { id: 'session.view.changes', combos: ['mod+alt+2'], scopes: ['S'] },
  { id: 'session.view.terminal', combos: ['mod+alt+3'], scopes: ['S'] },
  { id: 'session.flipView', combos: ['mod+shift+t'], scopes: ['S', 'T'] },
  { id: 'composer.send', combos: ['enter'], scopes: ['S'], context: 'composer' },
  { id: 'composer.newline', combos: ['shift+enter'], scopes: ['S'], context: 'composer' },
  { id: 'composer.sendNow', combos: ['alt+enter'], scopes: ['S'], context: 'composer' },
  { id: 'turn.stop', combos: ['mod+period'], scopes: ['S'] },
  { id: 'answer.primary', combos: ['mod+enter'], scopes: ['S'] },
  { id: 'answer.deny', combos: ['mod+backspace'], scopes: ['S', 'C', 'L'] },
  { id: 'model.menu', combos: ['mod+shift+m'], scopes: ['S'] },
  { id: 'ide.open', combos: ['mod+o'], scopes: ['S'] },
  { id: 'ide.diff', combos: ['mod+shift+o'], scopes: ['S'] },
  { id: 'split.add', combos: ['mod+d'], scopes: ['S', 'L'] },
  { id: 'find', combos: ['mod+f'], scopes: ['S'] },
  // T — Split and Team
  { id: 'team.view.panes', combos: ['mod+alt+1'], scopes: ['T'] },
  { id: 'team.view.board', combos: ['mod+alt+2'], scopes: ['T'] },
  { id: 'team.view.floor', combos: ['mod+alt+3'], scopes: ['T'] },
  { id: 'tile.focus', combos: digits(1, 6, 'mod+'), scopes: ['T'] },
  { id: 'tile.move', combos: ['mod+alt+arrowleft', 'mod+alt+arrowright', 'mod+alt+arrowup', 'mod+alt+arrowdown'], scopes: ['T'] },
  { id: 'tile.zoom', combos: ['mod+shift+enter'], scopes: ['T'] },
  // A — Accounts & health below 1200 px
  { id: 'accounts.section.accounts', combos: ['mod+alt+1'], scopes: ['A'] },
  { id: 'accounts.section.health', combos: ['mod+alt+2'], scopes: ['A'] },
  // C — a focused card
  { id: 'card.allowOnce', combos: ['1'], scopes: ['C'] },
  { id: 'card.allowAlways', combos: ['2'], scopes: ['C'] },
  { id: 'card.deny', combos: ['3'], scopes: ['C'] },
  { id: 'card.pickOption', combos: digits(1, 9), scopes: ['C'], context: 'question' },
  { id: 'card.denyWhy', combos: ['tab'], scopes: ['C'] },
  { id: 'card.leave', combos: ['escape'], scopes: ['C'] },
  // L — lists
  { id: 'list.move', combos: ['arrowup', 'arrowdown'], scopes: ['L'] },
  { id: 'list.open', combos: ['enter'], scopes: ['L'] },
  { id: 'list.expandCollapse', combos: ['arrowright', 'arrowleft'], scopes: ['L'] },
  { id: 'list.toggle', combos: ['space'], scopes: ['L'] },
  { id: 'list.later', combos: ['l'], scopes: ['L'], context: 'queueRows' },
  { id: 'list.markSeen', combos: ['backspace'], scopes: ['L'], context: 'newReplyRows' },
  { id: 'list.message', combos: ['m'], scopes: ['L'], context: 'boardMemberRows' },
  { id: 'list.moveProject', combos: ['alt+shift+arrowup', 'alt+shift+arrowdown'], scopes: ['L'], context: 'projectHeaders' },
  // H — History
  { id: 'history.continue', combos: ['enter'], scopes: ['H'], context: 'results' },
  { id: 'history.read', combos: ['mod+arrowdown'], scopes: ['H'], context: 'results' },
  // N — New session sheet
  { id: 'newSession.start', combos: ['enter'], scopes: ['N'], context: 'prompt' },
  { id: 'newSession.newline', combos: ['shift+enter'], scopes: ['N'], context: 'prompt' },
  { id: 'newSession.chip', combos: digits(1, 8, 'alt+'), scopes: ['N'] },
  // X — any sheet
  { id: 'sheet.cancel', combos: ['escape', 'mod+period'], scopes: ['X'] },
  { id: 'sheet.confirm', combos: ['mod+enter'], scopes: ['X'], context: 'nonDestructive' },
  // F — the Floor
  { id: 'floor.move', combos: ['arrowup', 'arrowdown', 'arrowleft', 'arrowright'], scopes: ['F'] },
  { id: 'floor.nextCharacter', combos: ['tab'], scopes: ['F'] },
  { id: 'floor.previousCharacter', combos: ['shift+tab'], scopes: ['F'] },
  { id: 'floor.open', combos: ['enter'], scopes: ['F'] },
  { id: 'floor.addToSplit', combos: ['mod+d'], scopes: ['F'] },
  { id: 'floor.nameCard', combos: ['space'], scopes: ['F'] },
  { id: 'floor.nextTicket', combos: ['j'], scopes: ['F'] },
  { id: 'floor.followTeam', combos: ['f'], scopes: ['F'] },
  { id: 'floor.zoomIn', combos: ['equal', 'shift+equal'], scopes: ['F'] },
  { id: 'floor.zoomOut', combos: ['minus'], scopes: ['F'] },
  { id: 'floor.zoomReset', combos: ['0'], scopes: ['F'] },
  { id: 'floor.trafficLines', combos: ['m'], scopes: ['F'] },
  { id: 'floor.accounts', combos: ['a'], scopes: ['F'] },
  { id: 'floor.tidy', combos: ['t'], scopes: ['F'] },
  { id: 'floor.pause', combos: ['period'], scopes: ['F'] },
  { id: 'floor.legend', combos: ['l'], scopes: ['F'] },
  // pixel-office §6.2 additions (A23.4): replay the last 10 minutes at 6×, step it 30 s back / forward
  { id: 'floor.replay', combos: ['r'], scopes: ['F'] },
  { id: 'floor.stepBack', combos: ['bracketleft'], scopes: ['F'] },
  { id: 'floor.stepForward', combos: ['bracketright'], scopes: ['F'] },
  { id: 'floor.stepOut', combos: ['escape'], scopes: ['F'] },
];

/** The keymap entry of an id (for a help line or an `aria-keyshortcuts`). */
export function keymapEntry(id: string): KeymapEntry | undefined {
  return KEYMAP.find((e) => e.id === id);
}
