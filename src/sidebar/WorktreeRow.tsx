/**
 * Worktree row — 26 px, one line (docs/ui-mockups/Main.dc.html).
 *
 * Reads one graph node and nothing else. Branch, `+N −M`, dirty count, `↑k`,
 * the lock and the unmerged badge all live on that node: `metrics` is folded
 * from `gilde://worktree/changed`, `badges` from the same fold and from the
 * roster's `locked` column. Two surfaces reading two sources is how a sidebar
 * and a graph start disagreeing, so there is exactly one.
 *
 * **The name owns the row** (BACKLOG 7). Every fact used to be a chip on the
 * same line, and in a 300 px sidebar the meta pushed the name off the row —
 * `+792 −115 ↑8 · 세션 1 · 잠김 · 미병합 · clean ●` left `r(` of the name
 * visible, which is the one thing the row exists to show. So the line is now
 * exactly what the mockup draws: branch glyph, name (flexing, ellipsised),
 * change size, then the status word and its dot at the right edge. Everything
 * else — branch, dirty count, ahead, sessions, lock, unmerged — is one
 * `title` string on the row and is spelled out in full in the inspector.
 *
 * UI-GUIDELINES §1 forbids a colour per item, so identity is carried by the
 * branch glyph and the slug, and the only colour on the row is the shared
 * status token.
 *
 * ## The narrow row (BACKLOG 15d)
 *
 * BACKLOG 7 got the meta down to two items, and at the default 300 px sidebar
 * the name was *still* cut — `readme-date-cl…`, which is every worktree that
 * shares a prefix rendered identically. Two things fix it, and neither is
 * "make the font smaller":
 *
 *  - below [`NARROW_ROW_PX`] the status **word** goes too, leaving the change
 *    counts and the status dot (which keeps the word as its tooltip, so nothing
 *    is lost, and colour is still not the only channel — the row tooltip spells
 *    the status out);
 *  - a name longer than [`NAME_MAX_CHARS`] is elided **in the middle**, because
 *    the end is the half that distinguishes it: `readme-date-…-claude-676a02`
 *    identifies a worktree, `readme-date-cl…` does not.
 *
 * The CSS ellipsis stays as the backstop for anything the character budget
 * misjudges (a name of wide CJK glyphs, a user font), and the name keeps its
 * own `title`, so the full string is always one hover away.
 */

import { StatusDot, statusMessageKey, statusTone } from '../components/StatusDot';
import { ChevronDownIcon, ChevronRightIcon, GitBranchIcon, PlusIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey, MessageParams } from '../i18n';
import { formatNumber } from '../lib/format';
import type { GraphNode, Locale } from '../lib/ipc';

/**
 * Lucide `trash-2`, inline like every other icon here (UI-GUIDELINES §2-3:
 * 16 px, 1.5 stroke, `currentColor`, no icon font). It sits in this file rather
 * than in `components/icons.tsx` only because that file belongs to T05.
 */
function TrashGlyph() {
  return (
    <svg viewBox="0 0 24 24" className="i sm" aria-hidden="true" focusable="false">
      <polyline points="3 6 5 6 21 6" />
      <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" />
      <path d="M10 11v6" />
      <path d="M14 11v6" />
    </svg>
  );
}

/**
 * Sidebar width at which the row stops printing the status word (BACKLOG 15d).
 *
 * The default sidebar is 300 px and `--sidebar-min` is 260, so the default is
 * deliberately on the narrow side of this line: that is the width the bug was
 * reported at. Past it there is room for the word as well as the name.
 */
export const NARROW_ROW_PX = 340;

export function isNarrowRow(sidebarWidth: number): boolean {
  return sidebarWidth < NARROW_ROW_PX;
}

/**
 * How many characters of a name a narrow row prints before eliding.
 *
 * A 300 px sidebar leaves the name roughly 160 px after the indent, the caret,
 * the branch glyph, the change counts and the dot; the row is 12 px monospace,
 * so ~22 characters fit. 24 is one notch above that on purpose — the CSS
 * ellipsis is the backstop, and eliding a name that would have fitted is worse
 * than letting the browser trim one character.
 */
export const NAME_MAX_CHARS = 24;

/**
 * `readme-date-sync-x-claude-676a02` → `readme-…-x-claude-676a02`.
 *
 * The tail gets two thirds of the budget because that is where the identity
 * lives: these names are `<subject>-<agent>-<short-sha>`, so a head-only
 * ellipsis throws away the only part that differs between two of them. Counted
 * in code points (`Array.from`) so a Korean or emoji-bearing name is cut on a
 * character rather than mid-surrogate.
 */
export function middleEllipsis(name: string, max = NAME_MAX_CHARS): string {
  const chars = Array.from(name);
  if (chars.length <= max || max < 3) return name;
  const budget = max - 1; // the ellipsis itself
  const head = Math.max(1, Math.floor(budget / 3));
  const tail = budget - head;
  return `${chars.slice(0, head).join('')}…${chars.slice(chars.length - tail).join('')}`;
}

/** `+N −M`, or null when the worktree has no diff against its base. */
export function changeCount(node: GraphNode, locale: Locale): string | null {
  const { insertions, deletions } = node.metrics;
  if (!(insertions ?? 0) && !(deletions ?? 0)) return null;
  return `+${formatNumber(insertions ?? 0, locale)} −${formatNumber(deletions ?? 0, locale)}`;
}

/**
 * Everything the row no longer has room to print, as one sentence for the
 * `title` tooltip (BACKLOG 7). Order is stable: branch, change size, dirty
 * count, ahead, sessions, lock, unmerged.
 */
export function worktreeMeta(
  node: GraphNode,
  sessionCount: number,
  t: (key: MessageKey, params?: MessageParams) => string,
  locale: Locale,
): string {
  const parts: string[] = [];
  if (node.sublabel) parts.push(node.sublabel);
  const change = changeCount(node, locale);
  if (change) parts.push(change);
  const { dirtyFiles, ahead } = node.metrics;
  if (dirtyFiles) parts.push(t('worktree.dirtyShort', { n: formatNumber(dirtyFiles, locale) }));
  if (ahead) parts.push(`${t('inspector.ahead')} ${formatNumber(ahead, locale)}`);
  if (sessionCount > 0) parts.push(t('sidebar.sessionCount', { n: sessionCount }));
  if (node.badges.includes('locked')) parts.push(t('badge.locked'));
  if (node.badges.includes('unmerged')) parts.push(t('badge.unmerged'));
  parts.push(t(statusMessageKey(node.status)));
  return parts.join(' · ');
}

export interface WorktreeRowProps {
  node: GraphNode;
  depth: number;
  collapsed: boolean;
  /** Session and run children in the projection. */
  sessionCount: number;
  /** The sidebar is too narrow for the status word (BACKLOG 15d). */
  narrow: boolean;
  selected: boolean;
  onToggle: () => void;
  onSelect: () => void;
  onOpen: () => void;
  onNewSession: () => void;
  onRemove: () => void;
}

export function WorktreeRow({
  node,
  depth,
  collapsed,
  sessionCount,
  narrow,
  selected,
  onToggle,
  onSelect,
  onOpen,
  onNewSession,
  onRemove,
}: WorktreeRowProps) {
  const { t, locale } = useI18n();

  const change = changeCount(node, locale);
  const tone = statusTone(node.status);
  const meta = worktreeMeta(node, sessionCount, t, locale);
  const statusWord = t(statusMessageKey(node.status));
  const name = narrow ? middleEllipsis(node.label) : node.label;

  return (
    <div
      className="tree-row"
      style={{ paddingLeft: 8 + depth * 12 }}
      role="treeitem"
      aria-expanded={!collapsed}
      aria-selected={selected}
      aria-label={node.label}
      title={meta}
      onClick={onSelect}
      onDoubleClick={onOpen}
    >
      <button
        type="button"
        className="sb-caret"
        aria-label={node.label}
        onClick={(event) => {
          event.stopPropagation();
          onToggle();
        }}
      >
        {collapsed ? <ChevronRightIcon size="sm" /> : <ChevronDownIcon size="sm" />}
      </button>

      <GitBranchIcon size="sm" className="sb-wt-icon" />
      {/* The name is the one thing that ellipsises, so it carries its own
          tooltip: hovering the name reads the name in full, hovering anywhere
          else on the row reads the meta the row no longer prints (BACKLOG 7 —
          `title={meta}` above starts at the branch and never repeats it). */}
      <span className="mono s12 sb-wt-name" title={node.label}>
        {name}
      </span>

      {change ? <span className="mono s11 sb-stat">{change}</span> : null}

      <span className="sb-actions">
        <button
          type="button"
          className="sb-iconbtn"
          title={t('sidebar.newSession')}
          aria-label={t('sidebar.newSession')}
          onClick={(event) => {
            event.stopPropagation();
            onNewSession();
          }}
        >
          <PlusIcon size="sm" />
        </button>
        <button
          type="button"
          className="sb-iconbtn"
          title={t('worktree.remove')}
          aria-label={t('worktree.remove')}
          onClick={(event) => {
            event.stopPropagation();
            onRemove();
          }}
        >
          <TrashGlyph />
        </button>
      </span>

      {/* The word is the first thing to go when the row runs out of width: the
          dot keeps it as a tooltip and the row tooltip states it in full, so a
          narrow sidebar loses no fact — only the space the name needed. */}
      {narrow ? null : (
        <span className="s11 sb-stat" data-tone={tone === 'neutral' ? undefined : tone}>
          {statusWord}
        </span>
      )}
      <StatusDot status={node.status} confidence={node.confidence} title={statusWord} />
    </div>
  );
}
