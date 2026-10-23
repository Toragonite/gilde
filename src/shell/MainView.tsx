/**
 * The main zone: exactly one screen at a time (ux-ia §2.4) inside the page's
 * one `main` landmark, named after the screen. Home is the shell's own; the
 * other screens come from their owners through `slots.ts`. A screen whose
 * module is not in this build shows one neutral sentence.
 */

import { useI18n } from '../i18n/useI18n';
import { HomeScreen } from '../home/HomeScreen';
import { useAppStore } from '../store/useAppStore';
import { useMainDrop } from '../sidebar/splitAdd';
import { slots } from './slots';

function NotInBuild({ title }: { title: string }) {
  const { t } = useI18n();
  return (
    <div className="shell-notinbuild">
      <h1 className="g-sr-only">{title}</h1>
      <p>{t('shell.notInBuild')}</p>
    </div>
  );
}

function Screen({ sidebarShown, title }: { sidebarShown: boolean; title: string }) {
  const view = useAppStore((s) => s.ui.view);
  const s = slots();
  switch (view.screen) {
    case 'home':
      return <HomeScreen sidebarShown={sidebarShown} />;
    case 'session':
      return s.SessionScreen ? <s.SessionScreen key={view.uid} uid={view.uid} sub={view.sub} /> : <NotInBuild title={title} />;
    case 'split':
      return s.SplitScreen ? <s.SplitScreen /> : <NotInBuild title={title} />;
    case 'team':
      return s.TeamScreen ? <s.TeamScreen key={view.teamKey} teamKey={view.teamKey} mode={view.mode} /> : <NotInBuild title={title} />;
    case 'floor':
      return s.FloorScreen ? <s.FloorScreen /> : <NotInBuild title={title} />;
    case 'history':
      return s.HistoryScreen ? <s.HistoryScreen /> : <NotInBuild title={title} />;
    case 'accounts':
      return s.AccountsScreen ? <s.AccountsScreen section={view.section} /> : <NotInBuild title={title} />;
    case 'settings':
      return s.SettingsScreen ? <s.SettingsScreen /> : <NotInBuild title={title} />;
  }
}

export function MainView({ label, sidebarShown }: { label: string; sidebarShown: boolean }) {
  const { t } = useI18n();
  const screen = useAppStore((s) => s.ui.view.screen);
  // A sidebar session row dropped here goes to the Split (ux-ia §3.8, T32).
  const drop = useMainDrop();
  return (
    <main className="shell-view" aria-label={label} data-region="main" data-screen={screen} tabIndex={-1} {...drop.props}>
      <Screen sidebarShown={sidebarShown} title={label} />
      {drop.over ? (
        <div className="shell-drop" aria-hidden="true">
          <span className="shell-drop-label">{t('session.addToSplit')}</span>
        </div>
      ) : null}
    </main>
  );
}
