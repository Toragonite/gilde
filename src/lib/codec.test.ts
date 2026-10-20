/**
 * base64 codec tests. The PTY path must be byte exact — a session that emits
 * invalid UTF-8 (a binary flood, a truncated multi-byte character at a 64 KiB
 * boundary) must survive the round trip untouched (DESIGN §6-2).
 */

import { assert, assertEqual, test } from '../test/harness';
import { b64ToBytes, bytesToB64, textToB64 } from './codec';

function roundTrip(bytes: Uint8Array): boolean {
  const back = b64ToBytes(bytesToB64(bytes));
  if (back.length !== bytes.length) return false;
  for (let i = 0; i < bytes.length; i++) if (back[i] !== bytes[i]) return false;
  return true;
}

test('every byte value survives the round trip', () => {
  const all = new Uint8Array(256);
  for (let i = 0; i < 256; i++) all[i] = i;
  assert(roundTrip(all), '0..255 round trips');
});

test('invalid UTF-8 and split multi-byte sequences survive', () => {
  // A lone continuation byte, a truncated three-byte sequence, and a NUL.
  const broken = new Uint8Array([0x80, 0xe2, 0x94, 0x00, 0xff, 0xfe, 0xed]);
  assert(roundTrip(broken), 'invalid UTF-8 round trips');

  const text = new TextEncoder().encode('│ > 한글 프롬프트');
  const head = text.subarray(0, text.length - 1);
  assert(roundTrip(head), 'a truncated multi-byte tail round trips');
});

test('random buffers round trip', () => {
  for (let n = 0; n < 200; n++) {
    const bytes = new Uint8Array(1 + ((n * 37) % 5000));
    for (let i = 0; i < bytes.length; i++) bytes[i] = (i * 31 + n * 17) & 0xff;
    assert(roundTrip(bytes), `random buffer ${n} round trips`);
  }
});

test('textToB64 encodes UTF-8 before base64', () => {
  assertEqual(textToB64('ok'), bytesToB64(new Uint8Array([0x6f, 0x6b])), 'ascii matches');
  const decoded = new TextDecoder().decode(b64ToBytes(textToB64('한글')));
  assertEqual(decoded, '한글', 'Korean input survives the encode/decode pair');
});
