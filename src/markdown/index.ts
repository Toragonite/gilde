/** The AST markdown renderer (contract §9.1). */

export { MAX_RENDER_DEPTH, Markdown } from './Markdown';
export type { MarkdownProps } from './Markdown';
export {
  collectDefinitions,
  linkTarget,
  MARKDOWN_LIMITS,
  MAX_URL_LENGTH,
  parseMarkdown,
  plainText,
  plainTextReason,
} from './parse';
export type { LinkTarget } from './parse';
