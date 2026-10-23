/**
 * Deterministic ids for the showcase world (contract §3.2 formats).
 *
 * The core derives ids from sha256 of real paths; the showcase only needs ids
 * of the right shape that are stable across reloads, so a page, a screenshot
 * and a gate run always see the same keys. FNV-1a over a seed string, twice
 * with different offsets, gives the 16 hex digits.
 */

import type { AccountId } from '../lib/gen/AccountId';
import type { BlockId } from '../lib/gen/BlockId';
import type { EpisodeId } from '../lib/gen/EpisodeId';
import type { LocationId } from '../lib/gen/LocationId';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { MessageKey } from '../lib/gen/MessageKey';
import type { PaneId } from '../lib/gen/PaneId';
import type { ProcessKey } from '../lib/gen/ProcessKey';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { PtyId } from '../lib/gen/PtyId';
import type { RequestId } from '../lib/gen/RequestId';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { TaskKey } from '../lib/gen/TaskKey';
import type { TeamKey } from '../lib/gen/TeamKey';

function fnv32(text: string, offset: number): number {
  let h = offset >>> 0;
  for (let i = 0; i < text.length; i++) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

/** 16 lowercase hex digits from a seed string. */
export function hex16(seed: string): string {
  return fnv32(seed, 0x811c9dc5).toString(16).padStart(8, '0') + fnv32(seed, 0x2545f491).toString(16).padStart(8, '0');
}

/** `s_<UUIDv7>`: a fixed time prefix and the number in the last group. */
export function sessionUid(n: number): SessionUid {
  return `s_0192f000-0000-7000-8000-${n.toString(16).padStart(12, '0')}` as SessionUid;
}

export const projectId = (name: string) => `p_${hex16('project\0' + name)}` as ProjectId;
export const locationId = (path: string) => `l_${hex16('location\0' + path)}` as LocationId;
export const teamKey = (name: string) => `t_${hex16('team\0' + name)}` as TeamKey;
export const memberKey = (team: string, name: string) => `m_${hex16('member\0' + team + '\0' + name)}` as MemberKey;
export const taskKey = (team: string, n: number) => `k_${hex16('task\0' + team + '\0' + n)}` as TaskKey;
export const messageKey = (team: string, n: number) => `g_${hex16('message\0' + team + '\0' + n)}` as MessageKey;
export const accountId = (vendor: string, label: string) => `a_${hex16('account\0' + vendor + '\0' + label)}` as AccountId;
export const processKey = (pid: number, startSec: number) => `x_${pid}_${startSec}` as ProcessKey;
export const episodeId = (uid: SessionUid, n: number) => `e_${uid.slice(-6)}_${n}` as EpisodeId;
export const requestId = (uid: SessionUid, n: number) => `r_${uid.slice(-8)}_${n}` as RequestId;
export const blockId = (uid: SessionUid, n: number) => `b_${uid.slice(-8)}_${n}` as BlockId;
export const paneId = (n: number) => `%${n}` as PaneId;
export const ptyId = (uid: SessionUid) => `y_${uid.slice(-12)}` as PtyId;
