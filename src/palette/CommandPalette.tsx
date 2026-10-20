/**
 * ⌘K — the command palette (T27, DESIGN §9-8).
 *
 * `PaletteOverlay` is the frame both overlays share: a portal, a backdrop, one
 * search field and a keyboard-driven list. `WorktreeSwitcher` (⌘P) reuses it so
 * the two feel like one surface with two datasets, which is the whole point of
 * a palette layer.
 *
 * Keyboard contract, and why it is safe over a live terminal: the field is a
 * text entry, so `useShellHotkeys`'s focus guard lets every key through to it
 * except the bindings marked `allowInTextEntry` (Escape). ↑ ↓ ↵ are handled
 * here, on the field, and never reach the window layer.
 */

import { useEffect, useMemo, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent, ReactNode } from 'react';
import { createPortal } from 'react-dom';

import { Kbd } from '../components/Badge';
import { EmptyState } from '../components/EmptyState';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey, MessageParams } from '../i18n';
import { useAppStore } from '../store/useAppStore';
import {
  PALETTE_GROUPS,
  PALETTE_GROUP_LABEL,
  buildPaletteActions,
  formatCombo,
  paletteContext,
  rankActions,
} from './commands';
import type { PaletteAction, PaletteGroup } from './commands';
import './palette.css';

// ---------------------------------------------------------------------------
// Shared frame
// ---------------------------------------------------------------------------

export interface PaletteOverlayProps {
  open: boolean;
  label: string;
  query: string;
  placeholder: string;
  hint: string;
  onQuery: (value: string) => void;
  onClose: () => void;
  /** ↑ ↓ ↵ and anything else the list wants; the frame owns nothing but focus. */
  onKeyDown: (event: ReactKeyboardEvent<HTMLInputElement>) => void;
  /** The id of the active row, for `aria-activedescendant`. */
  activeId?: string;
  children: ReactNode;
}

export function PaletteOverlay({
  open,
  label,
  query,
  placeholder,
  hint,
  onQuery,
  onClose,
  onKeyDown,
  activeId,
  children,
}: PaletteOverlayProps) {
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) input.current?.focus();
  }, [open]);

  if (!open) return null;

  const content = (
    <>
      <div className="sheet-backdrop" onClick={onClose} />
      <div className="palette-layer">
        <div className="palette" role="dialog" aria-modal="true" aria-label={label}>
          <div className="palette-field">
            <input
              ref={input}
              type="text"
              className="palette-input"
              role="combobox"
              aria-expanded="true"
              aria-controls="gilde-palette-list"
              aria-label={label}
              {...(activeId ? { 'aria-activedescendant': activeId } : {})}
              value={query}
              placeholder={placeholder}
              spellCheck={false}
              autoComplete="off"
              onChange={(event) => onQuery(event.target.value)}
              onKeyDown={onKeyDown}
            />
          </div>
          <div className="palette-list scroll-y" id="gilde-palette-list" role="listbox" aria-label={label}>
            {children}
          </div>
          <div className="palette-hint s11 muted">{hint}</div>
        </div>
      </div>
    </>
  );

  // The portal is a browser affordance; with no `document` (the unit tests
  // render this tree to static markup on Node) the same content renders inline.
  if (typeof document === 'undefined') return content;
  return createPortal(content, document.body);
}

export interface PaletteRowProps {
  id: string;
  active: boolean;
  disabled?: boolean;
  title?: string;
  onPick: () => void;
  onHover: () => void;
  children: ReactNode;
}

/** One list row. `onMouseDown` runs the action so the field never loses focus. */
export function PaletteRow({
  id,
  active,
  disabled,
  title,
  onPick,
  onHover,
  children,
}: PaletteRowProps) {
  return (
    <div
      id={id}
      role="option"
      aria-selected={active}
      aria-disabled={disabled ? 'true' : undefined}
      className="palette-row"
      data-active={active ? 'true' : undefined}
      data-disabled={disabled ? 'true' : undefined}
      title={title}
      onMouseMove={onHover}
      onMouseDown={(event) => {
        event.preventDefault();
        if (!disabled) onPick();
      }}
    >
      {children}
    </div>
  );
}

/**
 * Move a selection over a list, skipping rows that cannot be activated. Used by
 * both overlays; returns the same index when every row is disabled.
 */
export function stepSelection(
  index: number,
  delta: number,
  length: number,
  enabled: (i: number) => boolean,
): number {
  if (length === 0) return 0;
  let next = index;
  for (let i = 0; i < length; i++) {
    next = (next + delta + length) % length;
    if (enabled(next)) return next;
  }
  return index;
}

// ---------------------------------------------------------------------------
// The palette itself
// ---------------------------------------------------------------------------

/** Stable empty list, so a closed palette memoizes to the same reference. */
const NO_ACTIONS: PaletteAction[] = [];

export function CommandPalette() {
  const { t, locale } = useI18n();
  const open = useAppStore((s) => s.ui.paletteOpen);
  const setPaletteOpen = useAppStore((s) => s.setPaletteOpen);

  const [query, setQuery] = useState('');
  const [index, setIndex] = useState(0);

  // Everything the context is derived from, subscribed individually: the list
  // has to follow a worktree that appears or an agent that is detected while
  // the palette is open, without re-rendering on every unrelated store write.
  const graph = useAppStore((s) => s.graph);
  const sessions = useAppStore((s) => s.sessions);
  const agents = useAppStore((s) => s.agents);
  const tabs = useAppStore((s) => s.ui.tabs);
  const panes = useAppStore((s) => s.ui.panes);
  const activePaneId = useAppStore((s) => s.ui.activePaneId);
  const selectedNodeId = useAppStore((s) => s.ui.selectedNodeId);
  const lastAgentByWorktree = useAppStore((s) => s.lastAgentByWorktree);

  // Closed, the list is not rebuilt at all: this component stays mounted and
  // the graph patches at 10 Hz (DESIGN §9-3).
  const actions = useMemo<PaletteAction[]>(
    () => (open ? buildPaletteActions(paletteContext(useAppStore.getState())) : NO_ACTIONS),
    [
      open,
      graph,
      sessions,
      agents,
      tabs,
      panes,
      activePaneId,
      selectedNodeId,
      lastAgentByWorktree,
      locale,
    ],
  );

  useEffect(() => {
    if (!open) return;
    setQuery('');
    setIndex(0);
  }, [open]);

  const label = (action: PaletteAction): string => {
    const params: MessageParams = { ...action.labelParams };
    for (const [name, key] of Object.entries(action.labelParamKeys ?? {})) {
      params[name] = t(key as MessageKey);
    }
    return t(action.labelKey, params);
  };

  const ranked = useMemo(
    () => rankActions(query, actions, label),
    // `locale` is a real dependency: the labels the ranking sees are translated.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [query, actions, locale],
  );

  // Grouped for the eye, ranked for the keyboard: with no query the list keeps
  // its declared order under section headers; a query drops the headers so the
  // best match is the first row.
  const sections = useMemo(() => {
    if (query.trim().length > 0) return [{ group: null, rows: ranked }];
    return PALETTE_GROUPS.map((group) => ({
      group: group as string | null,
      rows: ranked.filter((row) => row.action.group === group),
    })).filter((section) => section.rows.length > 0);
  }, [ranked, query]);

  /** The keyboard index space is the order the rows are actually painted in. */
  const ordered = useMemo(() => sections.flatMap((section) => section.rows), [sections]);

  const enabled = (i: number) => ordered[i]?.action.disabledKey === undefined;

  useEffect(() => {
    if (index < ordered.length && ordered[index]?.action.disabledKey === undefined) return;
    const first = ordered.findIndex((row) => row.action.disabledKey === undefined);
    setIndex(first < 0 ? 0 : first);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ordered]);

  if (!open) return null;

  const close = () => setPaletteOpen(false);

  const runAt = (i: number) => {
    const entry = ordered[i];
    if (!entry || entry.action.disabledKey) return;
    close();
    void entry.action.run();
  };

  const onKeyDown = (event: ReactKeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      setIndex((current) => stepSelection(current, 1, ordered.length, enabled));
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      setIndex((current) => stepSelection(current, -1, ordered.length, enabled));
    } else if (event.key === 'Enter') {
      event.preventDefault();
      runAt(index);
    }
  };

  let cursor = -1;

  return (
    <PaletteOverlay
      open={open}
      label={t('app.commandPalette')}
      query={query}
      placeholder={t('palette.placeholder')}
      hint={t('palette.hint')}
      onQuery={setQuery}
      onClose={close}
      onKeyDown={onKeyDown}
      {...(ordered[index] ? { activeId: `gilde-cmd-${ordered[index].action.id}` } : {})}
    >
      {ordered.length === 0 ? (
        <EmptyState message={t('palette.empty')} />
      ) : (
        sections.map((section) => (
          <div key={section.group ?? 'all'}>
            {section.group ? (
              <div className="sec palette-sec">
                {t(PALETTE_GROUP_LABEL[section.group as PaletteGroup])}
              </div>
            ) : null}
            {section.rows.map((row) => {
              cursor += 1;
              const at = cursor;
              const action = row.action;
              const disabled = action.disabledKey !== undefined;
              return (
                <PaletteRow
                  key={action.id}
                  id={`gilde-cmd-${action.id}`}
                  active={at === index}
                  disabled={disabled}
                  {...(action.disabledKey ? { title: t(action.disabledKey) } : {})}
                  onPick={() => runAt(at)}
                  onHover={() => {
                    if (!disabled && at !== index) setIndex(at);
                  }}
                >
                  <span className="ell grow">{label(action)}</span>
                  {action.disabledKey ? (
                    <span className="s11 muted ell palette-reason">{t(action.disabledKey)}</span>
                  ) : action.detail ? (
                    <span className="mono s11 muted ell palette-detail">{action.detail}</span>
                  ) : null}
                  {action.combo ? <Kbd>{formatCombo(action.combo)}</Kbd> : null}
                </PaletteRow>
              );
            })}
          </div>
        ))
      )}
    </PaletteOverlay>
  );
}
