/**
 * The Floor's isolated harness: the Floor alone on the showcase backend, for
 * the M16 geometry gate and the frame-rate checks before (and besides) the
 * app shell. Not part of the app: nothing imports it, and only the build
 * below uses `index.html` next to it.
 *
 *   VITE_GILDE_SHOWCASE=1 npx vite build src/floor/harness --config vite.config.ts --outDir <dir> --emptyOutDir
 *   node scripts/gates/browser-gates.mjs --dist <dir> --gates floor --out <tmp>
 *
 * It boots exactly like the app (the event bridge: `app_bootstrap`, then
 * `store_subscribe` on the scene's world) and renders the main view the
 * scene opens: the global Floor, or Team › Floor of the scene's team.
 */

import React, { useEffect } from 'react';
import ReactDOM from 'react-dom/client';

import { LiveRegion } from '../../components/LiveRegion';
import { useEventBridge } from '../../hooks/useEventBridge';
import { useI18n } from '../../i18n/useI18n';
import { useAppStore } from '../../store/useAppStore';
import { FloorScreen, TeamFloor } from '../index';
import '../../styles/global.css';

function Root() {
  useEventBridge();
  const { t } = useI18n();
  const view = useAppStore((s) => s.ui.view);
  const theme = useAppStore((s) => s.ui.theme);
  const locale = useAppStore((s) => s.ui.locale);
  useEffect(() => {
    const root = document.documentElement;
    if (theme === 'system') root.removeAttribute('data-theme');
    else root.setAttribute('data-theme', theme);
    root.lang = locale;
  }, [theme, locale]);
  return (
    <>
      <main aria-label={t('floor.name')} style={{ height: '100vh', display: 'flex', flexDirection: 'column' }}>
        {view.screen === 'team' ? <TeamFloor teamKey={view.teamKey} /> : <FloorScreen />}
      </main>
      <LiveRegion />
    </>
  );
}

async function start(): Promise<void> {
  if (import.meta.env.VITE_GILDE_SHOWCASE === '1') {
    try {
      const { startShowcase } = await import('../../showcase/showcase');
      await startShowcase(window.location.search);
    } catch (e) {
      console.error('[gilde] showcase failed to start', e);
    }
  }
  ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
    <React.StrictMode>
      <Root />
    </React.StrictMode>,
  );
}

void start();
