/**
 * Global keyboard layer (DESIGN §9-8).
 *
 * Two rules make this coexist with four live terminals:
 *
 *  1. Bindings are matched on `event.code`, not `event.key`. With Option held
 *     macOS rewrites `key` (⌘⌥B arrives as `∫`), and a Korean layout rewrites it
 *     too; `code` is the physical key and survives both.
 *  2. The listener runs in the **capture** phase on `window`, so it sees the
 *     ⌘-combos before xterm swallows the event, while every non-⌘ key still
 *     reaches the terminal untouched.
 *
 * Focus guard: a hotkey never fires while a text entry has focus. A terminal
 * host opts back in by marking its container `data-gilde-hotkeys="allow"` — xterm's
 * helper textarea is a text entry, but ⌘-codes must still work over a terminal.
 */

import { useEffect, useMemo, useRef } from 'react';

import { experimentalLanes } from '../features';
import { useAppStore } from '../store/useAppStore';
import type { SplitCount } from '../store/uiSlice';

export interface HotkeyBinding {
  id: string;
  /** `mod`(⌘) `alt`(⌥) `shift` + a key name, e.g. `mod+shift+backslash`. */
  combo: string;
  run: (event: KeyboardEvent) => void;
  /** Set for bindings that must fire even inside a text entry (Escape). */
  allowInTextEntry?: boolean;
  /**
   * Extra condition. A binding whose predicate is false is not matched at all,
   * so the key is never swallowed — `Escape` must still reach a TUI unless an
   * overlay is actually open.
   */
  when?: () => boolean;
}

// ---------------------------------------------------------------------------
// Combo parsing
// ---------------------------------------------------------------------------

interface ParsedCombo {
  mod: boolean;
  alt: boolean;
  shift: boolean;
  code: string;
}

const CODE_ALIASES: Record<string, string> = {
  backslash: 'Backslash',
  slash: 'Slash',
  comma: 'Comma',
  period: 'Period',
  escape: 'Escape',
  enter: 'Enter',
  space: 'Space',
  arrowleft: 'ArrowLeft',
  arrowright: 'ArrowRight',
  arrowup: 'ArrowUp',
  arrowdown: 'ArrowDown',
};

export function codeFor(key: string): string {
  const lower = key.toLowerCase();
  if (CODE_ALIASES[lower]) return CODE_ALIASES[lower];
  if (/^[a-z]$/.test(lower)) return `Key${lower.toUpperCase()}`;
  if (/^[0-9]$/.test(lower)) return `Digit${lower}`;
  return key;
}

export function parseCombo(combo: string): ParsedCombo {
  const parts = combo.split('+').map((p) => p.trim().toLowerCase());
  const parsed: ParsedCombo = { mod: false, alt: false, shift: false, code: '' };
  for (const part of parts) {
    if (part === 'mod' || part === 'cmd' || part === 'meta') parsed.mod = true;
    else if (part === 'alt' || part === 'opt') parsed.alt = true;
    else if (part === 'shift') parsed.shift = true;
    else parsed.code = codeFor(part);
  }
  return parsed;
}

/**
 * `mod` is ⌘ only, never Ctrl: this is a macOS app and Ctrl-chords belong to the
 * terminal (Ctrl+B, Ctrl+W, Ctrl+C are all live keys inside an agent TUI).
 */
export function matchesCombo(event: KeyboardEvent, combo: string): boolean {
  const want = parseCombo(combo);
  if (event.code !== want.code) return false;
  if (event.metaKey !== want.mod) return false;
  if (event.ctrlKey) return false;
  if (event.altKey !== want.alt) return false;
  if (event.shiftKey !== want.shift) return false;
  return true;
}

// ---------------------------------------------------------------------------
// Focus guard
// ---------------------------------------------------------------------------

export function isTextEntryTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.closest('[data-gilde-hotkeys="allow"]')) return false;
  if (target.isContentEditable) return true;
  const tag = target.tagName;
  if (tag === 'TEXTAREA') return true;
  if (tag === 'SELECT') return true;
  if (tag === 'INPUT') {
    const type = (target as HTMLInputElement).type;
    return !['button', 'checkbox', 'radio', 'range', 'submit', 'reset', 'color'].includes(type);
  }
  return false;
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

export function useHotkeys(bindings: HotkeyBinding[], enabled = true): void {
  const ref = useRef(bindings);
  ref.current = bindings;

  useEffect(() => {
    if (!enabled) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.repeat) return;
      const inText = isTextEntryTarget(event.target);
      for (const binding of ref.current) {
        if (inText && !binding.allowInTextEntry) continue;
        if (binding.when && !binding.when()) continue;
        if (!matchesCombo(event, binding.combo)) continue;
        event.preventDefault();
        event.stopPropagation();
        binding.run(event);
        return;
      }
    };
    window.addEventListener('keydown', onKeyDown, { capture: true });
    return () => window.removeEventListener('keydown', onKeyDown, { capture: true });
  }, [enabled]);
}

// ---------------------------------------------------------------------------
// Intents — actions whose implementation lands in a later wave
// ---------------------------------------------------------------------------

export type IntentId =
  | 'newSession'
  | 'newWorktree'
  | 'newDispatch'
  | 'saveTranscript'
  | 'reloadSignatures';

const intents = new Map<IntentId, () => void>();

/** Register the real implementation; returns the unregister function. */
export function registerIntent(id: IntentId, run: () => void): () => void {
  intents.set(id, run);
  return () => {
    if (intents.get(id) === run) intents.delete(id);
  };
}

export function runIntent(id: IntentId): boolean {
  const run = intents.get(id);
  if (!run) return false;
  run();
  return true;
}

// ---------------------------------------------------------------------------
// The shell's own bindings
// ---------------------------------------------------------------------------

/** Canonical combo per action, also rendered by the help sheet (`⌘/`). */
export const SHELL_HOTKEYS = {
  palette: 'mod+k',
  viewTeam: 'mod+1',
  viewWorkers: 'mod+2',
  switcher: 'mod+p',
  newSession: 'mod+t',
  closePane: 'mod+w',
  splitRight: 'mod+backslash',
  splitDown: 'mod+shift+backslash',
  toggleSidebar: 'mod+b',
  toggleInspector: 'mod+alt+b',
  toggleDrawer: 'mod+j',
  viewGraph: 'mod+shift+g',
  viewMonitor: 'mod+shift+m',
  viewDispatch: 'mod+shift+d',
  settings: 'mod+comma',
  help: 'mod+slash',
  prevTab: 'mod+alt+arrowleft',
  nextTab: 'mod+alt+arrowright',
  closeOverlay: 'escape',
} as const;

export type ShellHotkeyId = keyof typeof SHELL_HOTKEYS;

/**
 * Bindings that belong to the experimental lanes (`src/features.ts`). In the
 * preview build none of them is claimed, so the key reaches whatever has focus.
 */
export const EXPERIMENTAL_HOTKEYS: ReadonlySet<ShellHotkeyId> = new Set<ShellHotkeyId>([
  'switcher',
  'newSession',
  'splitRight',
  'splitDown',
  'toggleInspector',
  'toggleDrawer',
  'viewGraph',
  'viewMonitor',
  'viewDispatch',
]);

/** The shell bindings the current build claims, in `SHELL_HOTKEYS` order. */
export function activeHotkeyIds(): ShellHotkeyId[] {
  const lanes = experimentalLanes();
  return (Object.keys(SHELL_HOTKEYS) as ShellHotkeyId[]).filter(
    (id) => lanes || !EXPERIMENTAL_HOTKEYS.has(id),
  );
}

/**
 * Focus a tab by its number — experimental lanes only, where terminal tabs pile
 * up. ⌘1/⌘2 are the Team and Worker accounts views, so the range lives on ⌘⌥.
 */
export function focusTabCombo(n: number): string {
  return `mod+alt+${n}`;
}

/**
 * Mounted once by `AppShell`. Every action here is a store action or an intent,
 * so the palette (T27) can trigger the exact same code paths.
 */
export function useShellHotkeys(): void {
  const store = useAppStore;

  const bindings = useMemo<HotkeyBinding[]>(() => {
    const s = () => store.getState();

    const all: HotkeyBinding[] = [
      { id: 'palette', combo: SHELL_HOTKEYS.palette, run: () => s().setPaletteOpen(true) },
      { id: 'viewTeam', combo: SHELL_HOTKEYS.viewTeam, run: () => s().setActiveView('team') },
      {
        id: 'viewWorkers',
        combo: SHELL_HOTKEYS.viewWorkers,
        run: () => s().setActiveView('workers'),
      },
      { id: 'switcher', combo: SHELL_HOTKEYS.switcher, run: () => s().setSwitcherOpen(true) },
      {
        id: 'newSession',
        combo: SHELL_HOTKEYS.newSession,
        run: () => {
          runIntent('newSession');
        },
      },
      { id: 'closePane', combo: SHELL_HOTKEYS.closePane, run: () => s().closeActivePane() },
      {
        id: 'splitRight',
        combo: SHELL_HOTKEYS.splitRight,
        run: () => {
          const split = s().ui.panes.split;
          s().setSplit((split >= 3 ? 1 : split + 1) as SplitCount);
        },
      },
      {
        id: 'splitDown',
        combo: SHELL_HOTKEYS.splitDown,
        run: () => s().setSplit(s().ui.panes.split === 4 ? 2 : 4),
      },
      { id: 'toggleSidebar', combo: SHELL_HOTKEYS.toggleSidebar, run: () => s().toggleSidebar() },
      {
        id: 'toggleInspector',
        combo: SHELL_HOTKEYS.toggleInspector,
        run: () => s().toggleInspector(),
      },
      { id: 'toggleDrawer', combo: SHELL_HOTKEYS.toggleDrawer, run: () => s().toggleDrawer() },
      { id: 'viewGraph', combo: SHELL_HOTKEYS.viewGraph, run: () => s().setActiveView('graph') },
      {
        id: 'viewMonitor',
        combo: SHELL_HOTKEYS.viewMonitor,
        run: () => s().setActiveView('monitor'),
      },
      {
        id: 'viewDispatch',
        combo: SHELL_HOTKEYS.viewDispatch,
        run: () => {
          s().setActiveView('dispatch');
          runIntent('newDispatch');
        },
      },
      { id: 'settings', combo: SHELL_HOTKEYS.settings, run: () => s().setSettingsOpen(true) },
      { id: 'help', combo: SHELL_HOTKEYS.help, run: () => s().setHelpOpen(true) },
      { id: 'prevTab', combo: SHELL_HOTKEYS.prevTab, run: () => s().cycleTab(-1) },
      { id: 'nextTab', combo: SHELL_HOTKEYS.nextTab, run: () => s().cycleTab(1) },
      {
        id: 'closeOverlay',
        combo: SHELL_HOTKEYS.closeOverlay,
        allowInTextEntry: true,
        // Only claim Escape while something is actually open.
        when: () => {
          const { paletteOpen, switcherOpen, helpOpen, settingsOpen } = s().ui;
          return paletteOpen || switcherOpen || helpOpen || settingsOpen;
        },
        run: () => s().closeOverlays(),
      },
    ];

    const active = new Set<string>(activeHotkeyIds());
    const list = all.filter((binding) => active.has(binding.id));

    if (experimentalLanes()) {
      for (let i = 1; i <= 9; i++) {
        list.push({
          id: `focusTab${i}`,
          combo: focusTabCombo(i),
          run: () => s().focusTabIndex(i - 1),
        });
      }
    }

    return list;
  }, [store]);

  useHotkeys(bindings);
}
