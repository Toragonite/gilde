/**
 * Title bar — 32px, drag region, current context and the global entry points.
 *
 * The window keeps its native macOS decorations, so this bar carries no window
 * buttons: it is the VS Code style command centre plus the two switches that
 * must be reachable from anywhere (theme, language).
 *
 * The context on the left follows the view (F6): the selected team's name on
 * Team, "Worker accounts" on Workers, and the project · branch of the selection
 * on the experimental lanes. The theme switch is an icon with a tooltip, the
 * language switch names the language it switches to (F16).
 */

import type { ComponentType } from 'react';

import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { Kbd } from '../components/Badge';
import { Mono } from '../components/Mono';
import { MoonIcon, SearchIcon, SunIcon, ThemeAutoIcon } from '../components/icons';
import type { IconProps } from '../components/icons';
import { flat, CAP } from '../team/model';
import { ancestorOfKind, nodesOfKind } from '../store/graphSlice';
import type { AppStore } from '../store/useAppStore';
import { useAppStore, useShallow } from '../store/useAppStore';
import type { Locale, ThemePreference } from '../lib/ipc';

const THEME_ORDER: ThemePreference[] = ['system', 'light', 'dark'];
const THEME_LABEL: Record<ThemePreference, MessageKey> = {
  system: 'app.theme.system',
  light: 'app.theme.light',
  dark: 'app.theme.dark',
};
const THEME_ICON: Record<ThemePreference, ComponentType<IconProps>> = {
  system: ThemeAutoIcon,
  light: SunIcon,
  dark: MoonIcon,
};

/** Each language by its own name — what the switch offers, never a code. */
export const LANGUAGE_NAME: Record<Locale, string> = { ko: '한국어', en: 'English' };

export interface TitleContext {
  /** A message key to translate, or null when `text` is shown as it is. */
  key: MessageKey | null;
  text: string;
}

/** What the left of the title bar says for the active view (F6). Pure. */
export function titleContext(s: AppStore): TitleContext {
  const view = s.ui.activeView;
  if (view === 'team') {
    const selected = s.team.selectedTeamId === null ? undefined : s.team.views.get(s.team.selectedTeamId);
    const name = selected ? flat(selected.state.name, CAP.name) : '';
    return name ? { key: null, text: name } : { key: 'title.teams', text: '' };
  }
  if (view === 'workers') return { key: 'view.workers', text: '' };
  const node = s.ui.selectedNodeId;
  const project = node ? ancestorOfKind(s.graph, node, 'project') : null;
  const fallback = project ?? nodesOfKind(s.graph, 'project')[0];
  const worktree = node ? ancestorOfKind(s.graph, node, 'worktree') : null;
  const branch = worktree?.meta.branch ?? worktree?.sublabel ?? null;
  if (!fallback) return { key: 'app.name', text: '' };
  return { key: null, text: `${fallback.label}${branch ? ` · ${branch}` : ''}` };
}

export function TitleBar() {
  const { t, locale, setLocale } = useI18n();
  const theme = useAppStore((s) => s.ui.theme);
  const setTheme = useAppStore((s) => s.setTheme);
  const setPaletteOpen = useAppStore((s) => s.setPaletteOpen);

  // `useShallow` keeps the snapshot stable: a selector that builds a fresh
  // object on every store change would re-render this bar 10 times a second.
  const context = useAppStore(useShallow(titleContext));

  const nextTheme = THEME_ORDER[(THEME_ORDER.indexOf(theme) + 1) % THEME_ORDER.length];
  const ThemeIcon = THEME_ICON[theme];
  const nextLocale: Locale = locale === 'ko' ? 'en' : 'ko';
  const themeTitle = t('app.theme.title', {
    theme: t(THEME_LABEL[theme]),
    next: t(THEME_LABEL[nextTheme]),
  });

  return (
    <header className="titlebar" data-tauri-drag-region>
      <Mono className="ell" small>
        {context.key ? t(context.key) : context.text}
      </Mono>

      <div className="grow" />

      <button
        type="button"
        className="btn ghost"
        onClick={() => setPaletteOpen(true)}
        title={t('app.commandPalette')}
      >
        <SearchIcon size="sm" />
        <span>{t('app.commandPalette')}</span>
        <Kbd>{'⌘K'}</Kbd>
      </button>

      <button
        type="button"
        className="btn icon ghost"
        onClick={() => setTheme(nextTheme)}
        title={themeTitle}
        aria-label={themeTitle}
        data-theme-pref={theme}
      >
        <ThemeIcon size="sm" />
      </button>

      <button
        type="button"
        className="btn ghost"
        onClick={() => setLocale(nextLocale)}
        title={t('app.locale.switch')}
        aria-label={`${t('app.locale.switch')} · ${LANGUAGE_NAME[nextLocale]}`}
        lang={nextLocale}
      >
        {LANGUAGE_NAME[nextLocale]}
      </button>
    </header>
  );
}
