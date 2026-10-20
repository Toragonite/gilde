/**
 * base64 ⇄ bytes.
 *
 * PTY chunks arrive as base64 (`PtyChunk.b64`) and are decoded exactly once,
 * straight into `term.write(Uint8Array)` — the bytes never become a JS string
 * and never enter React state (DESIGN §9-3 rule 1). Keyboard input takes the
 * reverse path.
 *
 * `atob`/`btoa` operate on latin-1 strings, which is exactly the byte-per-code-
 * unit mapping we want here; `TextDecoder` would mangle invalid UTF-8.
 */

/** Chunked so a multi-megabyte scrollback snapshot cannot blow the argument limit. */
const CHUNK = 0x8000;

export function b64ToBytes(s: string): Uint8Array {
  const binary = atob(s);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

export function bytesToB64(b: Uint8Array): string {
  let binary = '';
  for (let i = 0; i < b.length; i += CHUNK) {
    binary += String.fromCharCode(...b.subarray(i, i + CHUNK));
  }
  return btoa(binary);
}

/** UTF-8 encode then base64 — the path `term.onData` takes to `session_write`. */
export function textToB64(text: string): string {
  return bytesToB64(new TextEncoder().encode(text));
}
