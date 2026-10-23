/**
 * Conversation blocks → display rows (ux-ia §3.4 "How the conversation is
 * rendered"). One rule set for every vendor:
 *
 *  - each user turn, assistant text, thinking, interaction outcome, system
 *    note and unknown block is one row;
 *  - consecutive tool calls fold into one row per run of the same category:
 *    edits (a `fileChange`, or verb edit/create) become a file list with
 *    +/−; commands ("Ran …") show the command and its result line; anything
 *    else folds into a summary ("Read 6 files · searched 2 patterns");
 *  - an assistant text row shows its speaker line only when it starts a new
 *    speaker run (after a user turn, a different agent, or anything else).
 *
 * Row keys are the first block's id, so a row keeps its measured size while
 * blocks are appended to its run (VirtualList caches sizes by key).
 */

import type { Block } from '../lib/gen/Block';
import type { BlockKind } from '../lib/gen/BlockKind';
import type { ToolVerb } from '../lib/gen/ToolVerb';

export type ToolBlock = Block & { kind: Extract<BlockKind, { kind: 'toolCall' }> };

export type ToolCategory = 'edit' | 'run' | 'other';

export type Row =
  | { kind: 'user'; key: string; block: Block & { kind: Extract<BlockKind, { kind: 'userText' }> } }
  | { kind: 'assistant'; key: string; block: Block & { kind: Extract<BlockKind, { kind: 'assistantText' }> }; speaker: boolean }
  | { kind: 'thinking'; key: string; block: Block & { kind: Extract<BlockKind, { kind: 'thinking' }> } }
  | { kind: 'tools'; key: string; category: ToolCategory; blocks: ToolBlock[] }
  | { kind: 'interaction'; key: string; block: Block & { kind: Extract<BlockKind, { kind: 'interaction' }> } }
  | { kind: 'system'; key: string; block: Block & { kind: Extract<BlockKind, { kind: 'system' }> } }
  | { kind: 'unknown'; key: string; block: Block & { kind: Extract<BlockKind, { kind: 'unknown' }> } };

export function toolCategory(block: ToolBlock): ToolCategory {
  const verb = block.kind.summary.verb;
  if (block.kind.fileChange !== null || verb === 'edit' || verb === 'create') return 'edit';
  if (verb === 'run') return 'run';
  return 'other';
}

/** Fold blocks into rows. Pure; O(n). */
export function toRows(blocks: readonly Block[]): Row[] {
  const rows: Row[] = [];
  let lastAgent: string | null | undefined;
  for (const block of blocks) {
    const k = block.kind;
    switch (k.kind) {
      case 'toolCall': {
        const tb = block as ToolBlock;
        const category = toolCategory(tb);
        const prev = rows[rows.length - 1];
        if (prev && prev.kind === 'tools' && prev.category === category) prev.blocks.push(tb);
        else rows.push({ kind: 'tools', key: block.id, category, blocks: [tb] });
        lastAgent = undefined;
        break;
      }
      case 'assistantText': {
        const prev = rows[rows.length - 1];
        const speaker = !(prev && prev.kind === 'assistant' && lastAgent === k.agent);
        rows.push({ kind: 'assistant', key: block.id, block: block as Extract<Row, { kind: 'assistant' }>['block'], speaker });
        lastAgent = k.agent;
        break;
      }
      case 'userText':
        rows.push({ kind: 'user', key: block.id, block: block as Extract<Row, { kind: 'user' }>['block'] });
        lastAgent = undefined;
        break;
      case 'thinking':
        rows.push({ kind: 'thinking', key: block.id, block: block as Extract<Row, { kind: 'thinking' }>['block'] });
        break;
      case 'interaction':
        rows.push({ kind: 'interaction', key: block.id, block: block as Extract<Row, { kind: 'interaction' }>['block'] });
        lastAgent = undefined;
        break;
      case 'system':
        rows.push({ kind: 'system', key: block.id, block: block as Extract<Row, { kind: 'system' }>['block'] });
        lastAgent = undefined;
        break;
      case 'unknown':
        rows.push({ kind: 'unknown', key: block.id, block: block as Extract<Row, { kind: 'unknown' }>['block'] });
        lastAgent = undefined;
        break;
    }
  }
  return rows;
}

/** Counts per verb of a run, in the order verbs first appear. */
export function verbCounts(blocks: readonly ToolBlock[]): Array<{ verb: ToolVerb; n: number }> {
  const order: ToolVerb[] = [];
  const counts = new Map<ToolVerb, number>();
  for (const b of blocks) {
    const verb = b.kind.summary.verb;
    if (!counts.has(verb)) order.push(verb);
    counts.set(verb, (counts.get(verb) ?? 0) + 1);
  }
  return order.map((verb) => ({ verb, n: counts.get(verb) ?? 0 }));
}

/** Sum of +/− over the edits of a run (files counted once by path; the last change wins). */
export function editTotals(blocks: readonly ToolBlock[]): {
  files: Array<{ path: string; added: number; removed: number; change: 'modified' | 'added' | 'deleted' }>;
  added: number;
  removed: number;
} {
  const byPath = new Map<string, { path: string; added: number; removed: number; change: 'modified' | 'added' | 'deleted' }>();
  for (const b of blocks) {
    const fc = b.kind.fileChange;
    const path = fc?.pathRel ?? b.kind.summary.object ?? '';
    if (path === '') continue;
    const prev = byPath.get(path);
    const added = (prev?.added ?? 0) + (fc?.added ?? 0);
    const removed = (prev?.removed ?? 0) + (fc?.removed ?? 0);
    // A file created in this run stays "new" even after later edits.
    const change = prev?.change === 'added' ? 'added' : (fc?.change ?? 'modified');
    byPath.set(path, { path, added, removed, change });
  }
  const files = [...byPath.values()];
  return { files, added: files.reduce((s, f) => s + f.added, 0), removed: files.reduce((s, f) => s + f.removed, 0) };
}

/** The first non-empty line of a preview, capped (a result line such as "24 passed"). */
export function firstLine(text: string | null | undefined, max = 120): string {
  if (!text) return '';
  const line = text.split('\n').find((l) => l.trim() !== '') ?? '';
  const trimmed = line.trim();
  return trimmed.length > max ? `${trimmed.slice(0, max - 1)}…` : trimmed;
}
