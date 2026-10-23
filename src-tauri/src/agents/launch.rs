//! Launch templates — DESIGN §6-6, M1-PLAN T09.6–T09.8.
//!
//! One template per agent the app can start in the **interactive** (PTY) lane.
//! The headless lane never comes through here; that argv knowledge lives inside
//! the agent-mesh adapters and is not ours to duplicate (DESIGN §0 rule 1).
//!
//! ## What a template is for
//!
//! `pty::manager` needs four things to open a session: an absolute binary, the
//! interactive argv, which environment variable carries the account directory,
//! and how a first prompt reaches the CLI. That is the whole struct.
//!
//! ## `Argv` is the default because it has no injection surface
//!
//! `CommandBuilder` takes an argv **array** and there is no shell in the path,
//! so a prompt beginning with `--model` cannot hijack parsing. `TypeAfterReady`
//! is strictly worse — it depends on recognising a ready prompt, and the text it
//! types is indistinguishable from what the user types.
//!
//! ## Measured on this machine, 2026-09-07 (M1-PLAN T09.7)
//!
//! DESIGN shipped `claude -- "<prompt>"` and `codex -- "<prompt>"` as
//! `verified: false`, because the `--` fence had only been confirmed for the
//! *headless* argv the adapters build. Both were then exercised for real, in a
//! throw-away directory, over a real pty:
//!
//! * `claude 2.1.263` — the TUI came up, the prompt appeared as the first turn
//!   (`❯ AOPROBE …`) and the model answered. **Verified.**
//! * `codex-cli 0.145.0` — same, `› AOPROBE …` followed by the answer.
//!   **Verified.**
//!
//! So both are promoted to `verified: true` here, and DECISIONS §M1 세부 기본값
//! item 4 (fall back to `TypeAfterReady` if argv delivery fails) does **not**
//! trigger. `agy`, `cursor-agent` and `gemini` stay `TypeAfterReady`; `agy`
//! deliberately carries **no `-p`** (M1-PLAN T09.6) — `-p` is its headless
//! one-shot flag and putting it in a TUI defeats the interactive lane.

use serde::{Deserialize, Serialize};

use crate::agents::{AgentBinary, AgentKind};
use crate::config::model::{AppConfig, LaunchOverride};

/// How a prompt reaches an interactive CLI (DESIGN §6-6).
///
/// Re-exported from `config::model` rather than redefined: the launch template
/// and `config.agents.launchOverrides.<agent>.promptMode` must be the same
/// closed set, or an override could name a mode the template cannot express.
pub use crate::config::model::PromptMode;

/// Placeholder substituted in [`LaunchTemplate::prompt_argv`].
pub const PROMPT_PLACEHOLDER: &str = "{PROMPT}";

/// Placeholder substituted in [`LaunchTemplate::resume_args`] (M2 slot).
pub const CONVERSATION_PLACEHOLDER: &str = "{ID}";

/// Everything `pty::manager` needs to start one agent (DESIGN §6-6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchTemplate {
    pub agent: AgentKind,
    /// Absolute path resolved by [`crate::agents::detect`]. Never a bare name —
    /// DESIGN §6-1 and identity fence 2 both depend on that.
    pub binary: String,
    /// Interactive TUI argv. Never `-p`.
    pub args: Vec<String>,
    pub account_env_key: Option<String>,
    pub prompt_mode: PromptMode,
    /// Template carrying [`PROMPT_PLACEHOLDER`].
    pub prompt_argv: Option<Vec<String>>,
    /// M2 slot, populated now so no migration is needed later.
    pub resume_args: Option<Vec<String>>,
    /// `false` ⇒ the UI marks this launch path "unverified on this machine".
    pub verified: bool,
}

impl LaunchTemplate {
    /// Full argv for an interactive session with no initial prompt.
    pub fn interactive_argv(&self) -> Vec<String> {
        let mut argv = vec![self.binary.clone()];
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// Full argv for an interactive session that starts with `prompt`.
    ///
    /// `None` when this agent cannot take a prompt on argv — the caller then
    /// opens the session bare and types the prompt after the ready gate
    /// (DESIGN §6-5 `session_send_prompt`).
    pub fn argv_with_prompt(&self, prompt: &str) -> Option<Vec<String>> {
        if self.prompt_mode != PromptMode::Argv {
            return None;
        }
        let template = self.prompt_argv.as_ref()?;
        let mut argv = self.interactive_argv();
        argv.extend(
            template
                .iter()
                .map(|part| part.replace(PROMPT_PLACEHOLDER, prompt)),
        );
        Some(argv)
    }

    /// M2 slot: full argv to resume `conversation_id`.
    pub fn argv_for_resume(&self, conversation_id: &str) -> Option<Vec<String>> {
        let template = self.resume_args.as_ref()?;
        let mut argv = self.interactive_argv();
        argv.extend(
            template
                .iter()
                .map(|part| part.replace(CONVERSATION_PLACEHOLDER, conversation_id)),
        );
        Some(argv)
    }
}

// ---------------------------------------------------------------------------
// the table
// ---------------------------------------------------------------------------

/// The immutable half of a template — everything that does not depend on where
/// the binary happens to live.
struct TemplateSpec {
    args: &'static [&'static str],
    prompt_mode: PromptMode,
    prompt_argv: Option<&'static [&'static str]>,
    resume_args: Option<&'static [&'static str]>,
    verified: bool,
}

/// DESIGN §6-6, with the two promotions measured in T09 (see the module docs).
fn spec_for(kind: AgentKind) -> TemplateSpec {
    match kind {
        AgentKind::Claude => TemplateSpec {
            args: &[],
            prompt_mode: PromptMode::Argv,
            prompt_argv: Some(&["--", PROMPT_PLACEHOLDER]),
            resume_args: Some(&["--resume", CONVERSATION_PLACEHOLDER]),
            // Measured 2026-09-07 against claude 2.1.263 over a real pty.
            verified: true,
        },
        AgentKind::Codex => TemplateSpec {
            args: &[],
            prompt_mode: PromptMode::Argv,
            prompt_argv: Some(&["--", PROMPT_PLACEHOLDER]),
            resume_args: Some(&["resume", CONVERSATION_PLACEHOLDER]),
            // Measured 2026-09-07 against codex-cli 0.145.0 over a real pty.
            verified: true,
        },
        AgentKind::Antigravity => TemplateSpec {
            // No `-p`: that is agy's headless one-shot flag (M1-PLAN T09.6).
            args: &[],
            prompt_mode: PromptMode::TypeAfterReady,
            prompt_argv: None,
            resume_args: Some(&["--conversation", CONVERSATION_PLACEHOLDER]),
            // `AGY_HOME` is an upstream guess and the ready gate is unmeasured.
            verified: false,
        },
        AgentKind::Cursor => TemplateSpec {
            args: &[],
            prompt_mode: PromptMode::TypeAfterReady,
            prompt_argv: None,
            resume_args: Some(&["--resume", CONVERSATION_PLACEHOLDER]),
            // Not installed on this machine — nothing about it is measured.
            verified: false,
        },
        AgentKind::GeminiCli => TemplateSpec {
            args: &[],
            prompt_mode: PromptMode::TypeAfterReady,
            prompt_argv: None,
            resume_args: None,
            verified: false,
        },
        AgentKind::Shell => TemplateSpec {
            args: &["-l"],
            prompt_mode: PromptMode::None,
            prompt_argv: None,
            resume_args: None,
            verified: true,
        },
    }
}

/// Build one template per **installed** agent, applying
/// `config.agents.launchOverrides.<agent>` on top (M1-PLAN T09.8).
///
/// Agents with no resolved binary are omitted rather than emitted with an empty
/// `binary`: DESIGN §6-1 requires an absolute path, and a template that cannot
/// honour that invariant is not a launchable thing. The UI learns what is
/// missing from `agents_detect`, which reports every kind including the
/// uninstalled ones.
pub fn templates(cfg: &AppConfig, bins: &[AgentBinary]) -> Vec<LaunchTemplate> {
    bins.iter()
        .filter_map(|bin| template_for(cfg, bin))
        .collect()
}

/// One template, or `None` when there is no binary to launch.
pub fn template_for(cfg: &AppConfig, bin: &AgentBinary) -> Option<LaunchTemplate> {
    let kind = bin.kind;
    let spec = spec_for(kind);
    let override_ = cfg.agents.launch_overrides.get(kind.key());

    let binary = override_
        .and_then(|o| o.binary.clone())
        .filter(|b| !b.trim().is_empty())
        .or_else(|| bin.abs_path.as_ref().map(|p| p.to_string_lossy().into_owned()))?;

    let mut template = LaunchTemplate {
        agent: kind,
        binary,
        args: spec.args.iter().map(|s| (*s).to_string()).collect(),
        account_env_key: kind.account_env_key().map(str::to_string),
        prompt_mode: spec.prompt_mode,
        prompt_argv: spec
            .prompt_argv
            .map(|v| v.iter().map(|s| (*s).to_string()).collect()),
        resume_args: spec
            .resume_args
            .map(|v| v.iter().map(|s| (*s).to_string()).collect()),
        verified: spec.verified,
    };

    if let Some(over) = override_ {
        apply_override(&mut template, over);
    }
    Some(template)
}

/// Overlay one `launchOverrides` entry.
///
/// Anything the user re-specifies about *prompt delivery* clears `verified`:
/// our measurement was of the shipped argv, and silently keeping the badge on a
/// path nobody has run would be exactly the dishonesty DESIGN §0 rule 7 exists
/// to prevent. Overriding only `args` or `binary` leaves it alone.
fn apply_override(template: &mut LaunchTemplate, over: &LaunchOverride) {
    if let Some(args) = &over.args {
        template.args = args.clone();
    }
    if let Some(mode) = over.prompt_mode {
        if mode != template.prompt_mode {
            template.prompt_mode = mode;
            template.verified = false;
        }
    }
    if let Some(prompt_argv) = &over.prompt_argv {
        let replacement = if prompt_argv.is_empty() {
            None
        } else {
            Some(prompt_argv.clone())
        };
        if replacement != template.prompt_argv {
            template.prompt_argv = replacement;
            template.verified = false;
        }
    }
    if let Some(resume_args) = &over.resume_args {
        template.resume_args = if resume_args.is_empty() {
            None
        } else {
            Some(resume_args.clone())
        };
    }
}

// ---------------------------------------------------------------------------
// Gilde 2 argv builders (contract §6.1.1, §6.2.1)
// ---------------------------------------------------------------------------
//
// The tables of §6.1.1 and §6.2.1, exactly. `argv[0]` is always the resolved,
// checked absolute path of the vendor binary (or `/bin/sh` for a hosted pane);
// the supervisor's allowlist refuses anything else (§9.3). Every value that
// lands on argv is checked here first: an id that could be read as a flag, a
// model outside the model rule, or a control character never reaches a
// command line.

use crate::contract::common::{AbsPath, PermissionMode};
use crate::contract::process::ProcKind;
use crate::contract::IpcError;

/// How a Conversation-mode claude starts (§6.1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoLaunch {
    /// `--session-id <uuid v4 minted by Gilde>`.
    New { session_id: String },
    /// `--resume <sessionId>`.
    Continue { session_id: String },
    /// `--resume <sessionId> --fork-session`.
    Copy { session_id: String },
    /// `--resume <absolute transcript path> --fork-session` (a copy until
    /// LP-25 can run, §6.5).
    OtherAccount { transcript: AbsPath },
}

/// Everything §6.1.1's argv depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvoArgs {
    pub launch: ConvoLaunch,
    /// Always passed (default `default`) [R6 §1.5].
    pub permission_mode: PermissionMode,
    pub model: Option<String>,
}

/// The argv tail (after `<claude>`) of a Conversation-mode session (§6.1.1):
/// `-p --input-format stream-json --output-format stream-json --verbose
/// --include-partial-messages --replay-user-messages --permission-prompt-tool
/// stdio --permission-prompts host` + the launch flags + `--permission-mode
/// <m>` [+ `--model <m>`] [+ `--allow-dangerously-skip-permissions` with
/// `bypassPermissions` chosen at start].
pub fn conversation_args(args: &ConvoArgs) -> Result<Vec<String>, IpcError> {
    let mut out: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--replay-user-messages",
        "--permission-prompt-tool",
        "stdio",
        "--permission-prompts",
        "host",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    match &args.launch {
        ConvoLaunch::New { session_id } => {
            check_uuid("sessionId", session_id)?;
            out.extend(["--session-id".to_string(), session_id.clone()]);
        }
        ConvoLaunch::Continue { session_id } => {
            check_id("sessionId", session_id)?;
            out.extend(["--resume".to_string(), session_id.clone()]);
        }
        ConvoLaunch::Copy { session_id } => {
            check_id("sessionId", session_id)?;
            out.extend([
                "--resume".to_string(),
                session_id.clone(),
                "--fork-session".to_string(),
            ]);
        }
        ConvoLaunch::OtherAccount { transcript } => {
            out.extend([
                "--resume".to_string(),
                transcript.as_str().to_string(),
                "--fork-session".to_string(),
            ]);
        }
    }
    push_mode_and_model(&mut out, args.permission_mode, args.model.as_deref())?;
    if args.permission_mode == PermissionMode::BypassPermissions {
        out.push("--allow-dangerously-skip-permissions".to_string());
    }
    Ok(out)
}

/// How a first prompt reaches a Terminal-mode CLI (§6.2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirstPrompt {
    /// No prompt.
    None,
    /// On argv (already in the args).
    Argv,
    /// `pty_paste(text, submit = true)` once the session is ready (§6.2.2).
    TypedAfterReady(String),
}

/// One Terminal-mode launch: what to hand `PtyApi::open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalArgv {
    /// `Pty`, `Attach` or `Pane`.
    pub kind: ProcKind,
    pub argv0: AbsPath,
    pub args: Vec<String>,
    pub prompt: FirstPrompt,
}

/// The rows of §6.2.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalLaunch {
    /// `<claude> --session-id <uuid> --permission-mode <m> [--model m] [-- <prompt>]`.
    ClaudeNew {
        session_id: String,
        permission_mode: PermissionMode,
        model: Option<String>,
        prompt: Option<String>,
    },
    /// `<claude> --resume <id> [--fork-session] --permission-mode <m> [--model m]`;
    /// the prompt is typed after ready.
    ClaudeResume {
        session_id: String,
        fork: bool,
        permission_mode: PermissionMode,
        model: Option<String>,
        prompt: Option<String>,
    },
    /// `<claude> --teammate-mode tmux --session-id <uuid> --permission-mode <m>
    /// --settings '{"permissions":{"defaultMode":"<m>"}}' [--model m] [-- <prompt>]`
    /// (+ the team env of §7.4.1, composed by the caller).
    ClaudeHostedLead {
        session_id: String,
        permission_mode: PermissionMode,
        model: Option<String>,
        prompt: Option<String>,
    },
    /// `<claude> attach <sessionId>` [R6 §6 LP-22].
    ClaudeAttach { session_id: String },
    /// The claude sign-in TUI: `<claude> --permission-mode default`, then
    /// `/login` typed after ready (§4.2.7) [F-44].
    ClaudeSignIn,
    /// `<codex> [resume <id>] [--model <m>] [<prompt>]` (argv when new, typed
    /// after ready when resuming).
    Codex {
        resume: Option<String>,
        model: Option<String>,
        prompt: Option<String>,
    },
    /// `codex login`.
    CodexSignIn,
    /// `<cursor-agent> [--resume <chatId>] [--model <m>]`; prompt typed after
    /// ready [R1 §3 Bug].
    Cursor {
        resume: Option<String>,
        model: Option<String>,
        prompt: Option<String>,
    },
    /// `cursor-agent login`.
    CursorSignIn,
    /// `<agy> [--conversation <id>]`; prompt typed after ready.
    Agy {
        conversation: Option<String>,
        prompt: Option<String>,
    },
    /// The agy TUI (it signs in from there).
    AgySignIn,
    /// A hosted pane: `/bin/sh -c <command string from respawn-pane>` (§7.4.5),
    /// used as given.
    Pane { command: String },
}

/// The one shell a hosted pane runs in (§7.4.5, §9.3).
pub const PANE_SHELL: &str = "/bin/sh";

/// Build one Terminal-mode launch (§6.2.1). `binary` is the resolved,
/// checked absolute path of the vendor binary (ignored for a pane, which
/// always runs `/bin/sh`).
pub fn terminal_argv(binary: &AbsPath, launch: &TerminalLaunch) -> Result<TerminalArgv, IpcError> {
    let mut args: Vec<String> = Vec::new();
    let mut prompt = FirstPrompt::None;
    let mut kind = ProcKind::Pty;
    let mut argv0 = binary.clone();
    let typed = |text: &Option<String>| match text.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(t) => FirstPrompt::TypedAfterReady(t.to_string()),
        None => FirstPrompt::None,
    };
    match launch {
        TerminalLaunch::ClaudeNew {
            session_id,
            permission_mode,
            model,
            prompt: text,
        } => {
            check_uuid("sessionId", session_id)?;
            args.extend(["--session-id".to_string(), session_id.clone()]);
            push_mode_and_model(&mut args, *permission_mode, model.as_deref())?;
            prompt = push_prompt_after_dashes(&mut args, text)?;
        }
        TerminalLaunch::ClaudeResume {
            session_id,
            fork,
            permission_mode,
            model,
            prompt: text,
        } => {
            check_id("sessionId", session_id)?;
            args.extend(["--resume".to_string(), session_id.clone()]);
            if *fork {
                args.push("--fork-session".to_string());
            }
            push_mode_and_model(&mut args, *permission_mode, model.as_deref())?;
            prompt = typed(text);
        }
        TerminalLaunch::ClaudeHostedLead {
            session_id,
            permission_mode,
            model,
            prompt: text,
        } => {
            check_uuid("sessionId", session_id)?;
            args.extend([
                "--teammate-mode".to_string(),
                "tmux".to_string(),
                "--session-id".to_string(),
                session_id.clone(),
                "--permission-mode".to_string(),
                permission_mode.as_cli().to_string(),
                "--settings".to_string(),
                lead_settings_json(*permission_mode),
            ]);
            if let Some(model) = model {
                check_model(model)?;
                args.extend(["--model".to_string(), model.clone()]);
            }
            prompt = push_prompt_after_dashes(&mut args, text)?;
        }
        TerminalLaunch::ClaudeAttach { session_id } => {
            check_id("sessionId", session_id)?;
            kind = ProcKind::Attach;
            args.extend(["attach".to_string(), session_id.clone()]);
        }
        TerminalLaunch::ClaudeSignIn => {
            push_mode_and_model(&mut args, PermissionMode::Default, None)?;
            prompt = FirstPrompt::TypedAfterReady("/login".to_string());
        }
        TerminalLaunch::Codex {
            resume,
            model,
            prompt: text,
        } => {
            if let Some(id) = resume {
                check_id("resume", id)?;
                args.extend(["resume".to_string(), id.clone()]);
            }
            if let Some(model) = model {
                check_model(model)?;
                args.extend(["--model".to_string(), model.clone()]);
            }
            match text.as_deref().filter(|t| !t.trim().is_empty()) {
                None => {}
                // A resumed session gets its prompt typed after ready.
                Some(t) if resume.is_some() => prompt = FirstPrompt::TypedAfterReady(t.to_string()),
                // A prompt that starts with `-` would be read as a flag:
                // typed after ready instead, never on argv.
                Some(t) if t.starts_with('-') => prompt = FirstPrompt::TypedAfterReady(t.to_string()),
                Some(t) => {
                    check_argv_text("prompt", t)?;
                    args.push(t.to_string());
                    prompt = FirstPrompt::Argv;
                }
            }
        }
        TerminalLaunch::CodexSignIn => args.push("login".to_string()),
        TerminalLaunch::Cursor {
            resume,
            model,
            prompt: text,
        } => {
            if let Some(id) = resume {
                check_id("resume", id)?;
                args.extend(["--resume".to_string(), id.clone()]);
            }
            if let Some(model) = model {
                check_model(model)?;
                args.extend(["--model".to_string(), model.clone()]);
            }
            prompt = typed(text);
        }
        TerminalLaunch::CursorSignIn => args.push("login".to_string()),
        TerminalLaunch::Agy {
            conversation,
            prompt: text,
        } => {
            if let Some(id) = conversation {
                check_id("conversation", id)?;
                args.extend(["--conversation".to_string(), id.clone()]);
            }
            prompt = typed(text);
        }
        TerminalLaunch::AgySignIn => {}
        TerminalLaunch::Pane { command } => {
            if command.is_empty() || command.contains('\0') {
                return Err(IpcError::invalid_argument("command", "required"));
            }
            kind = ProcKind::Pane;
            argv0 = AbsPath::parse(PANE_SHELL).map_err(|_| IpcError::internal("pane-shell"))?;
            args.extend(["-c".to_string(), command.clone()]);
        }
    }
    Ok(TerminalArgv {
        kind,
        argv0,
        args,
        prompt,
    })
}

/// The hosted lead's inline settings: the only `--settings` Gilde ever
/// passes; it writes no file (§6.2.1, frozen LP-10/LP-11).
pub fn lead_settings_json(mode: PermissionMode) -> String {
    serde_json::json!({ "permissions": { "defaultMode": mode.as_cli() } }).to_string()
}

fn push_mode_and_model(out: &mut Vec<String>, mode: PermissionMode, model: Option<&str>) -> Result<(), IpcError> {
    out.extend(["--permission-mode".to_string(), mode.as_cli().to_string()]);
    if let Some(model) = model {
        check_model(model)?;
        out.extend(["--model".to_string(), model.to_string()]);
    }
    Ok(())
}

/// `-- <prompt>`: after the `--` fence nothing can be read as a flag.
fn push_prompt_after_dashes(out: &mut Vec<String>, text: &Option<String>) -> Result<FirstPrompt, IpcError> {
    match text.as_deref().filter(|t| !t.trim().is_empty()) {
        None => Ok(FirstPrompt::None),
        Some(t) => {
            check_argv_text("prompt", t)?;
            out.extend(["--".to_string(), t.to_string()]);
            Ok(FirstPrompt::Argv)
        }
    }
}

fn check_uuid(field: &str, s: &str) -> Result<(), IpcError> {
    if crate::contract::ids::is_uuid(s, None) {
        Ok(())
    } else {
        Err(IpcError::invalid_argument(field, "pattern"))
    }
}

/// A vendor session/chat id on argv: non-empty, ≤ 256 bytes, never starting
/// with `-` (it would be read as a flag), no whitespace or control characters.
fn check_id(field: &str, s: &str) -> Result<(), IpcError> {
    let ok = !s.is_empty()
        && s.len() <= 256
        && !s.starts_with('-')
        && !s.chars().any(|c| c.is_control() || c.is_whitespace());
    if ok {
        Ok(())
    } else {
        Err(IpcError::invalid_argument(field, "pattern"))
    }
}

fn check_model(model: &str) -> Result<(), IpcError> {
    if crate::contract::ipc::is_model_id(model) {
        Ok(())
    } else {
        Err(IpcError::invalid_argument("model", "pattern"))
    }
}

/// Text on argv cannot hold NUL.
fn check_argv_text(field: &str, s: &str) -> Result<(), IpcError> {
    if s.contains('\0') {
        Err(IpcError::invalid_argument(field, "nul"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentBinary, BinarySource};
    use std::path::PathBuf;

    fn installed(kind: AgentKind, path: &str) -> AgentBinary {
        AgentBinary {
            kind,
            abs_path: Some(PathBuf::from(path)),
            version: Some("1.0.0".into()),
            installed: true,
            ok: true,
            verified: false,
            verified_range: None,
            warning: None,
            source: BinarySource::Detected,
            detected_at: Some(0),
        }
    }

    fn missing(kind: AgentKind) -> AgentBinary {
        AgentBinary {
            kind,
            abs_path: None,
            version: None,
            installed: false,
            ok: false,
            verified: false,
            verified_range: None,
            warning: None,
            source: BinarySource::NotFound,
            detected_at: Some(0),
        }
    }

    fn all_installed() -> Vec<AgentBinary> {
        vec![
            installed(AgentKind::Claude, "/nvm/bin/claude"),
            installed(AgentKind::Codex, "/nvm/bin/codex"),
            installed(AgentKind::Cursor, "/usr/local/bin/cursor-agent"),
            installed(AgentKind::Antigravity, "/home/.local/bin/agy"),
            installed(AgentKind::GeminiCli, "/nvm/bin/gemini"),
            installed(AgentKind::Shell, "/bin/zsh"),
        ]
    }

    fn template(kind: AgentKind, cfg: &AppConfig) -> LaunchTemplate {
        templates(cfg, &all_installed())
            .into_iter()
            .find(|t| t.agent == kind)
            .unwrap_or_else(|| panic!("no template for {kind:?}"))
    }

    #[test]
    fn the_design_table_is_implemented_verbatim() {
        let cfg = AppConfig::default();

        let claude = template(AgentKind::Claude, &cfg);
        assert_eq!(claude.binary, "/nvm/bin/claude");
        assert!(claude.args.is_empty());
        assert_eq!(claude.account_env_key.as_deref(), Some("CLAUDE_CONFIG_DIR"));
        assert_eq!(claude.prompt_mode, PromptMode::Argv);
        assert_eq!(
            claude.prompt_argv,
            Some(vec!["--".to_string(), "{PROMPT}".to_string()])
        );

        let codex = template(AgentKind::Codex, &cfg);
        assert_eq!(codex.account_env_key.as_deref(), Some("CODEX_HOME"));
        assert_eq!(codex.resume_args, Some(vec!["resume".into(), "{ID}".into()]));

        let agy = template(AgentKind::Antigravity, &cfg);
        assert_eq!(agy.account_env_key.as_deref(), Some("AGY_HOME"));
        assert_eq!(agy.prompt_mode, PromptMode::TypeAfterReady);

        let cursor = template(AgentKind::Cursor, &cfg);
        assert_eq!(
            cursor.account_env_key, None,
            "cursor-agent has no config-dir env — ambient login only"
        );

        let shell = template(AgentKind::Shell, &cfg);
        assert_eq!(shell.args, vec!["-l".to_string()]);
        assert_eq!(shell.prompt_mode, PromptMode::None);
    }

    /// M1-PLAN T09.6, called out as 필수.
    #[test]
    fn agy_never_carries_dash_p_in_the_interactive_template() {
        let cfg = AppConfig::default();
        let agy = template(AgentKind::Antigravity, &cfg);

        assert!(!agy.args.iter().any(|a| a == "-p" || a == "--print"));
        assert_eq!(agy.prompt_mode, PromptMode::TypeAfterReady);
        assert!(agy.prompt_argv.is_none());
        assert!(
            agy.argv_with_prompt("hello").is_none(),
            "a TypeAfterReady agent must not produce a prompt argv"
        );
    }

    /// M1-PLAN T09.7. Both promotions are backed by a real pty run; the three
    /// unmeasured CLIs stay honest about it.
    #[test]
    fn measured_agents_are_verified_and_unmeasured_ones_are_not() {
        let cfg = AppConfig::default();
        assert!(template(AgentKind::Claude, &cfg).verified);
        assert!(template(AgentKind::Codex, &cfg).verified);
        assert!(!template(AgentKind::Antigravity, &cfg).verified);
        assert!(!template(AgentKind::Cursor, &cfg).verified);
        assert!(!template(AgentKind::GeminiCli, &cfg).verified);
    }

    #[test]
    fn prompt_argv_renders_the_placeholder_without_a_shell_in_the_way() {
        let cfg = AppConfig::default();
        let claude = template(AgentKind::Claude, &cfg);

        assert_eq!(
            claude.argv_with_prompt("--model opus; rm -rf /"),
            Some(vec![
                "/nvm/bin/claude".to_string(),
                "--".to_string(),
                "--model opus; rm -rf /".to_string(),
            ]),
            "the prompt stays one argv element, fenced by --"
        );
        assert_eq!(
            claude.argv_for_resume("abc-123"),
            Some(vec![
                "/nvm/bin/claude".to_string(),
                "--resume".to_string(),
                "abc-123".to_string(),
            ])
        );
    }

    #[test]
    fn an_uninstalled_agent_gets_no_template() {
        let cfg = AppConfig::default();
        let bins = vec![
            installed(AgentKind::Claude, "/nvm/bin/claude"),
            missing(AgentKind::Cursor),
            missing(AgentKind::GeminiCli),
        ];
        let got = templates(&cfg, &bins);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].agent, AgentKind::Claude);
    }

    /// M1-PLAN T09.8 — every field is overridable.
    #[test]
    fn every_template_field_is_overridable_from_config() {
        let mut cfg = AppConfig::default();
        cfg.agents.launch_overrides.insert(
            "claude".into(),
            LaunchOverride {
                prompt_mode: Some(PromptMode::TypeAfterReady),
                args: Some(vec!["--dangerously-skip-permissions".into()]),
                prompt_argv: Some(vec!["--prompt".into(), "{PROMPT}".into()]),
                resume_args: Some(vec!["-c".into()]),
                binary: Some("/opt/pinned/claude".into()),
                ..LaunchOverride::default()
            },
        );

        let claude = template(AgentKind::Claude, &cfg);
        assert_eq!(claude.binary, "/opt/pinned/claude");
        assert_eq!(claude.args, vec!["--dangerously-skip-permissions".to_string()]);
        assert_eq!(claude.prompt_mode, PromptMode::TypeAfterReady);
        assert_eq!(
            claude.prompt_argv,
            Some(vec!["--prompt".into(), "{PROMPT}".into()])
        );
        assert_eq!(claude.resume_args, Some(vec!["-c".to_string()]));
        assert!(
            !claude.verified,
            "an overridden prompt path is no longer the one we measured"
        );
    }

    #[test]
    fn the_default_config_override_is_a_no_op_and_keeps_the_measured_badge() {
        // `AgentsConfig::default()` ships `{"claude": {"promptMode": "argv",
        // "args": []}}` (DESIGN §10-3). Restating the shipped values must not
        // cost the verified badge.
        let cfg = AppConfig::default();
        let claude = template(AgentKind::Claude, &cfg);
        assert_eq!(claude.prompt_mode, PromptMode::Argv);
        assert!(claude.verified);
    }

    #[test]
    fn an_empty_prompt_argv_override_means_no_argv_delivery() {
        let mut cfg = AppConfig::default();
        cfg.agents.launch_overrides.insert(
            "codex".into(),
            LaunchOverride {
                prompt_argv: Some(Vec::new()),
                ..LaunchOverride::default()
            },
        );
        let codex = template(AgentKind::Codex, &cfg);
        assert_eq!(codex.prompt_argv, None);
        assert!(codex.argv_with_prompt("x").is_none());
        assert!(!codex.verified);
    }

    #[test]
    fn the_wire_shape_matches_ipc_ts() {
        let cfg = AppConfig::default();
        let value = serde_json::to_value(template(AgentKind::Cursor, &cfg)).expect("json");
        assert_eq!(value["agent"], serde_json::json!("cursorAgent"));
        assert_eq!(value["accountEnvKey"], serde_json::Value::Null);
        assert_eq!(value["promptMode"], serde_json::json!("typeAfterReady"));
        assert_eq!(value["promptArgv"], serde_json::Value::Null);
        assert_eq!(value["verified"], serde_json::json!(false));
    }
}
