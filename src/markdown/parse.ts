/**
 * Markdown → mdast, and the link rules (contract §9.1).
 *
 * Agent output is attacker-controlled text. It is parsed to an AST (micromark
 * + mdast-util-from-markdown with the GFM extensions: tables, strikethrough,
 * task lists, autolinks, footnotes) and only that AST is rendered, by React
 * elements in `Markdown.tsx`. Nothing here produces markup: raw HTML in the
 * source arrives as `html` nodes, which the renderer shows as literal text.
 */

import type { Definition, Nodes, Root } from 'mdast';
import { fromMarkdown } from 'mdast-util-from-markdown';
import { gfmFromMarkdown } from 'mdast-util-gfm';
import { gfm } from 'micromark-extension-gfm';

// ---------------------------------------------------------------------------
// Pathological input
// ---------------------------------------------------------------------------

/**
 * Limits past which agent text is shown as plain text instead of parsed.
 *
 * The parser is linear on ordinary text but not on hostile structure: about
 * 20,000 nested `>` overflow its stack, and long `*`/`_` runs, deeply nested
 * lists, thousands of emphasis delimiters, nested `[` and very wide tables
 * cost seconds — on the WebView's main thread, again on every streaming delta.
 * Real agent output stays far below every limit; past one, the text is still
 * shown in full, just not formatted.
 */
export const MARKDOWN_LIMITS = {
  /** Characters parsed at all. */
  chars: 100_000,
  /** `>` levels on one line. */
  quoteDepth: 8,
  /** Indentation (columns) before a list marker. */
  listIndent: 48,
  /** One run of `*`, `_` or `~` (a line that is only a rule is exempt). */
  delimiterRun: 32,
  /** All `*` and `_` outside fenced code. */
  delimiters: 2_000,
  /** All `[` outside fenced code. */
  brackets: 1_000,
  /** `|` on one line (table columns). */
  pipes: 64,
} as const;

/** Why `text` is shown as plain text, or null when it is safe to parse. Linear in the text. */
export function plainTextReason(text: string): string | null {
  const L = MARKDOWN_LIMITS;
  if (text.length > L.chars) return 'chars';
  let delimiters = 0;
  let brackets = 0;
  let fence: string | null = null;
  for (const line of text.split('\n')) {
    const fenceMark = /^ {0,3}(`{3,}|~{3,})/.exec(line);
    if (fence !== null) {
      if (fenceMark && fenceMark[1]?.[0] === fence[0] && (fenceMark[1]?.length ?? 0) >= fence.length) fence = null;
      continue;
    }
    if (fenceMark) {
      fence = fenceMark[1] ?? null;
      continue;
    }
    const quotes = /^(?:\s*>)+/.exec(line);
    if (quotes && (quotes[0].match(/>/g)?.length ?? 0) > L.quoteDepth) return 'quoteDepth';
    const list = /^([ \t]*)(?:[-*+]|\d{1,9}[.)])(?:[ \t]|$)/.exec(line);
    if (list && (list[1] ?? '').replace(/\t/g, '    ').length > L.listIndent) return 'listIndent';
    const isRule = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/.test(line);
    if (!isRule) {
      const run = /([*_~])\1*/g;
      for (let m = run.exec(line); m; m = run.exec(line)) {
        if (m[0].length > L.delimiterRun) return 'delimiterRun';
      }
    }
    let pipes = 0;
    for (let i = 0; i < line.length; i++) {
      const c = line.charCodeAt(i);
      if (c === 0x2a /* * */ || c === 0x5f /* _ */) delimiters++;
      else if (c === 0x5b /* [ */) brackets++;
      else if (c === 0x7c /* | */) pipes++;
    }
    if (pipes > L.pipes) return 'pipes';
  }
  if (delimiters > L.delimiters) return 'delimiters';
  if (brackets > L.brackets) return 'brackets';
  return null;
}

/** Parse `text` (GFM). Pure: the same text always gives an equal tree. */
export function parseMarkdown(text: string): Root {
  return fromMarkdown(text, {
    extensions: [gfm()],
    mdastExtensions: [gfmFromMarkdown()],
  });
}

/** Link definitions of a document, by their normalized identifier. */
export function collectDefinitions(root: Root): Map<string, Definition> {
  const out = new Map<string, Definition>();
  const visit = (node: Nodes): void => {
    if (node.type === 'definition') {
      // CommonMark: the first definition of a label wins.
      if (!out.has(node.identifier)) out.set(node.identifier, node);
      return;
    }
    if ('children' in node) for (const child of node.children) visit(child as Nodes);
  };
  visit(root);
  return out;
}

/** The plain text of a node (what a screen reader would read for it). */
export function plainText(node: Nodes): string {
  switch (node.type) {
    case 'text':
    case 'inlineCode':
    case 'code':
    case 'html':
      return node.value;
    case 'image':
    case 'imageReference':
      return node.alt ?? '';
    case 'break':
      return ' ';
    default:
      if ('children' in node) return node.children.map((c) => plainText(c as Nodes)).join('');
      return '';
  }
}

// ---------------------------------------------------------------------------
// Link targets
// ---------------------------------------------------------------------------

/** What a link or image destination may do when activated. */
export type LinkTarget =
  /** http/https without userinfo, ≤ 2048 chars: opened through `link_open` (Rust re-checks). */
  | { kind: 'web'; url: string; host: string }
  /** A file path: activatable only when the screen provides `onOpenPath` (→ `ide_open`). */
  | { kind: 'path'; path: string }
  /** Everything else (`javascript:`, `data:`, `file:`, `mailto:`, fragments, …): plain text. */
  | { kind: 'inert' };

/** Rust's `link_open` limit (`LinkOpenReq`). */
export const MAX_URL_LENGTH = 2048;
const MAX_PATH_LENGTH = 4096;

const INERT: LinkTarget = { kind: 'inert' };

// C0 controls and DEL anywhere in a destination make it inert outright.
const CONTROL = /[\u0000-\u001f\u007f]/;

/**
 * A path, as opposed to a URL: absolute (`/…`), home-relative (`~/…`),
 * dot-relative (`./…`, `../…`), or a bare relative path with a file-like
 * segment (`src/main.rs`, `README.md`). Fragments, queries and
 * protocol-relative `//host` are not paths.
 */
function looksLikePath(dest: string): boolean {
  if (dest.startsWith('//')) return false;
  if (dest.startsWith('#') || dest.startsWith('?')) return false;
  if (/^(?:\/|~\/|\.\.?\/)/.test(dest)) return true;
  if (/^[\w.@+-]+(?:\/[\w.@+ -]+)*\/?$/.test(dest) && /[/.]/.test(dest)) return true;
  return false;
}

function safeDecode(s: string): string {
  try {
    return decodeURI(s);
  } catch {
    return s;
  }
}

/** Classify a link or image destination (as the AST holds it, references decoded). */
export function linkTarget(destination: string | null | undefined): LinkTarget {
  if (typeof destination !== 'string') return INERT;
  const dest = destination.trim();
  if (dest === '' || CONTROL.test(dest)) return INERT;

  let parsed: URL | null = null;
  try {
    parsed = new URL(dest);
  } catch {
    parsed = null;
  }

  if (parsed) {
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return INERT;
    if (parsed.username !== '' || parsed.password !== '') return INERT;
    if (parsed.host === '') return INERT;
    const url = parsed.href;
    if (url.length > MAX_URL_LENGTH || dest.length > MAX_URL_LENGTH) return INERT;
    return { kind: 'web', url, host: parsed.host };
  }

  // Not an absolute URL. A scheme-like prefix that `URL` refused (`javascript:`
  // with odd characters, `c:\…`) stays inert; so do fragments and queries.
  if (/^[a-z][a-z0-9+.-]*:/i.test(dest)) return INERT;
  if (dest.length > MAX_PATH_LENGTH || !looksLikePath(dest)) return INERT;
  return { kind: 'path', path: safeDecode(dest) };
}
