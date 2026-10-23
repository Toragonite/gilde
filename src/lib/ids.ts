/**
 * Ids the WebView mints (contract §3.2): idempotency keys for the commands that
 * start something (`clientRequestId`, `clientMsgId`, both lowercase UUIDv4) and
 * the per-view `ViewId` of a conversation subscription.
 */

import type { ClientMsgId } from './gen/ClientMsgId';
import type { ClientRequestId } from './gen/ClientRequestId';
import type { ViewId } from './gen/ViewId';

function randomBytes(n: number): Uint8Array {
  const out = new Uint8Array(n);
  globalThis.crypto.getRandomValues(out);
  return out;
}

function hex(bytes: Uint8Array): string {
  let s = '';
  for (const b of bytes) s += b.toString(16).padStart(2, '0');
  return s;
}

/** A lowercase RFC 4122 version-4 UUID. */
export function uuidV4(): string {
  const b = randomBytes(16);
  b[6] = ((b[6] ?? 0) & 0x0f) | 0x40;
  b[8] = ((b[8] ?? 0) & 0x3f) | 0x80;
  const h = hex(b);
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

export const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

/** For `session_start`, `session_continue`, `session_retry_as_terminal`, `account_sign_in`. */
export function newClientRequestId(): ClientRequestId {
  return uuidV4() as ClientRequestId;
}

/** For `turn_send`, `turn_retry`. */
export function newClientMsgId(): ClientMsgId {
  return uuidV4() as ClientMsgId;
}

/** `^[a-z0-9-]{1,32}$`: one per mounted conversation view. */
export function newViewId(prefix = 'v'): ViewId {
  const safe = prefix.toLowerCase().replace(/[^a-z0-9-]/g, '').slice(0, 15) || 'v';
  return `${safe}-${hex(randomBytes(8))}` as ViewId;
}

/** A correlation id for an error the WebView itself raises (`IpcError.detail` of INTERNAL). */
export function newCorrelationId(): string {
  return `ui-${hex(randomBytes(8))}`;
}
