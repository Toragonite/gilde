/**
 * The palette (`⌘K`, ux-ia §2.4 Layer 1, §7.4): go to anything, do anything.
 *
 * A modal dialog with a combobox and a listbox: focus is trapped and returns
 * to where it was; ↑↓ move the active entry, ⏎ runs it, `Esc` (or `⌘K`
 * again) closes. `⏎` during Korean composition only finishes the syllable.
 * Disabled commands stay listed with their reason (ux-ia §7.4).
 */

import { useId, useMemo, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';
import { createPortal } from 'react-dom';

import { useFocusTrap } from '../hooks/useFocusTrap';
import { isImeComposing, useHotkeyScope, useHotkeys } from '../hooks/useHotkeys';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { AccountId } from '../lib/gen/AccountId';
import { newClientRequestId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { appStore, useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import type { EntryGroup, PaletteAction } from './entries';
import { buildEntries, rankEntries } from './entries';
import { addToSplitOrAsk } from '../split/replace';
import './palette.css';

const GROUP_LABEL: Readonly<Record<EntryGroup, MessageKey>> = {
  sessions: 'palette.group.sessions',
  projects: 'palette.group.projects',
  teams: 'palette.group.teams',
  members: 'palette.group.members',
  accounts: 'palette.group.accounts',
  commands: 'palette.group.commands',
};

function fail(error: unknown): void {
  appStore.getState().pushErrorToast(error as IpcError);
}

/** The session the palette's context commands act on. */
function frontUid() {
  const s = appStore.getState();
  const view = s.ui.view;
  if (view.screen === 'session') return view.uid;
  if (view.screen === 'split') return s.ui.split.focused;
  return null;
}

/** Run a palette action (exported for the tests). */
export function runPaletteAction(action: PaletteAction): void {
  const s = appStore.getState();
  s.setPaletteOpen(false);
  switch (action.kind) {
    case 'openSession':
      s.openSession(action.uid);
      return;
    case 'navigate':
      s.navigate(action.view);
      return;
    case 'sheet':
      s.openSheet(action.sheet, action.context);
      return;
    case 'run':
      break;
  }
  const uid = frontUid();
  const id = action.id;
  if (id.startsWith('makeDefault:')) {
    void commands.account_set_default({ accountId: id.slice('makeDefault:'.length) as AccountId }).catch(fail);
    return;
  }
  switch (id) {
    case 'addToSplit':
      // A full Split asks which tile to replace (A30.5); addToSplitOrAsk opens the Split for that.
      if (uid && addToSplitOrAsk(uid) !== 'ask') s.navigate({ screen: 'split' });
      return;
    case 'goToOwner':
      if (uid) void commands.owner_focus({ uid }).catch(fail);
      return;
    case 'continue':
    case 'continueCopy':
      if (uid && sessionOf(s.world, uid)) {
        void commands
          .session_continue({
            clientRequestId: newClientRequestId(),
            uid,
            how: { kind: id === 'continue' ? 'resume' : 'fork' },
            controlMode: null,
            model: null,
            permissionMode: null,
          })
          .catch(fail);
      }
      return;
    case 'continueAll': {
      const launch = s.ui.launch;
      if (launch && (launch.kind === 'afterQuit' || launch.kind === 'afterCrash')) {
        void commands.continue_all({ recordId: launch.recordId, uids: null }).catch(fail);
      }
      return;
    }
    case 'pause1h':
      void commands.notify_pause({ until: 'oneHour' }).catch(fail);
      return;
    case 'pauseUntilResume':
      void commands.notify_pause({ until: 'untilResume' }).catch(fail);
      return;
    case 'resume':
      void commands.notify_resume({}).catch(fail);
      return;
    case 'toggleSidebar':
      s.toggleSidebar();
      return;
  }
}

export function Palette() {
  const { t, locale } = useI18n();
  const world = useAppStore((s) => s.world);
  const ui = useAppStore((s) => s.ui);
  const close = useAppStore((s) => s.setPaletteOpen);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const dialog = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const listId = useId();
  const titleId = useId();

  useFocusTrap(dialog, true, { initialFocus: input });
  useHotkeyScope('X', true, dialog);
  useHotkeys([{ id: 'palette.close', combo: ['escape', 'mod+k'], allowInTextEntry: true, run: () => close(false) }], {
    scope: 'X',
    element: dialog,
  });

  const entries = useMemo(() => buildEntries({ world, ui, t, locale }), [world, ui, t, locale]);
  const ranked = useMemo(() => rankEntries(entries, query), [entries, query]);
  const current = Math.min(active, Math.max(0, ranked.length - 1));
  const optionId = (i: number) => `${listId}-o${i}`;

  const run = (i: number) => {
    const item = ranked[i];
    if (!item || item.entry.disabledReason) return;
    runPaletteAction(item.entry.action);
  };

  const onKeyDown = (event: ReactKeyboardEvent<HTMLInputElement>) => {
    if (isImeComposing(event.nativeEvent)) return;
    switch (event.code) {
      case 'ArrowDown':
        event.preventDefault();
        setActive(Math.min(current + 1, ranked.length - 1));
        break;
      case 'ArrowUp':
        event.preventDefault();
        setActive(Math.max(current - 1, 0));
        break;
      case 'Enter':
        event.preventDefault();
        if (!event.repeat) run(current);
        break;
    }
  };

  let lastGroup: EntryGroup | null = null;
  return createPortal(
    <div className="pal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && close(false)}>
      <div ref={dialog} className="pal" role="dialog" aria-modal="true" aria-labelledby={titleId}>
        <h2 id={titleId} className="g-sr-only">
          {t('palette.title')}
        </h2>
        <input
          ref={input}
          className="pal-input"
          type="text"
          role="combobox"
          aria-label={t('palette.input')}
          aria-expanded={ranked.length > 0}
          aria-controls={listId}
          aria-autocomplete="list"
          aria-activedescendant={ranked.length > 0 ? optionId(current) : undefined}
          placeholder={t('palette.input')}
          spellCheck={false}
          autoComplete="off"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setActive(0);
          }}
          onKeyDown={onKeyDown}
        />
        <p className="pal-hint">{t('palette.hint')}</p>
        {ranked.length === 0 ? (
          <p className="pal-empty">{t('palette.empty')}</p>
        ) : (
          <ul className="pal-list" id={listId} role="listbox" aria-label={t('palette.results', { n: ranked.length })}>
            {ranked.map(({ entry }, i) => {
              const header = entry.group !== lastGroup ? t(GROUP_LABEL[entry.group]) : null;
              lastGroup = entry.group;
              return (
                <li
                  key={entry.id}
                  id={optionId(i)}
                  role="option"
                  aria-selected={i === current}
                  aria-disabled={entry.disabledReason ? true : undefined}
                  className="pal-item"
                  data-group-start={header ? 'true' : undefined}
                  onMouseMove={() => i !== current && setActive(i)}
                  onClick={() => run(i)}
                >
                  {header ? (
                    <span className="pal-group" aria-hidden="true">
                      {header}
                    </span>
                  ) : null}
                  <span className="pal-row">
                    <span className="pal-label ell">{entry.label}</span>
                    <span className="pal-detail ell">{entry.disabledReason ?? entry.detail}</span>
                  </span>
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </div>,
    document.body,
  );
}
