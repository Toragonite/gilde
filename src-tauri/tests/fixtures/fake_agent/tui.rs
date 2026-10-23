//! The screen-drawing modes of `fake_agent`: `claude-tui`, `tui-idle-ready`
//! and `term-query` (contract §14.1 row "`fake_agent` (extended)", §6.2.4;
//! shapes from R6 §4 LP-12 and the recorded grids `probes/term/lp12-*.txt`).
//!
//! The screens are a **binding interface**: Gilde's grid emulator, its
//! screen-prompt grammar and its per-vendor ready signatures are tested
//! end-to-end against exactly these rows, so a change here is a contract
//! change. Rows are counted from 1; `R` = rows, `C` = columns.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::term::{
    self, line, out_str, rule, sanitize, App, Frame, Input, Key, RawMode, Session, Step, ALT_ENTER,
};

/// OSC 0 title of the claude style (R6 §4: "✳ Claude Code").
pub const CLAUDE_TITLE: &str = "\x1b]0;✳ Claude Code\x07";

const SPINNER: [char; 5] = ['✢', '✳', '✶', '✻', '✽'];
const INTERRUPTED: &str = "  ⎿  Interrupted · What should Claude do instead?";
const PLACEHOLDER: &str = "Try \"how does <filepath> work?\"";
const FOOTER_MANUAL: &str = "  ⏸ manual mode on · ? for shortcuts";

/// Default `--work-ms`.
const DEFAULT_WORK_MS: u64 = 300;
/// How long `tui-idle-ready` shows `working… (esc to interrupt)` per line.
const IDLE_READY_WORK: Duration = Duration::from_millis(200);
/// Spinner redraw period.
const SPIN: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// the claude main screen (shared by claude-tui and tui-idle-ready claude)
// ---------------------------------------------------------------------------

/// Rows 1–3 of every claude main / prompt screen.
fn claude_header(f: &mut Frame) {
    f.set(1, " ▐▛███▜▌   Claude Code (fake)");
    f.set(2, "▝▜█████▛▘  Fake model · Test plan");
    f.set(3, "  ▘▘ ▝▝    /tmp/fake-project");
}

/// Transcript lines in rows `first..=last`, keeping the newest that fit.
fn transcript_rows(f: &mut Frame, lines: &[String], first: i32, last: i32) {
    if last < first {
        return;
    }
    let fit = (last - first + 1) as usize;
    let shown = &lines[lines.len().saturating_sub(fit)..];
    for (i, l) in shown.iter().enumerate() {
        f.set(first + i as i32, l.clone());
    }
}

/// The idle main screen: header, transcript (rows 5..=R-7), optional status
/// line on R-6, rule R-4, composer R-3, rule R-2, footer R-1.
fn claude_main(
    f: &mut Frame,
    rows: u16,
    cols: u16,
    transcript: &[String],
    composer: &str,
    status: Option<String>,
) {
    let r = i32::from(rows);
    claude_header(f);
    transcript_rows(f, transcript, 5, r - 7);
    if let Some(s) = status {
        f.set(r - 6, s);
    }
    f.set(r - 4, rule(cols));
    if composer.is_empty() {
        f.set(r - 3, format!("❯ {PLACEHOLDER}"));
    } else {
        f.set(r - 3, format!("❯ {composer}"));
    }
    f.set(r - 2, rule(cols));
    f.set(r - 1, FOOTER_MANUAL);
}

// ---------------------------------------------------------------------------
// claude-tui
// ---------------------------------------------------------------------------

/// Called by the claude TUI at turn boundaries (the `teammate` mode writes
/// its transcript rows and `idle_notification` from these).
pub trait TurnHooks {
    /// A composer line was submitted (a turn starts).
    fn on_submit(&mut self, _text: &str) {}
    /// A turn ended: `interrupted` = Esc / "No" / Esc at a prompt.
    fn on_turn_end(&mut self, _interrupted: bool) {}
}

/// No hooks (plain `claude-tui`, the lead).
pub struct NoHooks;
impl TurnHooks for NoHooks {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    Idle,
    Permission,
    Choice,
    Worker,
    Trust,
    Theme,
    Chrome,
    Onboarding,
}

impl Scenario {
    pub fn parse(s: &str) -> Option<Scenario> {
        Some(match s {
            "idle" => Scenario::Idle,
            "permission" => Scenario::Permission,
            "choice" => Scenario::Choice,
            "worker" => Scenario::Worker,
            "trust" => Scenario::Trust,
            "theme" => Scenario::Theme,
            "chrome" => Scenario::Chrome,
            "onboarding" => Scenario::Onboarding,
            _ => return None,
        })
    }
}

/// `claude-tui` options.
#[derive(Debug, Clone)]
pub struct TuiOpts {
    pub work_ms: u64,
    pub trust_default_yes: bool,
    pub trust_flaky: bool,
}

impl Default for TuiOpts {
    fn default() -> Self {
        TuiOpts {
            work_ms: DEFAULT_WORK_MS,
            trust_default_yes: false,
            trust_flaky: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Screen {
    Idle,
    /// `hl` 1..=4; `amend` = Tab pressed (the typed note).
    Permission {
        hl: usize,
        amend: Option<String>,
    },
    /// `hl` 1..=5.
    Choice {
        hl: usize,
    },
    /// `hl` 1..=3.
    Worker {
        hl: usize,
    },
    /// `yes` = highlight on "Yes, I trust this folder"; `flaked` = the
    /// `--trust-flaky` screen replaced the dialog.
    Trust {
        yes: bool,
        flaked: bool,
    },
    /// `hl` 0..=3 (0 = Auto, 1 = Dark mode).
    Theme {
        hl: usize,
    },
    Security,
    /// `yes` = highlight on "Yes, use my browser".
    Chrome {
        yes: bool,
    },
}

/// What follows a finished "working" phase.
#[derive(Debug, Clone, PartialEq, Eq)]
enum After {
    Idle,
    Prompt(Screen),
}

struct Work {
    started: Instant,
    until: Instant,
    /// When the last spinner frame was drawn.
    last_frame: Instant,
    after: After,
}

pub struct ClaudeTui<'h> {
    opts: TuiOpts,
    screen: Screen,
    /// Setup screens still to show before idle (`onboarding`: trust).
    setup_queue: VecDeque<Screen>,
    transcript: Vec<String>,
    composer: String,
    work: Option<Work>,
    /// Lines submitted while working; run one after another.
    queued: VecDeque<String>,
    hooks: &'h mut dyn TurnHooks,
}

const THEMES: [&str; 4] = [
    "Auto (match terminal)",
    "Dark mode",
    "Light mode",
    "Dark mode (colorblind-friendly)",
];

impl<'h> ClaudeTui<'h> {
    pub fn new(scenario: Scenario, opts: TuiOpts, hooks: &'h mut dyn TurnHooks) -> ClaudeTui<'h> {
        let trust = Screen::Trust {
            yes: opts.trust_default_yes,
            flaked: false,
        };
        let mut setup_queue = VecDeque::new();
        let screen = match scenario {
            Scenario::Idle => Screen::Idle,
            Scenario::Permission => Screen::Permission { hl: 1, amend: None },
            Scenario::Choice => Screen::Choice { hl: 1 },
            Scenario::Worker => Screen::Worker { hl: 1 },
            Scenario::Trust => trust,
            Scenario::Theme => Screen::Theme { hl: 1 },
            Scenario::Chrome => Screen::Chrome { yes: false },
            Scenario::Onboarding => {
                setup_queue.push_back(trust);
                Screen::Theme { hl: 1 }
            }
        };
        ClaudeTui {
            opts,
            screen,
            setup_queue,
            transcript: Vec::new(),
            composer: String::new(),
            work: None,
            queued: VecDeque::new(),
            hooks,
        }
    }

    fn start_work(&mut self, now: Instant, after: After) {
        self.work = Some(Work {
            started: now,
            until: now + Duration::from_millis(self.opts.work_ms),
            last_frame: now,
            after,
        });
    }

    fn submit(&mut self, text: String, now: Instant) -> Step {
        if text.trim() == "/exit" {
            return Step::Exit(0);
        }
        self.hooks.on_submit(&text);
        self.transcript.push(format!("⏺ got: {text}"));
        let after = if text.contains("permission") {
            After::Prompt(Screen::Permission { hl: 1, amend: None })
        } else if text.contains("choice") {
            After::Prompt(Screen::Choice { hl: 1 })
        } else if text.contains("worker") {
            After::Prompt(Screen::Worker { hl: 1 })
        } else if text.contains("trust") {
            After::Prompt(Screen::Trust {
                yes: self.opts.trust_default_yes,
                flaked: false,
            })
        } else {
            After::Idle
        };
        self.start_work(now, after);
        Step::Redraw
    }

    /// Esc / "No": the turn ends, the composer waits.
    fn interrupt(&mut self) -> Step {
        self.screen = Screen::Idle;
        self.work = None;
        // Lines typed while working go back into the composer.
        if !self.queued.is_empty() {
            let mut restored: Vec<String> = self.queued.drain(..).collect();
            restored.push(std::mem::take(&mut self.composer));
            self.composer = restored.join(" ").trim_end().to_string();
        }
        self.transcript.push(INTERRUPTED.to_string());
        self.hooks.on_turn_end(true);
        Step::Redraw
    }

    /// A prompt answer that lets the tool run: note it, work, idle.
    fn proceed(&mut self, note: String, now: Instant) -> Step {
        self.screen = Screen::Idle;
        self.transcript.push(note);
        self.start_work(now, After::Idle);
        Step::Redraw
    }

    /// Leave a setup screen: the next queued one, else idle.
    fn next_setup(&mut self) -> Step {
        self.screen = self.setup_queue.pop_front().unwrap_or(Screen::Idle);
        Step::Redraw
    }

    fn key_idle(&mut self, key: Key, now: Instant) -> Step {
        match key {
            Key::Char(c) => {
                self.composer.push(c);
                Step::Redraw
            }
            Key::Paste(p) => {
                self.composer.push_str(&sanitize(&p));
                Step::Redraw
            }
            Key::Backspace => {
                if self.composer.pop().is_some() {
                    Step::Redraw
                } else {
                    Step::Nothing
                }
            }
            Key::Enter => {
                if self.composer.is_empty() {
                    return Step::Nothing;
                }
                let text = std::mem::take(&mut self.composer);
                if self.work.is_some() {
                    self.queued.push_back(text);
                    Step::Redraw
                } else {
                    self.submit(text, now)
                }
            }
            Key::Esc if self.work.is_some() => self.interrupt(),
            _ => Step::Nothing,
        }
    }

    fn key_permission(&mut self, hl: usize, amend: Option<String>, key: Key, now: Instant) -> Step {
        if let Some(mut note) = amend {
            return match key {
                Key::Char(c) => {
                    note.push(c);
                    self.screen = Screen::Permission {
                        hl: 1,
                        amend: Some(note),
                    };
                    Step::Redraw
                }
                Key::Paste(p) => {
                    note.push_str(&sanitize(&p));
                    self.screen = Screen::Permission {
                        hl: 1,
                        amend: Some(note),
                    };
                    Step::Redraw
                }
                Key::Backspace => {
                    note.pop();
                    self.screen = Screen::Permission {
                        hl: 1,
                        amend: Some(note),
                    };
                    Step::Redraw
                }
                Key::Enter => self.proceed(format!("⏺ approved; note: {note}"), now),
                Key::Esc => self.interrupt(),
                _ => Step::Nothing,
            };
        }
        let choose = |this: &mut Self, n: usize| match n {
            1..=3 => this.proceed(format!("⏺ selected {n}"), now),
            _ => this.interrupt(),
        };
        match key {
            Key::Char(c @ '1'..='4') => choose(self, c as usize - '0' as usize),
            Key::Esc => self.interrupt(),
            Key::Enter => choose(self, hl),
            Key::Up | Key::Down => {
                let hl = move_hl(hl, &key, 1, 4);
                self.screen = Screen::Permission { hl, amend: None };
                Step::Redraw
            }
            Key::Tab => {
                self.screen = Screen::Permission {
                    hl: 1,
                    amend: Some(String::new()),
                };
                Step::Redraw
            }
            _ => Step::Nothing,
        }
    }

    fn key_choice(&mut self, hl: usize, key: Key, now: Instant) -> Step {
        let choose = |this: &mut Self, n: usize| match n {
            1..=3 => {
                let label = ["Red", "Green", "Blue"][n - 1];
                this.proceed(format!("⏺ answered {label}"), now)
            }
            4 => {
                this.screen = Screen::Idle;
                this.transcript.push("⏺ typing".to_string());
                Step::Redraw
            }
            _ => {
                this.screen = Screen::Idle;
                this.transcript.push("⏺ chat".to_string());
                Step::Redraw
            }
        };
        match key {
            Key::Char(c @ '1'..='5') => choose(self, c as usize - '0' as usize),
            Key::Esc => self.interrupt(),
            Key::Enter => choose(self, hl),
            Key::Up | Key::Down => {
                self.screen = Screen::Choice {
                    hl: move_hl(hl, &key, 1, 5),
                };
                Step::Redraw
            }
            _ => Step::Nothing,
        }
    }

    fn key_worker(&mut self, hl: usize, key: Key, now: Instant) -> Step {
        let choose = |this: &mut Self, n: usize| match n {
            1 | 2 => this.proceed(format!("⏺ selected {n}"), now),
            _ => this.interrupt(),
        };
        match key {
            Key::Char(c @ '1'..='3') => choose(self, c as usize - '0' as usize),
            Key::Esc => self.interrupt(),
            Key::Enter => choose(self, hl),
            Key::Up | Key::Down => {
                self.screen = Screen::Worker {
                    hl: move_hl(hl, &key, 1, 3),
                };
                Step::Redraw
            }
            _ => Step::Nothing,
        }
    }

    fn key_trust(&mut self, yes: bool, flaked: bool, key: Key) -> Step {
        if flaked {
            // The replacement screen keeps everything (no options to act on).
            return Step::Nothing;
        }
        match key {
            Key::Up | Key::Down => {
                if self.opts.trust_flaky {
                    self.screen = Screen::Trust { yes, flaked: true };
                } else {
                    // Clamped, no wrap; redrawn even when it did not move.
                    self.screen = Screen::Trust {
                        yes: key == Key::Down,
                        flaked: false,
                    };
                }
                Step::Redraw
            }
            Key::Enter if yes => {
                self.transcript.push("⏺ trusted".to_string());
                self.next_setup()
            }
            Key::Enter | Key::Esc => Step::Exit(1),
            // Digits (and everything else) are ignored: nothing is redrawn.
            _ => Step::Nothing,
        }
    }
}

/// Up/Down on a 1-line-per-option list, clamped to `lo..=hi`.
fn move_hl(hl: usize, key: &Key, lo: usize, hi: usize) -> usize {
    match key {
        Key::Up => hl.saturating_sub(1).max(lo),
        Key::Down => (hl + 1).min(hi),
        _ => hl,
    }
}

/// `   2. Yes, and always allow access to <P> from`, with `<P>` sized so the
/// line ends 2 columns before the right edge: inside the last 6 columns, and
/// " this" (5 columns) cannot follow it, so the label wraps exactly as the
/// real one does at 120 columns (R6 §4, `lp12-11-perm-prompt.txt`).
fn permission_option2(cols: u16, marker: &str) -> String {
    let head = format!("{marker}2. Yes, and always allow access to ");
    let tail = " from";
    let base = "/tmp/fake-project/";
    let fixed = head.chars().count() + base.len() + tail.len();
    let target = usize::from(cols).saturating_sub(2);
    let mut path = base.to_string();
    if target > fixed {
        let extra = target - fixed;
        path.push_str(&"nested/".repeat(extra / 7));
        path.push_str(&"nested"[..extra % 7]);
    }
    format!("{head}{path}{tail}")
}

impl App for ClaudeTui<'_> {
    fn frame(&self, rows: u16, cols: u16, now: Instant) -> Frame {
        let r = i32::from(rows);
        let mut f = Frame::new();
        let mark = |sel: bool| if sel { " ❯ " } else { "   " };
        let block: Vec<String> = match &self.screen {
            Screen::Idle => {
                let status = self.work.as_ref().map(|w| {
                    let elapsed = now.saturating_duration_since(w.started);
                    let spin =
                        SPINNER[(elapsed.as_millis() / SPIN.as_millis()) as usize % SPINNER.len()];
                    format!(
                        "{spin} Working… ({}s · esc to interrupt)",
                        elapsed.as_secs()
                    )
                });
                claude_main(&mut f, rows, cols, &self.transcript, &self.composer, status);
                return f;
            }
            Screen::Permission { hl, amend } => {
                let opt1 = match amend {
                    None => "Yes".to_string(),
                    Some(n) if n.is_empty() => "Yes, and tell Claude what to do next".to_string(),
                    Some(n) => format!("Yes, {n}"),
                };
                vec![
                    rule(cols),
                    " Bash command".into(),
                    " Create empty file demo.txt".into(),
                    "╌".repeat(usize::from(cols)),
                    " touch demo.txt".into(),
                    "╌".repeat(usize::from(cols)),
                    " Do you want to proceed?".into(),
                    format!("{}1. {opt1}", mark(*hl == 1)),
                    permission_option2(cols, mark(*hl == 2)),
                    "      this project".into(),
                    format!(
                        "{}3. Yes, and switch to auto mode · auto mode handles these prompts for you",
                        mark(*hl == 3)
                    ),
                    format!("{}4. No", mark(*hl == 4)),
                    String::new(),
                    if amend.is_some() {
                        " Esc to cancel".into()
                    } else {
                        " Esc to cancel · Tab to amend".into()
                    },
                ]
            }
            Screen::Choice { hl } => {
                let m = |n: usize| if *hl == n { "❯ " } else { "  " };
                vec![
                    rule(cols),
                    " ☐ Colour".into(),
                    "Which colour do you prefer?".into(),
                    format!("{}1. Red", m(1)),
                    "     Choose red".into(),
                    format!("{}2. Green", m(2)),
                    "     Choose green".into(),
                    format!("{}3. Blue", m(3)),
                    "     Choose blue".into(),
                    format!("{}4. Type something.", m(4)),
                    rule(cols),
                    format!("{}5. Chat about this", m(5)),
                    "Enter to select · ↑/↓ to navigate · Esc to cancel".into(),
                ]
            }
            Screen::Worker { hl } => vec![
                " Do you want to proceed?".into(),
                format!("{}1. Yes", mark(*hl == 1)),
                format!("{}2. Yes, and switch to auto mode", mark(*hl == 2)),
                format!("{}3. No", mark(*hl == 3)),
                String::new(),
                " Esc to cancel".into(),
            ],
            Screen::Trust { flaked: true, .. } => {
                // `--trust-flaky`: a different screen, no options.
                f.set(r - 3, " Something else happened");
                return f;
            }
            Screen::Trust { yes, .. } => {
                let block = vec![
                    rule(cols),
                    " Accessing workspace:".into(),
                    String::new(),
                    " /tmp/fake-project".into(),
                    String::new(),
                    " Quick safety check: Is this a project you created or one you trust?".into(),
                    String::new(),
                    format!("{}No, exit", mark(!*yes)),
                    format!("{}Yes, I trust this folder", mark(*yes)),
                    String::new(),
                    " Enter to confirm · Esc to cancel".into(),
                ];
                // Setup screens show the dialog alone.
                f.place_bottom(&block, r - 1);
                return f;
            }
            Screen::Theme { hl } => {
                let mut block = vec![
                    " Choose the text style that looks best with your terminal".to_string(),
                    " To change this later, run /theme".into(),
                    String::new(),
                ];
                for (i, t) in THEMES.iter().enumerate() {
                    block.push(if i == *hl {
                        format!(" ❯ {t}")
                    } else {
                        format!("     {t}")
                    });
                }
                f.place_bottom(&block, r - 1);
                return f;
            }
            Screen::Security => {
                let block = vec![
                    " Security notes:".to_string(),
                    " 1. Claude can make mistakes.".into(),
                    " 2. Due to prompt injection risks, only use it with code you trust".into(),
                    String::new(),
                    " Press Enter to continue…".into(),
                ];
                f.place_bottom(&block, r - 1);
                return f;
            }
            Screen::Chrome { yes } => {
                let m = |sel: bool| if sel { "  ❯ " } else { "    " };
                let block = vec![
                    "  Claude in Chrome extension detected".to_string(),
                    String::new(),
                    format!("{}No, keep browser tools off", m(!*yes)),
                    format!("{}Yes, use my browser", m(*yes)),
                    String::new(),
                    "  Enter to confirm · Esc to keep browser tools off".into(),
                ];
                f.place_bottom(&block, r - 1);
                return f;
            }
        };
        // Prompts: header and transcript above, the block at the bottom
        // (composer and footer hidden).
        claude_header(&mut f);
        let top = r - block.len() as i32;
        transcript_rows(&mut f, &self.transcript, 5, top - 2);
        f.place_bottom(&block, r - 1);
        f
    }

    fn key(&mut self, key: Key, now: Instant) -> Step {
        match self.screen.clone() {
            Screen::Idle => self.key_idle(key, now),
            Screen::Permission { hl, amend } => self.key_permission(hl, amend, key, now),
            Screen::Choice { hl } => self.key_choice(hl, key, now),
            Screen::Worker { hl } => self.key_worker(hl, key, now),
            Screen::Trust { yes, flaked } => self.key_trust(yes, flaked, key),
            Screen::Theme { hl } => match key {
                Key::Up | Key::Down => {
                    self.screen = Screen::Theme {
                        hl: move_hl(hl, &key, 0, THEMES.len() - 1),
                    };
                    Step::Redraw
                }
                Key::Enter => {
                    self.screen = Screen::Security;
                    Step::Redraw
                }
                _ => Step::Nothing,
            },
            Screen::Security => match key {
                Key::Enter => self.next_setup(),
                _ => Step::Nothing,
            },
            Screen::Chrome { .. } => match key {
                Key::Up | Key::Down => {
                    self.screen = Screen::Chrome {
                        yes: key == Key::Down,
                    };
                    Step::Redraw
                }
                Key::Enter | Key::Esc => self.next_setup(),
                _ => Step::Nothing,
            },
        }
    }

    fn tick(&mut self, now: Instant) -> Step {
        let Some(work) = self.work.as_mut() else {
            return Step::Nothing;
        };
        if now < work.until {
            // Spinner frames every 100 ms while working.
            if now.saturating_duration_since(work.last_frame) >= SPIN {
                work.last_frame = now;
                return Step::Redraw;
            }
            return Step::Nothing;
        }
        let after = self.work.take().map(|w| w.after).unwrap_or(After::Idle);
        match after {
            After::Prompt(screen) => self.screen = screen,
            After::Idle => {
                self.screen = Screen::Idle;
                self.hooks.on_turn_end(false);
                if let Some(next) = self.queued.pop_front() {
                    return self.submit(next, now);
                }
            }
        }
        Step::Redraw
    }

    fn deadline(&self) -> Option<Instant> {
        let work = self.work.as_ref()?;
        Some((work.last_frame + SPIN).min(work.until))
    }
}

/// Enter the alternate screen with the claude title.
pub fn enter_claude_alt_screen() {
    out_str(ALT_ENTER);
    out_str(CLAUDE_TITLE);
}

/// `claude-tui <scenario> [--work-ms N] [--trust-default no|yes] [--trust-flaky]`.
pub fn mode_claude_tui(rest: &[String]) -> i32 {
    let Some(scenario) = rest.first().and_then(|s| Scenario::parse(s)) else {
        eprintln!(
            "fake_agent: claude-tui needs a scenario: \
             idle|permission|choice|worker|trust|theme|chrome|onboarding"
        );
        return crate::EXIT_USAGE;
    };
    let mut opts = TuiOpts::default();
    let mut it = rest[1..].iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--work-ms" => match it.next().and_then(|v| v.parse().ok()) {
                Some(ms) => opts.work_ms = ms,
                None => {
                    eprintln!("fake_agent: --work-ms needs a non-negative integer");
                    return crate::EXIT_USAGE;
                }
            },
            "--trust-default" => match it.next().map(String::as_str) {
                Some("yes") => opts.trust_default_yes = true,
                Some("no") => opts.trust_default_yes = false,
                _ => {
                    eprintln!("fake_agent: --trust-default needs yes or no");
                    return crate::EXIT_USAGE;
                }
            },
            "--trust-flaky" => opts.trust_flaky = true,
            other => {
                eprintln!("fake_agent: claude-tui: unknown option {other:?}");
                return crate::EXIT_USAGE;
            }
        }
    }
    let mut hooks = NoHooks;
    run_claude_tui(scenario, opts, &mut hooks, &mut || {})
}

/// The claude TUI on the alternate screen until it exits (trust "No" → 1,
/// `/exit` → 0) or a signal ends it. `on_exit` runs after the terminal is
/// restored.
pub fn run_claude_tui(
    scenario: Scenario,
    opts: TuiOpts,
    hooks: &mut dyn TurnHooks,
    on_exit: &mut dyn FnMut(),
) -> i32 {
    term::install_signal_handlers();
    let raw = RawMode::enable();
    run_claude_tui_in(scenario, opts, hooks, on_exit, raw)
}

/// [`run_claude_tui`] for a mode that already switched to raw mode.
pub fn run_claude_tui_in(
    scenario: Scenario,
    opts: TuiOpts,
    hooks: &mut dyn TurnHooks,
    on_exit: &mut dyn FnMut(),
    raw: RawMode,
) -> i32 {
    enter_claude_alt_screen();
    let mut session = Session {
        raw,
        input: Input::new(),
        alt: true,
    };
    let mut app = ClaudeTui::new(scenario, opts, hooks);
    term::run(&mut app, &mut session, on_exit)
}

// ---------------------------------------------------------------------------
// tui-idle-ready
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Claude,
    Codex,
    Cursor,
    Agy,
}

impl Style {
    fn parse(s: &str) -> Option<Style> {
        Some(match s {
            "claude" => Style::Claude,
            "codex" => Style::Codex,
            "cursor" => Style::Cursor,
            "agy" => Style::Agy,
            _ => return None,
        })
    }

    /// codex draws on the main screen; the others on the alternate screen.
    fn alt_screen(self) -> bool {
        self != Style::Codex
    }

    fn name(self) -> &'static str {
        match self {
            Style::Claude => "claude",
            Style::Codex => "codex",
            Style::Cursor => "cursor",
            Style::Agy => "agy",
        }
    }
}

struct IdleReady {
    style: Style,
    transcript: Vec<String>,
    input: String,
    queued: VecDeque<String>,
    /// The line being processed and when it is done.
    working: Option<(String, Instant)>,
}

const WORKING: &str = "working… (esc to interrupt)";

impl IdleReady {
    fn start_next(&mut self, now: Instant) {
        if self.working.is_none() {
            if let Some(next) = self.queued.pop_front() {
                self.working = Some((next, now + IDLE_READY_WORK));
            }
        }
    }
}

impl App for IdleReady {
    fn frame(&self, rows: u16, cols: u16, _now: Instant) -> Frame {
        let r = i32::from(rows);
        let c = usize::from(cols);
        let mut f = Frame::new();
        let status = self.working.as_ref().map(|_| WORKING.to_string());
        match self.style {
            Style::Claude => {
                claude_main(&mut f, rows, cols, &self.transcript, &self.input, status);
            }
            Style::Codex => {
                f.set(1, ">_ Fake Codex (v0.0.0)");
                transcript_rows(&mut f, &self.transcript, 3, r - 6);
                if let Some(s) = status {
                    f.set(r - 5, s);
                }
                if self.input.is_empty() {
                    f.set(r - 3, "› Find and fix a bug in @filename");
                } else {
                    f.set(r - 3, format!("› {}", self.input));
                }
                // An explicitly cleared row, so R-1 is reached relatively.
                f.set(r - 2, "");
                f.set(r - 1, "  fake-model low · /tmp/fake-project");
            }
            Style::Cursor => {
                transcript_rows(&mut f, &self.transcript, 2, r - 7);
                if let Some(s) = status {
                    f.set(r - 6, s);
                }
                let inner = c.saturating_sub(2);
                f.set(r - 4, format!("╭{}╮", "─".repeat(inner)));
                let text = if self.input.is_empty() {
                    " → Plan, search, build anything".to_string()
                } else {
                    format!(" → {}", self.input)
                };
                let text: String = text.chars().take(inner).collect();
                let pad = inner - text.chars().count();
                f.set(r - 3, format!("│{text}{}│", " ".repeat(pad)));
                f.set(r - 2, format!("╰{}╯", "─".repeat(inner)));
                f.set(r - 1, "  Fake Cursor Agent · / for commands · @ for files");
            }
            Style::Agy => {
                transcript_rows(&mut f, &self.transcript, 2, r - 6);
                if let Some(s) = status {
                    f.set(r - 5, s);
                }
                f.set(r - 3, format!("> {}", self.input));
                f.set(r - 2, "");
                f.set(r - 1, "  ? for shortcuts · Fake Antigravity");
            }
        }
        f
    }

    fn key(&mut self, key: Key, now: Instant) -> Step {
        match key {
            Key::Char(c) => {
                self.input.push(c);
                Step::Redraw
            }
            Key::Paste(p) => {
                self.input.push_str(&sanitize(&p));
                Step::Redraw
            }
            Key::Backspace => {
                if self.input.pop().is_some() {
                    Step::Redraw
                } else {
                    Step::Nothing
                }
            }
            Key::Enter if !self.input.is_empty() => {
                let line = std::mem::take(&mut self.input);
                self.queued.push_back(line);
                self.start_next(now);
                Step::Redraw
            }
            _ => Step::Nothing,
        }
    }

    fn tick(&mut self, now: Instant) -> Step {
        match &self.working {
            Some((_, until)) if *until <= now => {
                if let Some((line, _)) = self.working.take() {
                    self.transcript.push(format!("got: {line}"));
                }
                self.start_next(now);
                Step::Redraw
            }
            _ => Step::Nothing,
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.working.as_ref().map(|(_, until)| *until)
    }
}

/// `tui-idle-ready <ms> <style>`: a "starting" screen for `<ms>`, during
/// which every stdin byte is read and **discarded** (logged, never decoded —
/// the real bug where an early first prompt was lost [R1 §3]); then the
/// style's ready screen, which echoes each submitted line as `got: <line>`.
pub fn mode_tui_idle_ready(rest: &[String]) -> i32 {
    let ms = match crate::parse_u64(rest.first(), "ms") {
        Ok(v) => v,
        Err(c) => return c,
    };
    let Some(style) = rest.get(1).and_then(|s| Style::parse(s)) else {
        eprintln!("fake_agent: tui-idle-ready needs a style: claude|codex|cursor|agy");
        return crate::EXIT_USAGE;
    };
    term::install_signal_handlers();
    let raw = RawMode::enable();
    let mut session = Session {
        raw,
        input: Input::new(),
        alt: style.alt_screen(),
    };
    if style.alt_screen() {
        out_str(term::ALT_ENTER);
        if style == Style::Claude {
            out_str(CLAUDE_TITLE);
        }
    } else {
        // Main screen: bracketed paste only.
        out_str("\x1b[?2004h");
    }

    let starting = |rows: u16, cols: u16| {
        let mut f = Frame::new();
        f.set(1, "Starting…");
        f.set(2, format!("  fake {} is loading", style.name()));
        term::out(&f.render(rows, cols));
    };
    let (mut rows, mut cols) = term::window_size();
    starting(rows, cols);
    let ready_at = Instant::now() + Duration::from_millis(ms);
    loop {
        if let Some(sig) = term::term_signal() {
            session.leave();
            term::reraise(sig);
        }
        if term::take_winch() {
            (rows, cols) = term::window_size();
            starting(rows, cols);
        }
        let now = Instant::now();
        if now >= ready_at {
            break;
        }
        // Read and drop (the input log keeps them).
        let _ = session.input.read_timeout(ready_at - now);
    }
    // Whatever is already queued at this moment is lost too.
    let _ = session.input.drain_now();

    let mut app = IdleReady {
        style,
        transcript: Vec::new(),
        input: String::new(),
        queued: VecDeque::new(),
        working: None,
    };
    term::run(&mut app, &mut session, &mut || {})
}

// ---------------------------------------------------------------------------
// term-query
// ---------------------------------------------------------------------------

/// How long `term-query` waits for replies.
const QUERY_WAIT: Duration = Duration::from_secs(3);

/// The first `ESC [ ? <digits;…> c` in `buf`.
fn find_da1(buf: &[u8]) -> Option<&[u8]> {
    (0..buf.len()).find_map(|i| {
        let rest = &buf[i..];
        if !rest.starts_with(b"\x1b[?") {
            return None;
        }
        let body = rest[3..]
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b';')
            .count();
        (rest.get(3 + body) == Some(&b'c')).then(|| &rest[..3 + body + 1])
    })
}

/// The first `ESC [ <digits> ; <digits> R` in `buf`.
fn find_dsr(buf: &[u8]) -> Option<&[u8]> {
    (0..buf.len()).find_map(|i| {
        let rest = &buf[i..];
        if !rest.starts_with(b"\x1b[") {
            return None;
        }
        let a = rest[2..].iter().take_while(|b| b.is_ascii_digit()).count();
        if a == 0 || rest.get(2 + a) != Some(&b';') {
            return None;
        }
        let b = rest[3 + a..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        (b > 0 && rest.get(3 + a + b) == Some(&b'R')).then(|| &rest[..3 + a + b + 1])
    })
}

/// ESC as `\e`, other non-printables as `\xNN`.
fn show(bytes: &[u8]) -> String {
    let mut s = String::new();
    for b in bytes {
        match b {
            0x1b => s.push_str("\\e"),
            0x20..=0x7e => s.push(*b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s
}

/// `term-query`: DA1, DSR (cursor at row 3 column 5) and XTVERSION, then the
/// replies (or `none`) after at most 3 s, then hang until killed.
pub fn mode_term_query() -> i32 {
    term::install_signal_handlers();
    let mut raw = RawMode::enable();
    let mut input = Input::new();
    out_str("query-at\r\n\x1b[3;5H\x1b[c\x1b[6n\x1b[>0q");
    let deadline = Instant::now() + QUERY_WAIT;
    let mut got: Vec<u8> = Vec::new();
    loop {
        if let Some(sig) = term::term_signal() {
            raw.restore();
            term::reraise(sig);
        }
        if find_da1(&got).is_some() && find_dsr(&got).is_some() {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        got.extend_from_slice(&input.read_timeout(deadline - now));
    }
    out_str("\r\n");
    line(&format!(
        "da1={}",
        find_da1(&got).map_or("none".to_string(), show)
    ));
    line(&format!(
        "dsr={}",
        find_dsr(&got).map_or("none".to_string(), show)
    ));
    line("done");
    loop {
        if let Some(sig) = term::term_signal() {
            raw.restore();
            term::reraise(sig);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
