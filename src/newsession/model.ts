/**
 * The New session sheet's rules as pure functions (ux-ia §3.12, §5.8;
 * contract §4.2.2, A7.5, A24.4, A26.2):
 *
 *  - which project the sheet opens on (the sheet's context first, then what
 *    the main view shows, then the first project);
 *  - what every chip resolves to: the user's pick when the current options
 *    still allow it, else the core's `defaults`, else the first option — so
 *    `⌘N ⏎` needs no choice at all;
 *  - the worktree slug (first five words of the prompt, else
 *    `<yyyymmdd>-<4 hex>`, contract §4.2.2 F-25);
 *  - the `SessionStartReq` built from all of it.
 *
 * No React, no store writes: the sheet calls these with what it holds.
 */

import { clockTime } from '../shell/words';
import type { Account } from '../lib/gen/Account';
import type { AccountId } from '../lib/gen/AccountId';
import type { AccountOption } from '../lib/gen/AccountOption';
import type { ClientRequestId } from '../lib/gen/ClientRequestId';
import type { ControlMode } from '../lib/gen/ControlMode';
import type { Locale } from '../lib/gen/Locale';
import type { LocationId } from '../lib/gen/LocationId';
import type { NewSessionOptions } from '../lib/gen/NewSessionOptions';
import type { Notice } from '../lib/gen/Notice';
import type { PermissionMode } from '../lib/gen/PermissionMode';
import type { Project } from '../lib/gen/Project';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { Quota } from '../lib/gen/Quota';
import type { SessionStartReq } from '../lib/gen/SessionStartReq';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { StartLocation } from '../lib/gen/StartLocation';
import type { Vendor } from '../lib/gen/Vendor';
import type { MainView } from '../store/uiSlice';
import type { WorldState } from '../store/worldSlice';
import { sessionOf } from '../store/worldSlice';

// ---------------------------------------------------------------------------
// Chips
// ---------------------------------------------------------------------------

/** The chips in ⌥1…⌥8 order (ux-ia §3.12 "Chip keys"). */
export const CHIP_ORDER = ['project', 'vendor', 'account', 'model', 'where', 'permissions', 'mode', 'team'] as const;
export type ChipId = (typeof CHIP_ORDER)[number];

/** `NewWorktree.slug` (contract §4.2.2). */
export const SLUG_RE = /^[a-z0-9][a-z0-9-]{0,62}$/;
/** `SessionStartReq.prompt` ≤ 256 KiB of UTF-8. */
export const PROMPT_MAX_BYTES = 256 * 1024;
/** An account at or above this share of any window is near its limit (ux-ia §5.8). */
export const NEAR_LIMIT_PCT = 90;

// ---------------------------------------------------------------------------
// Which project
// ---------------------------------------------------------------------------

/** Projects in the order the sidebar shows them: active before earlier, then the user order. */
export function orderedProjects(world: Pick<WorldState, 'projects'>): Project[] {
  return [...world.projects.values()].sort(
    (a, b) => Number(a.earlier) - Number(b.earlier) || a.order - b.order || a.name.localeCompare(b.name),
  );
}

/** The project of whatever the main view shows (a session, a team, the focused Split tile). */
export function projectOfView(world: WorldState, view: MainView, splitFocus: SessionUid | null): ProjectId | null {
  switch (view.screen) {
    case 'session':
      return sessionOf(world, view.uid)?.projectId ?? null;
    case 'team':
      return world.teams.get(view.teamKey)?.projectId ?? null;
    case 'split':
      return sessionOf(world, splitFocus)?.projectId ?? null;
    default:
      return null;
  }
}

/**
 * The project the sheet opens on (A26.2): the sheet's context (`+` on a
 * project, the palette's "New session in {project}"), else the project open in
 * Main, else the first project. A context naming an unknown project is still
 * honoured — a folder picked a moment ago is a project at once (A24.4) even
 * before its first patch arrives.
 */
export function preselectProject(
  world: WorldState,
  view: MainView,
  splitFocus: SessionUid | null,
  contextProject: ProjectId | null | undefined,
): ProjectId | null {
  if (contextProject) return contextProject;
  const fromView = projectOfView(world, view, splitFocus);
  if (fromView && world.projects.has(fromView)) return fromView;
  return orderedProjects(world)[0]?.id ?? null;
}

// ---------------------------------------------------------------------------
// Worktree slug
// ---------------------------------------------------------------------------

/** The slug of the prompt's first five words (ASCII letters and digits), or '' when none survive. */
export function slugOfPrompt(prompt: string): string {
  const words = prompt
    .normalize('NFKD')
    .replace(/[̀-ͯ]/g, '')
    .toLowerCase()
    .split(/\s+/)
    .filter((w) => w !== '')
    .slice(0, 5)
    .map((w) => w.replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, ''))
    .filter((w) => w !== '');
  let slug = words.join('-').replace(/-{2,}/g, '-');
  if (slug.length > 63) slug = slug.slice(0, 63);
  slug = slug.replace(/-+$/g, '');
  return SLUG_RE.test(slug) ? slug : '';
}

/** `<yyyymmdd>-<4 hex>`: the default when the prompt gives no slug (e.g. a Korean prompt) [F-25]. */
export function fallbackSlug(now: Date, hex4: string): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}-${hex4}`;
}

/** Four lowercase hex digits. */
export function randomHex4(random: () => number = Math.random): string {
  return Math.floor(random() * 0x10000)
    .toString(16)
    .padStart(4, '0');
}

/** The slug the chip shows and the start sends: the user's edit, else the prompt's, else the fallback. */
export function effectiveSlug(prompt: string, edited: string | null, fallback: string): string {
  if (edited !== null) return edited;
  return slugOfPrompt(prompt) || fallback;
}

// ---------------------------------------------------------------------------
// Resolving the chips
// ---------------------------------------------------------------------------

/** Where the session runs; a new worktree's slug is added at start. */
export type Where =
  | { kind: 'here'; locationId: LocationId | null }
  | { kind: 'newWorktree' }
  | { kind: 'existingWorktree'; locationId: LocationId };

/** What the user picked explicitly; anything unset follows the core's defaults. */
export interface Picks {
  vendor?: Vendor;
  accountId?: AccountId;
  /** `null` = the vendor's default model. */
  model?: string | null;
  controlMode?: ControlMode;
  permissionMode?: PermissionMode;
  where?: Where;
  team?: boolean;
}

export interface Resolved {
  vendor: Vendor | null;
  accounts: AccountOption[];
  accountId: AccountId | null;
  model: string | null;
  controlMode: ControlMode | null;
  modes: ControlMode[];
  permissionMode: PermissionMode | null;
  where: Where;
  team: boolean;
  teamAllowed: boolean;
}

function whereOf(location: StartLocation | null): Where {
  if (!location) return { kind: 'newWorktree' };
  switch (location.kind) {
    case 'here':
      return { kind: 'here', locationId: location.locationId };
    case 'newWorktree':
      return { kind: 'newWorktree' };
    case 'existingWorktree':
      return { kind: 'existingWorktree', locationId: location.locationId };
  }
}

function whereAllowed(where: Where, options: NewSessionOptions): boolean {
  if (where.kind === 'newWorktree') return true;
  if (where.kind === 'here' && where.locationId === null) return true;
  return options.locations.some(
    (l) =>
      l.location.kind === where.kind &&
      'locationId' in l.location &&
      l.location.locationId === where.locationId,
  );
}

/** The installed vendors, in the order the core lists them. */
export function installedVendors(options: NewSessionOptions | null): Vendor[] {
  return options ? options.vendors.filter((v) => v.installed).map((v) => v.vendor) : [];
}

/**
 * Every chip's value: a pick the options still allow, else the default, else
 * the first option. Team forces Terminal (a team needs `vendor = claude ∧
 * mode = Terminal`, contract §4.2.2).
 */
export function resolve(options: NewSessionOptions | null, picks: Picks): Resolved {
  if (!options) {
    return {
      vendor: picks.vendor ?? null,
      accounts: [],
      accountId: null,
      model: picks.model ?? null,
      controlMode: null,
      modes: [],
      permissionMode: null,
      where: picks.where ?? { kind: 'here', locationId: null },
      team: false,
      teamAllowed: false,
    };
  }
  const installed = installedVendors(options);
  const vendor =
    (picks.vendor && installed.includes(picks.vendor) ? picks.vendor : null) ??
    (options.defaults.vendor && (installed.length === 0 || installed.includes(options.defaults.vendor)) ? options.defaults.vendor : null) ??
    installed[0] ??
    null;

  const accounts = vendor ? options.accounts.filter((a) => a.vendor === vendor) : [];
  const hasAccount = (id: AccountId | null | undefined) => !!id && accounts.some((a) => a.accountId === id);
  const accountId =
    (hasAccount(picks.accountId) ? (picks.accountId as AccountId) : null) ??
    (hasAccount(options.defaults.accountId) ? options.defaults.accountId : null) ??
    accounts.find((a) => a.isDefault)?.accountId ??
    accounts[0]?.accountId ??
    null;

  const modelIds = options.models.map((m) => m.id);
  let model: string | null;
  if (picks.model !== undefined && (picks.model === null || modelIds.includes(picks.model))) model = picks.model;
  else if (options.defaults.model && modelIds.includes(options.defaults.model)) model = options.defaults.model;
  else model = null;

  const teamAllowed = vendor === 'claude' && options.teamAllowed && options.modes.includes('terminal');
  const team = teamAllowed && picks.team === true;

  const modes = options.modes;
  let controlMode: ControlMode | null;
  if (team) controlMode = 'terminal';
  else if (picks.controlMode && modes.includes(picks.controlMode)) controlMode = picks.controlMode;
  else if (options.defaults.controlMode && modes.includes(options.defaults.controlMode)) controlMode = options.defaults.controlMode;
  else controlMode = modes[0] ?? null;

  const perms = options.permissionModes;
  let permissionMode: PermissionMode | null;
  if (picks.permissionMode && perms.includes(picks.permissionMode)) permissionMode = picks.permissionMode;
  else if (options.defaults.permissionMode && perms.includes(options.defaults.permissionMode)) permissionMode = options.defaults.permissionMode;
  else permissionMode = perms[0] ?? null;

  const fallbackWhere = whereOf(options.defaults.location);
  const where = picks.where && whereAllowed(picks.where, options) ? picks.where : whereAllowed(fallbackWhere, options) ? fallbackWhere : { kind: 'here' as const, locationId: null };

  return { vendor, accounts, accountId, model, controlMode, modes, permissionMode, where, team, teamAllowed };
}

/** Picks that belong to one vendor are dropped when the vendor changes. */
export function picksForVendor(picks: Picks, vendor: Vendor): Picks {
  return { vendor, where: picks.where, team: vendor === 'claude' ? picks.team : false };
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

/** Why the sheet cannot start yet (each has a line), or null when it can. */
export type Blocker = 'noProject' | 'loading' | 'noAgents' | 'noAccount' | 'slugInvalid' | 'promptTooLong';

export function utf8Bytes(text: string): number {
  return new TextEncoder().encode(text).length;
}

export function blockerOf(
  projectId: ProjectId | null,
  options: NewSessionOptions | null,
  r: Resolved,
  slug: string,
  prompt: string,
): Blocker | null {
  if (!projectId) return 'noProject';
  if (!options) return 'loading';
  if (!r.vendor || !r.controlMode) return 'noAgents';
  if (!r.accountId) return 'noAccount';
  if (r.where.kind === 'newWorktree' && !SLUG_RE.test(slug)) return 'slugInvalid';
  if (utf8Bytes(prompt) > PROMPT_MAX_BYTES) return 'promptTooLong';
  return null;
}

/** The location the start request carries. */
export function startLocation(where: Where, slug: string): StartLocation {
  switch (where.kind) {
    case 'here':
      return { kind: 'here', locationId: where.locationId };
    case 'existingWorktree':
      return { kind: 'existingWorktree', locationId: where.locationId };
    case 'newWorktree':
      return { kind: 'newWorktree', slug, base: null };
  }
}

/**
 * The request (contract §4.2.2). A prompt of only whitespace is "no first
 * message"; any other prompt is sent as typed.
 */
export function buildStartReq(
  clientRequestId: ClientRequestId,
  projectId: ProjectId,
  r: Resolved,
  slug: string,
  prompt: string,
): SessionStartReq {
  return {
    clientRequestId,
    projectId,
    vendor: r.vendor as Vendor,
    accountId: r.accountId as AccountId,
    model: r.model,
    location: startLocation(r.where, slug),
    permissionMode: r.permissionMode,
    controlMode: r.controlMode as ControlMode,
    team: r.team,
    prompt: prompt.trim() === '' ? null : prompt,
  };
}

// ---------------------------------------------------------------------------
// Accounts and notices
// ---------------------------------------------------------------------------

/** The highest known share of any window, or null when none is known. */
export function maxUsedPct(quota: Quota | null | undefined): number | null {
  if (!quota) return null;
  let max: number | null = null;
  for (const w of quota.windows) {
    if (w.usedPct === null || !Number.isFinite(w.usedPct) || w.usedPct < 0 || w.usedPct > 100) continue;
    max = max === null ? w.usedPct : Math.max(max, w.usedPct);
  }
  return max;
}

/**
 * A notice's parameters ready for `td`: the core sends reset times as epoch
 * milliseconds (`accounts.allNear {time}`), which the sentence shows as a
 * clock time, never as a raw number.
 */
export function noticeParams(notice: Notice, locale: Locale): Record<string, string | number | boolean> {
  const out: Record<string, string | number | boolean> = { ...notice.params };
  for (const [k, v] of Object.entries(out)) {
    if ((k === 'time' || k === 'resetsAt') && typeof v === 'number' && Number.isFinite(v) && v > 1e11) {
      out[k] = clockTime(v, locale);
    }
  }
  return out;
}

/**
 * The account a `accounts.switched {a, b}` line switched away from, so the
 * line's one-click undo can pick it again; null when the label is not unique.
 */
export function switchedFrom(notice: Notice, accounts: readonly AccountOption[]): AccountId | null {
  if (notice.key !== 'accounts.switched') return null;
  const label = notice.params.a;
  if (typeof label !== 'string') return null;
  const matches = accounts.filter((a) => a.label === label);
  return matches.length === 1 ? (matches[0] as AccountOption).accountId : null;
}

/** The store's account for an option (quota and sign-in come from the store, A24.1/A24.8). */
export function storeAccount(world: Pick<WorldState, 'accounts'>, id: AccountId | null): Account | null {
  return id ? (world.accounts.get(id) ?? null) : null;
}
