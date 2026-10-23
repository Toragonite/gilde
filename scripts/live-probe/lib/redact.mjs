/**
 * Command-line redaction for everything the kit records (contract §9.7
 * "Command-line redaction", F-03), mirrored from the core's rules:
 *  1. an `env` assignment or any word `KEY=value` → `KEY=***`;
 *  2. the value of `--settings`, `--mcp-config`, `--append-system-prompt`,
 *     `--system-prompt` → `***`;
 *  3. the value of a flag or assignment whose name matches
 *     `(_KEY|_TOKEN|_SECRET|_URL|PASSWORD|CREDENTIAL)` or starts with `AWS_`,
 *     `ANTHROPIC_`, `GOOGLE_`, `AZURE_`, `OPENAI_` → `***`;
 *  4. a word that looks like a credential (`Bearer …`, `sk-…`, `ghp_…`,
 *     `xox?-…`, a 32+ character base64/hex run) → `***`;
 *  5. a positional prompt (after `--`, the value of `-p <text>`, or any
 *     positional word of a claude argv) → `<prompt>`.
 * A `sh -c` command string is split into shell words and each word redacted;
 * if it cannot be split, the whole string becomes `<command redacted>`.
 */

const VALUE_SECRET_FLAGS = new Set(['--settings', '--mcp-config', '--append-system-prompt', '--system-prompt']);
const SECRET_NAME = /(_KEY|_TOKEN|_SECRET|_URL|PASSWORD|CREDENTIAL)/i;
const SECRET_PREFIX = /^(AWS_|ANTHROPIC_|GOOGLE_|AZURE_|OPENAI_)/i;
const CREDENTIAL = /(^Bearer$|^sk-[A-Za-z0-9_-]{6,}|^ghp_[A-Za-z0-9]{8,}|^xox[a-z]-|[A-Za-z0-9+/=]{32,})/;
/** Paths are not credentials (a long path segment chain would otherwise look like a base64 run). */
const PATH_LIKE = /^[/~.]/;
const ASSIGNMENT = /^([A-Za-z_][A-Za-z0-9_]*)=(.*)$/s;

/** Flags of the claude CLI that take a value (everything else is a switch). */
export const CLAUDE_VALUE_FLAGS = new Set([
  '--input-format', '--output-format', '--session-id', '--model', '--permission-mode', '--permission-prompt-tool',
  '--resume', '--settings', '--teammate-mode', '--agent-id', '--agent-name', '--team-name', '--agent-color',
  '--parent-session-id', '--permission-prompts', '--mcp-config', '--append-system-prompt', '--system-prompt',
  '--add-dir', '--fork-session-id', '--max-turns', '--allowedTools', '--disallowedTools',
]);

const secretName = (name) => SECRET_NAME.test(name) || SECRET_PREFIX.test(name);

/** Split a POSIX shell command into words (quotes and backslashes), or null when it is malformed. */
export function shellWords(command) {
  const words = [];
  let cur = '';
  let inWord = false;
  let i = 0;
  while (i < command.length) {
    const c = command[i];
    if (c === "'") {
      const end = command.indexOf("'", i + 1);
      if (end < 0) return null;
      cur += command.slice(i + 1, end);
      inWord = true;
      i = end + 1;
    } else if (c === '"') {
      let j = i + 1;
      let buf = '';
      for (; j < command.length && command[j] !== '"'; j++) {
        if (command[j] === '\\' && j + 1 < command.length && '"\\$`'.includes(command[j + 1])) {
          buf += command[j + 1];
          j++;
        } else buf += command[j];
      }
      if (j >= command.length) return null;
      cur += buf;
      inWord = true;
      i = j + 1;
    } else if (c === '\\') {
      if (i + 1 >= command.length) return null;
      cur += command[i + 1];
      inWord = true;
      i += 2;
    } else if (/\s/.test(c)) {
      if (inWord) words.push(cur);
      cur = '';
      inWord = false;
      i++;
    } else {
      cur += c;
      inWord = true;
      i++;
    }
  }
  if (inWord) words.push(cur);
  return words;
}

/** One word without context (rules 1, 3, 4). */
function redactWord(word) {
  const a = ASSIGNMENT.exec(word);
  if (a) return `${a[1]}=***`;
  if (word.startsWith('--') && word.includes('=')) {
    const [flag] = word.split('=', 1);
    if (VALUE_SECRET_FLAGS.has(flag) || secretName(flag.slice(2))) return `${flag}=***`;
  }
  if (!PATH_LIKE.test(word) && CREDENTIAL.test(word)) return '***';
  return word;
}

/**
 * Redact one argv. `claude` (default true) treats unconsumed positional words
 * as prompts (rule 5). `sh -c <string>` is redacted word by word.
 */
export function redactArgv(argv, { claude = true } = {}) {
  const out = [];
  for (let i = 0; i < argv.length; i++) {
    const word = String(argv[i]);
    if (i === 0) {
      out.push(word);
      continue;
    }
    if (word === '-c' && /(^|\/)(ba|z|da)?sh$/.test(String(argv[0])) && i + 1 < argv.length) {
      out.push('-c', redactCommand(String(argv[i + 1])));
      i++;
      continue;
    }
    if (word === '--') {
      out.push('--');
      if (i + 1 < argv.length) out.push('<prompt>');
      break;
    }
    if (VALUE_SECRET_FLAGS.has(word) && i + 1 < argv.length) {
      out.push(word, '***');
      i++;
      continue;
    }
    if (word.startsWith('--') && !word.includes('=') && secretName(word.slice(2)) && i + 1 < argv.length) {
      out.push(word, '***');
      i++;
      continue;
    }
    if (word === '-p' && i + 1 < argv.length && !String(argv[i + 1]).startsWith('-')) {
      out.push('-p', '<prompt>');
      i++;
      continue;
    }
    if (claude && CLAUDE_VALUE_FLAGS.has(word) && i + 1 < argv.length) {
      out.push(word, redactWord(String(argv[i + 1])));
      i++;
      continue;
    }
    if (claude && !word.startsWith('-') && !ASSIGNMENT.test(word)) {
      out.push('<prompt>');
      continue;
    }
    out.push(redactWord(word));
  }
  return out;
}

/** Redact a shell command string word by word (`env` prefixes keep their key names). */
export function redactCommand(command) {
  const words = shellWords(command);
  if (words === null) return '<command redacted>';
  const out = [];
  let claudeAt = -1;
  for (let i = 0; i < words.length; i++) {
    const w = words[i];
    if (/(^|\/)claude(\.exe)?$/.test(w)) {
      claudeAt = i;
      break;
    }
    out.push(redactWord(w));
  }
  if (claudeAt >= 0) out.push(...redactArgv(words.slice(claudeAt)));
  return out.join(' ');
}
