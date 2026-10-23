/**
 * The one error shape on the WebView side (contract §4.4, A7.3).
 *
 * Rust rejects every command with an `IpcError`. Tauri itself rejects some
 * calls with a plain string before our code runs (malformed JSON, lone
 * surrogates, nesting deeper than 128, a command no capability grants), and a
 * missing runtime throws a `TypeError`. `toIpcError` turns every rejection that
 * is not an `IpcError` into `INTERNAL` with a fresh correlation id in `detail`.
 * The platform string itself is never shown and never logged — serde messages
 * can quote request content — only "platform rejection", the command name and
 * the string's length (§13.5, T-ipc-platformReject).
 */

import type { ErrorCode } from './gen/ErrorCode';
import type { IpcError } from './gen/IpcError';
import { newCorrelationId } from './ids';

export type { IpcError, ErrorCode };

export const ERROR_CODES: readonly ErrorCode[] = [
  'INVALID_ARGUMENT',
  'NOT_FOUND',
  'NOT_READY',
  'BUSY',
  'UNSUPPORTED',
  'REFUSED',
  'CONFLICT',
  'STALE_PROMPT',
  'SESSION_LIVE_ELSEWHERE',
  'PERMISSION_DENIED',
  'DISK_FULL',
  'SPAWN_FAILED',
  'GIT_FAILED',
  'VENDOR_ERROR',
  'SIDECAR_DOWN',
  'TIMEOUT',
  'VERSION_MISMATCH',
  'INTERNAL',
];

const CODES = new Set<string>(ERROR_CODES);

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/**
 * True only for the exact contract shape: a known code, a non-empty key, a
 * params object of strings/numbers/booleans, `detail` string or null, and a
 * boolean `retryable`.
 */
export function isIpcError(value: unknown): value is IpcError {
  if (!isPlainObject(value)) return false;
  const { code, key, params, detail, retryable } = value;
  if (typeof code !== 'string' || !CODES.has(code)) return false;
  if (typeof key !== 'string' || key === '') return false;
  if (!isPlainObject(params)) return false;
  for (const v of Object.values(params)) {
    if (typeof v !== 'string' && typeof v !== 'number' && typeof v !== 'boolean') return false;
  }
  if (detail !== null && detail !== undefined && typeof detail !== 'string') return false;
  return typeof retryable === 'boolean';
}

/** An `INTERNAL` error raised by the WebView itself. */
export function internalError(correlationId: string = newCorrelationId()): IpcError {
  return { code: 'INTERNAL', key: 'error.internal', params: {}, detail: correlationId, retryable: false };
}

/** What the log may say about a rejection that was not an `IpcError`: never its text. */
export interface PlatformRejectionLog {
  command: string;
  kind: 'string' | 'error' | 'other';
  length: number;
  correlationId: string;
}

let logSink: (message: string, fields: PlatformRejectionLog) => void = (message, fields) => {
  console.warn(message, fields);
};

/** Tests only: capture the platform-rejection log line; `null` restores console.warn. */
export function setRejectionLogForTests(sink: ((message: string, fields: PlatformRejectionLog) => void) | null): void {
  logSink =
    sink ??
    ((message, fields) => {
      console.warn(message, fields);
    });
}

function lengthOf(reason: unknown): { kind: PlatformRejectionLog['kind']; length: number } {
  if (typeof reason === 'string') return { kind: 'string', length: reason.length };
  if (reason instanceof Error) return { kind: 'error', length: reason.message.length };
  try {
    return { kind: 'other', length: String(JSON.stringify(reason) ?? '').length };
  } catch {
    return { kind: 'other', length: 0 };
  }
}

/** Normalize anything a command promise can reject with (contract A7.3). */
export function toIpcError(reason: unknown, command: string): IpcError {
  if (isIpcError(reason)) {
    return {
      code: reason.code,
      key: reason.key,
      params: { ...reason.params },
      detail: reason.detail ?? null,
      retryable: reason.retryable,
    };
  }
  const correlationId = newCorrelationId();
  logSink('[gilde] platform rejection', { command, ...lengthOf(reason), correlationId });
  return internalError(correlationId);
}

/** A wave-1 stub's refusal (`UNSUPPORTED{cap:"notBuilt"}`): screens degrade quietly on it. */
export function isNotBuilt(error: unknown): boolean {
  return isIpcError(error) && error.code === 'UNSUPPORTED' && error.params.cap === 'notBuilt';
}

/** The retry-after hint of a `BUSY`, if any. */
export function retryAfterMs(error: IpcError): number | null {
  const v = error.params.retryAfterMs;
  return typeof v === 'number' && Number.isFinite(v) && v >= 0 ? v : null;
}
