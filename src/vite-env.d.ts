/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** `1` turns on the experimental lanes (sessions, dispatch, graph, monitor, drawer). See `src/features.ts`. */
  readonly VITE_GILDE_EXPERIMENTAL_LANES?: string;
  /** `1` builds the showcase: demo data in a plain browser, no Tauri. See `src/features.ts`. */
  readonly VITE_GILDE_SHOWCASE?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
