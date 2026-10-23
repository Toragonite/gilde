/**
 * Build switches the shell reads.
 *
 * Gilde 2 has one build of the UI: every screen of ux-ia §2.6 ships, and what
 * a session may offer comes from the core (`Session.caps`, `Team.caps`,
 * `AppFeatures`), never from a client-side flag. The only build switch left
 * is the showcase.
 *
 * `VITE_GILDE_SHOWCASE=1` builds the SHOWCASE — the real UI on T19's demo
 * backend (`src/showcase/`), in a plain browser with no Tauri runtime. The
 * demo code is loaded only through the guarded dynamic `import()` in
 * `main.tsx`, so a normal build drops it
 * (`scripts/check-showcase-tree-shaking.mjs`). Which backend answers the IPC
 * boundary at run time is `lib/mode.ts`'s `showcaseMode()`.
 */

export const SHOWCASE: boolean = import.meta.env.VITE_GILDE_SHOWCASE === '1';
