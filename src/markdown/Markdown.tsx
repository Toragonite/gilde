/**
 * The markdown renderer (contract §9.1, PM-03).
 *
 * Agent text is parsed to an mdast tree (`parse.ts`) and each node becomes a
 * React element; React escapes every string, and nothing is ever inserted as
 * markup. The rules that keep hostile agent output inert:
 *
 *  - Raw HTML in the source (`html` nodes) is shown as literal text.
 *  - Code blocks and inline code are text.
 *  - Links show their text plus the host in parentheses. Only http/https are
 *    activatable, and only through `link_open` (Rust: http/https, no
 *    userinfo, ≤ 2048 chars, a native confirmation for local addresses).
 *    The element has no `href`, so the WebView itself never navigates.
 *    File paths are activatable only when the screen passes `onOpenPath`
 *    (which it maps to `ide_open`); every other scheme is plain text.
 *  - Images are never fetched: an image is a link labelled "Image: {alt}".
 *  - Task-list checkboxes are drawn, not form controls.
 *  - Headings start at `headingBase` (default h3), so agent output never
 *    produces a page-level heading.
 */

import './markdown.css';

import type {
  Definition,
  FootnoteDefinition,
  List,
  ListItem,
  Nodes,
  Parents,
  Root,
  Table,
} from 'mdast';
import { Fragment, useCallback, useMemo } from 'react';
import type { KeyboardEvent, MouseEvent, ReactNode } from 'react';

import type { MessageKey, MessageParams } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { IpcError } from '../lib/gen/IpcError';
import { collectDefinitions, linkTarget, parseMarkdown, plainText, plainTextReason } from './parse';
import type { LinkTarget } from './parse';

export interface MarkdownProps {
  /** The markdown source (already capped and sanitized by Rust). */
  text: string;
  /** The element level of a `#` heading (1–6, default 3); deeper headings follow, capped at h6. */
  headingBase?: number;
  /** Makes file-path links activatable (screens map it to `ide_open`); without it paths are text. */
  onOpenPath?: (path: string) => void;
  /** Called when `link_open` rejects (the rejection is otherwise ignored). */
  onLinkError?: (error: IpcError) => void;
  /** Extra class on the root element. */
  className?: string;
}

interface RenderContext {
  t: (key: MessageKey, params?: MessageParams) => string;
  headingBase: number;
  definitions: Map<string, Definition>;
  openWeb: (url: string) => void;
  onOpenPath: ((path: string) => void) | undefined;
  /** Inside a link: nested links and images render as text (no nested controls). */
  inLink: boolean;
  /** Element nesting so far; past `MAX_RENDER_DEPTH` a subtree renders as its text. */
  depth: number;
}

/** Element nesting past which a subtree is rendered as its plain text (defence in depth). */
export const MAX_RENDER_DEPTH = 48;

/** The parsed tree, or null when the text is shown as plain text (`plainTextReason`, or the parser failed). */
function parseOrPlain(text: string): Root | null {
  if (plainTextReason(text) !== null) return null;
  try {
    return parseMarkdown(text);
  } catch {
    return null;
  }
}

/** Render `text` as markdown. Parsing is memoized per input string. */
export function Markdown({ text, headingBase = 3, onOpenPath, onLinkError, className }: MarkdownProps) {
  const { t } = useI18n();
  const root = useMemo<Root | null>(() => parseOrPlain(text), [text]);
  const definitions = useMemo(() => (root ? collectDefinitions(root) : new Map<string, Definition>()), [root]);
  const openWeb = useCallback(
    (url: string) => {
      commands.link_open({ url }).catch((error: unknown) => {
        if (onLinkError) onLinkError(error as IpcError);
      });
    },
    [onLinkError],
  );

  const cls = className ? `md ${className}` : 'md';
  if (!root) {
    return (
      <div className={cls} data-plain="true">
        <p className="md-raw md-plain">{text}</p>
      </div>
    );
  }

  const ctx: RenderContext = {
    t,
    headingBase: clampLevel(headingBase),
    definitions,
    openWeb,
    onOpenPath,
    inLink: false,
    depth: 0,
  };

  return <div className={cls}>{renderFlow(root.children, ctx)}</div>;
}

function clampLevel(level: number): number {
  if (!Number.isFinite(level)) return 3;
  return Math.min(6, Math.max(1, Math.round(level)));
}

// ---------------------------------------------------------------------------
// Node rendering
// ---------------------------------------------------------------------------

/** Block-level children (root, blockquote, list item, footnote definition). */
function renderFlow(children: readonly Nodes[], ctx: RenderContext, tight = false): ReactNode[] {
  const inner = { ...ctx, depth: ctx.depth + 1 };
  return children.map((child, i) => renderNode(child, inner, i, { inline: false, tight }));
}

/** Phrasing children (paragraph, heading, emphasis, link, cell). */
function renderPhrasing(parent: Parents, ctx: RenderContext): ReactNode[] {
  const inner = { ...ctx, depth: ctx.depth + 1 };
  return parent.children.map((child, i) => renderNode(child as Nodes, inner, i, { inline: true, tight: false }));
}

interface Place {
  /** In phrasing content (raw HTML becomes a span, not a paragraph). */
  inline: boolean;
  /** A paragraph in a tight list item renders without its <p>. */
  tight: boolean;
}

function renderNode(node: Nodes, ctx: RenderContext, key: number, place: Place): ReactNode {
  if (ctx.depth > MAX_RENDER_DEPTH) return <Fragment key={key}>{plainText(node)}</Fragment>;
  switch (node.type) {
    case 'paragraph':
      return place.tight ? (
        <Fragment key={key}>{renderPhrasing(node, ctx)}</Fragment>
      ) : (
        <p key={key}>{renderPhrasing(node, ctx)}</p>
      );

    case 'heading':
      return renderHeading(Math.min(6, ctx.headingBase + node.depth - 1), key, renderPhrasing(node, ctx));

    case 'thematicBreak':
      return <hr key={key} />;

    case 'blockquote':
      return <blockquote key={key}>{renderFlow(node.children, ctx)}</blockquote>;

    case 'list':
      return renderList(node, ctx, key);

    case 'listItem':
      return renderListItem(node, ctx, key, false);

    case 'html':
      // Raw HTML is shown as the characters the agent wrote, never as markup.
      return place.inline ? (
        <span key={key} className="md-raw">
          {node.value}
        </span>
      ) : (
        <p key={key} className="md-raw">
          {node.value}
        </p>
      );

    case 'code': {
      const lang = typeof node.lang === 'string' && /^[\w+#.-]{1,32}$/.test(node.lang) ? node.lang : null;
      const label = lang ? `${ctx.t('markdown.code')} · ${lang}` : ctx.t('markdown.code');
      return (
        <div key={key} className="md-codeblock" role="group" aria-label={label}>
          <pre>
            <code data-lang={lang ?? undefined}>{node.value}</code>
          </pre>
        </div>
      );
    }

    case 'definition':
      // Metadata for references; never shown.
      return null;

    case 'text':
      return <Fragment key={key}>{node.value}</Fragment>;

    case 'emphasis':
      return <em key={key}>{renderPhrasing(node, ctx)}</em>;

    case 'strong':
      return <strong key={key}>{renderPhrasing(node, ctx)}</strong>;

    case 'delete':
      return <del key={key}>{renderPhrasing(node, ctx)}</del>;

    case 'inlineCode':
      return (
        <code key={key} className="md-inline-code">
          {node.value}
        </code>
      );

    case 'break':
      return <br key={key} />;

    case 'link':
      return (
        <MdLink key={key} target={linkTarget(node.url)} label={plainText(node)} ctx={ctx}>
          {renderPhrasing(node, { ...ctx, inLink: true })}
        </MdLink>
      );

    case 'linkReference': {
      const definition = ctx.definitions.get(node.identifier);
      const children = renderPhrasing(node, { ...ctx, inLink: true });
      if (!definition) return <Fragment key={key}>{children}</Fragment>;
      return (
        <MdLink key={key} target={linkTarget(definition.url)} label={plainText(node)} ctx={ctx}>
          {children}
        </MdLink>
      );
    }

    case 'image':
    case 'imageReference': {
      const url = node.type === 'image' ? node.url : (ctx.definitions.get(node.identifier)?.url ?? null);
      const label = ctx.t('markdown.image', { alt: node.alt ?? '' });
      return (
        <MdLink key={key} target={linkTarget(url)} label={label} ctx={ctx}>
          {label}
        </MdLink>
      );
    }

    case 'table':
      return renderTable(node, ctx, key);

    case 'footnoteReference':
      return (
        <sup key={key} className="md-fnref">
          [{node.label ?? node.identifier}]
        </sup>
      );

    case 'footnoteDefinition':
      return renderFootnote(node, ctx, key);

    default: {
      // A node type this renderer does not know (a future extension): its
      // text, never markup.
      const unknown = node as Nodes;
      if ('value' in unknown && typeof unknown.value === 'string') return <Fragment key={key}>{unknown.value}</Fragment>;
      if ('children' in unknown) return <Fragment key={key}>{renderFlow(unknown.children as Nodes[], ctx)}</Fragment>;
      return null;
    }
  }
}

function renderHeading(level: number, key: number, children: ReactNode[]): ReactNode {
  switch (level) {
    case 1:
      return <h1 key={key}>{children}</h1>;
    case 2:
      return <h2 key={key}>{children}</h2>;
    case 3:
      return <h3 key={key}>{children}</h3>;
    case 4:
      return <h4 key={key}>{children}</h4>;
    case 5:
      return <h5 key={key}>{children}</h5>;
    default:
      return <h6 key={key}>{children}</h6>;
  }
}

function renderList(node: List, ctx: RenderContext, key: number): ReactNode {
  const items = node.children.map((item, i) => renderListItem(item, ctx, i, node.spread !== true));
  if (node.ordered) {
    const start = typeof node.start === 'number' && node.start !== 1 ? node.start : undefined;
    return (
      <ol key={key} start={start}>
        {items}
      </ol>
    );
  }
  return <ul key={key}>{items}</ul>;
}

function renderListItem(node: ListItem, ctx: RenderContext, key: number, listTight: boolean): ReactNode {
  const tight = listTight && node.spread !== true;
  const task = typeof node.checked === 'boolean';
  return (
    <li key={key} className={task ? 'md-task' : undefined}>
      {task ? (
        <span
          className="md-check"
          role="img"
          data-checked={node.checked ? 'true' : 'false'}
          aria-label={ctx.t(node.checked ? 'markdown.taskDone' : 'markdown.taskOpen')}
        />
      ) : null}
      {renderFlow(node.children, ctx, tight)}
    </li>
  );
}

function renderTable(node: Table, ctx: RenderContext, key: number): ReactNode {
  const [head, ...body] = node.children;
  const align = (i: number): string | undefined => node.align?.[i] ?? undefined;
  return (
    <div key={key} className="md-table-wrap">
      <table className="md-table">
        {head ? (
          <thead>
            <tr>
              {head.children.map((cell, i) => (
                <th key={i} scope="col" data-align={align(i)}>
                  {renderPhrasing(cell, ctx)}
                </th>
              ))}
            </tr>
          </thead>
        ) : null}
        {body.length > 0 ? (
          <tbody>
            {body.map((row, r) => (
              <tr key={r}>
                {row.children.map((cell, i) => (
                  <td key={i} data-align={align(i)}>
                    {renderPhrasing(cell, ctx)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        ) : null}
      </table>
    </div>
  );
}

function renderFootnote(node: FootnoteDefinition, ctx: RenderContext, key: number): ReactNode {
  return (
    <div key={key} className="md-footnote">
      <span className="md-fnlabel">[{node.label ?? node.identifier}]</span> {renderFlow(node.children, ctx, true)}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

interface MdLinkProps {
  target: LinkTarget;
  /** Plain text of the link (its accessible name). */
  label: string;
  ctx: RenderContext;
  children: ReactNode;
}

/**
 * An activatable link has no `href`: the WebView must never navigate. It is a
 * focusable element with role "link" that calls `link_open` (or the screen's
 * `onOpenPath`) on click, Enter or Space. Key repeat and IME composition never
 * activate it.
 */
function MdLink({ target, label, ctx, children }: MdLinkProps) {
  if (ctx.inLink) return <>{children}</>;

  let activate: (() => void) | null = null;
  let shown: string | null = null;
  if (target.kind === 'web') {
    const url = target.url;
    activate = () => ctx.openWeb(url);
    shown = target.host;
  } else if (target.kind === 'path' && ctx.onOpenPath) {
    const open = ctx.onOpenPath;
    const path = target.path;
    activate = () => open(path);
    shown = path;
  }

  if (!activate || shown === null) {
    return (
      <span className="md-link-inert">
        {children}
        <span className="md-sr-only"> ({ctx.t('link.notOpenable')})</span>
      </span>
    );
  }

  const run = activate;
  const onClick = (event: MouseEvent<HTMLElement>) => {
    event.preventDefault();
    run();
  };
  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.repeat || event.nativeEvent.isComposing || event.keyCode === 229) return;
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      run();
    }
  };
  const name = label.trim() === '' ? ctx.t('link.open', { host: shown }) : `${label}, ${ctx.t('link.open', { host: shown })}`;

  return (
    <a className="md-link" role="link" tabIndex={0} aria-label={name} onClick={onClick} onKeyDown={onKeyDown}>
      {children}
      {target.kind === 'web' || label !== shown ? <span className="md-link-host"> ({shown})</span> : null}
    </a>
  );
}
