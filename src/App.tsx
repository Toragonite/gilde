/**
 * App — mounts the event bridge exactly once, boots the shell state, renders
 * the shell.
 *
 * Boot order mirrors what the backend can answer during M1: each step is
 * optional, because Rust commands land wave by wave and a partially wired
 * backend must still produce a usable window (an empty shell, not a crash).
 *
 *   1. `app_bootstrap()`   → instance ids, paths, locale, recovery count
 *   2. `app_settings_get()`→ locale, theme, persisted layout; then the active
 *                            view (Team on a first launch) is put in front
 *   3. `graph_snapshot()`  → first paint of the projection; every later change
 *                            arrives as `gilde://graph/patch`  [lanes only]
 *   4. `agents_detect()`   → the CLIs a terminal session could launch [lanes only]
 *   5. `health_get()`      → the status bar's health indicator (both builds)
 *   6. `sidecar_status()`  → the status bar's connection indicator
 *
 * Steps 3 and 4 serve the experimental lanes only (`src/features.ts`); the
 * preview build skips them, so it never asks for work no visible view uses.
 */

import { useEffect, useRef } from 'react';

import { experimentalLanes } from './features';
import { cmd, isUnavailable } from './lib/commands';
import { useEventBridge } from './hooks/useEventBridge';
import { AppShell } from './shell/AppShell';
import { useAppStore } from './store/useAppStore';
// T20: registers the terminal lane with the shell seams (the `terminal` pane
// view, the ⌘T overlay, the `newSession` intent and the session inspector
// actions). It belongs here rather than in the store: importing it from
// `useAppStore` closes an import cycle through `useHotkeys`, whose registry is
// still in its temporal dead zone when the module body runs.
import './terminal/register';
// T21: registers the sidebar (project/worktree/session tree), the new-worktree
// and worktree-removal overlays, and the `newWorktree` intent. Same reason as
// the line above: it must run after the store module is initialized.
import './sidebar/register';
// T24: registers the dispatch board pane view, the composer overlay (⌘⇧D)
// and the `newDispatch` intent. Same reason as the lines above: it must run
// after the store module is initialized.
import './dispatch/register';
// T23: registers the process monitor (the `monitor` pane view, the drawer's
// monitor panel and the status-bar budget meter) and the two subscriptions that
// let the rail and the sidebar's orphan group reach it. Same reason as the
// lines above: it must run after the store module is initialized.
import './monitor/register';
// T25: registers the live status graph (the `graph` pane view) and the two
// subscriptions that put the canvas in front when ⌘⇧G or the rail switches to
// it. Same reason as the lines above: it must run after the store exists.
import './graph/register';
// T8 (team contract v2 §8.2): registers the read-only team preview (the `team`
// pane view). Same reason as the lines above: it must run after the store exists.
import './team/register';
// Plan #3c: registers the read-only worker accounts view (the `workers` pane
// view). Same reason as the lines above: it must run after the store exists.
import './workers/register';
// T26: registers the toast stack, the settings sheet (⌘,) and the Recovered
// Sessions sheet, which must be in front of the main UI when the boot sweep
// left rows undecided. Same reason as the lines above: after the store exists.
import './dialogs/register';
// T27: registers the command palette (⌘K), the worktree switcher (⌘P) and the
// keyboard help sheet (⌘/). It comes last on purpose: the palette only *runs*
// intents the other lanes registered above, and registers none of its own.
import './palette/register';

async function boot(): Promise<void> {
  const store = useAppStore.getState;

  try {
    const info = await cmd.appBootstrap();
    store().setRecoveryPending(info.recoveryPending);
  } catch (e) {
    if (!isUnavailable(e)) console.warn('[gilde] app_bootstrap failed', e);
  }

  try {
    const config = await cmd.appSettingsGet();
    store().hydrateFromConfig(config);
  } catch (e) {
    if (!isUnavailable(e)) console.warn('[gilde] app_settings_get failed', e);
  }

  // The restored (or default) view is put in front: a first launch opens Team.
  store().revealView(store().ui.activeView);

  if (experimentalLanes()) {
    try {
      const snapshot = await cmd.graphSnapshot();
      store().applyGraphSnapshot(snapshot);
    } catch (e) {
      if (!isUnavailable(e)) console.warn('[gilde] graph_snapshot failed', e);
    }

    try {
      const agents = await cmd.agentsDetect(false);
      store().setAgents(agents);
    } catch (e) {
      if (!isUnavailable(e)) console.warn('[gilde] agents_detect failed', e);
    }
  }

  // The health surface's first reading; every later one is `gilde://health`.
  await store().loadHealth();

  try {
    const status = await cmd.sidecarStatus();
    store().setSidecar({
      state: status.state,
      generation: status.generation,
      restarts: status.restarts,
      lastError: status.lastError,
    });
  } catch (e) {
    if (!isUnavailable(e)) console.warn('[gilde] sidecar_status failed', e);
  }
}

export default function App() {
  // The one and only place `gilde://` events are subscribed (DESIGN §9-3 rule 3).
  useEventBridge();

  const booted = useRef(false);
  useEffect(() => {
    if (booted.current) return;
    booted.current = true;
    void boot();
  }, []);

  return <AppShell />;
}
