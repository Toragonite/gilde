/**
 * The one place an agent CLI gets its user-visible name (F13).
 *
 * Internal ids differ by layer — `cursorAgent` (binary detection), `cursor`
 * (dispatch vendor), `cursor-agent` (the CLI), `agy` / `gemini` (the
 * orchestrator calls the Antigravity CLI `gemini`) — but a person reads one
 * name per product. Unknown ids are shown as they are, never hidden.
 */

const NAMES: Record<string, string> = {
  claude: 'Claude Code',
  codex: 'Codex',
  cursor: 'Cursor',
  cursorAgent: 'Cursor',
  'cursor-agent': 'Cursor',
  agy: 'Antigravity',
  gemini: 'Antigravity',
  antigravity: 'Antigravity',
  shell: 'Shell',
};

/** `claude` → `Claude Code`, `cursorAgent` → `Cursor`, `gemini`/`agy` → `Antigravity`. */
export function agentDisplayName(id: string | null | undefined): string {
  if (typeof id !== 'string' || id === '') return '';
  return NAMES[id] ?? id;
}
