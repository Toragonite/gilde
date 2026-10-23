/**
 * The conversation fold (contract §4.2.4) and T-ui-orderTolerance (§13.4):
 * a store patch and a conversation event about the same request may arrive in
 * either order; the card comes from the store, the block from the channel.
 */

import type { ConvoEnvelope } from '../lib/gen/ConvoEnvelope';
import type { ConvoEvent } from '../lib/gen/ConvoEvent';
import type { RequestId } from '../lib/gen/RequestId';
import type { SubId } from '../lib/gen/SubId';
import { block, pending, text } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import { MAX_BLOCKS, emptyConvo, interactionDisplay, reduceConvo } from './convo';
import type { ConvoView } from './convo';

const SUB = 'v_0123456789abcdef' as SubId;

function feed(view: ConvoView, events: ConvoEvent[], firstSeq = 0): ConvoView {
  let v = view;
  events.forEach((event, i) => {
    const env: ConvoEnvelope = { subId: SUB, seq: firstSeq + i, event };
    v = reduceConvo(v, env);
  });
  return v;
}

test('reset, append, update, finalize, older', () => {
  let v = feed(emptyConvo(), [
    { kind: 'reset', blocks: [text('a', 1, 'hello')], hasOlder: true, reason: 'initial' },
    { kind: 'appended', block: text('b', 2, 'wor', true) },
    { kind: 'updated', blockId: text('b', 2, '').id, patch: { kind: 'textDelta', append: 'ld' } },
  ]);
  assertEqual(v.status, 'live', 'live after Reset');
  assertEqual(v.subId, SUB, 'bound to the subscription');
  const b = v.blocks[1]?.kind;
  assertEqual(b?.kind === 'assistantText' ? b.markdown : '', 'world', 'delta appended');
  const keptA = v.blocks[0];
  v = feed(v, [{ kind: 'updated', blockId: text('b', 2, '').id, patch: { kind: 'finalized', block: text('b', 2, 'world.') } }], 3);
  assertEqual(v.blocks[0], keptA, 'untouched blocks keep identity');
  const fin = v.blocks[1]?.kind;
  assertEqual(fin?.kind === 'assistantText' ? fin.markdown : '', 'world.', 'finalized replaces the streamed text');
  v = feed(v, [{ kind: 'older', blocks: [text('z', 0, 'earlier'), text('a', 1, 'dup')], hasOlder: false }], 4);
  assertDeepEqual(v.blocks.map((x) => x.id), ['z', 'a', 'b'], 'older prepended, duplicates skipped');
  assertEqual(v.hasOlder, false, 'hasOlder updated');
});

test('duplicates and foreign subscriptions are dropped; Reset replaces everything', () => {
  let v = feed(emptyConvo(), [{ kind: 'reset', blocks: [text('a', 1, 'x')], hasOlder: false, reason: 'initial' }]);
  const same = reduceConvo(v, { subId: SUB, seq: 0, event: { kind: 'appended', block: text('q', 9, 'late') } });
  assertEqual(same, v, 'a seq at or below the last one is dropped');
  const foreign = reduceConvo(v, { subId: 'v_ffffffffffffffff' as SubId, seq: 5, event: { kind: 'ended' } });
  assertEqual(foreign, v, 'another subscription is ignored');
  v = feed(v, [{ kind: 'appended', block: text('a', 1, 'again') }], 1);
  assertEqual(v.blocks.length, 1, 'an append of a known id replaces it');
  v = feed(v, [{ kind: 'reset', blocks: [text('n', 1, 'new')], hasOlder: false, reason: 'replaced' }], 2);
  assertDeepEqual(v.blocks.map((x) => x.id), ['n'], 'Reset replaces');
  assertEqual(v.resetCount, 2, 'resets counted');
  v = feed(v, [{ kind: 'evicted' }], 3);
  assertEqual(v.status, 'evicted', 'evicted');
});

test('at most 2,000 blocks stay in memory', () => {
  const many = Array.from({ length: MAX_BLOCKS + 10 }, (_, i) => text(`b${i}`, i, 'x'));
  const v = feed(emptyConvo(), [{ kind: 'reset', blocks: many, hasOlder: false, reason: 'initial' }]);
  assertEqual(v.blocks.length, MAX_BLOCKS, 'capped');
  assertEqual(v.blocks[0]?.id, 'b10', 'the oldest dropped');
  assert(v.hasOlder, 'and reachable again through load older');
});

test('T-ui-orderTolerance: card and interaction block in either order', () => {
  const req = 'req-1' as RequestId;
  const interaction = block('i1', 5, { kind: 'interaction', requestId: req, interactionKind: 'toolApproval', outcome: 'pending' });

  // Order A: the block arrives first, the store patch later.
  let v = feed(emptyConvo(), [
    { kind: 'reset', blocks: [], hasOlder: false, reason: 'initial' },
    { kind: 'appended', block: interaction },
  ]);
  assertEqual(interactionDisplay('pending', req, []), 'pending', 'block first: a neutral waiting line, no guessed outcome');
  assertEqual(interactionDisplay('pending', req, [pending(req)]), 'card', 'store arrives: the card shows');

  // Order B: the store has the card before the block exists.
  assertEqual(interactionDisplay('pending', req, [pending(req)]), 'card', 'store first: the card shows');

  // Answer: the store removes the pending entry before the block's outcome update…
  assertEqual(interactionDisplay('pending', req, []), 'pending', 'between the two: still no guessed outcome');
  v = feed(v, [{ kind: 'updated', blockId: interaction.id, patch: { kind: 'interaction', outcome: 'allowed' } }], 2);
  const k = v.blocks[0]?.kind;
  assertEqual(k?.kind === 'interaction' ? k.outcome : null, 'allowed', 'the outcome lands on the block');
  assertEqual(interactionDisplay('allowed', req, []), 'allowed', 'and is shown');
  // …or the outcome first while the store still lists the request: the card stays.
  assertEqual(interactionDisplay('allowed', req, [pending(req)]), 'card', 'the store decides whether a card shows');
});
