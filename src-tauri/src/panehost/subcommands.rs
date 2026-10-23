//! The strict subcommand surface (§7.4.4): one parser, no semantics.
//!
//! Owner: T07.
//!
//! Input: the argv **after** `-L claude-swarm-<pid>`. Output: a [`Cmd`] or a
//! [`ParseError`]. Parsing is strict per row: flags may appear in any order,
//! each at most once; flags precede positionals (as tmux's getopt reads
//! them); values are validated (`%<n>` ≤ 6 digits, `@<n>` ≤ 6 digits,
//! percentages 1..99); `respawn-pane`'s command is exactly one argument after
//! `--`. The surface is frozen for 2.1.307 (46 calls, 0 deviations [R6 §5
//! LP-10]); anything outside it is [`ParseError::Unsupported`] — a
//! **deviation** when an authorized lead sent it (§7.4.9) — while a value
//! refusal ([`ParseError::Refused`]: a title or command with control
//! characters, a pane id of more than 6 digits) is never one.
//!
//! Notes recorded for deviations are content-free: subcommand and flag names
//! only (sanitized), never values.

/// The one session name and window name of external swarm mode [R3 §2.1].
pub const SESSION: &str = "claude-swarm";
pub const WINDOW: &str = "swarm-view";
/// `-F` formats the surface knows.
pub const FMT_PANE_ID: &str = "#{pane_id}";
pub const FMT_WINDOW_NAME: &str = "#{window_name}";
/// The placeholder command of new panes.
pub const PLACEHOLDER: &str = "cat";
/// The cap of a `respawn-pane` command (§7.4.5).
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
/// The cap of a pane title (§7.4.4 row 8), in characters.
pub const MAX_TITLE_CHARS: usize = 64;

/// A window target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowTarget {
    /// `claude-swarm`: the session's current window.
    Current,
    /// `claude-swarm:swarm-view`.
    Named,
    /// `@<n>`.
    Id(u32),
}

impl WindowTarget {
    /// The tmux spelling (for `can't find window: …`).
    pub fn spelling(&self) -> String {
        match self {
            WindowTarget::Current => SESSION.to_string(),
            WindowTarget::Named => format!("{SESSION}:{WINDOW}"),
            WindowTarget::Id(n) => format!("@{n}"),
        }
    }
}

/// The pane options of row 7/14.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaneOption {
    WindowStyle,
    PaneBorderStyle,
    PaneActiveBorderStyle,
    PaneBorderFormat,
    RemainOnExit,
}

impl PaneOption {
    fn parse(name: &str) -> Option<PaneOption> {
        Some(match name {
            "window-style" => PaneOption::WindowStyle,
            "pane-border-style" => PaneOption::PaneBorderStyle,
            "pane-active-border-style" => PaneOption::PaneActiveBorderStyle,
            "pane-border-format" => PaneOption::PaneBorderFormat,
            "remain-on-exit" => PaneOption::RemainOnExit,
            _ => return None,
        })
    }
}

/// One parsed call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// 11a.
    HasSession,
    /// 11b.
    NewSession,
    /// 12a.
    ListWindows,
    /// 12b.
    NewWindow,
    /// 4.
    ListPanes { target: WindowTarget },
    /// 5/6. `-h`/`-v`/`-l` are layout hints only (Gilde owns the layout).
    SplitWindow { target: u32, horizontal: bool, percent: Option<u8> },
    /// 7, 14: one of the five known options.
    SetPaneOption { target: u32, option: PaneOption, value: String },
    /// 7: an unknown option name → exit 0 + a deviation note.
    SetPaneUnknownOption { target: u32, name: String },
    /// 9: `set-option -w`: ignored (a note when it is not
    /// `pane-border-status top`).
    SetWindowOption { target: WindowTarget, known: bool, name: String },
    /// 8: the title is checked (no control characters) but not yet sanitized.
    SelectPaneTitle { target: u32, title: String },
    /// 10: ignored.
    SelectLayout { target: WindowTarget },
    /// 13: ignored.
    ResizePane { target: u32 },
    /// 15: the command is opaque (checked against the input rules only).
    RespawnPane { target: u32, command: String },
    /// 16.
    KillPane { target: u32 },
}

impl Cmd {
    /// The subcommand name (for logs: never values).
    pub fn name(&self) -> &'static str {
        match self {
            Cmd::HasSession => "has-session",
            Cmd::NewSession => "new-session",
            Cmd::ListWindows => "list-windows",
            Cmd::NewWindow => "new-window",
            Cmd::ListPanes { .. } => "list-panes",
            Cmd::SplitWindow { .. } => "split-window",
            Cmd::SetPaneOption { .. } | Cmd::SetPaneUnknownOption { .. } | Cmd::SetWindowOption { .. } => "set-option",
            Cmd::SelectPaneTitle { .. } => "select-pane",
            Cmd::SelectLayout { .. } => "select-layout",
            Cmd::ResizePane { .. } => "resize-pane",
            Cmd::RespawnPane { .. } => "respawn-pane",
            Cmd::KillPane { .. } => "kill-pane",
        }
    }
}

/// Why argv did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Outside the §7.4.4 surface: exit 1 `gilde-tmux: unsupported command:
    /// <name>`; a deviation (with `note`) when an authorized lead sent it.
    Unsupported { name: String, note: String },
    /// A value refusal: exit 1 with `stderr`; never a deviation.
    Refused { stderr: String },
}

impl ParseError {
    fn unsupported(sub: &str, what: &str) -> ParseError {
        let name = display_name(sub);
        let note = if what.is_empty() {
            format!("unsupported shim command: {name}")
        } else {
            format!("unsupported {what} on {name}")
        };
        ParseError::Unsupported { name, note }
    }

    fn refused(stderr: impl Into<String>) -> ParseError {
        ParseError::Refused { stderr: stderr.into() }
    }
}

/// A subcommand or flag name as it may appear in a note or a stderr line:
/// printable ASCII only (anything else is `?`), at most 40 characters.
pub fn display_name(raw: &str) -> String {
    if raw.is_empty() {
        return "(none)".to_string();
    }
    let mut out: String = raw
        .chars()
        .take(40)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect();
    if raw.chars().count() > 40 {
        out.push('…');
    }
    out
}

/// The refusal texts of this module and the server (tmux-shaped where tmux
/// has a message; `gilde-tmux:`-prefixed otherwise).
pub mod texts {
    pub const CANT_FIND_SESSION: &str = "can't find session: claude-swarm\n";
    pub const DUPLICATE_SESSION: &str = "duplicate session: claude-swarm\n";
    pub const NO_SPACE: &str = "no space for new pane\n";
    pub const RESPAWN_FAILED: &str = "respawn-pane: failed to spawn\n";
    pub const BAD_TITLE: &str = "gilde-tmux: pane title refused\n";
    pub const BAD_COMMAND: &str = "gilde-tmux: respawn-pane command refused\n";
    pub const BAD_OPTION_VALUE: &str = "gilde-tmux: option value refused\n";

    pub fn cant_find_pane(n: &str) -> String {
        format!("can't find pane: %{n}\n")
    }

    pub fn cant_find_window(spelling: &str) -> String {
        format!("can't find window: {spelling}\n")
    }
}

/// Parsed flags of one call.
struct Flags<'a> {
    bools: Vec<char>,
    values: Vec<(char, &'a str)>,
    positionals: Vec<&'a str>,
    /// `--` was present (the positionals came after it).
    dashdash: bool,
}

impl<'a> Flags<'a> {
    fn has(&self, c: char) -> bool {
        self.bools.contains(&c)
    }

    fn value(&self, c: char) -> Option<&'a str> {
        self.values.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
    }
}

/// Split `args` into flags (`-x` bool, `-x value`) and positionals. Every
/// flag word is exactly one known flag; each at most once.
fn flags<'a>(sub: &str, args: &'a [String], bool_flags: &str, value_flags: &str) -> Result<Flags<'a>, ParseError> {
    let mut f = Flags { bools: Vec::new(), values: Vec::new(), positionals: Vec::new(), dashdash: false };
    let mut i = 0;
    while i < args.len() {
        let w = args[i].as_str();
        if w == "--" {
            f.dashdash = true;
            f.positionals.extend(args[i + 1..].iter().map(String::as_str));
            return Ok(f);
        }
        let b = w.as_bytes();
        let is_flag = b.len() == 2 && b[0] == b'-' && b[1].is_ascii_alphabetic();
        if !is_flag {
            // getopt stops at the first non-option word.
            f.positionals.extend(args[i..].iter().map(String::as_str));
            return Ok(f);
        }
        let c = b[1] as char;
        if f.has(c) || f.value(c).is_some() {
            return Err(ParseError::unsupported(sub, &format!("repeated flag -{c}")));
        }
        if bool_flags.contains(c) {
            f.bools.push(c);
            i += 1;
        } else if value_flags.contains(c) {
            let Some(v) = args.get(i + 1) else {
                return Err(ParseError::unsupported(sub, &format!("flag -{c} without a value")));
            };
            f.values.push((c, v.as_str()));
            i += 2;
        } else {
            return Err(ParseError::unsupported(sub, &format!("flag -{c}")));
        }
    }
    Ok(f)
}

/// `%<n>` with ≤ 6 digits → `n`; `%<more digits>` → a refusal (unknown pane,
/// never a deviation); any other form → a deviation.
fn pane_target(sub: &str, t: &str) -> Result<u32, ParseError> {
    let digits = t
        .strip_prefix('%')
        .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| ParseError::unsupported(sub, "target form"))?;
    if digits.len() > 6 {
        return Err(ParseError::refused(texts::cant_find_pane(digits)));
    }
    digits
        .parse::<u32>()
        .map_err(|_| ParseError::refused(texts::cant_find_pane(digits)))
}

fn window_target(sub: &str, t: &str) -> Result<WindowTarget, ParseError> {
    if t == SESSION {
        return Ok(WindowTarget::Current);
    }
    if t == format!("{SESSION}:{WINDOW}") {
        return Ok(WindowTarget::Named);
    }
    if let Some(d) = t.strip_prefix('@') {
        if !d.is_empty() && d.len() <= 6 && d.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(n) = d.parse::<u32>() {
                return Ok(WindowTarget::Id(n));
            }
        }
    }
    Err(ParseError::unsupported(sub, "target form"))
}

/// `<1..99>%`.
fn percent(sub: &str, flag: char, v: &str) -> Result<u8, ParseError> {
    let n = v
        .strip_suffix('%')
        .filter(|d| !d.is_empty() && d.len() <= 2 && d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse::<u8>().ok())
        .filter(|n| (1..=99).contains(n));
    n.ok_or_else(|| ParseError::unsupported(sub, &format!("value of -{flag}")))
}

fn require(sub: &str, ok: bool, what: &str) -> Result<(), ParseError> {
    if ok {
        Ok(())
    } else {
        Err(ParseError::unsupported(sub, what))
    }
}

fn need_value<'a>(sub: &str, f: &Flags<'a>, c: char) -> Result<&'a str, ParseError> {
    f.value(c).ok_or_else(|| ParseError::unsupported(sub, &format!("missing -{c}")))
}

/// `-- cat` exactly.
fn placeholder_command(sub: &str, f: &Flags<'_>) -> Result<(), ParseError> {
    require(sub, f.dashdash && f.positionals == [PLACEHOLDER], "placeholder command")
}

fn no_positionals(sub: &str, f: &Flags<'_>) -> Result<(), ParseError> {
    require(sub, f.positionals.is_empty() && !f.dashdash, "extra arguments")
}

/// Parse the argv after `-L claude-swarm-<pid>`.
pub fn parse(args: &[String]) -> Result<Cmd, ParseError> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(ParseError::unsupported("", ""));
    };
    let sub = sub.as_str();
    match sub {
        "has-session" => {
            let f = flags(sub, rest, "", "t")?;
            no_positionals(sub, &f)?;
            require(sub, need_value(sub, &f, 't')? == SESSION, "target form")?;
            Ok(Cmd::HasSession)
        }
        "new-session" => {
            let f = flags(sub, rest, "dP", "snF")?;
            require(sub, f.has('d') && f.has('P'), "missing -d/-P")?;
            require(sub, need_value(sub, &f, 's')? == SESSION, "session name")?;
            require(sub, need_value(sub, &f, 'n')? == WINDOW, "window name")?;
            require(sub, need_value(sub, &f, 'F')? == FMT_PANE_ID, "format")?;
            placeholder_command(sub, &f)?;
            Ok(Cmd::NewSession)
        }
        "list-windows" => {
            let f = flags(sub, rest, "", "tF")?;
            no_positionals(sub, &f)?;
            require(sub, need_value(sub, &f, 't')? == SESSION, "target form")?;
            require(sub, need_value(sub, &f, 'F')? == FMT_WINDOW_NAME, "format")?;
            Ok(Cmd::ListWindows)
        }
        "new-window" => {
            let f = flags(sub, rest, "P", "tnF")?;
            require(sub, f.has('P'), "missing -P")?;
            require(sub, need_value(sub, &f, 't')? == SESSION, "target form")?;
            require(sub, need_value(sub, &f, 'n')? == WINDOW, "window name")?;
            require(sub, need_value(sub, &f, 'F')? == FMT_PANE_ID, "format")?;
            placeholder_command(sub, &f)?;
            Ok(Cmd::NewWindow)
        }
        "list-panes" => {
            let f = flags(sub, rest, "", "tF")?;
            no_positionals(sub, &f)?;
            let target = window_target(sub, need_value(sub, &f, 't')?)?;
            require(sub, need_value(sub, &f, 'F')? == FMT_PANE_ID, "format")?;
            Ok(Cmd::ListPanes { target })
        }
        "split-window" => {
            let f = flags(sub, rest, "dPhv", "tlF")?;
            require(sub, f.has('d') && f.has('P'), "missing -d/-P")?;
            require(sub, f.has('h') != f.has('v'), "direction")?;
            let target = pane_target(sub, need_value(sub, &f, 't')?)?;
            let percent = match f.value('l') {
                Some(v) => Some(percent(sub, 'l', v)?),
                None => None,
            };
            require(sub, need_value(sub, &f, 'F')? == FMT_PANE_ID, "format")?;
            placeholder_command(sub, &f)?;
            Ok(Cmd::SplitWindow { target, horizontal: f.has('h'), percent })
        }
        "set-option" => {
            let f = flags(sub, rest, "pw", "t")?;
            require(sub, f.has('p') != f.has('w'), "scope")?;
            require(sub, !f.dashdash && f.positionals.len() == 2, "argument count")?;
            let (name, value) = (f.positionals[0], f.positionals[1]);
            let t = need_value(sub, &f, 't')?;
            if f.has('p') {
                let target = pane_target(sub, t)?;
                if value.chars().any(char::is_control) {
                    return Err(ParseError::refused(texts::BAD_OPTION_VALUE));
                }
                match PaneOption::parse(name) {
                    Some(option) => Ok(Cmd::SetPaneOption { target, option, value: value.to_string() }),
                    None => Ok(Cmd::SetPaneUnknownOption { target, name: display_name(name) }),
                }
            } else {
                let target = window_target(sub, t)?;
                let known = name == "pane-border-status" && value == "top";
                Ok(Cmd::SetWindowOption { target, known, name: display_name(name) })
            }
        }
        "select-pane" => {
            let f = flags(sub, rest, "", "tT")?;
            no_positionals(sub, &f)?;
            let target = pane_target(sub, need_value(sub, &f, 't')?)?;
            let title = need_value(sub, &f, 'T')?;
            if title.chars().any(char::is_control) {
                return Err(ParseError::refused(texts::BAD_TITLE));
            }
            Ok(Cmd::SelectPaneTitle { target, title: title.to_string() })
        }
        "select-layout" => {
            let f = flags(sub, rest, "", "t")?;
            require(sub, !f.dashdash && f.positionals.len() == 1, "argument count")?;
            require(sub, matches!(f.positionals[0], "tiled" | "main-vertical"), "layout")?;
            let target = window_target(sub, need_value(sub, &f, 't')?)?;
            Ok(Cmd::SelectLayout { target })
        }
        "resize-pane" => {
            let f = flags(sub, rest, "", "tx")?;
            no_positionals(sub, &f)?;
            let target = pane_target(sub, need_value(sub, &f, 't')?)?;
            percent(sub, 'x', need_value(sub, &f, 'x')?)?;
            Ok(Cmd::ResizePane { target })
        }
        "respawn-pane" => {
            let f = flags(sub, rest, "k", "t")?;
            require(sub, f.has('k'), "missing -k")?;
            require(sub, f.dashdash && f.positionals.len() == 1, "command shape")?;
            let target = pane_target(sub, need_value(sub, &f, 't')?)?;
            let command = f.positionals[0];
            check_command(command)?;
            Ok(Cmd::RespawnPane { target, command: command.to_string() })
        }
        "kill-pane" => {
            let f = flags(sub, rest, "", "t")?;
            no_positionals(sub, &f)?;
            let target = pane_target(sub, need_value(sub, &f, 't')?)?;
            Ok(Cmd::KillPane { target })
        }
        other => Err(ParseError::unsupported(other, "")),
    }
}

/// The input rules of a pane command (§7.4.5, footgun row "respawn-pane
/// command with control characters / NUL / > 64 KiB"): non-empty, ≤ 64 KiB,
/// no NUL and no `\p{Cc}` at all (Claude refuses those itself [R3 §2.2]).
pub fn check_command(command: &str) -> Result<(), ParseError> {
    if command.trim().is_empty() || command.len() > MAX_COMMAND_BYTES || command.chars().any(char::is_control) {
        return Err(ParseError::refused(texts::BAD_COMMAND));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(words: &[&str]) -> Result<Cmd, ParseError> {
        let v: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        parse(&v)
    }

    fn is_deviation(r: Result<Cmd, ParseError>) -> bool {
        matches!(r, Err(ParseError::Unsupported { .. }))
    }

    fn is_refused(r: Result<Cmd, ParseError>) -> bool {
        matches!(r, Err(ParseError::Refused { .. }))
    }

    /// Every call of the 2.1.307 trace (R6 §5 LP-10) parses.
    #[test]
    fn the_recorded_surface_parses() {
        let cmd = "cd /p && env CLAUDECODE=1 /x/claude.exe --agent-id a@t --team-name t --parent-session-id s";
        let ok: Vec<Vec<&str>> = vec![
            vec!["has-session", "-t", "claude-swarm"],
            vec!["new-session", "-d", "-s", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["list-panes", "-t", "claude-swarm:swarm-view", "-F", "#{pane_id}"],
            vec!["set-option", "-w", "-t", "claude-swarm:swarm-view", "pane-border-status", "top"],
            vec!["set-option", "-p", "-t", "%0", "window-style", "bg=default,fg=blue"],
            vec!["set-option", "-p", "-t", "%0", "pane-border-style", "fg=blue"],
            vec!["set-option", "-p", "-t", "%0", "pane-active-border-style", "fg=blue"],
            vec!["select-pane", "-t", "%0", "-T", "alpha"],
            vec!["set-option", "-p", "-t", "%0", "pane-border-format", "#[fg=blue,bold] #{pane_title} #[default]"],
            vec!["set-option", "-p", "-t", "%0", "remain-on-exit", "failed"],
            vec!["list-windows", "-t", "claude-swarm", "-F", "#{window_name}"],
            vec!["respawn-pane", "-k", "-t", "%0", "--", cmd],
            vec!["split-window", "-d", "-t", "%0", "-v", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["split-window", "-d", "-t", "%0", "-h", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["select-layout", "-t", "claude-swarm:swarm-view", "tiled"],
            vec!["kill-pane", "-t", "%2"],
            // R3 §2.2 rows outside the 2.1.307 trace but inside §7.4.4.
            vec!["new-window", "-t", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["split-window", "-d", "-t", "%1", "-h", "-l", "70%", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["select-layout", "-t", "@0", "main-vertical"],
            vec!["resize-pane", "-t", "%0", "-x", "30%"],
            vec!["list-panes", "-t", "@3", "-F", "#{pane_id}"],
            vec!["list-panes", "-t", "claude-swarm", "-F", "#{pane_id}"],
        ];
        for words in ok {
            assert!(p(&words).is_ok(), "{words:?} → {:?}", p(&words));
        }
    }

    #[test]
    fn flags_may_come_in_any_order() {
        assert_eq!(
            p(&["new-session", "-P", "-F", "#{pane_id}", "-n", "swarm-view", "-s", "claude-swarm", "-d", "--", "cat"]),
            Ok(Cmd::NewSession)
        );
        assert_eq!(
            p(&["split-window", "-v", "-F", "#{pane_id}", "-P", "-t", "%0", "-d", "--", "cat"]),
            Ok(Cmd::SplitWindow { target: 0, horizontal: false, percent: None })
        );
    }

    #[test]
    fn parsed_values_are_carried() {
        assert_eq!(
            p(&["respawn-pane", "-k", "-t", "%12", "--", "echo hi"]),
            Ok(Cmd::RespawnPane { target: 12, command: "echo hi".into() })
        );
        assert_eq!(
            p(&["set-option", "-p", "-t", "%3", "remain-on-exit", "failed"]),
            Ok(Cmd::SetPaneOption { target: 3, option: PaneOption::RemainOnExit, value: "failed".into() })
        );
        assert_eq!(
            p(&["select-pane", "-t", "%0", "-T", "alpha"]),
            Ok(Cmd::SelectPaneTitle { target: 0, title: "alpha".into() })
        );
        assert_eq!(
            p(&["split-window", "-d", "-t", "%1", "-h", "-l", "70%", "-P", "-F", "#{pane_id}", "--", "cat"]),
            Ok(Cmd::SplitWindow { target: 1, horizontal: true, percent: Some(70) })
        );
        assert_eq!(
            p(&["set-option", "-p", "-t", "%0", "synchronize-panes", "on"]),
            Ok(Cmd::SetPaneUnknownOption { target: 0, name: "synchronize-panes".into() })
        );
        assert_eq!(
            p(&["set-option", "-w", "-t", "@0", "mouse", "on"]),
            Ok(Cmd::SetWindowOption { target: WindowTarget::Id(0), known: false, name: "mouse".into() })
        );
    }

    /// PM-11: a changed surface is a deviation, never a silent no-op.
    #[test]
    fn anything_outside_the_surface_is_a_deviation() {
        let deviations: Vec<Vec<&str>> = vec![
            vec!["display-message", "-p", "#{pane_id}"],
            vec!["send-keys", "-t", "%0", "x"],
            vec!["load-buffer", "-"],
            vec![],
            vec!["has-session", "-t", "other"],
            vec!["has-session"],
            vec!["has-session", "-t", "claude-swarm", "-t", "claude-swarm"],
            vec!["new-session", "-d", "-s", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "cat"],
            vec!["new-session", "-d", "-s", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{window_id}", "--", "cat"],
            vec!["new-session", "-d", "-s", "x", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["new-session", "-d", "-s", "claude-swarm", "-n", "swarm-view", "-P", "-F", "#{pane_id}", "--", "bash"],
            vec!["list-panes", "-t", "claude-swarm:swarm-view", "-F", "#{pane_index}"],
            vec!["list-panes", "-t", "%0", "-F", "#{pane_id}"],
            vec!["list-panes", "-a", "-F", "#{pane_id}"],
            vec!["split-window", "-d", "-t", "%0", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["split-window", "-d", "-t", "%0", "-h", "-v", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["split-window", "-d", "-t", "%0", "-h", "-l", "150%", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["split-window", "-d", "-t", "%0", "-h", "-l", "0%", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["split-window", "-d", "-t", "%0", "-h", "-l", "70", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["split-window", "-dP", "-t", "%0", "-h", "-F", "#{pane_id}", "--", "cat"],
            vec!["split-window", "-d", "-t", "claude-swarm:swarm-view.1", "-h", "-P", "-F", "#{pane_id}", "--", "cat"],
            vec!["set-option", "-t", "%0", "remain-on-exit", "on"],
            vec!["set-option", "-p", "-w", "-t", "%0", "remain-on-exit", "on"],
            vec!["set-option", "-p", "-t", "%0", "remain-on-exit"],
            vec!["set-option", "-g", "-t", "%0", "remain-on-exit", "on"],
            vec!["select-pane", "-t", "%0"],
            vec!["select-pane", "-t", "%0", "-T", "a", "extra"],
            vec!["select-layout", "-t", "claude-swarm:swarm-view", "even-horizontal"],
            vec!["resize-pane", "-t", "%0", "-x", "120"],
            vec!["respawn-pane", "-t", "%0", "--", "x"],
            vec!["respawn-pane", "-k", "-t", "%0", "x"],
            vec!["respawn-pane", "-k", "-t", "%0", "--", "a", "b"],
            vec!["respawn-pane", "-k", "-t", "%0", "--"],
            vec!["kill-pane", "-a", "-t", "%0"],
            vec!["kill-pane"],
            vec!["-V"],
        ];
        for words in deviations {
            assert!(is_deviation(p(&words)), "{words:?} → {:?}", p(&words));
        }
    }

    #[test]
    fn value_refusals_are_never_deviations() {
        assert!(is_refused(p(&["select-pane", "-t", "%0", "-T", "a\u{7}b"])));
        assert!(is_refused(p(&["select-pane", "-t", "%0", "-T", "a\u{9b}b"])), "C1 controls too");
        assert!(is_refused(p(&["respawn-pane", "-k", "-t", "%0", "--", "a\nb"])));
        assert!(is_refused(p(&["respawn-pane", "-k", "-t", "%0", "--", "a\u{0}b"])));
        assert!(is_refused(p(&["respawn-pane", "-k", "-t", "%0", "--", "a\u{1b}[31m"])));
        assert!(is_refused(p(&["respawn-pane", "-k", "-t", "%0", "--", "   "])));
        let big = "x".repeat(MAX_COMMAND_BYTES + 1);
        assert!(is_refused(p(&["respawn-pane", "-k", "-t", "%0", "--", &big])));
        let at_cap = "x".repeat(MAX_COMMAND_BYTES);
        assert!(p(&["respawn-pane", "-k", "-t", "%0", "--", &at_cap]).is_ok());
        assert_eq!(
            p(&["kill-pane", "-t", "%999999999"]),
            Err(ParseError::Refused { stderr: "can't find pane: %999999999\n".into() })
        );
        assert!(p(&["kill-pane", "-t", "%999999"]).is_ok(), "6 digits is a valid form");
        assert!(is_refused(p(&["set-option", "-p", "-t", "%0", "pane-border-format", "a\u{1b}b"])));
    }

    #[test]
    fn notes_and_names_carry_no_values() {
        let Err(ParseError::Unsupported { name, note }) = p(&["display-message", "-p", "secret-value"]) else {
            panic!("expected a deviation")
        };
        assert_eq!(name, "display-message");
        assert_eq!(note, "unsupported shim command: display-message");
        let Err(ParseError::Unsupported { note, .. }) =
            p(&["split-window", "-d", "-t", "%0", "-h", "-P", "-F", "#{secret}", "--", "cat"])
        else {
            panic!("expected a deviation")
        };
        assert!(!note.contains("secret"), "{note}");
        assert_eq!(display_name("a\u{1b}b"), "a?b");
        assert_eq!(display_name(""), "(none)");
        assert!(display_name(&"x".repeat(100)).chars().count() <= 41);
    }
}
