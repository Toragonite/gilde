import React from 'react';
import ReactDOM from 'react-dom/client';

import App from './App';
import './styles/global.css';

async function start(): Promise<void> {
  // The showcase (`VITE_GILDE_SHOWCASE=1`, `SHOWCASE` in src/features.ts) seeds
  // the store with its demo data before the first paint. Only this guarded
  // dynamic import reaches `src/showcase/`. The guard is `SHOWCASE`'s own
  // expression, spelled out here on purpose: the bundler folds it inside this
  // module and then never emits the showcase chunk at all, whereas an imported
  // constant only drops the call and still writes the chunk into dist/
  // (scripts/check-showcase-tree-shaking.mjs checks the difference).
  if (import.meta.env.VITE_GILDE_SHOWCASE === '1') {
    try {
      const { startShowcase } = await import('./showcase/showcase');
      await startShowcase(window.location.search);
    } catch (e) {
      // A broken showcase must say so, not render an empty window silently.
      console.error('[gilde] showcase failed to start', e);
    }
  }

  ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
    <React.StrictMode>
      <App />
    </React.StrictMode>,
  );
}

void start();
