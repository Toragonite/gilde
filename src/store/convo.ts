/**
 * Conversation view state — the fold of one `convo_subscribe` channel
 * (contract §4.2.4 `ConvoEvent`). Blocks live here, per mounted view, never in
 * the global store; pending cards live in the World mirror (`Session.pending`).
 * The two are keyed by `requestId` and either may arrive first
 * (T-ui-orderTolerance, §13.4): a card shows while its request is pending in
 * the store, whatever the conversation has seen; an interaction block shows
 * the outcome the conversation reports.
 *
 * Rules:
 *  - events of a subscription arrive with strictly increasing `seq`; an event
 *    at or below the last seen `seq` is a duplicate and is dropped;
 *  - the first event is `Reset`, and a `Reset` (truncation, replaced source,
 *    lagging) replaces every block;
 *  - `Appended` with an id already present replaces that block (idempotent);
 *  - `TextDelta` appends to the streaming text; `Finalized` replaces the block
 *    with the authoritative one;
 *  - at most `MAX_BLOCKS` blocks stay in memory (PM-08); the oldest are dropped
 *    and `hasOlder` becomes true.
 */

import type { Block } from '../lib/gen/Block';
import type { BlockId } from '../lib/gen/BlockId';
import type { BlockPatch } from '../lib/gen/BlockPatch';
import type { ConvoEnvelope } from '../lib/gen/ConvoEnvelope';
import type { ConvoSourceKind } from '../lib/gen/ConvoSourceKind';
import type { InteractionOutcome } from '../lib/gen/InteractionOutcome';
import type { Notice } from '../lib/gen/Notice';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { RequestId } from '../lib/gen/RequestId';
import type { ResetReason } from '../lib/gen/ResetReason';
import type { SubId } from '../lib/gen/SubId';

export const MAX_BLOCKS = 2000;

export type ConvoStatus = 'connecting' | 'live' | 'evicted' | 'ended' | 'unavailable';

export interface ConvoView {
  subId: SubId | null;
  lastSeq: number;
  blocks: readonly Block[];
  hasOlder: boolean;
  source: ConvoSourceKind | null;
  note: Notice | null;
  status: ConvoStatus;
  resetReason: ResetReason | null;
  /** Bumped on every Reset, so a list can drop its measurements and scroll state. */
  resetCount: number;
}

export function emptyConvo(): ConvoView {
  return {
    subId: null,
    lastSeq: -1,
    blocks: [],
    hasOlder: false,
    source: null,
    note: null,
    status: 'connecting',
    resetReason: null,
    resetCount: 0,
  };
}

function capBlocks(blocks: Block[], hasOlder: boolean): { blocks: Block[]; hasOlder: boolean } {
  if (blocks.length <= MAX_BLOCKS) return { blocks, hasOlder };
  return { blocks: blocks.slice(blocks.length - MAX_BLOCKS), hasOlder: true };
}

function patchBlock(block: Block, patch: BlockPatch): Block {
  const kind = block.kind;
  switch (patch.kind) {
    case 'finalized':
      return patch.block;
    case 'textDelta':
      if (kind.kind === 'assistantText') return { ...block, kind: { ...kind, markdown: kind.markdown + patch.append } };
      if (kind.kind === 'thinking') return { ...block, kind: { ...kind, text: (kind.text ?? '') + patch.append } };
      return block;
    case 'toolResult':
      if (kind.kind === 'toolCall') return { ...block, kind: { ...kind, status: patch.status, resultPreview: patch.preview } };
      return block;
    case 'interaction':
      if (kind.kind === 'interaction') return { ...block, kind: { ...kind, outcome: patch.outcome } };
      return block;
  }
}

/** Fold one envelope. Blocks an event did not touch keep their identity. */
export function reduceConvo(view: ConvoView, envelope: ConvoEnvelope): ConvoView {
  if (view.subId !== null && envelope.subId !== view.subId) return view;
  if (envelope.seq <= view.lastSeq) return view;
  const base: ConvoView = { ...view, subId: view.subId ?? envelope.subId, lastSeq: envelope.seq };
  const event = envelope.event;
  switch (event.kind) {
    case 'reset': {
      const capped = capBlocks([...event.blocks], event.hasOlder);
      return {
        ...base,
        ...capped,
        status: 'live',
        resetReason: event.reason,
        resetCount: view.resetCount + 1,
      };
    }
    case 'appended': {
      const at = view.blocks.findIndex((b) => b.id === event.block.id);
      if (at >= 0) {
        const blocks = view.blocks.slice();
        blocks[at] = event.block;
        return { ...base, blocks };
      }
      return { ...base, ...capBlocks([...view.blocks, event.block], view.hasOlder) };
    }
    case 'updated': {
      const at = view.blocks.findIndex((b) => b.id === event.blockId);
      if (at < 0) return base;
      const current = view.blocks[at] as Block;
      const next = patchBlock(current, event.patch);
      if (next === current) return base;
      const blocks = view.blocks.slice();
      blocks[at] = next;
      return { ...base, blocks };
    }
    case 'older': {
      const known = new Set<BlockId>(view.blocks.map((b) => b.id));
      const older = event.blocks.filter((b) => !known.has(b.id));
      const merged = [...older, ...view.blocks];
      // Older blocks never push newer ones out: they are what the user asked for.
      const blocks = merged.length > MAX_BLOCKS ? merged.slice(0, MAX_BLOCKS) : merged;
      return { ...base, blocks, hasOlder: event.hasOlder };
    }
    case 'source':
      return { ...base, source: event.source, note: event.note };
    case 'evicted':
      return { ...base, status: 'evicted' };
    case 'ended':
      return { ...base, status: 'ended' };
  }
}

/**
 * What an interaction block should show, given the session's pending list
 * from the store. A request pending in the store is shown by its card (the
 * block shows a placeholder line), whichever arrived first; otherwise the
 * block's own outcome — `pending` there means the store has not delivered the
 * card yet (or already removed it before the outcome update arrived), so the
 * block shows a neutral waiting line and never a guessed outcome.
 */
export function interactionDisplay(
  outcome: InteractionOutcome,
  requestId: RequestId,
  pending: readonly PendingInteraction[],
): 'card' | InteractionOutcome {
  if (pending.some((p) => p.requestId === requestId)) return 'card';
  return outcome;
}
