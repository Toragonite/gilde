//! Screen prompts, readiness and launch argv (contract §6.2.1–§6.2.4, §6.2.6;
//! T-pty-* of §14.2).
//!
//! * The **grammar corpus** reads the shapes recorded on 2.1.306/2.1.307
//!   [R6 §4] — wrapped labels, description lines, plain and titled dividers,
//!   `☐` headers, footers — re-typed here with synthetic paths only.
//! * The **false-positive corpus** proves the reader never offers a card for
//!   text that merely looks numbered (a card that is offered can be answered).
//! * The end-to-end tests drive `fake_agent claude-tui …` and friends over a
//!   real PTY: TOCTOU (`STALE_PROMPT`), digits without CR, Deny/Allow with a
//!   note, setup prompts never answered, the clicked trust action (A6),
//!   terminal queries, readiness for all four vendors, the first prompt typed
//!   after ready. No vendor CLI is ever started.

mod fixtures;

#[path = "fixtures/fake_agent/pty_harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::Duration;

use gilde_code_lib::agents::launch::{
    conversation_args, lead_settings_json, terminal_argv, ConvoArgs, ConvoLaunch, FirstPrompt,
    TerminalLaunch,
};
use gilde_code_lib::agents::signatures::{SignatureFile, Signatures};
use gilde_code_lib::contract::activity::{PtyStatus, ScreenPromptFacts};
use gilde_code_lib::contract::common::{AbsPath, Confidence, PermissionMode};
use gilde_code_lib::contract::ids::{ClientMsgId, RequestId, ViewId};
use gilde_code_lib::contract::interaction::{InteractionKind, OptionRole};
use gilde_code_lib::contract::ipc::{ScreenKeyOutcome, ScreenPromptAction, SendPriority};
use gilde_code_lib::contract::process::ProcKind;
use gilde_code_lib::contract::services::{OutgoingMessage, PtyApi, SessionDriver, SignalPlan};
use gilde_code_lib::contract::ErrorCode;
use gilde_code_lib::pty::{AnsiTail, PtyDriver, PtyOptions, ReadyOutcome};
use gilde_code_lib::status::evidence::{
    parse_screen_prompt, prompt_rev, screen_card, PromptGrammar, CONF_READY_SIGNATURE,
};

use harness::{contains, read_log, wait_for, Collect, Harness, CEILING};

// ---------------------------------------------------------------------------
// the reader, on grids
// ---------------------------------------------------------------------------

fn sigs() -> Signatures {
    Signatures::compile(&SignatureFile::defaults())
}

fn grammar<'a>(s: &'a Signatures, vendor: &str) -> PromptGrammar<'a> {
    PromptGrammar {
        card: s.prompt_card(vendor).expect("a promptCard for the vendor"),
        trust: s.trust_dialog(vendor),
        ready: s.ready_signature(vendor),
    }
}

/// `body` drawn at the bottom of a `rows`-high screen.
fn screen(rows: usize, body: &[&str]) -> Vec<String> {
    let mut out = vec![String::new(); rows.saturating_sub(body.len())];
    out.extend(body.iter().map(|l| l.to_string()));
    out
}

fn parse(vendor: &str, cols: usize, lines: &[String]) -> Option<ScreenPromptFacts> {
    let s = sigs();
    parse_screen_prompt(lines, cols, grammar(&s, vendor), 1_000)
}

/// Option 2 of the recorded permission prompt, sized so that it ends just
/// before the right margin of a 120-column screen and wraps "this project".
fn long_option_two() -> String {
    let prefix = "   2. Yes, and always allow access to ";
    let suffix = " from";
    let path_len = 119 - prefix.chars().count() - suffix.chars().count();
    let path = format!("/tmp/fake-project/{}", "n".repeat(path_len - 18));
    format!("{prefix}{path}{suffix}")
}

fn permission_prompt() -> Vec<String> {
    let dashed = "╌".repeat(120);
    let two = long_option_two();
    screen(
        40,
        &[
            "────────────────────────────────────────────────────────────────────────────────",
            " Bash command",
            " Tip: auto mode handles these prompts for you — choose \"switch to auto mode\" below",
            " Create empty file demo.txt",
            &dashed,
            " touch demo.txt",
            &dashed,
            " Do you want to proceed?",
            " ❯ 1. Yes",
            &two,
            "      this project",
            "   3. Yes, and switch to auto mode · auto mode handles these prompts for you",
            "   4. No",
            "",
            " Esc to cancel · Tab to amend",
        ],
    )
}

/// T-pty-promptGrammar: the recorded permission prompt, with the wrapped
/// option 2 [PM-54].
#[test]
fn the_permission_prompt_parses_with_its_wrapped_option() {
    let facts = parse("claude", 120, &permission_prompt()).expect("a prompt");
    assert_eq!(facts.question.as_deref(), Some("Do you want to proceed?"));
    assert_eq!(facts.options.len(), 4);
    let keys: Vec<&str> = facts.options.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, vec!["1", "2", "3", "4"]);
    assert!(
        facts.options[1].label.ends_with("from this project"),
        "the continuation is part of the label: {:?}",
        facts.options[1].label
    );
    assert!(facts.options[1].description.is_none());
    assert!(facts.options[0].selected && !facts.options[1].selected);
    assert!(facts.permission_shaped);
    assert!(!facts.setup && !facts.trust_dialog);
    assert_eq!(facts.footer.as_deref(), Some("Esc to cancel · Tab to amend"));
    assert!(facts.allows_note, "Tab to amend offers Allow with a note");
    assert!(facts.near_bottom);
    assert_eq!(facts.prompt_rev.len(), 16);
}

/// T-pty-promptGrammar: the AskUserQuestion choice prompt — `☐` header,
/// description lines, a divider before "Chat about this", the footer.
#[test]
fn the_choice_prompt_parses_with_header_descriptions_and_divider() {
    let lines = screen(
        40,
        &[
            "────────────────────────────────────────────────────────────────────────────────",
            " ☐ Colour",
            "Which colour do you prefer?",
            "❯ 1. Red",
            "     Choose red",
            "  2. Green",
            "     Choose green",
            "  3. Blue",
            "     Choose blue",
            "  4. Type something.",
            "────────────────────────────────────────────────────────────────────────────────",
            "  5. Chat about this",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ],
    );
    let facts = parse("claude", 120, &lines).expect("a prompt");
    // The `☐` line is part of the prompt box's header (A34.1, A35.2).
    assert_eq!(facts.header.as_deref(), Some(" ☐ Colour"));
    assert_eq!(facts.question.as_deref(), Some("Which colour do you prefer?"));
    let labels: Vec<&str> = facts.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, vec!["Red", "Green", "Blue", "Type something.", "Chat about this"]);
    assert_eq!(facts.options[0].description.as_deref(), Some("Choose red"));
    assert_eq!(facts.options[2].description.as_deref(), Some("Choose blue"));
    assert!(!facts.permission_shaped, "a choice is question-shaped");
    assert!(!facts.allows_note);
    assert_eq!(
        facts.footer.as_deref(),
        Some("Enter to select · ↑/↓ to navigate · Esc to cancel")
    );
}

/// T-pty-promptGrammar [F-09]: titled rules between AskUserQuestion options
/// in a lead, and the selection marker on a later option.
#[test]
fn titled_rule_dividers_keep_the_run_and_the_marker_names_the_current_option() {
    let lines = screen(
        30,
        &[
            " Which fruit should the team use?",
            "   1. Apple",
            "   ───Apple───",
            " ❯ 2. Banana",
            "   ─── Banana ───",
            "   3. Cherry",
            " Enter to select · Esc to cancel",
        ],
    );
    let facts = parse("claude", 100, &lines).expect("a prompt");
    let labels: Vec<&str> = facts.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, vec!["Apple", "Banana", "Cherry"]);
    let selected: Vec<bool> = facts.options.iter().map(|o| o.selected).collect();
    assert_eq!(selected, vec![false, true, false]);
}

/// The worker prompt shown in a lead (LP-11): three options, permission-shaped,
/// no Tab-to-amend.
#[test]
fn the_worker_prompt_is_permission_shaped_without_a_note() {
    let lines = screen(
        30,
        &[
            " Do you want to proceed?",
            " ❯ 1. Yes",
            "   2. Yes, and switch to auto mode",
            "   3. No",
            "",
            " Esc to cancel",
        ],
    );
    let facts = parse("claude", 100, &lines).expect("a prompt");
    assert!(facts.permission_shaped);
    assert!(!facts.allows_note);
    assert_eq!(facts.options.len(), 3);
}

/// After Tab: option 1 becomes the note field and the digest changes, so a
/// card for the old screen is stale.
#[test]
fn the_amend_field_changes_the_digest() {
    let mut amended = permission_prompt();
    let n = amended.len();
    for line in amended.iter_mut() {
        if line == " ❯ 1. Yes" {
            *line = " ❯ 1. Yes, and tell Claude what to do next".to_string();
        }
    }
    amended[n - 1] = " Esc to cancel".to_string();
    let before = parse("claude", 120, &permission_prompt()).expect("before");
    let after = parse("claude", 120, &amended).expect("after");
    assert_ne!(before.prompt_rev, after.prompt_rev);
    assert!(!after.allows_note);
    let s = sigs();
    assert!(s.prompt_card("claude").unwrap().amend_label.is_match(&after.options[0].label));
}

#[test]
fn the_digest_ignores_trailing_space_and_the_marker() {
    let a = parse("claude", 120, &permission_prompt()).expect("a");
    let mut moved = permission_prompt();
    for line in moved.iter_mut() {
        if line == " ❯ 1. Yes" {
            *line = "   1. Yes    ".to_string();
        } else if line == "   4. No" {
            *line = " ❯ 4. No".to_string();
        }
    }
    let b = parse("claude", 120, &moved).expect("b");
    assert_eq!(a.prompt_rev, b.prompt_rev, "a moved highlight is the same prompt");
    assert!(b.options[3].selected);
    // A34.1: the header is the box's rows from the solid rule down to the
    // question — the dashed lines around the command included (A38.5).
    let dashed = "╌".repeat(120);
    let header = [
        " Bash command",
        " Tip: auto mode handles these prompts for you — choose \"switch to auto mode\" below",
        " Create empty file demo.txt",
        dashed.as_str(),
        " touch demo.txt",
        dashed.as_str(),
    ]
    .join("\n");
    assert_eq!(a.header.as_deref(), Some(header.as_str()));
    assert_eq!(
        a.prompt_rev,
        prompt_rev(
            &header,
            Some("Do you want to proceed?"),
            &a.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>()
        )
    );
}

#[test]
fn a_prompt_without_a_question_line_has_none() {
    let lines = screen(
        30,
        &["", "", "", "", "", " ❯ 1. Yes", "   2. No", " Esc to cancel"],
    );
    let facts = parse("claude", 100, &lines).expect("a prompt");
    assert_eq!(facts.question, None, "the UI shows \"Question not shown\"");
}

#[test]
fn codex_s_numbered_notice_is_a_question_shaped_prompt() {
    let lines = screen(
        30,
        &[
            "  Update available! 0.0.1 -> 0.0.2",
            "",
            "› 1. Update now",
            "  2. Skip",
            "",
            "  Press enter to continue",
        ],
    );
    let facts = parse("codex", 100, &lines).expect("a prompt");
    assert_eq!(facts.options.len(), 2);
    assert!(!facts.permission_shaped);
    assert!(facts.options[0].selected);
}

/// T-pty-setupCard / A6 recognition: the folder-trust dialog is a setup
/// prompt and recognised as the trust dialog.
#[test]
fn the_trust_dialog_is_a_recognised_setup_prompt() {
    let lines = screen(
        40,
        &[
            "────────────────────────────────────────────────────────────────",
            " Accessing workspace:",
            "",
            " /tmp/fake-project",
            "",
            " Quick safety check: Is this a project you created or one you trust?",
            "",
            " ❯ No, exit",
            "   Yes, I trust this folder",
            "",
            " Enter to confirm · Esc to cancel",
        ],
    );
    let facts = parse("claude", 120, &lines).expect("a setup prompt");
    assert!(facts.setup && facts.trust_dialog);
    let labels: Vec<&str> = facts.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, vec!["No, exit", "Yes, I trust this folder"]);
    assert!(facts.options[0].selected);
    assert!(facts.options.iter().all(|o| o.key.is_empty()), "setup items have no digit");

    // Other setup menus are setup prompts, but not the trust dialog.
    let theme = screen(
        30,
        &[
            " Choose the text style that looks best with your terminal",
            "",
            "     Auto (match terminal)",
            " ❯ Dark mode",
            "     Light mode",
        ],
    );
    let facts = parse("claude", 100, &theme).expect("the theme picker");
    assert!(facts.setup && !facts.trust_dialog);
    assert_eq!(facts.options.len(), 3);
}

// ---------------------------------------------------------------------------
// the false-positive corpus (§14.2 T-pty-*: "false positives such as numbered
// lists in prose")
// ---------------------------------------------------------------------------

#[test]
fn numbered_text_that_is_not_a_prompt_never_becomes_a_card() {
    let composer_below = screen(
        40,
        &[
            "⏺ Here is the plan:",
            "  1. Read the file",
            "  2. Fix the bug",
            "  3. Run the tests",
            "",
            "────────────────────────────────────────",
            "❯ ",
            "────────────────────────────────────────",
            "  ⏸ manual mode on · ? for shortcuts",
        ],
    );
    let prose_at_the_bottom = screen(30, &["Steps:", "1. Install it", "2. Run it", "3. Done"]);
    let history_below = screen(
        30,
        &[
            "Options were:",
            "  1. Yes",
            "  2. No",
            "Then the agent continued with a long explanation that",
            "kept going over several lines of plain output text,",
            "and then some more lines of text after that,",
            "and a final line that ends the paragraph.",
            "Done.",
        ],
    );
    let single = screen(30, &["Do you want to proceed?", " ❯ 1. Yes", "", " Esc to cancel"]);
    let gap_in_digits = screen(30, &[" Pick one", " ❯ 1. Alpha", "   3. Gamma", " Esc to cancel"]);
    let starts_at_two = screen(30, &[" Pick one", " ❯ 2. Beta", "   3. Gamma", " Esc to cancel"]);
    let ten_and_up = screen(30, &[" Pick", " ❯ 10. Ten", "   11. Eleven", " Esc to cancel"]);
    let blank_between = screen(30, &[" Pick", " ❯ 1. One", "", "", "   2. Two", " Esc to cancel"]);
    let codex_composer = screen(
        30,
        &[
            "• Summary:",
            "  1. Changed the parser",
            "  2. Added tests",
            "",
            "› Find and fix a bug in @filename",
            "",
            "  fake-model low · /tmp/fake-project",
        ],
    );
    let lone_composer = screen(30, &["────────", "❯ Try \"hello\"", "────────"]);

    for (name, lines, vendor) in [
        ("numbered list above the composer", &composer_below, "claude"),
        ("prose list at the bottom (no footer, no marker)", &prose_at_the_bottom, "claude"),
        ("old options with text below", &history_below, "claude"),
        ("a single option", &single, "claude"),
        ("a gap in the digits", &gap_in_digits, "claude"),
        ("a run that starts at 2", &starts_at_two, "claude"),
        ("two-digit numbers", &ten_and_up, "claude"),
        ("blank lines between options", &blank_between, "claude"),
        ("codex list above its composer", &codex_composer, "codex"),
        ("the composer alone is no menu", &lone_composer, "claude"),
    ] {
        assert_eq!(parse(vendor, 100, lines), None, "{name} must not be a prompt");
    }
}

/// A vendor without a `promptCard` never yields a card.
#[test]
fn a_vendor_without_a_grammar_has_no_cards() {
    let s = sigs();
    assert!(s.prompt_card("shell").is_none());
    assert!(s.prompt_card("gemini").is_none());
    for vendor in ["claude", "codex", "cursor", "agy"] {
        assert!(s.prompt_card(vendor).is_some(), "{vendor}");
        assert!(s.ready_signature(vendor).is_some(), "{vendor} ships a ready signature (A5)");
    }
}

// ---------------------------------------------------------------------------
// the card
// ---------------------------------------------------------------------------

#[test]
fn the_card_carries_the_digest_roles_and_confidence() {
    let facts = parse("claude", 120, &permission_prompt()).expect("prompt");
    let card = screen_card(&facts, Some(0.85));
    assert_eq!(card.kind, InteractionKind::ScreenPrompt);
    assert_eq!(card.request_id.as_str(), format!("sp_{}", facts.prompt_rev));
    assert_eq!(card.prompt_rev.as_deref(), Some(facts.prompt_rev.as_str()));
    assert_eq!(card.confidence, Confidence::High);
    assert!(!card.default_to_no, "⌘⏎ is live on a High permission prompt");
    let roles: Vec<OptionRole> = card.options.iter().map(|o| o.role).collect();
    assert_eq!(
        roles,
        vec![OptionRole::AllowWithNote, OptionRole::Other, OptionRole::Other, OptionRole::Deny],
        "Tab to amend makes option 1 the note-capable allow (A25.1); the last option is the Deny that stops the turn [F-53]"
    );
    assert_eq!(card.title, "Do you want to proceed?");

    // Below the threshold, or away from the bottom: Likely, ⌘⏎ inert.
    let likely = screen_card(&facts, Some(0.5));
    assert_eq!(likely.confidence, Confidence::Likely);
    assert!(likely.default_to_no);
    let mut high_up = facts.clone();
    high_up.near_bottom = false;
    assert_eq!(screen_card(&high_up, Some(0.9)).confidence, Confidence::Likely);
}

/// A25.1: option roles come from the parsed grammar — `AllowWithNote` only
/// on option 1 of a permission prompt whose footer offers "Tab to amend"
/// while option 1 is highlighted; never on a question.
#[test]
fn the_note_role_needs_tab_to_amend_and_the_highlight_on_option_one() {
    let roles = |lines: &[String], cols: usize| -> Vec<OptionRole> {
        let facts = parse("claude", cols, lines).expect("a prompt");
        screen_card(&facts, Some(0.9)).options.iter().map(|o| o.role).collect()
    };
    // The worker prompt has no "Tab to amend": a plain allow.
    let worker = screen(
        30,
        &[" Do you want to proceed?", " ❯ 1. Yes", "   2. Yes, and switch to auto mode", "   3. No", "", " Esc to cancel"],
    );
    assert_eq!(roles(&worker, 100), vec![OptionRole::AllowOnce, OptionRole::Other, OptionRole::Deny]);

    // The user moved the highlight in the terminal: Tab would amend another
    // option, so the card does not offer the note.
    let moved: Vec<String> = permission_prompt()
        .into_iter()
        .map(|l| match l.as_str() {
            " ❯ 1. Yes" => "   1. Yes".to_string(),
            "   4. No" => " ❯ 4. No".to_string(),
            _ => l,
        })
        .collect();
    assert_eq!(roles(&moved, 120)[0], OptionRole::AllowOnce, "no note while option 4 is highlighted");

    // A question with the same footer is answered, never amended.
    let question = screen(
        30,
        &[" Which colour do you prefer?", " ❯ 1. Red", "   2. Green", "", " Esc to cancel · Tab to amend"],
    );
    assert_eq!(roles(&question, 100), vec![OptionRole::Answer, OptionRole::Answer]);

    // Every key the cards offer has the §3.4.5 form.
    for lines in [permission_prompt(), worker, moved, question] {
        let facts = parse("claude", 120, &lines).expect("a prompt");
        assert!(screen_card(&facts, Some(0.9))
            .options
            .iter()
            .all(|o| gilde_code_lib::contract::interaction::is_option_key(&o.key)));
    }
}

fn trust_dialog(highlight_trust: bool) -> Vec<String> {
    let (no, yes) = if highlight_trust {
        ("   No, exit", " ❯ Yes, I trust this folder")
    } else {
        (" ❯ No, exit", "   Yes, I trust this folder")
    };
    screen(
        40,
        &[
            " Accessing workspace:",
            "",
            " /tmp/fake-project",
            "",
            " Quick safety check: Is this a project you created or one you trust?",
            "",
            no,
            yes,
            "",
            " Enter to confirm · Esc to cancel",
        ],
    )
}

/// A25.1 + A6: the recognised trust dialog's card carries exactly one
/// `TrustFolder` option (the trust item: its menu position and text); every
/// other setup card offers none.
#[test]
fn the_trust_dialog_card_offers_the_trust_folder_option_and_nothing_else_does() {
    let facts = parse("claude", 120, &trust_dialog(false)).expect("the trust dialog");
    let card = screen_card(&facts, Some(0.9));
    assert_eq!(card.kind, InteractionKind::SetupPrompt);
    assert_eq!(card.options.len(), 1, "{:?}", card.options);
    let trust = &card.options[0];
    assert_eq!((trust.key.as_str(), trust.label.as_str(), trust.role), ("2", "Yes, I trust this folder", OptionRole::TrustFolder));
    assert!(card.options.iter().all(|o| o.role != OptionRole::AllowOnce), "no overloaded allow (A25.1)");

    // Moving the highlight keeps the card (and its digest) the same.
    let moved = parse("claude", 120, &trust_dialog(true)).expect("still the trust dialog");
    assert_eq!(moved.prompt_rev, facts.prompt_rev);
    assert_eq!(screen_card(&moved, Some(0.9)).options, card.options);

    // Without the recognised dialog — or with no single "Yes" item — no trust option.
    let theme = screen(
        30,
        &[" Choose the text style that looks best with your terminal", "", "     Auto (match terminal)", " ❯ Dark mode", "     Light mode"],
    );
    let theme = parse("claude", 100, &theme).expect("the theme picker");
    assert!(theme.setup && !theme.trust_dialog);
    assert!(screen_card(&theme, Some(0.9)).options.is_empty(), "other setup prompts are never answered");
    let mut not_recognised = facts.clone();
    not_recognised.trust_dialog = false;
    assert!(screen_card(&not_recognised, Some(0.9)).options.is_empty());
    let mut two_yes = facts.clone();
    two_yes.options[0].label = "Yes, exit".into();
    assert!(screen_card(&two_yes, Some(0.9)).options.is_empty(), "an ambiguous menu offers nothing");
    // A numbered prompt never carries the setup role.
    let numbered = parse("claude", 120, &permission_prompt()).expect("prompt");
    assert!(screen_card(&numbered, Some(0.9)).options.iter().all(|o| o.role != OptionRole::TrustFolder));
}

// ---------------------------------------------------------------------------
// pending wrap and the alternate screen, end to end through the grid
// ---------------------------------------------------------------------------

/// T-pty-pendingWrap (golden): a synthesized 2.1.307-style stream — alternate
/// screen, cell-diff moves, full-width rules followed by CR LF — replays into
/// exactly the expected grid.
#[test]
fn a_cell_diff_stream_replays_into_the_golden_grid() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/streams");
    let bytes = std::fs::read(dir.join("tui-pending-wrap.bin")).expect("stream fixture");
    let golden = std::fs::read_to_string(dir.join("tui-pending-wrap.grid.txt")).expect("golden grid");
    let mut tail = AnsiTail::with_size(60, 12);
    // Fed in small, uneven chunks: a split inside a sequence changes nothing.
    for chunk in bytes.chunks(7) {
        tail.feed(chunk);
    }
    let lines = tail.screen_lines();
    let expected: Vec<String> = golden.lines().map(|l| l.trim_end().to_string()).collect();
    assert_eq!(lines, expected, "\n{}\n", lines.join("\n"));
    assert!(tail.alt_screen());
    // The screen it draws is a prompt with its footer on the right row.
    let facts = parse("claude", 60, &lines).expect("the prompt on the golden grid");
    assert_eq!(facts.options.len(), 3);
    assert_eq!(facts.footer.as_deref(), Some("Esc to cancel · Tab to amend"));
}

// ---------------------------------------------------------------------------
// launch argv (§6.1.1, §6.2.1) — T-pty-permissionModeArgv
// ---------------------------------------------------------------------------

fn abs(p: &str) -> AbsPath {
    AbsPath::parse(p).expect("abs")
}

const UUID: &str = "0b7f6f9e-8a8f-4c4e-9d4b-6c1f2a3b4c5d";

#[test]
fn every_gilde_started_claude_gets_an_explicit_permission_mode() {
    let claude = abs("/opt/bin/claude");
    let new = terminal_argv(
        &claude,
        &TerminalLaunch::ClaudeNew {
            session_id: UUID.into(),
            permission_mode: PermissionMode::Default,
            model: Some("haiku".into()),
            prompt: Some("--model x; do it".into()),
        },
    )
    .expect("argv");
    assert_eq!(
        new.args,
        vec!["--session-id", UUID, "--permission-mode", "default", "--model", "haiku", "--", "--model x; do it"]
    );
    assert_eq!(new.prompt, FirstPrompt::Argv);
    assert_eq!(new.kind, ProcKind::Pty);

    let resume = terminal_argv(
        &claude,
        &TerminalLaunch::ClaudeResume {
            session_id: UUID.into(),
            fork: true,
            permission_mode: PermissionMode::AcceptEdits,
            model: None,
            prompt: Some("continue".into()),
        },
    )
    .expect("argv");
    assert_eq!(
        resume.args,
        vec!["--resume", UUID, "--fork-session", "--permission-mode", "acceptEdits"]
    );
    assert_eq!(resume.prompt, FirstPrompt::TypedAfterReady("continue".into()));

    let lead = terminal_argv(
        &claude,
        &TerminalLaunch::ClaudeHostedLead {
            session_id: UUID.into(),
            permission_mode: PermissionMode::Plan,
            model: None,
            prompt: Some("build the team".into()),
        },
    )
    .expect("argv");
    assert_eq!(
        lead.args,
        vec![
            "--teammate-mode",
            "tmux",
            "--session-id",
            UUID,
            "--permission-mode",
            "plan",
            "--settings",
            r#"{"permissions":{"defaultMode":"plan"}}"#,
            "--",
            "build the team"
        ]
    );
    assert_eq!(lead_settings_json(PermissionMode::BypassPermissions), r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#);

    let sign_in = terminal_argv(&claude, &TerminalLaunch::ClaudeSignIn).expect("argv");
    assert_eq!(sign_in.args, vec!["--permission-mode", "default"], "[F-44]");
    assert_eq!(sign_in.prompt, FirstPrompt::TypedAfterReady("/login".into()));

    let attach = terminal_argv(&claude, &TerminalLaunch::ClaudeAttach { session_id: UUID.into() }).expect("argv");
    assert_eq!(attach.args, vec!["attach", UUID]);
    assert_eq!(attach.kind, ProcKind::Attach);
}

#[test]
fn conversation_argv_is_the_exact_stem_plus_the_launch_flags() {
    let stem = [
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
    ];
    let new = conversation_args(&ConvoArgs {
        launch: ConvoLaunch::New { session_id: UUID.into() },
        permission_mode: PermissionMode::Default,
        model: None,
    })
    .expect("argv");
    let mut expected: Vec<String> = stem.iter().map(|s| s.to_string()).collect();
    expected.extend(["--session-id", UUID, "--permission-mode", "default"].map(String::from));
    assert_eq!(new, expected);

    let copy = conversation_args(&ConvoArgs {
        launch: ConvoLaunch::Copy { session_id: UUID.into() },
        permission_mode: PermissionMode::BypassPermissions,
        model: Some("opus".into()),
    })
    .expect("argv");
    assert!(copy.ends_with(&[
        "--resume".to_string(),
        UUID.to_string(),
        "--fork-session".to_string(),
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
        "--model".to_string(),
        "opus".to_string(),
        "--allow-dangerously-skip-permissions".to_string(),
    ]));

    let other = conversation_args(&ConvoArgs {
        launch: ConvoLaunch::OtherAccount { transcript: abs("/tmp/t/s.jsonl") },
        permission_mode: PermissionMode::Default,
        model: None,
    })
    .expect("argv");
    assert!(other.windows(3).any(|w| w == ["--resume", "/tmp/t/s.jsonl", "--fork-session"]));
}

#[test]
fn the_other_vendors_follow_their_rows_and_nothing_flag_like_reaches_argv() {
    let codex = abs("/opt/bin/codex");
    let new = terminal_argv(
        &codex,
        &TerminalLaunch::Codex { resume: None, model: Some("m-1".into()), prompt: Some("fix it".into()) },
    )
    .expect("argv");
    assert_eq!(new.args, vec!["--model", "m-1", "fix it"]);
    assert_eq!(new.prompt, FirstPrompt::Argv);
    let flaggy = terminal_argv(
        &codex,
        &TerminalLaunch::Codex { resume: None, model: None, prompt: Some("--yolo".into()) },
    )
    .expect("argv");
    assert!(flaggy.args.is_empty(), "a prompt that looks like a flag is never on argv");
    assert_eq!(flaggy.prompt, FirstPrompt::TypedAfterReady("--yolo".into()));
    let resumed = terminal_argv(
        &codex,
        &TerminalLaunch::Codex { resume: Some("abc".into()), model: None, prompt: Some("go".into()) },
    )
    .expect("argv");
    assert_eq!(resumed.args, vec!["resume", "abc"]);
    assert_eq!(resumed.prompt, FirstPrompt::TypedAfterReady("go".into()));

    let cursor = terminal_argv(
        &abs("/opt/bin/cursor-agent"),
        &TerminalLaunch::Cursor { resume: Some("chat-1".into()), model: None, prompt: Some("hi".into()) },
    )
    .expect("argv");
    assert_eq!(cursor.args, vec!["--resume", "chat-1"]);
    assert_eq!(cursor.prompt, FirstPrompt::TypedAfterReady("hi".into()), "[R1 §3 Bug]");

    let agy = terminal_argv(
        &abs("/opt/bin/agy"),
        &TerminalLaunch::Agy { conversation: Some("c-9".into()), prompt: None },
    )
    .expect("argv");
    assert_eq!(agy.args, vec!["--conversation", "c-9"]);

    let pane = terminal_argv(&abs("/opt/bin/claude"), &TerminalLaunch::Pane { command: "cd /tmp && exec x".into() })
        .expect("argv");
    assert_eq!(pane.kind, ProcKind::Pane);
    assert_eq!(pane.argv0.as_str(), "/bin/sh");
    assert_eq!(pane.args, vec!["-c", "cd /tmp && exec x"]);

    for bad in [
        TerminalLaunch::ClaudeResume {
            session_id: "--dangerously-skip-permissions".into(),
            fork: false,
            permission_mode: PermissionMode::Default,
            model: None,
            prompt: None,
        },
        TerminalLaunch::ClaudeNew {
            session_id: "not-a-uuid".into(),
            permission_mode: PermissionMode::Default,
            model: None,
            prompt: None,
        },
        TerminalLaunch::ClaudeNew {
            session_id: UUID.into(),
            permission_mode: PermissionMode::Default,
            model: Some("bad model".into()),
            prompt: None,
        },
        TerminalLaunch::Cursor { resume: Some("a b".into()), model: None, prompt: None },
        TerminalLaunch::ClaudeNew {
            session_id: UUID.into(),
            permission_mode: PermissionMode::Default,
            model: None,
            prompt: Some("nul\0byte".into()),
        },
    ] {
        let err = terminal_argv(&abs("/opt/bin/claude"), &bad).expect_err("refused");
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{bad:?}");
    }
}

// ---------------------------------------------------------------------------
// end to end, over a real PTY with the fake TUI
// ---------------------------------------------------------------------------

struct Tui {
    h: Harness,
    id: gilde_code_lib::contract::ids::PtyId,
    log: std::path::PathBuf,
}

impl Tui {
    async fn start(binary: &str, args: &[&str], options: PtyOptions) -> Tui {
        let h = Harness::new().await;
        let bin = h.install(binary);
        let log = h.temp_file("input.log");
        let mut spec = h.spec(&bin, args);
        spec.env.push(("FAKE_AGENT_INPUT_LOG".into(), log.to_string_lossy().into_owned()));
        let (handle, _exits) = h.open_with(spec, options).await;
        Tui { h, id: handle.pty_id, log }
    }

    async fn claude(args: &[&str]) -> Tui {
        Tui::start("claude", args, PtyOptions::default()).await
    }

    fn input(&self) -> Vec<u8> {
        read_log(&self.log)
    }

    fn screen(&self) -> String {
        self.h.screen(&self.id)
    }

    async fn prompt(&self) -> ScreenPromptFacts {
        let found = wait_for(CEILING, || self.h.manager.screen_prompt(&self.id).is_some()).await;
        assert!(found, "no prompt appeared; screen:\n{}", self.screen());
        self.h.manager.screen_prompt(&self.id).expect("prompt")
    }

    async fn shows(&self, text: &str) -> bool {
        wait_for(CEILING, || self.screen().contains(text)).await
    }

    async fn close(self) {
        let _ = self
            .h
            .manager
            .close(&self.id, SignalPlan::HupThenTerm { grace_ms: 2_000 })
            .await;
    }
}

/// T-pty-stale: a key for a screen that moved on is refused under the write
/// lock; nothing is typed.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_for_a_stale_prompt_is_refused_and_nothing_is_typed() {
    let t = Tui::claude(&["claude-tui", "permission"]).await;
    let old = t.prompt().await;
    // The user presses Tab in the terminal: option 1 becomes the note field.
    t.h.manager.write(&t.id, b"\t").await.expect("tab");
    assert!(
        wait_for(CEILING, || t
            .h
            .manager
            .screen_prompt(&t.id)
            .is_some_and(|p| p.prompt_rev != old.prompt_rev))
        .await,
        "the amend screen is a new prompt; screen:\n{}",
        t.screen()
    );
    let before = t.input();
    let err = t
        .h
        .manager
        .screen_action(&t.id, &old.prompt_rev, &ScreenPromptAction::Key { key: "1".into() })
        .await
        .expect_err("stale");
    assert_eq!(err.code, ErrorCode::StalePrompt);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(t.input(), before, "a stale key types nothing");
    t.close().await;
}

/// Digits select at once with no CR (frozen LP-12); a key the prompt does
/// not offer is UNSUPPORTED; `turn_send` is refused while a prompt is
/// pending.
#[tokio::test(flavor = "multi_thread")]
async fn a_digit_is_sent_without_cr_and_send_is_refused_while_the_prompt_is_up() {
    let t = Tui::claude(&["claude-tui", "permission"]).await;
    let p = t.prompt().await;

    let mut driver = PtyDriver::new(t.h.manager.clone(), t.id.clone());
    let err = driver
        .send(OutgoingMessage {
            client_msg_id: ClientMsgId::parse("0b7f6f9e-8a8f-4c4e-9d4b-6c1f2a3b4c5e").expect("id"),
            text: "hello".into(),
            priority: SendPriority::Next,
            images: Vec::new(),
        })
        .await
        .expect_err("refused");
    assert_eq!(err.code, ErrorCode::Refused);
    assert_eq!(err.params["reason"], "screenPromptPending");

    let err = t
        .h
        .manager
        .screen_action(&t.id, &p.prompt_rev, &ScreenPromptAction::Key { key: "9".into() })
        .await
        .expect_err("not an option");
    assert_eq!(err.code, ErrorCode::Unsupported);
    assert_eq!(err.params["cap"], "key");

    let req = RequestId::parse(format!("sp_{}", p.prompt_rev)).expect("request id");
    let outcome = driver
        .screen_key(&req, &p.prompt_rev, ScreenPromptAction::Key { key: "1".into() })
        .await
        .expect("sent");
    assert_eq!(outcome, ScreenKeyOutcome::Sent);
    assert!(t.shows("selected 1").await, "screen:\n{}", t.screen());
    assert_eq!(t.input(), b"1".to_vec(), "exactly the digit, no CR");
    // The card leaves once the grid stops matching.
    assert!(wait_for(CEILING, || t.h.manager.screen_prompt(&t.id).is_none()).await);
    t.close().await;
}

/// T-pty-denyWithReply: the "No" digit, then (once the prompt left the grid)
/// the reply as a bracketed paste + CR.
#[tokio::test(flavor = "multi_thread")]
async fn deny_with_reply_sends_no_then_the_reply_as_the_next_turn() {
    let t = Tui::claude(&["claude-tui", "permission"]).await;
    let p = t.prompt().await;
    let outcome = t
        .h
        .manager
        .screen_action(
            &t.id,
            &p.prompt_rev,
            &ScreenPromptAction::DenyWithReply { text: "use a temp dir instead".into() },
        )
        .await
        .expect("deny with reply");
    assert_eq!(outcome, ScreenKeyOutcome::Sent);
    let mut expected = b"4".to_vec();
    expected.extend_from_slice(b"\x1b[200~use a temp dir instead\x1b[201~\r");
    assert!(wait_for(CEILING, || t.input() == expected).await, "input: {:?}", String::from_utf8_lossy(&t.input()));
    assert!(t.shows("Interrupted").await);
    assert!(t.shows("got: use a temp dir instead").await, "screen:\n{}", t.screen());
    t.close().await;
}

/// T-pty-allowWithNote: Tab, wait for the note field, paste the note, CR; and
/// only where the footer offers "Tab to amend".
#[tokio::test(flavor = "multi_thread")]
async fn allow_with_note_tabs_into_the_field_then_pastes_and_submits() {
    let t = Tui::claude(&["claude-tui", "permission"]).await;
    let p = t.prompt().await;
    let outcome = t
        .h
        .manager
        .screen_action(
            &t.id,
            &p.prompt_rev,
            &ScreenPromptAction::AllowWithNote { text: "name it demo2.txt".into() },
        )
        .await
        .expect("allow with note");
    assert_eq!(outcome, ScreenKeyOutcome::Sent);
    let mut expected = b"\t".to_vec();
    expected.extend_from_slice(b"\x1b[200~name it demo2.txt\x1b[201~\r");
    assert!(wait_for(CEILING, || t.input() == expected).await, "input: {:?}", String::from_utf8_lossy(&t.input()));
    assert!(t.shows("approved; note:").await, "screen:\n{}", t.screen());
    t.close().await;

    let w = Tui::claude(&["claude-tui", "worker"]).await;
    let p = w.prompt().await;
    let err = w
        .h
        .manager
        .screen_action(&w.id, &p.prompt_rev, &ScreenPromptAction::AllowWithNote { text: "x".into() })
        .await
        .expect_err("no Tab to amend");
    assert_eq!(err.code, ErrorCode::Unsupported);
    assert_eq!(err.params["cap"], "allowWithNote");
    assert!(w.input().is_empty(), "nothing typed");
    w.close().await;
}

/// T-pty-setupCard: a setup prompt is never answered by a key, Gilde types
/// nothing by itself, and the Starting timeout does not run while it is up.
#[tokio::test(flavor = "multi_thread")]
async fn setup_prompts_are_never_answered_and_suspend_the_starting_timeout() {
    let t = Tui::claude(&["claude-tui", "theme"]).await;
    let p = t.prompt().await;
    assert!(p.setup && !p.trust_dialog);
    for action in [
        ScreenPromptAction::Key { key: "1".into() },
        ScreenPromptAction::Key { key: "esc".into() },
        ScreenPromptAction::DenyWithReply { text: "x".into() },
        ScreenPromptAction::TrustFolder,
    ] {
        let err = t
            .h
            .manager
            .screen_action(&t.id, &p.prompt_rev, &action)
            .await
            .expect_err("refused");
        assert_eq!(err.code, ErrorCode::Refused, "{action:?}");
        assert_eq!(err.params["reason"], "setupPrompt");
    }
    // A 1 s Starting budget does not run while the menu is on screen (it is
    // watched for 2 s); the notes screen after it is far shorter than 1 s.
    let waiting = tokio::spawn({
        let manager = t.h.manager.clone();
        let id = t.id.clone();
        async move { PtyDriver::wait_ready(&manager, &id, Duration::from_secs(3)).await }
    });
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        !waiting.is_finished(),
        "a 3 s budget is still running after 4 s: it is suspended while setup is pending"
    );
    assert!(t.input().is_empty(), "Gilde typed nothing into the setup prompt");
    // The user finishes onboarding in the terminal: Enter (theme), Enter (notes).
    t.h.manager.write(&t.id, b"\r").await.expect("enter");
    assert!(t.shows("Press Enter to continue").await, "screen:\n{}", t.screen());
    t.h.manager.write(&t.id, b"\r").await.expect("enter");
    let outcome = tokio::time::timeout(CEILING, waiting).await.expect("resolves").expect("join");
    assert_eq!(outcome, ReadyOutcome::Ready, "screen:\n{}", t.screen());
    t.close().await;
}

/// T-pty-trustCard (A6): no key without a click; on the click at most three
/// single arrows with a re-read after each, then one Enter.
#[tokio::test(flavor = "multi_thread")]
async fn the_clicked_trust_action_moves_to_the_trust_option_and_confirms_once() {
    let t = Tui::claude(&["claude-tui", "trust"]).await;
    let p = t.prompt().await;
    assert!(p.setup && p.trust_dialog);
    assert_eq!(p.options[0].label, "No, exit");
    assert!(p.options[0].selected);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(t.input().is_empty(), "no key without a click");

    let outcome = t
        .h
        .manager
        .screen_action(&t.id, &p.prompt_rev, &ScreenPromptAction::TrustFolder)
        .await
        .expect("trust");
    assert_eq!(outcome, ScreenKeyOutcome::Sent);
    let input = t.input();
    assert!(
        input == b"\x1b[B\r".to_vec() || input == b"\x1bOB\r".to_vec(),
        "one Down, then Enter: {:?}",
        String::from_utf8_lossy(&input)
    );
    assert!(t.shows("trusted").await, "screen:\n{}", t.screen());
    t.close().await;

    // Already on the trust option: just Enter.
    let y = Tui::claude(&["claude-tui", "trust", "--trust-default", "yes"]).await;
    let p = y.prompt().await;
    let outcome = y
        .h
        .manager
        .screen_action(&y.id, &p.prompt_rev, &ScreenPromptAction::TrustFolder)
        .await
        .expect("trust");
    assert_eq!(outcome, ScreenKeyOutcome::Sent);
    // Wait for the fake to act on the key before reading its input log
    // (A10.7: an event, not a wall-clock assumption).
    assert!(y.shows("trusted").await, "screen:\n{}", y.screen());
    assert_eq!(y.input(), b"\r".to_vec());
    y.close().await;
}

/// T-pty-trustCard (abort): the screen changes after a key → aborted, no
/// Enter.
#[tokio::test(flavor = "multi_thread")]
async fn the_trust_action_aborts_without_enter_when_the_screen_changes() {
    let t = Tui::claude(&["claude-tui", "trust", "--trust-flaky"]).await;
    let p = t.prompt().await;
    let outcome = t
        .h
        .manager
        .screen_action(&t.id, &p.prompt_rev, &ScreenPromptAction::TrustFolder)
        .await
        .expect("action");
    assert_eq!(outcome, ScreenKeyOutcome::Aborted);
    let input = t.input();
    assert!(!input.contains(&b'\r'), "no Enter after an abort: {:?}", String::from_utf8_lossy(&input));
    assert_eq!(input.iter().filter(|b| **b == 0x1b).count(), 1, "exactly one arrow was sent");
    t.close().await;
}

/// T-pty-trustCard (abort): a highlight that never reaches the trust option
/// within three keys → aborted, no Enter.
#[tokio::test(flavor = "multi_thread")]
async fn the_trust_action_aborts_after_three_keys_that_do_not_move_the_highlight() {
    let h = Harness::new().await;
    let log = h.temp_file("sh-input.log");
    // A dialog that ignores every key (raw, no echo; input goes to a log).
    let script = format!(
        "/bin/stty raw -echo; printf '\\033[?1049h\\033[2J\\033[H\\033[10;1H Quick safety check: trust it?\\r\\n\\r\\n \\342\\235\\257 No, exit\\r\\n   Yes, I trust this folder\\r\\n\\r\\n Enter to confirm \\302\\267 Esc to cancel'; exec /bin/cat > '{}'",
        log.display()
    );
    let (handle, _exits) = h
        .open_with(
            h.shell(&script),
            PtyOptions { signature_key: Some("claude".into()), ..PtyOptions::default() },
        )
        .await;
    assert!(
        wait_for(CEILING, || h.manager.screen_prompt(&handle.pty_id).is_some()).await,
        "screen:\n{}",
        h.screen(&handle.pty_id)
    );
    let p = h.manager.screen_prompt(&handle.pty_id).expect("prompt");
    assert!(p.trust_dialog);
    let outcome = h
        .manager
        .screen_action(&handle.pty_id, &p.prompt_rev, &ScreenPromptAction::TrustFolder)
        .await
        .expect("action");
    assert_eq!(outcome, ScreenKeyOutcome::Aborted);
    assert!(wait_for(CEILING, || read_log(&log).len() >= 9).await);
    let input = read_log(&log);
    assert_eq!(input, b"\x1b[B\x1b[B\x1b[B".to_vec(), "three single arrows, no Enter");
    let _ = h.manager.close(&handle.pty_id, SignalPlan::HupThenTerm { grace_ms: 2_000 }).await;
}

/// T-pty-termQueries: without a view the core answers DA1 and DSR; with a
/// view it leaves them to xterm.js.
#[tokio::test(flavor = "multi_thread")]
async fn terminal_queries_are_answered_by_the_core_only_without_a_view() {
    let hidden = Tui::start("agent", &["term-query"], PtyOptions::default()).await;
    assert!(hidden.shows("done").await, "screen:\n{}", hidden.screen());
    let screen = hidden.screen();
    assert!(screen.contains("da1=\\e[?1;2c"), "{screen}");
    assert!(screen.contains("dsr=\\e[3;5R"), "{screen}");
    hidden.close().await;

    let h = Harness::new().await;
    let bin = h.install("agent");
    let spec = h.spec(&bin, &["term-query"]);
    let view = Arc::new(Collect::default());
    // Attach as early as possible: the queries go to the view, which (in this
    // test) answers nothing.
    let (handle, _exits) = h.open(spec).await;
    h.manager
        .attach(&handle.pty_id, ViewId::parse("v1").expect("view"), view.clone())
        .expect("attach");
    assert!(wait_for(CEILING, || h.screen(&handle.pty_id).contains("done")).await);
    let screen = h.screen(&handle.pty_id);
    if screen.contains("da1=none") {
        assert!(screen.contains("dsr=none"), "{screen}");
    } else {
        // The fake asked before the view attached: the core answered — never both.
        assert!(screen.contains("da1=\\e[?1;2c"), "{screen}");
    }
    let _ = h.manager.close(&handle.pty_id, SignalPlan::HupThenTerm { grace_ms: 2_000 }).await;
}

/// T-pty-alternateIdle: all four vendors' ready screens reach Idle by their
/// signatures (0.9); a vendor without one reaches Idle at 0.5 by the
/// settled-grid fallback.
#[tokio::test(flavor = "multi_thread")]
async fn every_vendor_s_ready_screen_is_idle_and_the_fallback_reaches_half() {
    for (binary, style) in [("claude", "claude"), ("codex", "codex"), ("cursor-agent", "cursor"), ("agy", "agy")] {
        let t = Tui::start(binary, &["tui-idle-ready", "200", style], PtyOptions::default()).await;
        let idle = wait_for(CEILING, || {
            t.h.manager
                .facts(&t.id)
                .is_some_and(|f| f.status == PtyStatus::Idle && f.confidence >= CONF_READY_SIGNATURE)
        })
        .await;
        assert!(idle, "{style}: {:?}\nscreen:\n{}", t.h.manager.facts(&t.id), t.screen());
        assert_eq!(
            PtyDriver::wait_ready(&t.h.manager, &t.id, CEILING).await,
            ReadyOutcome::Ready,
            "{style}"
        );
        t.close().await;
    }

    // The same agy screen read as a vendor with no signature at all.
    let t = Tui::start(
        "agy",
        &["tui-idle-ready", "200", "agy"],
        PtyOptions { signature_key: Some("gemini".into()), ..PtyOptions::default() },
    )
    .await;
    let fallback = wait_for(CEILING, || {
        t.h.manager
            .facts(&t.id)
            .is_some_and(|f| f.status == PtyStatus::Idle && (f.confidence - 0.5).abs() < 1e-6)
    })
    .await;
    assert!(fallback, "{:?}\nscreen:\n{}", t.h.manager.facts(&t.id), t.screen());
    assert_eq!(PtyDriver::wait_ready(&t.h.manager, &t.id, CEILING).await, ReadyOutcome::Ready);
    t.close().await;
}

/// The first-prompt regression [R1 §3 Bug]: cursor and agy drop input that
/// arrives before their ready screen; the prompt is typed after ready.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_prompt_is_typed_after_ready_and_not_lost() {
    for (binary, style) in [("cursor-agent", "cursor"), ("agy", "agy")] {
        let t = Tui::start(binary, &["tui-idle-ready", "1500", style], PtyOptions::default()).await;
        let driver = PtyDriver::new(t.h.manager.clone(), t.id.clone());
        let outcome = driver
            .deliver_first_prompt("fix the failing test".into(), CEILING)
            .await
            .expect("join")
            .expect("delivered");
        assert_eq!(outcome, ReadyOutcome::Ready);
        assert!(t.shows("got: fix the failing test").await, "{style}: screen:\n{}", t.screen());
        t.close().await;
    }
}

/// T-pty-pasteMarker end to end: the end marker inside pasted text never
/// closes the paste.
#[tokio::test(flavor = "multi_thread")]
async fn a_pasted_end_marker_never_reaches_the_child_raw() {
    let t = Tui::claude(&["claude-tui", "idle"]).await;
    assert!(t.shows("manual mode on").await, "screen:\n{}", t.screen());
    t.h.manager
        .paste(&t.id, "a\x1b[201~b", true)
        .await
        .expect("paste");
    let expected = b"\x1b[200~a\x1b[201~\x1b[200~b\x1b[201~\r".to_vec();
    assert!(wait_for(CEILING, || t.input() == expected).await, "{:?}", String::from_utf8_lossy(&t.input()));
    assert!(!contains(&t.input(), b"a\x1b[201~b"));
    t.close().await;
}

/// The engine's facts reach a supplied evidence sink as `SourceFacts::Pty`
/// with the process key and spawn id as the identity hint.
#[tokio::test(flavor = "multi_thread")]
async fn s5_facts_reach_the_evidence_sink_with_an_identity_hint() {
    use gilde_code_lib::contract::services::{EvidenceSink, EvidenceUpdate, LocationFacts, SourceFacts};
    #[derive(Default)]
    struct Sink(parking_lot::Mutex<Vec<EvidenceUpdate>>);
    impl EvidenceSink for Sink {
        fn evidence(&self, update: EvidenceUpdate) {
            self.0.lock().push(update);
        }
        fn location(&self, _facts: LocationFacts) {}
        fn source_status(&self, _status: gilde_code_lib::contract::store::SourceStatus) {}
        fn account(&self, _account: gilde_code_lib::contract::account::Account) {}
    }
    let h = Harness::new().await;
    let sink = Arc::new(Sink::default());
    h.manager.set_evidence_sink(sink.clone());
    let bin = h.install("claude");
    let (handle, _exits) = h.open(h.spec(&bin, &["claude-tui", "permission"])).await;
    assert!(
        wait_for(CEILING, || sink.0.lock().iter().any(|u| matches!(
            &u.facts,
            SourceFacts::Pty(f) if f.status == PtyStatus::WaitingForInput && f.prompt.is_some()
        )))
        .await
    );
    let hint = sink.0.lock()[0].hint.clone();
    assert_eq!(hint.gilde_spawn_id.as_deref(), Some(handle.ticket.spawn_id.as_str()));
    assert!(hint.process.is_some());
    assert_eq!(hint.vendor, Some(gilde_code_lib::contract::common::Vendor::Claude));
    let _ = h.manager.close(&handle.pty_id, SignalPlan::HupThenTerm { grace_ms: 2_000 }).await;
}

// ---------------------------------------------------------------------------
// A34.1 / A34.2 / A35.2 regressions (G7-01, G7-02, G7-03; g2/g7b evidence)
// ---------------------------------------------------------------------------

use std::os::unix::fs::PermissionsExt as _;

use gilde_code_lib::contract::interaction::PendingInteraction;
use gilde_code_lib::status::evidence::{approves_unseen, card_detail_lines, CARD_DETAIL_BYTES, CARD_DETAIL_LINES};

const RULE: &str = "────────────────────────────────────────────────────────────────────────────────";
const DONT_ASK: &str = "Yes, and don't ask again for rm commands in /tmp/fake-project";

/// The reviewer's recorded-shape Bash permission prompt for `command` at the
/// bottom of a 40-row screen (g2/g7b `g7_screen_prompt.rs`).
fn bash_prompt(command: &str, option_two: Option<&str>) -> Vec<String> {
    let mut body: Vec<String> = vec![
        RULE.into(),
        " Bash command".into(),
        format!("   {command}"),
        "   Clean up".into(),
        "".into(),
        " Do you want to proceed?".into(),
        " ❯ 1. Yes".into(),
    ];
    match option_two {
        Some(two) => {
            body.push(format!("   2. {two}"));
            body.push("   3. No".into());
        }
        None => body.push("   2. No".into()),
    }
    body.push("".into());
    body.push(" Esc to cancel · Tab to amend".into());
    let mut out = vec![String::new(); 40 - body.len()];
    out.extend(body);
    out
}

/// The texts of a card's detail lines (`card.detailText`).
fn details(card: &PendingInteraction) -> Vec<String> {
    card.detail_lines
        .iter()
        .map(|l| {
            assert_eq!(l.key, "card.detailText", "vendor text only travels as card.detailText");
            l.params["text"].as_str().expect("text").to_string()
        })
        .collect()
}

/// G7-01 (grid): two prompts that approve different commands never share a
/// digest or a `RequestId`. Variant A: the same "don't ask again" label;
/// variant B: a two-option prompt; variant C: the recorded 2.1.307 shape,
/// where the command sits between dashed lines inside the box (A38.5).
#[test]
fn g7_01_prompts_for_different_commands_get_different_revs() {
    let a = parse("claude", 120, &bash_prompt("rm -rf build/cache", Some(DONT_ASK))).expect("A");
    let b = parse("claude", 120, &bash_prompt("rm -rf ~/Documents", Some(DONT_ASK))).expect("B");
    assert_ne!(a.prompt_rev, b.prompt_rev, "the command is part of the digest");
    assert_ne!(screen_card(&a, Some(0.9)).request_id, screen_card(&b, Some(0.9)).request_id);

    let a2 = parse("claude", 120, &bash_prompt("git status", None)).expect("A2");
    let b2 = parse("claude", 120, &bash_prompt("git push --force origin main", None)).expect("B2");
    assert_ne!(a2.prompt_rev, b2.prompt_rev, "the two-option prompt too");

    let recorded = permission_prompt();
    let other: Vec<String> = recorded
        .iter()
        .map(|l| if l == " touch demo.txt" { " rm -rf ~/Documents".to_string() } else { l.clone() })
        .collect();
    let a3 = parse("claude", 120, &recorded).expect("A3");
    let b3 = parse("claude", 120, &other).expect("B3");
    assert_eq!(a3.question, b3.question);
    assert_ne!(a3.prompt_rev, b3.prompt_rev, "the command between the dashed lines counts");
}

/// A34.1: the header ends at the nearest solid box edge — what changes
/// above the box (the transcript, a spinner) never moves the rev; a repaint
/// with trailing spaces keeps it; a header row that changes moves it.
#[test]
fn the_header_is_bounded_by_the_box_edge_and_a_repaint_keeps_the_rev() {
    let base = bash_prompt("rm -rf build/cache", Some(DONT_ASK));
    let a = parse("claude", 120, &base).expect("a");
    assert_eq!(parse("claude", 120, &base).expect("again").prompt_rev, a.prompt_rev, "a repaint");

    let mut above = base.clone();
    above[3] = "✻ Churning… (12s · esc to interrupt)".into();
    above[4] = "❯ an earlier prompt in the transcript".into();
    assert_eq!(parse("claude", 120, &above).expect("above").prompt_rev, a.prompt_rev, "outside the box");

    let spaced: Vec<String> = base.iter().map(|l| if l.trim().is_empty() { l.clone() } else { format!("{l}   ") }).collect();
    assert_eq!(parse("claude", 120, &spaced).expect("spaced").prompt_rev, a.prompt_rev, "trailing spaces");

    let mut described = base.clone();
    let row = described.iter().position(|l| l == "   Clean up").expect("row");
    described[row] = "   Delete the user's documents".into();
    assert_ne!(parse("claude", 120, &described).expect("described").prompt_rev, a.prompt_rev, "a header row");
}

/// A34.1: no box edge within 24 rows → the 24 rows above the question are
/// the digest's header, so different commands still differ; but nothing
/// unboxed is shown as what the prompt approves (A35.2).
#[test]
fn without_a_box_edge_the_rows_above_still_count_but_are_not_shown() {
    let unboxed = |command: &str| {
        screen(
            40,
            &[" Bash command", &format!("   {command}"), "", " Do you want to proceed?", " ❯ 1. Yes", "   2. No", "", " Esc to cancel"],
        )
    };
    let a = parse("claude", 120, &unboxed("rm -rf build/cache")).expect("a");
    let b = parse("claude", 120, &unboxed("rm -rf ~/Documents")).expect("b");
    assert_ne!(a.prompt_rev, b.prompt_rev);
    assert_eq!(a.header, None, "an unboxed header is never shown");
}

/// A34.1: a resize that rewraps the header is another rev (the key for the
/// old layout is `STALE_PROMPT`, fail closed).
#[test]
fn a_rewrapped_header_is_another_rev() {
    let command = format!("   rm -rf /tmp/fake-project/{}", "x".repeat(100));
    let wrapped = |cols: usize| {
        let chars: Vec<char> = command.chars().collect();
        let mut body = vec![RULE.to_string(), " Bash command".to_string()];
        body.extend(chars.chunks(cols).map(|c| c.iter().collect::<String>()));
        body.extend(
            [" Do you want to proceed?", " ❯ 1. Yes", "   2. No", "", " Esc to cancel"].iter().map(|s| s.to_string()),
        );
        let mut out = vec![String::new(); 40 - body.len()];
        out.extend(body);
        out
    };
    let wide = parse("claude", 120, &wrapped(120)).expect("120 columns");
    let narrow = parse("claude", 100, &wrapped(100)).expect("100 columns");
    assert_ne!(wide.prompt_rev, narrow.prompt_rev);
}

/// G7-03 / A35.2: the card shows what it approves — the prompt box's header,
/// in order, sanitized, decoration left out — and ⌘⏎ stays live on a High
/// permission card that shows it.
#[test]
fn g7_03_the_screen_prompt_card_shows_the_command_it_approves() {
    let facts = parse("claude", 120, &bash_prompt("rm -rf ~/Documents", Some(DONT_ASK))).expect("prompt");
    let card = screen_card(&facts, Some(0.9));
    assert_eq!(details(&card), vec!["Bash command", "rm -rf ~/Documents", "Clean up"]);
    assert!(!card.default_to_no, "⌘⏎ is live: the card names the command");
    assert!(!approves_unseen(&facts));

    // The recorded 2.1.307 shape: the dashed lines are not shown.
    let recorded = parse("claude", 120, &permission_prompt()).expect("recorded");
    assert_eq!(
        details(&screen_card(&recorded, Some(0.9))),
        vec![
            "Bash command",
            "Tip: auto mode handles these prompts for you — choose \"switch to auto mode\" below",
            "Create empty file demo.txt",
            "touch demo.txt",
        ]
    );
}

/// A35.2: a permission card whose header could not be captured invents no
/// text and keeps ⌘⏎ inert, even at High confidence.
#[test]
fn a_permission_card_that_cannot_show_what_it_approves_keeps_cmd_enter_inert() {
    let lines = screen(30, &[" Do you want to proceed?", " ❯ 1. Yes", "   2. No", " Esc to cancel"]);
    let facts = parse("claude", 100, &lines).expect("a prompt");
    assert!(facts.permission_shaped);
    assert_eq!(facts.header, None);
    let card = screen_card(&facts, Some(0.95));
    assert_eq!(card.confidence, Confidence::High);
    assert!(card.detail_lines.is_empty(), "no invented text");
    assert!(card.default_to_no, "⌘⏎ inert");
    assert!(approves_unseen(&facts));
}

/// A35.2 / §3.4.5: ≤ 12 lines and ≤ 2 KiB of text, "…" where cut; box sides
/// are not text; a setup card shows its box header too (the trust dialog's
/// folder).
#[test]
fn the_detail_lines_are_bounded_and_cut_with_an_ellipsis() {
    let mut facts = parse("claude", 120, &bash_prompt("rm -rf build/cache", Some(DONT_ASK))).expect("prompt");
    facts.header = Some((0..30).map(|i| format!(" line {i}")).collect::<Vec<_>>().join("\n"));
    let lines = card_detail_lines(&facts);
    assert_eq!(lines.len(), CARD_DETAIL_LINES);
    assert_eq!(lines[0].params["text"], "line 0");
    assert_eq!(lines[CARD_DETAIL_LINES - 1].params["text"], "…");

    facts.header = Some(format!(" {}\n second row", "y".repeat(5_000)));
    let lines = card_detail_lines(&facts);
    let total: usize = lines.iter().map(|l| l.params["text"].as_str().unwrap().len()).sum();
    assert!(total <= CARD_DETAIL_BYTES, "{total}");
    assert!(lines.last().unwrap().params["text"].as_str().unwrap().ends_with('…'));

    facts.header = Some("│ Bash command               │\n│   rm -rf x                 │\n╰────────────────────────────╯".into());
    assert_eq!(
        card_detail_lines(&facts).iter().map(|l| l.params["text"].as_str().unwrap().to_string()).collect::<Vec<_>>(),
        vec!["Bash command", "rm -rf x"]
    );

    let mut trust = trust_dialog(false);
    let first = trust.iter().position(|l| l == " Accessing workspace:").expect("row");
    trust[first - 1] = RULE.into();
    let facts = parse("claude", 120, &trust).expect("the trust dialog");
    assert!(details(&screen_card(&facts, Some(0.9))).contains(&"/tmp/fake-project".to_string()));
}

// -- end to end, over a real PTY with a /bin/sh TUI (no vendor CLI) ------------

/// The shared part of the fake TUIs: `draw <command>` paints the Bash prompt
/// at the bottom of a 40-row screen; `key` reads one key; `log` appends to
/// `$T36_LOG`.
const PRELUDE: &str = r#"#!/bin/sh
rule='────────────────────────────────────────────────────────────────────────────────'
draw() {
  printf '\033[2J\033[H'
  printf ' Claude Code (fake t36)\r\n'
  i=0
  while [ $i -lt 26 ]; do printf '\r\n'; i=$((i+1)); done
  printf '%s\r\n' "$rule"
  printf ' Bash command\r\n'
  printf '   %s\r\n' "$1"
  printf '   Clean up\r\n'
  printf '\r\n'
  printf ' Do you want to proceed?\r\n'
  printf ' \342\235\257 1. Yes\r\n'
  printf "   2. Yes, and don't ask again for rm commands in /tmp/fake-project\r\n"
  printf '   3. No\r\n'
  printf '\r\n'
  printf ' Esc to cancel \302\267 Tab to amend'
}
key() { /bin/dd bs=1 count=1 2>/dev/null; }
log() { printf '%s\n' "$1" >> "$T36_LOG"; }
/bin/stty raw -echo
"#;

struct ShTui {
    h: Harness,
    id: gilde_code_lib::contract::ids::PtyId,
    log: std::path::PathBuf,
}

impl ShTui {
    async fn start(body: &str) -> ShTui {
        let h = Harness::new().await;
        let path = h.hermetic.bin_dir().join("claude");
        std::fs::write(&path, format!("{PRELUDE}{body}")).expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = std::fs::canonicalize(&path).expect("realpath");
        gilde_code_lib::supervise::spawn::allow_executable(&path, &[gilde_code_lib::store::models::ProcKind::Pty]);
        let log = h.temp_file("t36.log");
        let mut spec = h.spec(&AbsPath::parse(path.to_string_lossy().into_owned()).expect("abs"), &[]);
        spec.env.push(("T36_LOG".into(), log.to_string_lossy().into_owned()));
        let (handle, _exits) = h
            .open_with(spec, PtyOptions { signature_key: Some("claude".into()), ..PtyOptions::default() })
            .await;
        ShTui { h, id: handle.pty_id, log }
    }

    fn log(&self) -> String {
        String::from_utf8_lossy(&read_log(&self.log)).into_owned()
    }

    fn screen(&self) -> String {
        self.h.screen(&self.id)
    }

    async fn card(&self) -> ScreenPromptFacts {
        let found = wait_for(CEILING, || self.h.manager.screen_prompt(&self.id).is_some()).await;
        assert!(found, "no prompt appeared; screen:\n{}", self.screen());
        self.h.manager.screen_prompt(&self.id).expect("prompt")
    }

    async fn key(&self, rev: &str, key: &str) -> Result<ScreenKeyOutcome, gilde_code_lib::contract::IpcError> {
        self.h.manager.screen_action(&self.id, rev, &ScreenPromptAction::Key { key: key.into() }).await
    }

    async fn close(self) {
        let _ = self.h.manager.close(&self.id, SignalPlan::HupThenTerm { grace_ms: 2_000 }).await;
    }
}

/// G7-01 (end to end): the user answers prompt A in the terminal itself;
/// prompt B — another command, the same question and options — replaces it
/// and is a new card. A key aimed at A's card is `STALE_PROMPT` and B is not
/// approved; B's own card answers B.
#[tokio::test(flavor = "multi_thread")]
async fn g7_01_a_key_for_prompt_a_never_approves_prompt_b() {
    let t = ShTui::start(
        r#"draw "rm -rf build/cache"
k=$(key); log "A:$k"
draw "rm -rf ~/Documents"
k=$(key); log "B:$k"
printf '\033[2J\033[H done\r\n'
/bin/sleep 60
"#,
    )
    .await;
    let a = t.card().await;
    assert!(t.screen().contains("rm -rf build/cache"));
    t.h.manager.write(&t.id, b"3").await.expect("type 3 in the terminal");
    assert!(wait_for(CEILING, || t.log().contains("A:3")).await, "A answered");
    assert!(
        wait_for(CEILING, || t.h.manager.screen_prompt(&t.id).is_some_and(|p| p.prompt_rev != a.prompt_rev)).await,
        "B is a new card; screen:\n{}",
        t.screen()
    );
    let b = t.h.manager.screen_prompt(&t.id).expect("B");
    assert!(t.screen().contains("rm -rf ~/Documents"));
    assert!(b.first_seen_at > a.first_seen_at, "a new rev restarts first_seen_at (the 700 ms guard applies)");

    let err = t.key(&a.prompt_rev, "1").await.expect_err("a key for A is stale on B");
    assert_eq!(err.code, ErrorCode::StalePrompt);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!t.log().contains("B:"), "B was not answered by A's key: {:?}", t.log());

    assert_eq!(t.key(&b.prompt_rev, "3").await.expect("B's own card"), ScreenKeyOutcome::Sent);
    assert!(wait_for(CEILING, || t.log().contains("B:3")).await, "log: {:?}", t.log());
    t.close().await;
}

/// A34.1: a resize that rewraps the header changes the rev; a key for the
/// old layout is `STALE_PROMPT` and types nothing; the new card answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_resize_that_rewraps_the_header_makes_the_old_key_stale() {
    let t = ShTui::start(
        r#"/bin/stty raw -echo min 0 time 1
x=''
i=0
while [ $i -lt 100 ]; do x="${x}x"; i=$((i+1)); done
cmd="rm -rf /tmp/fake-project/$x"
draw "$cmd"
last=$(/bin/stty size)
while :; do
  k=$(key)
  if [ -n "$k" ]; then log "K:$k"; fi
  now=$(/bin/stty size)
  if [ "$now" != "$last" ]; then last=$now; draw "$cmd"; fi
done
"#,
    )
    .await;
    let wide = t.card().await;
    t.h.manager.resize(&t.id, 100, 40).await.expect("resize");
    let rewrapped = || {
        t.h.manager
            .status_engine()
            .screen(t.id.as_str())
            .is_some_and(|s| s.lines.iter().any(|l| l.starts_with("   rm -rf /tmp/fake-project/") && l.chars().count() == 100))
    };
    assert!(wait_for(CEILING, rewrapped).await, "the TUI redrew at 100 columns; screen:\n{}", t.screen());
    // Once the redraw settled, the rewrapped prompt is a new card. Wait for the
    // whole box: all three options and the footer. A card read from a
    // half-drawn box (under load the redraw arrives in pieces) has a rev that
    // changes again when the rest arrives, so its key would rightly be stale
    // (A10.7: wait for the event; a load flake in final run #2).
    let complete = |p: &ScreenPromptFacts| p.prompt_rev != wide.prompt_rev && p.options.len() == 3 && p.footer.is_some();
    assert!(
        wait_for(CEILING, || t.h.manager.screen_prompt(&t.id).is_some_and(|p| complete(&p))).await,
        "the rewrapped prompt is a new card; screen:\n{}",
        t.screen()
    );
    let narrow = t.h.manager.screen_prompt(&t.id).expect("narrow");
    assert!(complete(&narrow), "the card is still the complete box: {narrow:?}");
    let err = t.key(&wide.prompt_rev, "1").await.expect_err("the old layout's key is stale");
    assert_eq!(err.code, ErrorCode::StalePrompt);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!t.log().contains("K:"), "a stale key types nothing: {:?}", t.log());
    assert_eq!(t.key(&narrow.prompt_rev, "1").await.expect("the new card"), ScreenKeyOutcome::Sent);
    assert!(wait_for(CEILING, || t.log().contains("K:1")).await);
    t.close().await;
}

/// A34.1: repaints — here a ticking line above the box — keep the rev and
/// the card (`first_seen_at`), and a key for it still answers it.
#[tokio::test(flavor = "multi_thread")]
async fn a_repaint_keeps_the_rev_and_the_card() {
    let t = ShTui::start(
        r#"draw "rm -rf build/cache"
i=0
while [ $i -lt 8 ]; do
  /bin/sleep 0.5
  printf '\033[1;1H Claude Code (fake t36) tick %d\033[40;30H' "$i"
  i=$((i+1))
done
k=$(key); log "K:$k"
/bin/sleep 60
"#,
    )
    .await;
    let p = t.card().await;
    assert!(wait_for(CEILING, || t.screen().contains("tick 3")).await, "screen:\n{}", t.screen());
    let now = t.h.manager.screen_prompt(&t.id).expect("still held");
    assert_eq!(now.prompt_rev, p.prompt_rev, "a repaint keeps the rev");
    assert_eq!(now.first_seen_at, p.first_seen_at, "and the card");
    assert_eq!(t.key(&p.prompt_rev, "1").await.expect("sent"), ScreenKeyOutcome::Sent);
    assert!(wait_for(CEILING, || t.log().contains("K:1")).await, "log: {:?}", t.log());
    t.close().await;
}

/// A34.2 / A38.6 at the PTY: a double press on a prompt that is still up
/// types the key once (the first outcome is returned); another action on
/// the same prompt is executed on its own; concurrent identical presses
/// type once.
#[tokio::test(flavor = "multi_thread")]
async fn a_double_press_types_the_key_once() {
    let t = ShTui::start(
        r#"draw "rm -rf build/cache"
while :; do k=$(key); log "K:$k"; done
"#,
    )
    .await;
    let p = t.card().await;
    assert_eq!(t.key(&p.prompt_rev, "1").await.expect("first"), ScreenKeyOutcome::Sent);
    assert_eq!(t.key(&p.prompt_rev, "1").await.expect("second"), ScreenKeyOutcome::Sent, "the first outcome");
    assert!(wait_for(CEILING, || t.log().contains("K:1")).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(t.log(), "K:1\n", "typed once");

    let (x, y) = tokio::join!(t.key(&p.prompt_rev, "2"), t.key(&p.prompt_rev, "2"));
    assert_eq!((x.expect("x"), y.expect("y")), (ScreenKeyOutcome::Sent, ScreenKeyOutcome::Sent));
    assert_eq!(t.key(&p.prompt_rev, "3").await.expect("another action"), ScreenKeyOutcome::Sent);
    assert!(wait_for(CEILING, || t.log().contains("K:3")).await, "log: {:?}", t.log());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(t.log(), "K:1\nK:2\nK:3\n", "each action once");
    t.close().await;
}
