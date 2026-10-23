//! Command-line redaction (contract §9.7) [F-03]: the one helper used by every
//! path that stores, logs, displays or exports an argv.
//!
//! Owner: T09.
//!
//! An argv may carry secrets: a teammate's pane command is
//! `sh -c "cd … && env ANTHROPIC_BASE_URL=… AWS_SECRET_ACCESS_KEY=… claude --settings '{…}' …"`,
//! and a prompt passed on the command line is the user's content. Nothing that
//! leaves the process — `proc_registry.argv`, a log line, Details,
//! `health_process_detail`, diagnostics — may hold any of it, so every such
//! path goes through [`redact_argv`] (or [`redact_command`] for one shell
//! string). `scripts/lint-forbidden.sh` bans formatting a raw argv elsewhere.
//!
//! The rules, applied to each argv element and — for a `sh -c` command string —
//! to each shell word (`shell-words` split; if the split fails, the whole string
//! becomes [`COMMAND_REDACTED`]):
//!
//! 1. an `env` assignment or any word matching `^[A-Za-z_][A-Za-z0-9_]*=` →
//!    `KEY=***` (the value is dropped);
//! 2. the value of `--settings` / `--settings=` (inline JSON or a path),
//!    `--mcp-config`, `--append-system-prompt`, `--system-prompt` → `***`;
//! 3. the value of any flag whose name matches
//!    `(?i)(_KEY|_TOKEN|_SECRET|_URL|PASSWORD|CREDENTIAL)` or starts with
//!    `AWS_`, `ANTHROPIC_`, `GOOGLE_`, `AZURE_`, `OPENAI_` (flag names are
//!    compared upper-cased with `-` read as `_`, so `--api-key` is `API_KEY`)
//!    → `***`;
//! 4. any word that looks like a credential (`Bearer …`, `sk-…`, `ghp_…`,
//!    `xox…-`, a 32+ character base64/hex run) → `***`;
//! 5. a positional prompt argument (`-- <prompt>`, `-p <text>`) → `<prompt>`
//!    (content). Every word after a bare `--` collapses into one `<prompt>`;
//!    a word holding a line break is typed text, never argv syntax, and is
//!    treated the same way.
//!
//! `argv[0]` is kept as given (the registry passes the executable's realpath
//! there), except that rule 4 still applies to it.
//!
//! The helper is total: it never fails and never panics, and its output never
//! contains a value that one of the rules removed. It deliberately errs on the
//! side of removing too much — a Details row that says `***` costs nothing,
//! a leaked key costs the user their account.

use std::sync::OnceLock;

use regex::Regex;

/// What a removed value is replaced with.
pub const REDACTED: &str = "***";
/// What a positional prompt is replaced with (rule 5).
pub const PROMPT: &str = "<prompt>";
/// A `sh -c` command string that could not be split into shell words.
pub const COMMAND_REDACTED: &str = "<command redacted>";

/// Flags whose value is always removed (rule 2).
const VALUE_FLAGS: [&str; 4] = [
    "--settings",
    "--mcp-config",
    "--append-system-prompt",
    "--system-prompt",
];

/// Flags whose next argument is a positional prompt (rule 5).
const PROMPT_FLAGS: [&str; 1] = ["-p"];

/// Shells whose `-c` argument is a command string (split into words).
const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];

/// Redact a whole argv (`argv[0]` first). See the module docs for the rules.
pub fn redact_argv<S: AsRef<str>>(argv: &[S]) -> Vec<String> {
    let words: Vec<&str> = argv.iter().map(|s| s.as_ref()).collect();
    let mut out = Vec::with_capacity(words.len());
    let Some((first, rest)) = words.split_first() else {
        return out;
    };
    out.push(redact_credential_word(first));
    let shell = is_shell(first);
    let mut i = 0usize;
    while i < rest.len() {
        let word = rest[i];
        // A shell's command string: split and redact word by word.
        if shell && (word == "-c" || is_combined_c_flag(word)) {
            out.push(word.to_string());
            if let Some(cmd) = rest.get(i + 1) {
                out.push(redact_command(cmd));
                // Arguments after the command string are `$0`, `$1`, …:
                // positional content as far as we can tell.
                if i + 2 < rest.len() {
                    out.push(PROMPT.to_string());
                }
            }
            break;
        }
        i += redact_word_at(rest, i, &mut out);
    }
    out
}

/// Redact one shell command string (the `sh -c` argument): split into shell
/// words, apply the rules to each, and join them back with shell quoting.
/// A string that does not split becomes [`COMMAND_REDACTED`].
pub fn redact_command(command: &str) -> String {
    let Ok(words) = shell_words::split(command) else {
        return COMMAND_REDACTED.to_string();
    };
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    let mut out: Vec<String> = Vec::with_capacity(refs.len());
    let mut i = 0usize;
    while i < refs.len() {
        i += redact_word_at(&refs, i, &mut out);
    }
    shell_words::join(out)
}

/// The redacted argv as one display line (each element shell-quoted).
pub fn redact_display<S: AsRef<str>>(argv: &[S]) -> String {
    shell_words::join(redact_argv(argv))
}

/// Redact the word at `words[i]` (and, for a flag with a separate value, the
/// value after it) into `out`. Returns how many words were consumed (≥ 1).
fn redact_word_at(words: &[&str], i: usize, out: &mut Vec<String>) -> usize {
    let word = words[i];

    // Rule 5: everything after a bare `--` is the prompt.
    if word == "--" {
        out.push(word.to_string());
        if i + 1 < words.len() {
            out.push(PROMPT.to_string());
        }
        return words.len() - i;
    }

    // Rule 1: an assignment keeps its name only.
    if let Some(name) = assignment_name(word) {
        out.push(format!("{name}={REDACTED}"));
        return 1;
    }

    if let Some(flag) = flag_name(word) {
        // `--flag=value`.
        if let Some((name, value)) = word.split_once('=') {
            if flag_value_is_secret(name) {
                out.push(format!("{name}={REDACTED}"));
            } else {
                out.push(format!("{name}={}", redact_credential_word(value)));
            }
            return 1;
        }
        // Rule 5: `-p <text>`.
        if PROMPT_FLAGS.contains(&word) {
            out.push(word.to_string());
            if let Some(next) = words.get(i + 1) {
                if flag_name(next).is_none() {
                    out.push(PROMPT.to_string());
                    return 2;
                }
            }
            return 1;
        }
        // Rules 2 and 3: `--flag value`.
        if flag_value_is_secret(flag) {
            out.push(word.to_string());
            if let Some(next) = words.get(i + 1) {
                if flag_name(next).is_none() || VALUE_FLAGS.contains(&flag) {
                    out.push(REDACTED.to_string());
                    return 2;
                }
            }
            return 1;
        }
        out.push(redact_credential_word(word));
        return 1;
    }

    if word.contains('\n') || word.contains('\r') {
        out.push(PROMPT.to_string());
        return 1;
    }
    out.push(redact_credential_word(word));
    1
}

/// `NAME` of a `NAME=value` word (rule 1).
fn assignment_name(word: &str) -> Option<&str> {
    let (name, _) = word.split_once('=')?;
    let mut bytes = name.bytes();
    let first = bytes.next()?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    if bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        Some(name)
    } else {
        None
    }
}

/// The flag part of a word that starts with `-` (not a bare `-` or `--`).
fn flag_name(word: &str) -> Option<&str> {
    if !word.starts_with('-') || word == "-" || word == "--" {
        return None;
    }
    // A negative number is not a flag.
    if word[1..].starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(word.split_once('=').map(|(n, _)| n).unwrap_or(word))
}

/// Whether the value of flag `name` (with its dashes) must be removed
/// (rules 2 and 3).
fn flag_value_is_secret(name: &str) -> bool {
    if VALUE_FLAGS.contains(&name) {
        return true;
    }
    let bare = name.trim_start_matches('-');
    if bare.is_empty() {
        return false;
    }
    let upper: String = bare
        .chars()
        .map(|c| if c == '-' { '_' } else { c.to_ascii_uppercase() })
        .collect();
    is_secret_name(&upper)
}

/// Rule 3 on an upper-cased name (`-` already read as `_`).
pub fn is_secret_name(upper: &str) -> bool {
    const INFIXES: [&str; 6] = ["_KEY", "_TOKEN", "_SECRET", "_URL", "PASSWORD", "CREDENTIAL"];
    const PREFIXES: [&str; 5] = ["AWS_", "ANTHROPIC_", "GOOGLE_", "AZURE_", "OPENAI_"];
    const EXACT: [&str; 5] = ["KEY", "TOKEN", "SECRET", "APIKEY", "AUTH"];
    INFIXES.iter().any(|s| upper.contains(s))
        || PREFIXES.iter().any(|p| upper.starts_with(p))
        || EXACT.contains(&upper)
}

/// Rule 4: a word (or a part of it) that looks like a credential becomes `***`.
fn redact_credential_word(word: &str) -> String {
    let explicit = explicit_regex().replace_all(word, regex::NoExpand(REDACTED));
    run_regex()
        .replace_all(&explicit, |caps: &regex::Captures<'_>| {
            let run = &caps[0];
            if looks_like_credential_run(run) {
                REDACTED.to_string()
            } else {
                run.to_string()
            }
        })
        .into_owned()
}

/// A 32+ character base64/hex run that is not a path or an identifier.
fn looks_like_credential_run(run: &str) -> bool {
    let core = run.trim_end_matches('=');
    if core.len() >= 32 && core.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    if run.starts_with(['/', '~', '.']) {
        return false;
    }
    let has_digit = run.bytes().any(|b| b.is_ascii_digit());
    let has_upper = run.bytes().any(|b| b.is_ascii_uppercase());
    let has_lower = run.bytes().any(|b| b.is_ascii_lowercase());
    has_digit && has_upper && has_lower
}

fn explicit_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"sk-[A-Za-z0-9_-]{8,}",
            r"|ghp_[A-Za-z0-9]{8,}",
            r"|gh[ousr]_[A-Za-z0-9]{8,}",
            r"|xox[a-z]?-[A-Za-z0-9-]+",
            r"|(?i:bearer)\s*\S+",
        ))
        .expect("static credential regex compiles")
    })
}

fn run_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9+/_=-]{32,}").expect("static run regex compiles"))
}

fn is_shell(argv0: &str) -> bool {
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    let base = base.strip_prefix('-').unwrap_or(base);
    SHELLS.contains(&base)
}

/// `-lc`, `-ec`, `-xc` … — a combined short-flag cluster ending in `c`.
fn is_combined_c_flag(word: &str) -> bool {
    word.len() > 2
        && word.starts_with('-')
        && !word.starts_with("--")
        && word.ends_with('c')
        && word[1..].bytes().all(|b| b.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    const CANARIES: [&str; 6] = [
        "AKIAIOSFODNN7EXAMPLEwJalrXUtnFEMI",
        "https://proxy.internal.example",
        "apiKeyHelper",
        "sk-ant-api03-abcdefghijklmnop",
        "please fix the login bug",
        "hunter2",
    ];

    fn assert_clean(out: &[String]) {
        let joined = out.join(" ");
        for c in CANARIES {
            assert!(!joined.contains(c), "{c:?} leaked into {joined:?}");
        }
    }

    #[test]
    fn rule1_assignments_keep_only_their_name() {
        let out = redact_argv(&s(&[
            "/usr/bin/env",
            "AWS_SECRET_ACCESS_KEY=AKIAIOSFODNN7EXAMPLEwJalrXUtnFEMI",
            "ANTHROPIC_BASE_URL=https://proxy.internal.example",
            "FOO=hunter2",
            "/bin/claude",
        ]));
        assert_eq!(
            out,
            s(&[
                "/usr/bin/env",
                "AWS_SECRET_ACCESS_KEY=***",
                "ANTHROPIC_BASE_URL=***",
                "FOO=***",
                "/bin/claude"
            ])
        );
    }

    #[test]
    fn rule2_settings_and_prompt_flags_lose_their_value() {
        let out = redact_argv(&s(&[
            "/x/claude",
            "--settings",
            r#"{"apiKeyHelper":"/bin/cat ~/.key"}"#,
            "--mcp-config=/tmp/mcp.json",
            "--append-system-prompt",
            "be nice",
            "--model",
            "opus",
        ]));
        assert_eq!(
            out,
            s(&[
                "/x/claude",
                "--settings",
                "***",
                "--mcp-config=***",
                "--append-system-prompt",
                "***",
                "--model",
                "opus"
            ])
        );
        assert_clean(&out);
    }

    #[test]
    fn rule3_secret_named_flags() {
        let out = redact_argv(&s(&[
            "/x/tool",
            "--api-key",
            "hunter2",
            "--base-url=https://proxy.internal.example",
            "--password",
            "hunter2",
            "--anthropic-model",
            "hunter2",
            "--verbose",
        ]));
        assert_eq!(
            out,
            s(&[
                "/x/tool",
                "--api-key",
                "***",
                "--base-url=***",
                "--password",
                "***",
                "--anthropic-model",
                "***",
                "--verbose"
            ])
        );
    }

    #[test]
    fn rule4_credential_shaped_words() {
        let out = redact_argv(&s(&[
            "/x/curl",
            "-H",
            "Authorization: Bearer abc.def",
            "sk-ant-api03-abcdefghijklmnop",
            "ghp_0123456789abcdefghij",
            "xoxb-1234-5678-abc",
            "0123456789abcdef0123456789abcdef01",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "short",
        ]));
        assert_eq!(out[0], "/x/curl");
        assert_eq!(out[1], "-H");
        assert!(out[2].ends_with(REDACTED) && !out[2].contains("abc.def"));
        assert_eq!(&out[3..8], &s(&["***", "***", "***", "***", "***"])[..]);
        assert_eq!(out[8], "short");
    }

    #[test]
    fn rule4_leaves_paths_uuids_and_identifiers_alone() {
        let argv = s(&[
            "/Users/someone/Documents/projects/a-long-directory-name/bin/claude.exe",
            "--resume",
            "0b3c1d2e-6c4f-4f6a-9d1e-2b7f8a9c0d1e",
            "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS",
            "--dangerously-skip-permissions",
        ]);
        assert_eq!(redact_argv(&argv), argv);
    }

    #[test]
    fn rule5_prompts() {
        let out = redact_argv(&s(&["/x/claude", "-p", "please fix the login bug", "--model", "x"]));
        assert_eq!(out, s(&["/x/claude", "-p", PROMPT, "--model", "x"]));
        let out = redact_argv(&s(&["/x/codex", "exec", "--", "please fix", "the login bug"]));
        assert_eq!(out, s(&["/x/codex", "exec", "--", PROMPT]));
        let out = redact_argv(&s(&["/x/agy", "line one\nline two"]));
        assert_eq!(out, s(&["/x/agy", PROMPT]));
    }

    #[test]
    fn sh_c_commands_are_split_and_redacted_word_by_word() {
        let cmd = "cd /Users/me/repo && env CLAUDECODE=1 ANTHROPIC_BASE_URL=https://proxy.internal.example \
                   AWS_SECRET_ACCESS_KEY=AKIAIOSFODNN7EXAMPLEwJalrXUtnFEMI /x/claude --agent-id a@t \
                   --agent-name a --team-name t --settings '{\"apiKeyHelper\":\"x\"}' -p 'please fix the login bug'";
        let out = redact_argv(&s(&["/bin/sh", "-c", cmd]));
        assert_eq!(out[0], "/bin/sh");
        assert_eq!(out[1], "-c");
        assert_eq!(out.len(), 3);
        assert_clean(&out);
        let words = shell_words::split(&out[2]).expect("re-joined command splits");
        assert!(words.contains(&"--agent-id".to_string()));
        assert!(words.contains(&"a@t".to_string()), "labels survive: {words:?}");
        assert!(words.contains(&"CLAUDECODE=***".to_string()));
        assert!(words.contains(&"&&".to_string()));
    }

    #[test]
    fn an_unsplittable_command_is_redacted_whole() {
        let out = redact_argv(&s(&["/bin/sh", "-c", "echo 'unterminated AWS_SECRET=x"]));
        assert_eq!(out, s(&["/bin/sh", "-c", COMMAND_REDACTED]));
        let out = redact_argv(&s(&["/bin/zsh", "-lc", "FOO=hunter2 run", "arg0", "please fix the login bug"]));
        assert_eq!(out, s(&["/bin/zsh", "-lc", "'FOO=***' run", PROMPT]));
        assert_clean(&out);
    }

    #[test]
    fn harmless_argv_is_unchanged() {
        let argv = s(&["/x/claude", "--resume", "0b3c1d2e", "--permission-mode", "default", "daemon"]);
        assert_eq!(redact_argv(&argv), argv);
        assert!(redact_argv::<String>(&[]).is_empty());
    }

    #[test]
    fn display_is_one_quoted_line() {
        let line = redact_display(&s(&["/x/claude", "-p", "hi there", "FOO=bar"]));
        assert_eq!(line, "/x/claude -p '<prompt>' 'FOO=***'");
    }
}
