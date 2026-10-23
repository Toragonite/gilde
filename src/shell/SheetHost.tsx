/**
 * The one sheet layer (ux-ia §2.4 Layer 2). `ui.sheet` names the open sheet;
 * its component comes from its owner's slot (`slots.ts`), the keyboard help
 * is the shell's own. The quit sheet also opens on `gilde://quit-requested`
 * (`ui.quitRequested`). A sheet whose module is not in this build is closed
 * again at once rather than leaving an invisible "open" sheet behind.
 */

import { useEffect } from 'react';
import type { ComponentType } from 'react';

import type { SheetId } from '../store/uiSlice';
import { useAppStore } from '../store/useAppStore';
import { HelpSheet } from './HelpSheet';
import { slots } from './slots';
import type { Slots } from './slots';

const SHEET_SLOT: Readonly<Record<Exclude<SheetId, 'help'>, keyof Slots>> = {
  newSession: 'NewSessionSheet',
  stopSession: 'StopSessionSheet',
  cleanUp: 'CleanUpSheet',
  commit: 'CommitSheet',
  merge: 'MergeSheet',
  discardWorktree: 'DiscardWorktreeSheet',
  closePane: 'ClosePaneSheet',
  quit: 'QuitSheet',
};

/** The component of a sheet in this build, or null. */
export function sheetComponent(sheet: SheetId): ComponentType | 'help' | null {
  if (sheet === 'help') return 'help';
  return (slots()[SHEET_SLOT[sheet]] as ComponentType | null) ?? null;
}

export function SheetHost() {
  const sheet = useAppStore((s) => s.ui.sheet);
  const quitRequested = useAppStore((s) => s.ui.quitRequested);
  const closeSheet = useAppStore((s) => s.closeSheet);
  const open: SheetId | null = sheet ?? (quitRequested ? 'quit' : null);
  const component = open ? sheetComponent(open) : null;

  useEffect(() => {
    if (sheet && sheetComponent(sheet) === null) closeSheet();
  }, [sheet, closeSheet]);

  if (!open || component === null) return null;
  if (component === 'help') return <HelpSheet onClose={closeSheet} />;
  const Component = component;
  return <Component />;
}
