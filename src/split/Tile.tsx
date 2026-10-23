/**
 * One session as a tile — Split (ux-ia §3.7) and Team › Panes (§3.8, §3.9).
 *
 * Every tile is a full session: its own conversation (or terminal / log via
 * `⇧⌘T`), the card stack (T15's `CardStack`, never re-implemented), the
 * ownership or continue bar when Gilde cannot type into it, and its own
 * composer and draft. The pieces are T15's; this file arranges them.
 *
 * Focus (contract §8.6, ux-ia §3.7, §7.5):
 *  - DOM focus entering a tile makes it the focused tile (`onFocused`); the
 *    screen writes that to the UI store, which reports `app_view_state` [F-04].
 *  - Only the focused tile's keys are live: `⌘⏎`/`⌘⌫` on its top card, the
 *    composer's `⌘.`/`⇧⌘M`, the bars' `⌘⏎`, `⇧⌘T`. With the mouse any tile's
 *    card can be answered (the click focuses the tile first).
 *  - Focus moves into a tile only on a request (`requestTileFocus`: card >
 *    composer > body; `requestComposerFocus`: composer, caret at the end).
 *    A new card, a new tile or a finished turn never moves it.
 *  - `Esc` in the composer goes to the tile body; `Esc` in the body goes to
 *    the tile frame. A terminal keeps its own `Esc` (the agent's TUI needs it).
 *  - A mirrored card (a teammate's prompt that renders in its lead) answers in
 *    the `answer_in` session [F-15] — `CardStack` routes it; the tile only
 *    gives it the keys while focused.
 */

import { useEffect, useId, useRef, useState } from 'react';
import type { CSSProperties, KeyboardEvent as ReactKeyboardEvent } from 'react';

import { CardStack, ownerWord } from '../approval';
import { Popover, popoverTriggerProps } from '../components/Popover';
import { StateGlyph } from '../components/StateGlyph';
import { CloseIcon, MoreIcon, TerminalIcon } from '../components/icons';
import { Composer } from '../composer/Composer';
import { ConversationView } from '../convo/ConversationView';
import { isImeComposing, useHotkeys } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { IdentityColour } from '../lib/gen/IdentityColour';
import type { SessionUid } from '../lib/gen/SessionUid';
import { vendorBadge } from '../lib/vendor';
import { ContinueBar, ErrorBar, OwnershipBar } from '../session/SessionBars';
import { SEEN_DWELL_MS, stateWord, thirdView } from '../session/SessionScreen';
import { useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import { LogView } from '../terminal/LogView';
import { TerminalView } from '../terminal/TerminalView';
import { takeFocusRequest, useFocusRequest } from './focus';
import { OpenIcon, RestoreIcon, ZoomIcon } from './icons';
import './tile.css';

export interface TileMenuItem {
  id: string;
  label: string;
  run: () => void;
}

export interface TileNotice {
  text: string;
  action?: { label: string; run: () => void };
}

export interface TileProps {
  /** The session shown; `null` for a teammate whose session is not linked yet. */
  uid: SessionUid | null;
  /** Position in display order (0-based); `⌘n` is shown for the first six. */
  index: number;
  /** Tiles on the screen (for the accessible name). */
  count: number;
  /** Display name: the member name (Team); default the session title, else its agent (Split). */
  name?: string;
  swatch?: IdentityColour | null;
  /** "Lead" after the name. */
  roleLabel?: string | null;
  context: 'split' | 'team';
  focused: boolean;
  zoomed: boolean;
  style?: CSSProperties;
  /** Just appeared: a 1 px outline pulse (none under reduced motion). */
  isNew?: boolean;
  /** DOM focus entered the tile. */
  onFocused: () => void;
  onToggleZoom: () => void;
  /** Split: remove the tile (never stops the session). */
  onClose?: () => void;
  menu?: readonly TileMenuItem[];
  /** A line under the header (Crashed · exit n, a placeholder state). */
  notice?: TileNotice | null;
  /** Body text when there is no session to show. */
  emptyText?: string;
  /** `⌘D` adds this tile's session to the Split (Team tiles). */
  allowAddToSplit?: boolean;
  onAddToSplit?: (uid: SessionUid) => void;
}

const NO_MENU: readonly TileMenuItem[] = [];

export function Tile(props: TileProps) {
  const {
    uid,
    index,
    count,
    name,
    swatch = null,
    roleLabel = null,
    context,
    focused,
    zoomed,
    style,
    isNew = false,
    onFocused,
    onToggleZoom,
    onClose,
    menu = NO_MENU,
    notice = null,
    emptyText,
    allowAddToSplit = false,
    onAddToSplit,
  } = props;
  const { t, td } = useI18n();
  const session = useAppStore((s) => sessionOf(s.world, uid));
  const project = useAppStore((s) => (session?.projectId ? (s.world.projects.get(session.projectId) ?? null) : null));
  const visible = useAppStore((s) => s.ui.visibility.documentVisible);
  const openSession = useAppStore((s) => s.openSession);
  const frameRef = useRef<HTMLElement | null>(null);
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const composerRef = useRef<HTMLTextAreaElement | null>(null);
  const [cardToken, setCardToken] = useState<number | null>(null);
  const [flipped, setFlipped] = useState(false);

  const third = session ? thirdView(session) : null;
  const showTerminal = flipped && third !== null;
  const control = session?.control ?? null;
  const watching = control?.kind === 'watching';
  const notRunning = control?.kind === 'notRunning';
  const canSend = !!session && session.caps.send !== 'none' && !watching && !notRunning;
  const pendingCount = session?.pending.length ?? 0;

  // Focus requests (⌘n, arrows, strip, Show pane, a prefilled draft).
  const request = useFocusRequest(session?.uid ?? uid);
  useEffect(() => {
    if (!request) return;
    const target = session?.uid ?? uid;
    if (!target) return;
    const taken = takeFocusRequest(target);
    if (!taken || !frameRef.current?.isConnected) return;
    // Effects run after the commit, so the composer and the card are in the DOM.
    const composer = composerRef.current;
    if (taken.target === 'composer' && composer) {
      composer.focus();
      const end = composer.value.length;
      composer.setSelectionRange(end, end);
      return;
    }
    if (!watching && pendingCount > 0) {
      setCardToken((n) => (n ?? 0) + 1);
      return;
    }
    if (composer) composer.focus();
    else (bodyRef.current ?? frameRef.current)?.focus();
  }, [request?.token]); // eslint-disable-line react-hooks/exhaustive-deps

  // New reply → seen after 2 s in the focused tile with the window visible (§3.5.5).
  const newReply = session?.activity.state.kind === 'newReply';
  const resolved = session?.uid ?? null;
  useEffect(() => {
    if (!focused || !newReply || !visible || !resolved) return;
    const timer = setTimeout(() => void commands.session_mark_seen({ uid: resolved }).catch(() => undefined), SEEN_DWELL_MS);
    return () => clearTimeout(timer);
  }, [focused, newReply, visible, resolved]);

  useHotkeys([{ id: 'session.flipView', combo: 'mod+shift+t', run: () => setFlipped((f) => !f) }], {
    scope: 'T',
    enabled: focused && third !== null,
  });
  useHotkeys([{ id: 'split.add', combo: 'mod+d', run: () => session && onAddToSplit?.(session.uid) }], {
    scope: 'S',
    enabled: focused && allowAddToSplit && !!session && !!onAddToSplit,
  });

  const onKeyDown = (event: ReactKeyboardEvent<HTMLElement>) => {
    if (event.key !== 'Escape' || event.defaultPrevented || isImeComposing(event.nativeEvent)) return;
    if (event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
    const target = event.target as HTMLElement;
    if (target.closest('[data-gilde-hotkeys="allow"]')) return; // the terminal's own Esc
    if (composerRef.current && target === composerRef.current) {
      event.preventDefault();
      bodyRef.current?.focus();
      return;
    }
    if (bodyRef.current && (target === bodyRef.current || bodyRef.current.contains(target))) {
      event.preventDefault();
      frameRef.current?.focus();
    }
  };

  const agent = session ? vendorBadge(session.vendor) : '';
  const shownName = name ?? session?.title ?? (agent || t('common.unknown'));
  const state = session ? stateWord(session.activity, t) : '';
  const meta =
    context === 'team'
      ? [agent, session?.model ?? null]
      : [
          project?.name ?? null,
          agent,
          control?.kind === 'watching' && session ? t('ownership.runsIn', { owner: ownerWord(session, t) }) : null,
        ];
  const metaText = meta.filter((x): x is string => typeof x === 'string' && x !== '').join(' · ');
  const evidence = session?.activity.evidence ?? null;
  const flipLabel = t(showTerminal ? 'session.tab.conversation' : third === 'terminal' ? 'session.tab.terminal' : 'session.tab.log');
  const label = t('tile.label', { index: index + 1, tiles: count, name: shownName, state: state || shownName });

  let body;
  if (!session) {
    body = <p className="g-tile-empty">{emptyText ?? t('tile.gone')}</p>;
  } else if (showTerminal && third === 'terminal' && control?.kind === 'inGilde' && control.pty) {
    body = <TerminalView ptyId={control.pty} label={t('terminal.label', { agent })} active={focused} />;
  } else if (showTerminal && third === 'log') {
    body = <LogView uid={session.uid} />;
  } else {
    body = <ConversationView uid={session.uid} />;
  }

  return (
    <section
      ref={frameRef}
      role="region"
      aria-label={label}
      className="g-tile"
      data-context={context}
      data-focused={focused ? 'true' : undefined}
      data-zoomed={zoomed ? 'true' : undefined}
      data-new={isNew ? 'true' : undefined}
      data-uid={session?.uid ?? uid ?? undefined}
      tabIndex={-1}
      style={style}
      onFocusCapture={() => {
        if (!focused) onFocused();
      }}
      onKeyDown={onKeyDown}
    >
      <div className="g-tile-head">
        {index < 6 && (
          <span className="g-tile-key" aria-hidden="true">
            ⌘{index + 1}
          </span>
        )}
        {swatch && <span className="g-swatch" data-colour={swatch} aria-hidden="true" />}
        {session && <StateGlyph state={session.activity.state.kind} likely={session.activity.confidence === 'likely'} />}
        <span className="g-tile-name">{shownName}</span>
        {roleLabel && <span className="g-tile-role">· {roleLabel}</span>}
        {metaText && <span className="g-tile-meta">{metaText}</span>}
        <span className="g-tile-fill" />
        {state && <span className="g-tile-state">{state}</span>}
        <span className="g-tile-actions">
          {third !== null && (
            <button
              type="button"
              className="g-tbtn"
              aria-label={flipLabel}
              aria-pressed={showTerminal}
              aria-keyshortcuts="Meta+Shift+T"
              onClick={() => setFlipped((f) => !f)}
            >
              <TerminalIcon size="sm" />
              <span className="g-tbtn-text" aria-hidden="true">
                {flipLabel}
              </span>
            </button>
          )}
          {session && (
            <button type="button" className="g-tbtn g-tbtn-icon" aria-label={t('tile.openSession')} onClick={() => openSession(session.uid)}>
              <OpenIcon />
            </button>
          )}
          <button
            type="button"
            className="g-tbtn g-tbtn-icon"
            aria-label={t('tile.zoom')}
            aria-pressed={zoomed}
            aria-keyshortcuts="Meta+Shift+Enter"
            onClick={onToggleZoom}
          >
            {zoomed ? <RestoreIcon /> : <ZoomIcon />}
          </button>
          {menu.length > 0 && <TileMenu items={menu} />}
          {onClose && (
            <button type="button" className="g-tbtn g-tbtn-icon" aria-label={t('action.closeTile')} title={t('tile.closeNote')} onClick={onClose}>
              <CloseIcon size="sm" />
            </button>
          )}
        </span>
      </div>
      {evidence && <p className="g-tile-line">{td(evidence.key, evidence.params)}</p>}
      {notice && (
        <p className="g-tile-line g-tile-notice">
          <span>{notice.text}</span>
          {notice.action && (
            <button type="button" className="g-sbtn" onClick={notice.action.run}>
              {notice.action.label}
            </button>
          )}
        </p>
      )}
      <div ref={bodyRef} className="g-tile-body" tabIndex={-1} data-view={showTerminal ? 'terminal' : 'conversation'}>
        {body}
      </div>
      {session && (
        <div className="g-tile-dock">
          {!watching && (
            <CardStack
              uid={session.uid}
              shortcuts={focused}
              focusToken={cardToken}
              onReply={() => composerRef.current?.focus()}
              onLeave={() => bodyRef.current?.focus()}
              onShowTerminal={() => setFlipped(true)}
            />
          )}
          <ErrorBar session={session} onShowLog={() => setFlipped(true)} />
          {watching && <OwnershipBar session={session} active={focused} />}
          {notRunning && <ContinueBar session={session} active={focused} />}
          {canSend && (
            <Composer
              uid={session.uid}
              inputRef={composerRef}
              active={focused}
              compact={!focused}
              onFocusCard={() => setCardToken((n) => (n ?? 0) + 1)}
            />
          )}
        </div>
      )}
    </section>
  );
}

/** The tile's `⋯` menu (ux-ia §7.6: every action also has a visible control). */
export function TileMenu({ items, label }: { items: readonly TileMenuItem[]; label?: string }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLButtonElement>(null);
  const id = useId();
  return (
    <>
      <button
        ref={anchor}
        type="button"
        className="g-tbtn g-tbtn-icon"
        aria-label={label ?? t('tile.menu')}
        {...popoverTriggerProps(open, id, 'menu')}
        onClick={() => setOpen((o) => !o)}
      >
        <MoreIcon size="sm" />
      </button>
      <Popover open={open} anchor={anchor} onClose={() => setOpen(false)} role="menu" id={id} label={label ?? t('tile.menu')} className="g-tile-menu">
        {items.map((item) => (
          <button
            key={item.id}
            type="button"
            role="menuitem"
            tabIndex={-1}
            className="g-tile-menuitem"
            onClick={() => {
              setOpen(false);
              item.run();
            }}
          >
            {item.label}
          </button>
        ))}
      </Popover>
    </>
  );
}
