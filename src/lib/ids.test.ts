/** Client-minted ids (contract §3.2). */

import { assert, assertEqual, assertMatch, test } from '../test/harness';
import { UUID_V4, newClientMsgId, newClientRequestId, newCorrelationId, newViewId, uuidV4 } from './ids';
import { vendorBadge } from './vendor';

test('client ids are lowercase UUIDv4 and unique', () => {
  const seen = new Set<string>();
  for (let i = 0; i < 200; i++) {
    const a = newClientRequestId();
    const b = newClientMsgId();
    assertMatch(a, UUID_V4, 'clientRequestId is a v4 UUID');
    assertMatch(b, UUID_V4, 'clientMsgId is a v4 UUID');
    seen.add(a);
    seen.add(b);
  }
  assertEqual(seen.size, 400, 'no repeats');
  const u = uuidV4();
  assertEqual(u, u.toLowerCase(), 'lowercase');
});

test('view ids match ^[a-z0-9-]{1,32}$; correlation ids are ui-<16 hex>', () => {
  for (const label of ['convo', 'Split Tile 3', '', 'x'.repeat(40), '한글']) {
    const id = newViewId(label);
    assertMatch(id, /^[a-z0-9-]{1,32}$/, `view id for "${label}"`);
  }
  assertMatch(newCorrelationId(), /^ui-[0-9a-f]{16}$/, 'correlation id');
  assert(newCorrelationId() !== newCorrelationId(), 'fresh each time');
});

test('vendor badges are the lowercase CLI names', () => {
  assertEqual(vendorBadge('claude'), 'claude', 'claude');
  assertEqual(vendorBadge('agy'), 'agy', 'agy');
  assertEqual(vendorBadge('mystery'), 'mystery', 'an unknown id is shown as it is');
  assertEqual(vendorBadge(null), '', 'none');
});
