/**
 * `answerCard` on screen prompts (A25.1): "allow" sends option 1's digit
 * whether the producer marks it `allowOnce` or `allowWithNote` (the
 * note-capable allow on a "Tab to amend" prompt); deny sends the last digit.
 */

import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { ScreenPromptKeyReq } from '../lib/gen/ScreenPromptKeyReq';
import { withFakeCore } from '../test/backend';
import { pending, session, uid } from '../test/fixtures';
import { assert, assertDeepEqual, test } from '../test/harness';
import { answerCard } from './actions';

function screenCard(first: 'allowOnce' | 'allowWithNote'): PendingInteraction {
  return pending('sp_00aa', {
    kind: 'screenPrompt',
    promptRev: 'rev-a',
    options: [
      { key: '1', label: 'Yes', role: first },
      { key: '2', label: 'Yes, and switch to auto mode', role: 'other' },
      { key: '3', label: 'No', role: 'deny' },
    ],
  });
}

test('answerCard: allow on a screen prompt sends option 1 for allowOnce and allowWithNote alike (A25.1)', () =>
  withFakeCore({ screen_prompt_key: () => ({ outcome: 'sent' }) }, async (core) => {
    for (const role of ['allowOnce', 'allowWithNote'] as const) {
      const target = { session: session(1), interaction: screenCard(role), answerUid: uid(1) };
      assert(await answerCard(target, 'allowOnce', 'button'), `${role}: taken`);
    }
    assert(await answerCard({ session: session(1), interaction: screenCard('allowWithNote'), answerUid: uid(1) }, 'deny', 'button'), 'deny taken');
    const keys = core.calls('screen_prompt_key').map((c) => (c.req as ScreenPromptKeyReq).action);
    assertDeepEqual(keys, [{ kind: 'key', key: '1' }, { kind: 'key', key: '1' }, { kind: 'key', key: '3' }], '1, 1, then the last digit');
  }),
);
