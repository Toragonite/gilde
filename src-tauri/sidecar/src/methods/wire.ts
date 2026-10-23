/**
 * methods/wire.ts — protocol 2 of the sidecar channel (contract §4.5): the
 * constants, the vendor name mapping, and the strict by-value readers of every
 * inbound params object.
 *
 * The types are the generated ones (`src/gen/**`, contract §3.1 — hand-written
 * mirrors are forbidden). What this module adds is the RUNTIME half: "both ends
 * reject unknown fields", so every reader here refuses a params object with a
 * key it does not know, a missing required key, or a value outside its rule —
 * with `-32602` and a `data.field` naming the offender, never a guess.
 */

import * as fs from 'node:fs';
import * as path from 'node:path';

import type { AccountEnv } from '../gen/AccountEnv';
import type { MeshConfigureParams } from '../gen/MeshConfigureParams';
import type { MeshHelloParams } from '../gen/MeshHelloParams';
import type { PermissionMode } from '../gen/PermissionMode';
import type { ProbeParams } from '../gen/ProbeParams';
import type { Providers } from '../gen/Providers';
import type { TurnCancelParams } from '../gen/TurnCancelParams';
import type { TurnStartParams } from '../gen/TurnStartParams';
import type { Vendor } from '../gen/Vendor';
import { RpcError, RPC_INVALID_PARAMS } from '../rpc';

/** The protocol number this build speaks (contract §4.5). */
export const PROTOCOL = 2;
/** The sidecar exits with this code when `mesh.hello` carries another build id. */
export const EXIT_BUILD_MISMATCH = 78;

/** `mesh.hello` protocol mismatch (a protocol-1 peer is stopped by the host). */
export const RPC_PROTOCOL_MISMATCH = -32002;
/** `mesh.hello` build id mismatch: answered, then the process exits 78. */
export const RPC_BUILD_MISMATCH = -32005;
/** The adapter (or the agent it drives) failed; `data.class` is the vendor error class. */
export const RPC_ADAPTER_ERROR = -32001;
/** A method that needs `mesh.hello` (or `mesh.configure`) first. */
export const RPC_NOT_READY = -32006;

/** The four vendors of the contract, in roster order. */
export const VENDORS: readonly Vendor[] = ['claude', 'codex', 'cursor', 'agy'];

/** agent-mesh's own vendor id of a contract vendor (its "gemini" adapter is agy). */
export function meshVendorOf(vendor: Vendor): string {
  return vendor === 'agy' ? 'gemini' : vendor;
}

/** The contract vendor of an agent-mesh vendor id, or `null` for one we do not ship. */
export function contractVendorOf(meshVendor: string): Vendor | null {
  switch (meshVendor) {
    case 'claude':
    case 'codex':
    case 'cursor':
      return meshVendor;
    case 'gemini':
    case 'agy':
      return 'agy';
    default:
      return null;
  }
}

/** The account env variable of a vendor (contract §6.0 account variable rule, C8). */
export const ACCOUNT_ENV_KEYS: Readonly<Record<Vendor, string | null>> = {
  claude: 'CLAUDE_CONFIG_DIR',
  codex: 'CODEX_HOME',
  cursor: null,
  agy: null,
};

const PERMISSION_MODES: readonly PermissionMode[] = [
  'default',
  'acceptEdits',
  'plan',
  'dontAsk',
  'auto',
  'bypassPermissions',
];

// ── the by-value readers ─────────────────────────────────────────────────────

export function invalid(field: string, rule: string, message?: string): RpcError {
  return new RpcError(RPC_INVALID_PARAMS, message ?? `${field}: ${rule}`, { field, rule });
}

export function isRecord(x: unknown): x is Record<string, unknown> {
  return typeof x === 'object' && x !== null && !Array.isArray(x);
}

/** `raw` is an object whose keys are exactly a subset of `allowed`, with every `required` present. */
export function exactObject(
  raw: unknown,
  field: string,
  allowed: readonly string[],
  required: readonly string[] = allowed,
): Record<string, unknown> {
  if (!isRecord(raw)) throw invalid(field, 'shape', `${field} must be an object`);
  for (const key of Object.keys(raw)) {
    if (!allowed.includes(key)) throw invalid(field === '$' ? key : `${field}.${key}`, 'unknown', `unknown field ${key}`);
  }
  for (const key of required) {
    if (!(key in raw)) throw invalid(field === '$' ? key : `${field}.${key}`, 'required', `missing field ${key}`);
  }
  return raw;
}

/** A string of 1..=`max` UTF-16 units with no NUL and no lone surrogate. */
export function text(value: unknown, field: string, max: number, opts: { allowEmpty?: boolean } = {}): string {
  if (typeof value !== 'string') throw invalid(field, 'type', `${field} must be a string`);
  if (!opts.allowEmpty && value.length === 0) throw invalid(field, 'required', `${field} must not be empty`);
  if (value.length > max) throw invalid(field, 'maxLength', `${field} is longer than ${max}`);
  if (value.includes('\u0000')) throw invalid(field, 'nul', `${field} contains NUL`);
  if (!isWellFormed(value)) throw invalid(field, 'shape', `${field} is not well-formed UTF-16`);
  return value;
}

/** Same as {@link text}, but `null` passes through and `""` reads as absent (contract §3.1 rule 4). */
export function optText(value: unknown, field: string, max: number): string | null {
  if (value === null) return null;
  if (value === '') return null;
  return text(value, field, max);
}

/** A printable-ASCII id (no space), 1..=`max` bytes. */
export function asciiId(value: unknown, field: string, max = 128): string {
  const s = text(value, field, max);
  if (!/^[\x21-\x7e]+$/.test(s)) throw invalid(field, 'pattern', `${field} must be printable ASCII without spaces`);
  return s;
}

export function bool(value: unknown, field: string): boolean {
  if (typeof value !== 'boolean') throw invalid(field, 'type', `${field} must be a boolean`);
  return value;
}

export function vendorOf(value: unknown, field: string): Vendor {
  if (typeof value !== 'string' || !(VENDORS as readonly string[]).includes(value)) {
    throw invalid(field, 'enum', `${field} must be one of ${VENDORS.join(', ')}`);
  }
  return value as Vendor;
}

export function permissionModeOf(value: unknown, field: string): PermissionMode | null {
  if (value === null) return null;
  if (typeof value !== 'string' || !(PERMISSION_MODES as readonly string[]).includes(value)) {
    throw invalid(field, 'enum', `${field} must be a permission mode`);
  }
  return value as PermissionMode;
}

/** `^a_[0-9a-f]{16}$` (contract §3.2). */
export function accountIdOf(value: unknown, field: string): string {
  if (typeof value !== 'string' || !/^a_[0-9a-f]{16}$/.test(value)) {
    throw invalid(field, 'pattern', `${field} must be an account id (a_<16 hex>)`);
  }
  return value;
}

/** `^s_<UUIDv7>$` (contract §3.2). */
export function sessionUidOf(value: unknown, field: string): string {
  if (
    typeof value !== 'string' ||
    !/^s_[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value)
  ) {
    throw invalid(field, 'pattern', `${field} must be a session uid`);
  }
  return value;
}

/** An absolute, canonical UTF-8 path ≤ 1024 bytes, no NUL, no trailing `/`, not `/` (§3.4.1). */
export function absPathOf(value: unknown, field: string): string {
  const s = text(value, field, 1024);
  if (Buffer.byteLength(s, 'utf8') > 1024) throw invalid(field, 'maxLength', `${field} is longer than 1024 bytes`);
  if (!path.isAbsolute(s) || s === '/' || s.endsWith('/') || path.normalize(s) !== s) {
    throw invalid(field, 'path', `${field} must be an absolute canonical path`);
  }
  return s;
}

/** A positive safe integer, or `null`. */
export function optPositiveInt(value: unknown, field: string, max = Number.MAX_SAFE_INTEGER): number | null {
  if (value === null) return null;
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value <= 0 || value > max) {
    throw invalid(field, 'range', `${field} must be an integer in 1..=${max}`);
  }
  return value;
}

function isWellFormed(s: string): boolean {
  for (let i = 0; i < s.length; i += 1) {
    const c = s.charCodeAt(i);
    if (c >= 0xd800 && c <= 0xdbff) {
      const next = s.charCodeAt(i + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) return false;
      i += 1;
    } else if (c >= 0xdc00 && c <= 0xdfff) {
      return false;
    }
  }
  return true;
}

// ── the params of each method ────────────────────────────────────────────────

export function providersOf(raw: unknown, field: string): Providers {
  const p = exactObject(raw, field, ['orchestrator']);
  return { orchestrator: bool(p.orchestrator, `${field}.orchestrator`) };
}

/**
 * `mesh.hello` params. The protocol is checked by the caller (a protocol-1 peer
 * gets its own error, not "unknown field"), so `protocol` is read first and alone.
 */
export function helloParamsOf(raw: unknown): MeshHelloParams {
  const p = exactObject(raw, '$', ['protocol', 'buildId', 'appInstanceId', 'providers']);
  return {
    protocol: typeof p.protocol === 'number' ? p.protocol : Number.NaN,
    buildId: text(p.buildId, 'buildId', 64),
    appInstanceId: asciiId(p.appInstanceId, 'appInstanceId', 96),
    providers: providersOf(p.providers, 'providers'),
  };
}

export function accountEnvOf(raw: unknown, field: string): AccountEnv {
  const p = exactObject(raw, field, ['accountId', 'vendor', 'envKey', 'configDir']);
  const vendor = vendorOf(p.vendor, `${field}.vendor`);
  const envKey = p.envKey === null ? null : text(p.envKey, `${field}.envKey`, 64);
  const configDir = p.configDir === null ? null : absPathOf(p.configDir, `${field}.configDir`);
  // The account variable rule (§6.0, C8): only claude/codex carry one, it is
  // exactly the vendor's variable, and it always comes with a directory.
  if (envKey !== null && envKey !== ACCOUNT_ENV_KEYS[vendor]) {
    throw invalid(`${field}.envKey`, 'enum', `${field}.envKey must be ${String(ACCOUNT_ENV_KEYS[vendor])} for ${vendor}`);
  }
  if (envKey !== null && configDir === null) {
    throw invalid(`${field}.configDir`, 'required', `${field}.configDir is required with an envKey`);
  }
  return { accountId: accountIdOf(p.accountId, `${field}.accountId`) as AccountEnv['accountId'], vendor, envKey, configDir };
}

export function configureParamsOf(raw: unknown): MeshConfigureParams {
  const p = exactObject(raw, '$', ['binaries', 'accounts', 'providers']);
  const binariesRaw = p.binaries;
  if (!isRecord(binariesRaw)) throw invalid('binaries', 'shape', 'binaries must be an object');
  const binaries: MeshConfigureParams['binaries'] = {};
  for (const [key, value] of Object.entries(binariesRaw)) {
    const vendor = vendorOf(key, `binaries.${key}`);
    binaries[vendor] = absPathOf(value, `binaries.${key}`);
  }
  if (!Array.isArray(p.accounts)) throw invalid('accounts', 'shape', 'accounts must be an array');
  if (p.accounts.length > 1000) throw invalid('accounts', 'maxItems', 'accounts has more than 1000 items');
  const accounts = p.accounts.map((a, i) => accountEnvOf(a, `accounts[${i}]`));
  const seen = new Set<string>();
  for (const a of accounts) {
    if (seen.has(a.accountId)) throw invalid('accounts', 'unique', `account ${a.accountId} is listed twice`);
    seen.add(a.accountId);
  }
  return { binaries, accounts, providers: providersOf(p.providers, 'providers') };
}

export function probeParamsOf(raw: unknown): ProbeParams {
  const p = exactObject(raw, '$', ['vendor', 'accountId']);
  return {
    vendor: vendorOf(p.vendor, 'vendor'),
    accountId: accountIdOf(p.accountId, 'accountId') as ProbeParams['accountId'],
  };
}

/** Prompt cap of `turn_send` (§4.2.3): 256 KiB of UTF-8. */
export const PROMPT_MAX_BYTES = 256 * 1024;

export function turnStartParamsOf(raw: unknown): TurnStartParams {
  const p = exactObject(raw, '$', [
    'turnId',
    'sessionUid',
    'vendor',
    'accountId',
    'cwd',
    'model',
    'permissionMode',
    'prompt',
    'resumeId',
    'timeoutMs',
  ]);
  const prompt = text(p.prompt, 'prompt', PROMPT_MAX_BYTES);
  if (Buffer.byteLength(prompt, 'utf8') > PROMPT_MAX_BYTES) {
    throw invalid('prompt', 'maxLength', 'prompt is longer than 256 KiB');
  }
  const model = optText(p.model, 'model', 128);
  if (model !== null && !/^[A-Za-z0-9._:[\]/-]{1,128}$/.test(model)) {
    throw invalid('model', 'pattern', 'model must match ^[A-Za-z0-9._:\\[\\]/-]{1,128}$');
  }
  const resumeId = optText(p.resumeId, 'resumeId', 128);
  // A blank or `-`-leading id would read as a flag (agent-mesh throws on it too).
  if (resumeId !== null && !/^[0-9A-Za-z][0-9A-Za-z._-]{0,127}$/.test(resumeId)) {
    throw invalid('resumeId', 'pattern', 'resumeId must be a vendor conversation id');
  }
  return {
    turnId: asciiId(p.turnId, 'turnId'),
    sessionUid: sessionUidOf(p.sessionUid, 'sessionUid') as TurnStartParams['sessionUid'],
    vendor: vendorOf(p.vendor, 'vendor'),
    accountId: accountIdOf(p.accountId, 'accountId') as TurnStartParams['accountId'],
    cwd: absPathOf(p.cwd, 'cwd'),
    model,
    permissionMode: permissionModeOf(p.permissionMode, 'permissionMode'),
    prompt,
    resumeId,
    timeoutMs: optPositiveInt(p.timeoutMs, 'timeoutMs', 24 * 60 * 60_000),
  };
}

export function turnCancelParamsOf(raw: unknown): TurnCancelParams {
  const p = exactObject(raw, '$', ['turnId']);
  return { turnId: asciiId(p.turnId, 'turnId') };
}

/**
 * The cwd of a turn must be a directory that exists right now: the sidecar never
 * lets an adapter inherit its own cwd.
 */
export function requireDirectory(dir: string, field: string): string {
  let stat: fs.Stats;
  try {
    stat = fs.statSync(dir);
  } catch {
    throw invalid(field, 'path', `${field} does not exist`);
  }
  if (!stat.isDirectory()) throw invalid(field, 'path', `${field} is not a directory`);
  return dir;
}
