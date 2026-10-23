/**
 * The build id rule (contract §4.7, A8.1) — the WebView half.
 *
 * One build-time value for the core, the WebView, the sidecar and the shim:
 * the env var `GILDE_BUILD_ID` when it is set and non-empty, else the app
 * version (`package.json` `version`, kept equal to `CARGO_PKG_VERSION` by
 * `check:version`). It names the `<app_data>/shim/<buildId>` directory and
 * travels in every handshake, so its value is strict:
 * `^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$`. Anything else fails the build with a
 * message naming `GILDE_BUILD_ID`; a value is never trimmed, escaped or
 * replaced.
 *
 * Pure (no Node API): `vite.config.ts` passes `process.env.GILDE_BUILD_ID`
 * and the package version in, and exposes the result as
 * `import.meta.env.VITE_GILDE_BUILD_ID`.
 */

export const BUILD_ID_PATTERN = /^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$/;

/** The env var that sets the build id. */
export const BUILD_ID_ENV = 'GILDE_BUILD_ID';

export function isValidBuildId(value: string): boolean {
  return BUILD_ID_PATTERN.test(value);
}

/**
 * The build id for `envValue` (`GILDE_BUILD_ID`, `undefined` when not set) and
 * the app `version`. Throws when the chosen value breaks the rule.
 */
export function resolveBuildId(envValue: string | undefined, version: string): string {
  const fromEnv = envValue !== undefined && envValue !== '';
  const id = fromEnv ? envValue : version;
  if (!isValidBuildId(id)) {
    const source = fromEnv ? `${BUILD_ID_ENV}=${JSON.stringify(id)}` : `${BUILD_ID_ENV} unset, package.json version ${JSON.stringify(id)}`;
    throw new Error(
      `invalid build id (${source}): it must match ${BUILD_ID_PATTERN.source} — ` +
        `1 to 64 ASCII letters, digits, ".", "_", "+" or "-", starting with a letter or digit, no whitespace or "/" (contract §4.7)`,
    );
  }
  return id;
}
