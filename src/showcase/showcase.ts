/**
 * The showcase — the real UI, fed neutral demo data, in a plain browser
 * (`VITE_GILDE_SHOWCASE=1 npx vite`, then `?scene=…`; see `scenes.ts` and
 * `site/README.md` § Screenshots).
 *
 * `main.tsx` loads this module only through its guarded dynamic import and
 * awaits `startShowcase` before the first paint. Nothing here renders anything
 * of its own: it installs the demo backend at the IPC boundary
 * (`lib/commands.ts`), then seeds the store ONCE through the same actions the
 * app's boot and views use (`hydrateFromConfig`, `loadHealth`, `refreshTeams`,
 * `loadRoster`, `resolveRepoIdentities`, `refreshWorkers`) and sets the scene's
 * UI state with the ordinary setters (active view, selected team and member,
 * grouping, scope, the health popover). From then on the app runs exactly as it
 * does against Rust — its polls simply get the same demo answers back.
 */

import { cmd, installShowcaseBackend } from '../lib/commands';
import { useAppStore } from '../store/useAppStore';
import { setFolderPicker } from '../store/worktreeSlice';
import { identityPaths } from '../team/grouping';
import { SHOWCASE_MARKER, createShowcaseBackend, sceneConfig } from './backend';
import { SCENES, parseShowcaseQuery } from './scenes';
import type { ShowcaseOptions } from './scenes';
import { MAIN_TEAM_ID, buildWorld } from './world';
import type { ShowcaseWorld } from './world';

export { SHOWCASE_MARKER };
export type { ShowcaseOptions };

export interface SeededShowcase {
  options: ShowcaseOptions;
  world: ShowcaseWorld;
  /** Every command the store asked the demo backend while seeding (and after). */
  asked: readonly string[];
}

/** Install the demo backend for one scene and seed the store. `now` anchors every time in the data. */
export async function seedShowcase(options: ShowcaseOptions, now: number = Date.now()): Promise<SeededShowcase> {
  const spec = SCENES[options.scene];
  const world = buildWorld(now, spec.world);
  const { backend, asked } = createShowcaseBackend(world, sceneConfig(options.locale, options.theme, spec));
  installShowcaseBackend(backend);
  // "Add project folder" opens a native picker in the app; a page has none, so it reads as cancelled.
  setFolderPicker(async () => null);

  const store = useAppStore.getState;

  // What the app's boot does (App.tsx): settings first (language, theme, the
  // restored layout), then the connection and the health reading.
  store().hydrateFromConfig(await cmd.appSettingsGet());
  const sidecar = await cmd.sidecarStatus();
  store().setSidecar({
    state: sidecar.state,
    generation: sidecar.generation,
    restarts: sidecar.restarts,
    lastError: sidecar.lastError,
  });
  await store().loadHealth();
  if (spec.snoozeAlerts) {
    // What closing an action alert's toast does in the app: dismiss it in the backend.
    for (const alert of store().health.snapshot?.alerts ?? []) {
      if (alert.severity === 'action' && !alert.dismissed) await store().dismissHealthAlert(alert.id);
    }
  }

  // What the views ask when they are shown — done once here so the first paint is complete.
  await store().refreshTeams();
  await store().loadRoster();
  await store().resolveRepoIdentities(
    identityPaths(
      store().team.views.values(),
      store().worktree.roster.map((entry) => entry.project),
    ),
  );
  await store().refreshWorkers(false);

  // The scene's UI state, with the setters a click would use.
  store().setTeamScope(spec.scope);
  store().setTeamGroupBy(spec.groupBy);
  store().setActiveView(spec.view);
  store().selectTeam(MAIN_TEAM_ID);
  store().selectTeamMember(options.member);
  store().setHealthPopover(spec.popover);

  return { options, world, asked };
}

/** The showcase build's start: the scene the URL names. */
export async function startShowcase(search: string): Promise<void> {
  await seedShowcase(parseShowcaseQuery(search));
}
