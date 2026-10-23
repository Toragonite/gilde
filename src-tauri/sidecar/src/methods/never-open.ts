/**
 * methods/never-open.ts — the sidecar half of contract §9.7 / C6: Gilde never
 * opens a credential-bearing file, never reads the keychain, and handles no
 * credentials. agent-mesh's adapters would do all three for introspection
 * (claude reads `.claude.json`, codex `auth.json`, cursor its keychain item and
 * login file, then a vendor usage endpoint with that token), so every adapter is
 * built with the three seams below instead of its defaults:
 *
 *   - {@link guardedReadTextFile} — the adapters' `readTextFile` seam. A path the
 *     never-open filter names is refused BEFORE `open` (resolves `null`, which the
 *     adapters read as "no such file"), and the refusal is recorded in the
 *     current {@link readGuard} scope so a probe can report `unknown` instead of
 *     the "logged out" an adapter infers from a missing file. Any other path is
 *     read as the vendor default would (`fs.promises.readFile`, `null` on error).
 *   - {@link refusedKeychain} — cursor's `readKeychain` seam: never spawns
 *     `/usr/bin/security`; answers "refused", which arms the adapter's own
 *     backoff so it does not ask again for hours.
 *   - {@link noNetworkFetch} — codex/cursor's `fetchFn` seam: the usage endpoint
 *     needs a token Gilde never reads, so no request is ever sent (and a test can
 *     never reach a vendor endpoint).
 */

import { AsyncLocalStorage } from 'node:async_hooks';
import * as fs from 'node:fs';
import * as path from 'node:path';

/**
 * Whether `p` names a file Gilde must never open (contract §9.7): the session
 * keys and daemon key, IDE lock files, `.credentials.json` / `.claude.json` in
 * any directory (existence by `stat` only), codex/cursor `auth.json`,
 * `oauth_creds.json`, `antigravity-oauth-token*`, cursor chat stores, any
 * `*.key` / `*.pem` / `*token*` / `*credential*` / `*oauth*` name, the
 * keychain databases, and `history.jsonl` (content).
 */
export function isNeverOpen(p: string): boolean {
  if (typeof p !== 'string' || p.length === 0) return true;
  const base = path.basename(p).toLowerCase();
  if (base === '.credentials.json' || base === '.claude.json') return true;
  if (base === 'auth.json' || base === 'oauth_creds.json' || base === 'history.jsonl') return true;
  if (base.endsWith('.key') || base.endsWith('.pem') || base.endsWith('.lock')) return true;
  if (base.includes('token') || base.includes('credential') || base.includes('oauth')) return true;
  if (base.endsWith('.keychain') || base.endsWith('.keychain-db')) return true;
  if (base === 'store.db' && p.split(path.sep).includes('chats')) return true;
  return false;
}

/**
 * What one probe scope saw. An adapter turns every one of these into the same
 * "logged out" / "no models" / "no quota" answer, so the probe needs them to
 * report `unknown` instead of a negative it does not know.
 */
export interface ReadGuardState {
  /** A read the never-open filter refused (or the keychain was asked for). */
  refused: boolean;
  /** A spawn was refused by the host (allowlist) or failed to start. */
  spawnFailed: boolean;
  /** A child hit its wall clock. */
  timedOut: boolean;
}

/** The scope a probe runs in. */
export const readGuard = new AsyncLocalStorage<ReadGuardState>();

/** Run `fn` in a fresh probe scope; resolves its value and what the scope saw. */
export async function withReadGuard<T>(fn: () => Promise<T>): Promise<{ value: T } & ReadGuardState> {
  const state: ReadGuardState = { refused: false, spawnFailed: false, timedOut: false };
  const value = await readGuard.run(state, fn);
  return { value, ...state };
}

/** Called by the spawn seam when a registration is refused or a spawn fails. */
export function noteSpawnFailure(): void {
  const scope = readGuard.getStore();
  if (scope !== undefined) scope.spawnFailed = true;
}

/** Called by the spawn seam when a child hits its wall clock. */
export function noteTimeout(): void {
  const scope = readGuard.getStore();
  if (scope !== undefined) scope.timedOut = true;
}

/** Counts every refusal (diagnostics; never a path). */
export const neverOpenStats = { refused: 0 };

/** The adapters' `readTextFile` seam (see the module header). Never throws. */
export async function guardedReadTextFile(p: string): Promise<string | null> {
  if (isNeverOpen(p)) {
    neverOpenStats.refused += 1;
    const scope = readGuard.getStore();
    if (scope !== undefined) scope.refused = true;
    return null;
  }
  try {
    return await fs.promises.readFile(p, 'utf8');
  } catch {
    return null;
  }
}

/** cursor's `readKeychain` seam: the keychain is never read (C6). */
export async function refusedKeychain(): Promise<{ found: false; refused: true }> {
  const scope = readGuard.getStore();
  if (scope !== undefined) scope.refused = true;
  return { found: false, refused: true };
}

/** codex/cursor's `fetchFn` seam: no vendor endpoint is ever called (no token is ever read). */
export const noNetworkFetch: typeof fetch = async () => {
  throw new Error('network access is disabled in the Gilde sidecar');
};
