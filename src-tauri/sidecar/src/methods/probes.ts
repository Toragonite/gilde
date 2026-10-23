/**
 * methods/probes.ts — `probe.authStatus` / `probe.models` / `probe.quota`
 * (contract §4.5): agent-mesh's `authStatus` / `availableModels` / `quota` for
 * one configured account, converted to the generated contract shapes. Never a
 * token, never a credential file (methods/never-open.ts), never a vendor endpoint.
 *
 * Every probe runs in a probe scope (`withReadGuard`) and under the registry
 * kind `probe`. The adapters answer "logged out" / "no quota" for ANY failure —
 * a missing file, a refused read, a spawn the host refused, a timeout — so the
 * scope is what turns the cases where we simply do not know into `unknown` /
 * `null` instead of a negative:
 *
 *   - sign-in: `loggedIn: true` → `signedIn`; `false` with nothing refused, failed
 *     or timed out → `signedOut`; otherwise `unknown`. Claude and codex keep their
 *     login state in files Gilde never opens (`.claude.json`, `auth.json`, C6),
 *     so their probe answers `unknown` unless an API key variable is set — their
 *     sign-in comes from the live session (claude: `initialize`'s `tokenSource`,
 *     T05) and from a turn's `auth` error.
 *   - quota: `null` when the adapter has none or could not read it; percentages
 *     outside 0..=100 are absent, never clamped (§3.4.3, A §13.3).
 */

import type { Account, Adapter, ModelInfo as MeshModelInfo, QuotaSnapshot } from '@toragonite/agent-mesh';

import { runCtx } from '../exec-registered';
import type { SidecarState } from '../fleet';
import type { AuthStatus } from '../gen/AuthStatus';
import type { ModelInfo } from '../gen/ModelInfo';
import type { ProbeParams } from '../gen/ProbeParams';
import type { Quota } from '../gen/Quota';
import type { QuotaWindow } from '../gen/QuotaWindow';
import type { QuotaWindowKind } from '../gen/QuotaWindowKind';
import type { Vendor } from '../gen/Vendor';
import { RpcError, RPC_UNSUPPORTED } from '../rpc';
import type { Rpc } from '../rpc';
import { withReadGuard } from './never-open';
import { meshAccountOf } from './turns';
import { RPC_NOT_READY, probeParamsOf } from './wire';

/** At most this many models are reported. */
export const MODELS_MAX = 256;
/** At most this many quota windows are reported. */
export const QUOTA_WINDOWS_MAX = 16;

const MODEL_ID = /^[A-Za-z0-9._:[\]/-]{1,128}$/;

function cleanLabel(value: unknown): string | null {
  if (typeof value !== 'string') return null;
  // eslint-disable-next-line no-control-regex
  const s = value.replace(/[\u0000-\u001f\u007f\ud800-\udfff]+/g, ' ').replace(/\s+/g, ' ').trim();
  return s.length === 0 ? null : s.slice(0, 120);
}

/** agent-mesh models → contract `ModelInfo[]`: ids by value, duplicates and junk dropped. */
export function modelsOf(models: readonly MeshModelInfo[] | null | undefined): ModelInfo[] {
  const out: ModelInfo[] = [];
  const seen = new Set<string>();
  for (const m of models ?? []) {
    if (out.length >= MODELS_MAX) break;
    if (typeof m !== 'object' || m === null || typeof m.id !== 'string' || !MODEL_ID.test(m.id)) continue;
    if (seen.has(m.id)) continue;
    seen.add(m.id);
    out.push({ id: m.id, label: cleanLabel(m.label), isDefault: m.default === true });
  }
  return out;
}

/**
 * A window kind from its label (agent-mesh labels: "5h", "weekly", "primary",
 * agy's `gemini-5h` / `gemini-weekly`, …). Anything not clearly one of the two
 * rolling windows is `other`.
 */
export function windowKindOf(label: string): QuotaWindowKind {
  if (/(?:^|[^0-9])5\s*h|five[ _-]?hour|session/i.test(label)) return 'fiveHour';
  if (/week|weekly|7\s*d|seven[ _-]?day/i.test(label)) return 'week';
  return 'other';
}

/** 0..=100 → rounded integer; anything else (NaN, < 0, > 100, a fraction > 1 read as %) → absent. */
export function usedPctOf(raw: unknown): number | null {
  if (typeof raw !== 'number' || !Number.isFinite(raw)) return null;
  if (raw < 0 || raw > 100) return null;
  return Math.round(raw);
}

/** An ISO 8601 instant → epoch ms, or absent. */
export function resetsAtOf(raw: unknown): number | null {
  if (typeof raw !== 'string' || raw.length === 0 || raw.length > 64) return null;
  const ms = Date.parse(raw);
  return Number.isFinite(ms) && ms > 0 && ms <= Number.MAX_SAFE_INTEGER ? ms : null;
}

/** agent-mesh `QuotaSnapshot | null` → contract `Quota | null`. */
export function quotaOf(snapshot: QuotaSnapshot | null | undefined, fetchedAt: number): Quota | null {
  if (snapshot === null || snapshot === undefined || typeof snapshot !== 'object') return null;
  const windows: QuotaWindow[] = [];
  for (const w of Array.isArray(snapshot.windows) ? snapshot.windows : []) {
    if (windows.length >= QUOTA_WINDOWS_MAX) break;
    if (typeof w !== 'object' || w === null) continue;
    const label = cleanLabel(w.label);
    windows.push({
      kind: label === null ? 'other' : windowKindOf(label),
      label,
      usedPct: usedPctOf(w.usedPercent),
      resetsAt: resetsAtOf(w.resetAt),
    });
  }
  return { windows, fetchedAt, stale: false, unavailable: null };
}

/** The probe's adapter and account, or the typed refusal. */
function target(state: SidecarState, p: ProbeParams): { adapter: Adapter; account: Account | undefined } {
  if (!state.built.configured.has(p.vendor)) {
    throw new RpcError(RPC_UNSUPPORTED, `${p.vendor} is not installed`, { cap: 'vendorNotInstalled' });
  }
  const adapter = state.built.adapters.get(p.vendor);
  if (adapter === undefined) {
    throw new RpcError(RPC_UNSUPPORTED, `${p.vendor} has no adapter`, { cap: 'vendorNotInstalled' });
  }
  return { adapter, account: meshAccountOf(state, p.accountId, p.vendor) };
}

function inProbe<T>(vendor: Vendor, fn: () => Promise<T>): Promise<T> {
  return runCtx.run({ turnId: null, vendor, kind: 'probe' }, fn);
}

function requireState(get: () => SidecarState | undefined): SidecarState {
  const state = get();
  if (state === undefined || !state.configured) {
    throw new RpcError(RPC_NOT_READY, 'mesh.hello and mesh.configure must come first', { name: 'NotReady' });
  }
  return state;
}

export function registerProbeMethods(rpc: Rpc, getState: () => SidecarState | undefined): void {
  rpc.on('probe.authStatus', async (raw: unknown): Promise<AuthStatus> => {
    const p = probeParamsOf(raw);
    const { adapter, account } = target(requireState(getState), p);
    try {
      const seen = await inProbe(p.vendor, () => withReadGuard(() => adapter.authStatus(account)));
      if (seen.value?.loggedIn === true) return { signIn: 'signedIn' };
      if (seen.refused || seen.spawnFailed || seen.timedOut) return { signIn: 'unknown' };
      return { signIn: seen.value?.loggedIn === false ? 'signedOut' : 'unknown' };
    } catch {
      return { signIn: 'unknown' };
    }
  });

  rpc.on('probe.models', async (raw: unknown): Promise<ModelInfo[]> => {
    const p = probeParamsOf(raw);
    const { adapter, account } = target(requireState(getState), p);
    try {
      const seen = await inProbe(p.vendor, () => withReadGuard(() => adapter.availableModels(account)));
      return modelsOf(seen.value);
    } catch {
      return [];
    }
  });

  rpc.on('probe.quota', async (raw: unknown): Promise<Quota | null> => {
    const p = probeParamsOf(raw);
    const { adapter, account } = target(requireState(getState), p);
    try {
      const seen = await inProbe(p.vendor, () => withReadGuard(() => adapter.quota(account)));
      return quotaOf(seen.value, Date.now());
    } catch {
      return null;
    }
  });
}
