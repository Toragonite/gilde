import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

import pkg from "./package.json";
import { resolveBuildId } from "./src/lib/buildId";

// Read the environment without depending on Node's type declarations.
const env: Record<string, string | undefined> =
  (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env ?? {};

const host = env.TAURI_DEV_HOST;

// The build id (contract §4.7, A8.1): GILDE_BUILD_ID when set and non-empty,
// else the app version. An invalid value fails the build here, before anything
// is bundled. The WebView always carries it: the boot check compares it with
// `app_bootstrap.buildId` (src/hooks/useEventBridge.ts).
const buildId = resolveBuildId(env.GILDE_BUILD_ID, pkg.version);

// https://vite.dev/config/
export default defineConfig(() => ({
  plugins: [react()],

  define: {
    "import.meta.env.VITE_GILDE_BUILD_ID": JSON.stringify(buildId),
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
