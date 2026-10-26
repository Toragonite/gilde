import { defineConfig } from "vite";
import type { Plugin } from "vite";
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

// `freezePrototype` (contract §9.2) freezes `Object.prototype` before any of
// our scripts runs. xterm and its WebGL addon carry VS Code's `KeyCodeUtils`
// namespace, which assigns `ns.toString = fn` on a plain object; with
// `toString` read-only on the frozen prototype that assignment throws in
// strict code (the "override mistake") and the WebView never boots. The
// member is defined instead of assigned. Only `ident.toString=ident` inside
// `@xterm/*` is touched. Tauri freezes in `tauri dev` too, so those packages
// skip the dev pre-bundle (which no plugin sees) and come through here.
function xtermOnFrozenPrototype(): Plugin {
  const assign = /(?<![\w$.])([A-Za-z_$][\w$]*)\.toString=(?!=)([A-Za-z_$][\w$]*)/g;
  return {
    name: "gilde-xterm-frozen-prototype",
    transform(code, id) {
      if (!/[\\/]node_modules[\\/]@xterm[\\/]/.test(id)) return null;
      const next = code.replace(assign, 'Object.defineProperty($1,"toString",{value:$2,writable:!0,enumerable:!0,configurable:!0})');
      return next === code ? null : { code: next, map: null };
    },
  };
}

// https://vite.dev/config/
export default defineConfig(() => ({
  plugins: [react(), xtermOnFrozenPrototype()],

  optimizeDeps: {
    exclude: ["@xterm/xterm", "@xterm/addon-webgl"],
  },

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
