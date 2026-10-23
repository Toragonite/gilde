/**
 * The release-profile checks of contract §14.6 G0 and A8.1, on built
 * artifacts. Pure functions over buffers and parsed JSON; the CLI
 * (`../release-profile-check.mjs`) reads the files.
 *
 * What a release build must satisfy:
 *  1. one build id: the WebView bundle, `sidecar.cjs`, the core binary (and the
 *     `gilde-tmux` shim, which uses the core constant) all contain the same id,
 *     and none carries a different release-form id;
 *  2. the `probe-log` feature is off: its only marker, the env name
 *     `GILDE_TMUX_LOG`, is absent from the binaries, and no build config turns
 *     the feature on;
 *  3. the debug capability is absent: the resolved capability set of the build
 *     (tauri-build's `gen/schemas/capabilities.json`) has no capability with
 *     the identifier of `capabilities/debug.json`, and `main.json` grants none
 *     of its permissions.
 * A release binary embeds no capability identifiers (Tauri keeps them only in
 * debug builds), so (3) is checked on the resolved set, not on the binary.
 */

import { RELEASE_ID_SCAN } from './buildid.mjs';

/** The env name only a `probe-log` shim reads (src-tauri/Cargo.toml, LP-10). */
export const PROBE_LOG_MARKERS = ['GILDE_TMUX_LOG'];

/** Whether a JS/HTML text holds `id` as a string literal ("…", '…' or `…`). */
export function hasQuotedLiteral(text, id) {
  return [`"${id}"`, `'${id}'`, `\`${id}\``].some((q) => text.includes(q));
}

/** Release-form ids in `text` that are not `expected` (nor a part of it). */
export function otherReleaseIds(text, expected) {
  const found = new Set(text.match(RELEASE_ID_SCAN) ?? []);
  return [...found].filter((m) => !m.includes(expected) && !expected.includes(m)).sort();
}

/**
 * One artifact against the expected id. `kind` is `js` (string literal
 * required) or `binary` (raw bytes). Returns `{ ok, detail }`.
 */
export function checkArtifactId(name, kind, buffers, expected) {
  const texts = buffers.map((b) => b.toString('latin1'));
  const present = kind === 'js' ? texts.some((t) => hasQuotedLiteral(t, expected)) : buffers.some((b) => b.includes(Buffer.from(expected, 'latin1')));
  const others = [...new Set(texts.flatMap((t) => otherReleaseIds(t, expected)))];
  if (!present && others.length) return { ok: false, detail: `${name}: carries ${others.join(', ')} instead of ${expected}` };
  if (!present) return { ok: false, detail: `${name}: build id ${expected} not found` };
  if (others.length) return { ok: false, detail: `${name}: carries ${expected} and also ${others.join(', ')}` };
  return { ok: true, detail: `${name}: build id ${expected}` };
}

/** `probe-log` markers present in a binary. */
export function probeLogMarkers(buffer) {
  return PROBE_LOG_MARKERS.filter((m) => buffer.includes(Buffer.from(m, 'latin1')));
}

/** Whether `[features] default = [...]` in a Cargo.toml text lists `feature`. */
export function cargoDefaultHas(cargoToml, feature) {
  const section = /^\[features\][^\n]*\n([\s\S]*?)(?=^\[|$(?![\s\S]))/m.exec(cargoToml);
  if (!section) return false;
  const def = /^default\s*=\s*\[([^\]]*)\]/m.exec(section[1]);
  if (!def) return false;
  return def[1].split(',').map((s) => s.trim().replace(/^"|"$/g, '')).includes(feature);
}

/** The features a tauri.conf.json asks cargo for (`build.features`), if any. */
export function tauriConfFeatures(conf) {
  const f = conf?.build?.features;
  return Array.isArray(f) ? f.filter((x) => typeof x === 'string') : [];
}

/** Capability identifiers in tauri-build's resolved file (object keyed by id, or an array). */
export function capabilityIds(resolved) {
  if (Array.isArray(resolved)) return resolved.map((c) => c?.identifier).filter((x) => typeof x === 'string');
  if (resolved && typeof resolved === 'object') {
    return Object.entries(resolved).map(([k, v]) => (v && typeof v === 'object' && typeof v.identifier === 'string' ? v.identifier : k));
  }
  return [];
}

/** Permission ids a capability file grants (strings or `{ identifier }` objects). */
export function grantedPermissions(capability) {
  const list = Array.isArray(capability?.permissions) ? capability.permissions : [];
  return list.map((p) => (typeof p === 'string' ? p : p?.identifier)).filter((x) => typeof x === 'string');
}
