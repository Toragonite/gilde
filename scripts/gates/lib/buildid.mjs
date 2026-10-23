/**
 * The build id rule (contract §4.7, A8.1) for the Node scripts.
 *
 * `buildId` = `GILDE_BUILD_ID` when set and non-empty, else the app version;
 * the value must match `^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$` and is never
 * trimmed, escaped or replaced. The same rule lives in `src/lib/buildId.ts`
 * (WebView), `src-tauri/src/lib.rs` (core) and `src-tauri/sidecar/esbuild.mjs`
 * (sidecar); `scripts/gates/test/buildid.test.mjs` pins this copy to the
 * WebView's pattern.
 *
 * A release build sets `GILDE_BUILD_ID=<version>+<git short sha>.<UTC
 * yyyymmddTHHMMSS>` (A8.1); `RELEASE_ID_SCAN` finds ids of that form inside an
 * artifact, so a component built with another id is caught even when it also
 * carries the plain version.
 */

export const BUILD_ID_PATTERN = /^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$/;
export const BUILD_ID_ENV = 'GILDE_BUILD_ID';

/** Ids of the release form, unanchored (artifacts pack strings without delimiters). */
export const RELEASE_ID_SCAN = /\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?\+[0-9a-f]{7,40}\.\d{8}T\d{6}/g;

export function isValidBuildId(value) {
  return typeof value === 'string' && BUILD_ID_PATTERN.test(value);
}

/**
 * The build id for `envValue` (undefined when unset) and `version`, or an
 * `{ error }` naming `GILDE_BUILD_ID` when the chosen value breaks the rule.
 */
export function resolveBuildId(envValue, version) {
  const fromEnv = envValue !== undefined && envValue !== '';
  const id = fromEnv ? envValue : version;
  if (!isValidBuildId(id)) {
    const source = fromEnv ? `${BUILD_ID_ENV}=${JSON.stringify(id)}` : `${BUILD_ID_ENV} unset, version ${JSON.stringify(id)}`;
    return { error: `invalid build id (${source}): it must match ${BUILD_ID_PATTERN.source} (contract §4.7, A8.1)` };
  }
  return { id, source: fromEnv ? 'env' : 'version' };
}
