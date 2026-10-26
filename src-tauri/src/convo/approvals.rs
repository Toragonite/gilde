//! Approvals, questions, plans and every decision's wire frame (contract
//! §6.1.4) [C12].
//!
//! Owner: T05.
//!
//! [`build_request`] turns one CLI `can_use_tool` request into the card the
//! store shows (`PendingInteraction`) plus everything needed to answer it
//! (the original `input` kept raw for `updatedInput`, the forwardable
//! `addRules` suggestions, the original question texts and labels).
//! [`decision_payload`] builds the exact `response.response` payload of each
//! decision (§6.1.4 decision table) and checks it against the R4 schema before
//! it may be sent ([`PermissionOut::to_checked_raw`]).
//!
//! Validation is by value: `tool_name` a non-empty string, `input` an object,
//! `tool_use_id` printable ASCII ≤ 128 (a sandbox ask carries a UUID [R6 §3
//! LP-07b]). A request that fails it becomes a **degraded card** (§6.1.9): raw
//! input as sanitized mono text, `[1] Allow once` only when `input` is an
//! object, `[3] Deny`.
//!
//! Card text: never English from Gilde. Detail lines are `EvidenceLine`s
//! [A19.7]: sanitized vendor text as `card.detailText {text}`
//! (`EvidenceLine::detail_text`), and the keys [`KEY_SUBAGENT`]
//! (`card.aSubagent`, a subagent whose task frame was not seen, F-28) and
//! [`KEY_DUPLICATE_QUESTION`] (`card.duplicateQuestion`, F-37) as
//! `EvidenceLine::key` [A19.3]. `title` is the CLI's `title` / `description` / `display_name`
//! (sanitized, ≤ 200 chars) or empty — an empty title means "render the
//! generic sentence for `tool.verb`" (ux-ia #73/#74) and `degraded: true`
//! means "render `card.degraded`" (both are WebView strings). `always_label`
//! is the quoted part of `approval.always` ("Always allow \"{prefix}\" in this
//! session"): the rule content of the first forwardable `Bash` rule, or the
//! host of the `WebFetch` `domain:<host>` rule of a sandbox ask.

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use crate::contract::activity::EvidenceLine;
use crate::contract::common::{Confidence, EpochMs};
use crate::contract::display::TextCap;
use crate::contract::ids::RequestId;
use crate::contract::interaction::{
    Decision, DecisionKind, InteractionKind, InteractionOption, OptionRole, PendingInteraction, QuestionAnswer,
    QuestionView,
};
use crate::convo::wire::{
    printable_ascii, PermissionOut, PermissionRule, SessionRulesOut, CLASS_PERMANENT, CLASS_REJECT, CLASS_TEMPORARY,
    DENY_MESSAGE, MAX_TOOL_USE_ID,
};
use crate::render::sanitize::{display, sanitize_display_text};
use crate::render::tools::{self, PathBase};

/// Card title (§3.4.5).
pub const TITLE_CAP: TextCap = TextCap::Chars(200);
/// Detail lines: ≤ 12 lines, ≤ 2 KiB in total (§3.4.5).
pub const DETAIL_LINES: usize = 12;
pub const DETAIL_BYTES: usize = 2 * 1024;
/// A degraded card's raw input (§6.1.9).
pub const DEGRADED_INPUT_CAP: TextCap = TextCap::Bytes(4 * 1024);
/// Question text ≤ 400, option label ≤ 120 (§3.4.5).
pub const QUESTION_CAP: TextCap = TextCap::Chars(400);
pub const LABEL_CAP: TextCap = TextCap::Chars(120);
/// A question's chip ≤ 40, an option's description ≤ 400, its preview ≤ 2 KiB.
pub const HEADER_CAP: TextCap = TextCap::Chars(40);
pub const DESCRIPTION_CAP: TextCap = TextCap::Chars(400);
pub const PREVIEW_CAP: TextCap = TextCap::Bytes(2 * 1024);
/// A subagent's label (F-28).
pub const AGENT_LABEL_CAP: TextCap = TextCap::Chars(60);
/// The label of a subagent whose task frame was not seen (F-28): the
/// `card.aSubagent` key, rendered by the WebView [A19.3].
pub const KEY_SUBAGENT: &str = "card.aSubagent";
/// The note of a request whose questions repeat a text (F-37) [A19.3].
pub const KEY_DUPLICATE_QUESTION: &str = "card.duplicateQuestion";

/// What kind of request this is (drives which decisions fit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    Tool,
    /// AskUserQuestion with its parsed questions.
    Question,
    /// ExitPlanMode (P10).
    Plan,
    /// A request Gilde could not fully read (§6.1.9).
    Degraded,
}

/// One question as the wire knows it (the `answers` map is keyed by the
/// original text; labels go back as the CLI sent them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireQuestion {
    /// Index in the original `questions` array.
    pub original_index: usize,
    pub text: String,
    pub labels: Vec<String>,
    pub multi_select: bool,
}

/// A pending CLI request: the card plus what answering it needs. Memory only.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    /// The card's id (`RequestId`): the CLI's id, or `h_<sha256/16>` of it
    /// when it is not printable ASCII ≤ 128 (§3.2).
    pub request_id: String,
    /// The CLI's `request_id` exactly as sent: the only id ever answered.
    pub wire_id: String,
    pub kind: RequestKind,
    pub tool_name: String,
    pub tool_use_id: Option<String>,
    /// The original `input`, kept verbatim for `updatedInput` (only when it is
    /// a JSON object).
    pub input: Option<Box<RawValue>>,
    /// Forwardable `addRules` entries naming this request's tool, each to be
    /// sent with `destination:"session"` [F-01]. Empty ⇒ no button 2.
    pub session_rules: Vec<Vec<PermissionRule>>,
    /// The original `questions` array (Answer payload) and its parsed form,
    /// one entry per card question.
    pub questions_raw: Option<Box<RawValue>>,
    pub questions: Vec<WireQuestion>,
    pub agent_id: Option<String>,
    pub decision_reason: Option<String>,
    /// Uptime ms when it arrived (`tooFresh`, §8.6).
    pub received_mono: u64,
    pub card: PendingInteraction,
}

impl PendingRequest {
    /// A runtime guard turned `allowSession` off: button 2 disappears from
    /// this card too, at once (PM-42, F-01).
    pub fn hide_allow_session(&mut self) {
        self.session_rules.clear();
        self.card.always_label = None;
        self.card.options.retain(|o| o.role != OptionRole::AllowSession);
    }
}

/// What card building depends on besides the request.
pub struct CardContext<'a> {
    /// Button 2 allowed for this session (flag `allowSession` and no runtime
    /// guard turned it off).
    pub allow_session: bool,
    /// P10 flags.
    pub plan_answer: bool,
    /// P11 sandbox ask shape.
    pub sandbox_ask: bool,
    pub base: PathBase<'a>,
    /// The subagent label for an `agent_id` (from `system/task_*` frames).
    pub agent_label: &'a dyn Fn(&str) -> Option<String>,
    pub received_at: EpochMs,
    pub received_mono: u64,
}

/// The original `input` (and its `questions`), kept byte-for-byte: a
/// re-encoded value could differ (key order, float digits) and §6.1.4 sends
/// the original input back as `updatedInput`.
#[derive(Debug, Deserialize)]
struct RawInputIn<'a> {
    #[serde(borrow, default)]
    input: Option<&'a RawValue>,
}

#[derive(Debug, Deserialize)]
struct RawQuestionsIn<'a> {
    #[serde(borrow, default)]
    questions: Option<&'a RawValue>,
}

/// Display labels of one question, made unique so a chosen label names exactly
/// one original option (two labels may share their first 120 characters or
/// differ only in stripped characters).
pub fn unique_labels(labels: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(labels.len());
    for (i, l) in labels.iter().enumerate() {
        let mut d = display(l, LABEL_CAP);
        let mut n = i + 1;
        while out.contains(&d) {
            d = format!("{} ({n})", display(l, TextCap::Chars(100)));
            n += 1;
        }
        out.push(d);
    }
    out
}

#[derive(Debug, Deserialize)]
struct QuestionIn {
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    options: Option<Vec<OptionIn>>,
    #[serde(rename = "multiSelect", default)]
    multi_select: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct OptionIn {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    preview: Option<String>,
}

/// A shown text, or `None` when absent or nothing is left after sanitising.
fn shown(text: Option<&str>, cap: TextCap) -> Option<String> {
    text.map(|t| display(t, cap)).filter(|t| !t.trim().is_empty())
}

fn str_of<'v>(o: &'v Map<String, Value>, k: &str) -> Option<&'v str> {
    o.get(k).and_then(Value::as_str)
}

fn valid_tool_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}

/// The detail lines of a card: bounded rendering of the input (command,
/// paths, host), the decision reason, the MCP server (untrusted plain text).
struct Details {
    lines: Vec<EvidenceLine>,
    /// Text bytes so far (the ≤ 2 KiB cap applies to the text) [A19.7].
    bytes: usize,
}

impl Details {
    fn new() -> Details {
        Details { lines: Vec::new(), bytes: 0 }
    }

    /// An i18n key line without params (never vendor text).
    fn push_key(&mut self, key: &str) {
        if self.lines.len() < DETAIL_LINES {
            self.lines.push(EvidenceLine::key(key));
        }
    }

    fn push(&mut self, text: &str, cap: TextCap) {
        for line in sanitize_display_text(text, cap).text.split('\n') {
            if self.lines.len() >= DETAIL_LINES {
                return;
            }
            let room = DETAIL_BYTES.saturating_sub(self.bytes);
            if room == 0 {
                return;
            }
            let l = if line.len() > room { display(line, TextCap::Bytes(room)) } else { line.to_owned() };
            self.bytes += l.len();
            self.lines.push(EvidenceLine::detail_text(l));
        }
    }
}

fn option(key: &str, label: &str, role: OptionRole) -> InteractionOption {
    InteractionOption { key: key.to_owned(), label: display(label, LABEL_CAP), role }
}

/// The forwardable `addRules` entries of `permission_suggestions` (F-01): only
/// `type:"addRules"` with `behavior:"allow"`, only rules naming this request's
/// tool (`toolName == tool_name`; for `SandboxNetworkAccess` the `WebFetch`
/// `domain:<host>` rule of this `input.host`). `setMode`, `addDirectories`,
/// `replaceRules`, `removeRules` never are.
pub fn forwardable_rules(suggestions: Option<&Value>, tool_name: &str, sandbox_host: Option<&str>) -> Vec<Vec<PermissionRule>> {
    let Some(list) = suggestions.and_then(Value::as_array) else { return Vec::new() };
    let mut out = Vec::new();
    for s in list.iter().take(32) {
        let Some(o) = s.as_object() else { continue };
        if str_of(o, "type") != Some("addRules") || str_of(o, "behavior") != Some("allow") {
            continue;
        }
        let Some(rules) = o.get("rules").and_then(Value::as_array) else { continue };
        let kept: Vec<PermissionRule> = rules
            .iter()
            .take(32)
            .filter_map(|r| serde_json::from_value::<PermissionRule>(r.clone()).ok())
            .filter(|r| r.rule_content.as_ref().is_none_or(|c| c.len() <= 4096))
            .filter(|r| match sandbox_host {
                Some(host) => r.tool_name == "WebFetch" && r.rule_content.as_deref() == Some(&format!("domain:{host}")),
                None => r.tool_name == tool_name && valid_tool_name(&r.tool_name),
            })
            .collect();
        if !kept.is_empty() {
            out.push(kept);
        }
    }
    out
}

/// Build the pending request (and its card) from a `can_use_tool` request
/// object. Never fails: an unreadable request becomes a degraded card.
pub fn build_request(request_id: &str, request: &RawValue, ctx: &CardContext<'_>) -> PendingRequest {
    let parsed: Option<Map<String, Value>> = serde_json::from_str(request.get()).ok();
    let o = parsed.unwrap_or_default();
    let tool_name = str_of(&o, "tool_name").filter(|t| valid_tool_name(t)).map(str::to_owned);
    let input_val = o.get("input").filter(|i| i.is_object());
    let tool_use_id = str_of(&o, "tool_use_id").filter(|t| printable_ascii(t, MAX_TOOL_USE_ID)).map(str::to_owned);
    let input_raw: Option<Box<RawValue>> = match input_val {
        Some(_) => serde_json::from_str::<RawInputIn<'_>>(request.get()).ok().and_then(|r| r.input).map(|r| r.to_owned()),
        None => None,
    };
    let agent_id = str_of(&o, "agent_id").filter(|a| printable_ascii(a, 64)).map(str::to_owned);
    let decision_reason = str_of(&o, "decision_reason").map(|r| display(r, TextCap::Chars(300))).filter(|r| !r.is_empty());

    let mut details = Details::new();
    if let Some(a) = &agent_id {
        match (ctx.agent_label)(a) {
            Some(label) => details.push(&label, AGENT_LABEL_CAP),
            None => details.push_key(KEY_SUBAGENT),
        }
    }

    let title = ["title", "description", "display_name"]
        .iter()
        .find_map(|k| str_of(&o, k).map(|t| display(t, TITLE_CAP)).filter(|t| !t.is_empty()))
        .unwrap_or_default();

    let base_card = |kind: InteractionKind, tool: Option<crate::contract::content::ToolSummary>| PendingInteraction {
        request_id: RequestId::parse(request_id.to_owned()).expect("caller checked the id"),
        kind,
        received_at: ctx.received_at,
        on_behalf_of: None,
        answer_in: None,
        title: title.clone(),
        tool,
        detail_lines: Vec::new(),
        options: Vec::new(),
        always_label: None,
        default_to_no: o.get("default_to_no").and_then(Value::as_bool).unwrap_or(false),
        degraded: false,
        prompt_rev: None,
        confidence: Confidence::High,
        multi_select: false,
        questions: Vec::new(),
    };

    let degraded = |details: Details, why_input: Option<&Value>| -> PendingRequest {
        let mut d = details;
        let raw_text = o
            .get("input")
            .or(why_input)
            .map(|v| tools::bounded_json(v, DEGRADED_INPUT_CAP).text)
            .unwrap_or_else(|| sanitize_display_text(request.get(), DEGRADED_INPUT_CAP).text);
        d.push(&raw_text, DEGRADED_INPUT_CAP);
        let mut card = base_card(InteractionKind::ToolApproval, None);
        card.title = String::new();
        card.degraded = true;
        card.detail_lines = d.lines;
        if input_raw.is_some() {
            card.options.push(option("1", "Allow once", OptionRole::AllowOnce));
        }
        card.options.push(option("3", "Deny", OptionRole::Deny));
        PendingRequest {
            request_id: request_id.to_owned(),
            wire_id: request_id.to_owned(),
            kind: RequestKind::Degraded,
            tool_name: tool_name.clone().unwrap_or_default(),
            tool_use_id: tool_use_id.clone(),
            input: input_raw.clone(),
            session_rules: Vec::new(),
            questions_raw: None,
            questions: Vec::new(),
            agent_id: agent_id.clone(),
            decision_reason: decision_reason.clone(),
            received_mono: ctx.received_mono,
            card,
        }
    };

    let (Some(name), Some(input), Some(_), Some(_)) = (tool_name.clone(), input_val, tool_use_id.clone(), input_raw.clone())
    else {
        return degraded(details, None);
    };
    let summary = tools::claude_tool_summary(&name, input, ctx.base);

    // --- questions ---------------------------------------------------------
    if name == "AskUserQuestion" {
        let Some(qs_val) = input.get("questions") else { return degraded(details, None) };
        let Ok(qs) = serde_json::from_value::<Vec<QuestionIn>>(qs_val.clone()) else { return degraded(details, None) };
        let mut wire: Vec<WireQuestion> = Vec::new();
        let mut views: Vec<QuestionView> = Vec::new();
        let mut duplicate = false;
        for (i, q) in qs.iter().enumerate().take(16) {
            let Some(text) = q.question.clone().filter(|t| !t.is_empty()) else { return degraded(details, None) };
            let offered: Vec<&OptionIn> = q.options.iter().flatten().filter(|op| op.label.is_some()).take(64).collect();
            let labels: Vec<String> = offered.iter().filter_map(|op| op.label.clone()).collect();
            // F-37: the `answers` map is keyed by question text; a repeated
            // text can carry only one answer, so only the first is offered.
            if wire.iter().any(|w| w.text == text) {
                duplicate = true;
                continue;
            }
            views.push(QuestionView {
                text: display(&text, QUESTION_CAP),
                header: shown(q.header.as_deref(), HEADER_CAP),
                options: unique_labels(&labels),
                descriptions: offered.iter().map(|op| shown(op.description.as_deref(), DESCRIPTION_CAP)).collect(),
                previews: offered.iter().map(|op| shown(op.preview.as_deref(), PREVIEW_CAP)).collect(),
                multi_select: q.multi_select.unwrap_or(false),
            });
            wire.push(WireQuestion { original_index: i, text, labels, multi_select: q.multi_select.unwrap_or(false) });
        }
        if wire.is_empty() {
            return degraded(details, None);
        }
        let mut card = base_card(InteractionKind::Question, Some(summary));
        card.multi_select = views.iter().any(|v| v.multi_select);
        for (k, label) in views[0].options.iter().enumerate().take(9) {
            card.options.push(option(&(k + 1).to_string(), label, OptionRole::Answer));
        }
        card.questions = views;
        if duplicate {
            // Not silent: the card says only the first can be answered [A19.3].
            details.push_key(KEY_DUPLICATE_QUESTION);
        }
        if let Some(r) = &decision_reason {
            details.push(r, TextCap::Chars(300));
        }
        card.detail_lines = details.lines;
        let questions_raw = input_raw
            .as_deref()
            .and_then(|r| serde_json::from_str::<RawQuestionsIn<'_>>(r.get()).ok())
            .and_then(|q| q.questions)
            .map(|q| q.to_owned());
        return PendingRequest {
            request_id: request_id.to_owned(),
            wire_id: request_id.to_owned(),
            kind: RequestKind::Question,
            tool_name: name,
            tool_use_id,
            input: input_raw,
            session_rules: Vec::new(),
            questions_raw,
            questions: wire,
            agent_id,
            decision_reason,
            received_mono: ctx.received_mono,
            card,
        };
    }

    // --- plans -------------------------------------------------------------
    if name == "ExitPlanMode" && ctx.plan_answer {
        let mut card = base_card(InteractionKind::PlanApproval, Some(summary));
        if let Some(plan) = input.get("plan").and_then(Value::as_str) {
            details.push(plan, TextCap::Bytes(DETAIL_BYTES));
        }
        card.detail_lines = details.lines;
        card.options = vec![option("1", "Approve plan", OptionRole::AllowOnce), option("3", "Keep planning", OptionRole::Deny)];
        return PendingRequest {
            request_id: request_id.to_owned(),
            wire_id: request_id.to_owned(),
            kind: RequestKind::Plan,
            tool_name: name,
            tool_use_id,
            input: input_raw,
            session_rules: Vec::new(),
            questions_raw: None,
            questions: Vec::new(),
            agent_id,
            decision_reason,
            received_mono: ctx.received_mono,
            card,
        };
    }

    // --- tool approvals (incl. the sandbox network ask) ---------------------
    let sandbox_host = if name == "SandboxNetworkAccess" {
        match input.get("host").and_then(Value::as_str).filter(|h| valid_host(h)) {
            Some(h) if ctx.sandbox_ask => Some(h.to_owned()),
            _ => return degraded(details, None),
        }
    } else {
        None
    };
    let mut card = base_card(InteractionKind::ToolApproval, Some(summary.clone()));
    // Bounded rendering of the input: the command for Bash, the path or host
    // for file and network tools, else the pretty input.
    let obj = input.as_object().expect("checked: an object");
    match name.as_str() {
        "Bash" => {
            if let Some(c) = str_of(obj, "command") {
                details.push(c, TextCap::Bytes(DETAIL_BYTES));
            }
        }
        _ => match &summary.object {
            Some(object) => details.push(object, TextCap::Chars(512)),
            None => details.push(&tools::input_preview(&name, input).text, TextCap::Bytes(DETAIL_BYTES)),
        },
    }
    if let Some(r) = &decision_reason {
        details.push(r, TextCap::Chars(300));
    }
    if let Some(server) = o.get("mcp_server").and_then(|m| m.get("name")).and_then(Value::as_str) {
        details.push(server, TextCap::Chars(64));
    }
    card.detail_lines = details.lines;

    let suppress = o.get("suppress_always_allow_rule").and_then(Value::as_bool).unwrap_or(false);
    // Exactly the rule the label names is forwarded: button 2 never grants
    // more than it says (a suggestion may carry several rules).
    let session_rules: Vec<Vec<PermissionRule>> = if ctx.allow_session && !suppress {
        forwardable_rules(o.get("permission_suggestions"), &name, sandbox_host.as_deref())
            .into_iter()
            .next()
            .and_then(|rules| rules.into_iter().next())
            .map(|r| vec![vec![r]])
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    card.always_label = session_rules.first().and_then(|rules| rules.first()).map(|r| match (&sandbox_host, &r.rule_content) {
        (Some(host), _) => display(host, LABEL_CAP),
        (None, Some(content)) => display(content, LABEL_CAP),
        (None, None) => display(&r.tool_name, LABEL_CAP),
    });
    card.options.push(option("1", "Allow once", OptionRole::AllowOnce));
    if let Some(label) = &card.always_label {
        card.options.push(option("2", label, OptionRole::AllowSession));
    }
    card.options.push(option("3", "Deny", OptionRole::Deny));
    PendingRequest {
        request_id: request_id.to_owned(),
        wire_id: request_id.to_owned(),
        kind: RequestKind::Tool,
        tool_name: name,
        tool_use_id,
        input: input_raw,
        session_rules,
        questions_raw: None,
        questions: Vec::new(),
        agent_id,
        decision_reason,
        received_mono: ctx.received_mono,
        card,
    }
}

/// A hostname as the sandbox asks it (`input.host`).
fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && h.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:[]_".contains(&b))
}

// ---------------------------------------------------------------------------
// decisions → payloads
// ---------------------------------------------------------------------------

/// What answering a request writes.
#[derive(Debug)]
pub struct Answer {
    /// The `response.response` payload, schema-checked.
    pub payload: Box<RawValue>,
    /// P5 fallback: send `interrupt` after the deny.
    pub then_interrupt: bool,
    /// The outcome the Interaction block shows.
    pub outcome: crate::contract::content::InteractionOutcome,
    /// The payload carries the CLI's own `input` (or its `questions`)
    /// verbatim: only such a frame may exceed 1 MiB, up to the 64 MiB inbound
    /// line cap [A19.4].
    pub echoes_input: bool,
}

/// Why a decision could not be turned into a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadError {
    /// `UNSUPPORTED{cap}` (e.g. `allowSessionHidden`, or Allow once on a
    /// degraded card whose input is not an object).
    Unsupported(&'static str),
    /// The built payload failed the R4 schema (a bug; never sent).
    Schema(&'static str),
}

/// The exact payload of `d` for `req` (§6.1.4 decision table).
pub fn decision_payload(req: &PendingRequest, d: &Decision, deny_and_stop: bool, free_text_answer: bool) -> Result<Answer, PayloadError> {
    use crate::contract::content::InteractionOutcome as O;
    let tuid = req.tool_use_id.as_deref();
    let allow_once = |outcome| -> Result<Answer, PayloadError> {
        let input = req.input.as_deref().ok_or(PayloadError::Unsupported("decisionKind"))?;
        let p = PermissionOut::Allow {
            updated_input: input,
            updated_permissions: None,
            tool_use_id: tuid,
            decision_classification: Some(CLASS_TEMPORARY),
        };
        Ok(Answer { payload: p.to_checked_raw().map_err(PayloadError::Schema)?, then_interrupt: false, outcome, echoes_input: true })
    };
    let deny_with = |message: &str, outcome| -> Result<Answer, PayloadError> {
        let p = PermissionOut::Deny { message, interrupt: None, tool_use_id: tuid, decision_classification: None };
        Ok(Answer { payload: p.to_checked_raw().map_err(PayloadError::Schema)?, then_interrupt: false, outcome, echoes_input: false })
    };
    match d {
        Decision::AllowOnce | Decision::ApprovePlan => allow_once(O::Allowed),
        Decision::AllowSession => {
            if req.session_rules.is_empty() || req.card.always_label.is_none() {
                return Err(PayloadError::Unsupported("allowSessionHidden"));
            }
            let input = req.input.as_deref().ok_or(PayloadError::Unsupported("decisionKind"))?;
            let perms: Vec<SessionRulesOut> = req.session_rules.iter().map(|r| SessionRulesOut::allow(r.clone())).collect();
            let p = PermissionOut::Allow {
                updated_input: input,
                updated_permissions: Some(&perms),
                tool_use_id: tuid,
                decision_classification: Some(CLASS_PERMANENT),
            };
            Ok(Answer { payload: p.to_checked_raw().map_err(PayloadError::Schema)?, then_interrupt: false, outcome: O::AllowedForSession, echoes_input: true })
        }
        Decision::Deny => {
            let p = PermissionOut::Deny {
                message: DENY_MESSAGE,
                interrupt: None,
                tool_use_id: tuid,
                decision_classification: Some(CLASS_REJECT),
            };
            Ok(Answer { payload: p.to_checked_raw().map_err(PayloadError::Schema)?, then_interrupt: false, outcome: O::Denied, echoes_input: false })
        }
        Decision::DenyWithMessage { message } | Decision::KeepPlanning { message } => deny_with(message, O::Denied),
        Decision::DenyAndStop => {
            let p = PermissionOut::Deny {
                message: DENY_MESSAGE,
                interrupt: deny_and_stop.then_some(true),
                tool_use_id: tuid,
                decision_classification: Some(CLASS_REJECT),
            };
            Ok(Answer { payload: p.to_checked_raw().map_err(PayloadError::Schema)?, then_interrupt: !deny_and_stop, outcome: O::Denied, echoes_input: false })
        }
        Decision::Answer { answers } => answer_payload(req, answers, free_text_answer),
    }
}

fn answer_payload(req: &PendingRequest, answers: &[QuestionAnswer], free_text_answer: bool) -> Result<Answer, PayloadError> {
    use crate::contract::content::InteractionOutcome as O;
    let raw_questions = req.questions_raw.as_deref().ok_or(PayloadError::Unsupported("decisionKind"))?;
    let mut map = Map::new();
    let mut free_texts: Vec<&str> = Vec::new();
    for a in answers {
        let Some(q) = req.questions.get(a.question_index as usize) else { continue };
        if map.contains_key(&q.text) {
            continue;
        }
        let card_q = req.card.questions.get(a.question_index as usize);
        let original = |label: &String| -> Option<String> {
            let pos = card_q?.options.iter().position(|o| o == label)?;
            q.labels.get(pos).cloned()
        };
        let free = a.free_text.as_deref().filter(|t| !t.is_empty());
        let value = match free {
            Some(t) => {
                free_texts.push(t);
                Value::String(t.to_owned())
            }
            None if q.multi_select => Value::Array(a.labels.iter().filter_map(original).map(Value::String).collect()),
            None => match a.labels.first().and_then(original) {
                Some(l) => Value::String(l),
                None => continue,
            },
        };
        map.insert(q.text.clone(), value);
    }
    if !free_texts.is_empty() && !free_text_answer {
        // P10 fallback: free text travels as a deny with the text.
        let message = free_texts.join("\n");
        let p = PermissionOut::Deny { message: &message, interrupt: None, tool_use_id: req.tool_use_id.as_deref(), decision_classification: None };
        return Ok(Answer { payload: p.to_checked_raw().map_err(PayloadError::Schema)?, then_interrupt: false, outcome: O::Answered, echoes_input: false });
    }
    #[derive(serde::Serialize)]
    struct UpdatedInput<'a> {
        questions: &'a RawValue,
        answers: Map<String, Value>,
    }
    let updated = serde_json::value::to_raw_value(&UpdatedInput { questions: raw_questions, answers: map })
        .map_err(|_| PayloadError::Schema("serialize"))?;
    let p = PermissionOut::Allow { updated_input: &updated, updated_permissions: None, tool_use_id: None, decision_classification: None };
    Ok(Answer { payload: p.to_checked_raw().map_err(PayloadError::Schema)?, then_interrupt: false, outcome: O::Answered, echoes_input: true })
}

/// The `DecisionKind` a ledger records for `d`.
pub fn kind_of(d: &Decision) -> DecisionKind {
    d.kind()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::content::InteractionOutcome;
    use serde_json::json;

    fn ctx(allow_session: bool) -> CardContext<'static> {
        static LABEL: fn(&str) -> Option<String> = |id| (id == "a0dff261b698289f3").then(|| "Create lp07-d.txt file".to_string());
        CardContext {
            allow_session,
            plan_answer: true,
            sandbox_ask: true,
            base: PathBase { location: Some("/w/p"), home: Some("/Users/u") },
            agent_label: &LABEL,
            received_at: 1_000,
            received_mono: 10,
        }
    }

    fn req(v: Value) -> PendingRequest {
        build_request("req-1", &serde_json::value::to_raw_value(&v).unwrap(), &ctx(true))
    }

    fn payload(r: &PendingRequest, d: Decision) -> Value {
        serde_json::from_str(decision_payload(r, &d, true, true).unwrap().payload.get()).unwrap()
    }

    fn bash() -> Value {
        json!({"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash",
               "input":{"command":"touch lp02-x.txt","description":"Create empty file lp02-x.txt"},
               "description":"Create empty file lp02-x.txt",
               "permission_suggestions":[
                 {"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch lp02-x.txt"}],"behavior":"allow","destination":"localSettings"},
                 {"type":"addDirectories","directories":["/w/p"],"destination":"session"},
                 {"type":"setMode","mode":"acceptEdits","destination":"session"}],
               "blocked_path":"/w/p","tool_use_id":"toolu_016x"})
    }

    // T-convo-golden: every row of the §6.1.4 decision table.
    #[test]
    fn golden_tool_decisions() {
        let r = req(bash());
        assert_eq!(r.card.kind, InteractionKind::ToolApproval);
        assert_eq!(r.card.title, "Create empty file lp02-x.txt");
        assert_eq!(r.card.always_label.as_deref(), Some("touch lp02-x.txt"));
        assert_eq!(r.card.options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["1", "2", "3"]);
        let input = json!({"command":"touch lp02-x.txt","description":"Create empty file lp02-x.txt"});
        assert_eq!(
            payload(&r, Decision::AllowOnce),
            json!({"behavior":"allow","updatedInput":input,"toolUseID":"toolu_016x","decisionClassification":"user_temporary"})
        );
        assert_eq!(
            payload(&r, Decision::AllowSession),
            json!({"behavior":"allow","updatedInput":input,"toolUseID":"toolu_016x","decisionClassification":"user_permanent",
                   "updatedPermissions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch lp02-x.txt"}],"behavior":"allow","destination":"session"}]})
        );
        assert_eq!(
            payload(&r, Decision::Deny),
            json!({"behavior":"deny","message":"The user denied this action.","toolUseID":"toolu_016x","decisionClassification":"user_reject"})
        );
        assert_eq!(
            payload(&r, Decision::DenyWithMessage { message: "use ls".into() }),
            json!({"behavior":"deny","message":"use ls","toolUseID":"toolu_016x"})
        );
        assert_eq!(
            payload(&r, Decision::DenyAndStop),
            json!({"behavior":"deny","message":"The user denied this action.","interrupt":true,"toolUseID":"toolu_016x","decisionClassification":"user_reject"})
        );
        // P5 fallback: a plain deny, then an interrupt request.
        let a = decision_payload(&r, &Decision::DenyAndStop, false, true).unwrap();
        assert!(a.then_interrupt);
        assert!(!a.payload.get().contains("interrupt"));
    }

    // T-convo-allowSessionRules (F-01): never setMode/addDirectories/foreign tools.
    #[test]
    fn allow_session_forwards_only_this_tools_add_rules() {
        let mut v = bash();
        v["permission_suggestions"].as_array_mut().unwrap().push(json!(
            {"type":"addRules","rules":[{"toolName":"Write"},{"toolName":"Bash","ruleContent":"ls"}],"behavior":"allow","destination":"userSettings"}));
        v["permission_suggestions"].as_array_mut().unwrap().push(json!(
            {"type":"replaceRules","rules":[{"toolName":"Bash"}],"behavior":"allow","destination":"session"}));
        let r = req(v);
        let p = payload(&r, Decision::AllowSession);
        let perms = p["updatedPermissions"].as_array().unwrap();
        assert_eq!(perms.len(), 1, "only the rule the label names");
        assert_eq!(perms[0]["rules"], json!([{"toolName":"Bash","ruleContent":"touch lp02-x.txt"}]));
        for entry in perms {
            assert_eq!(entry["type"], "addRules");
            assert_eq!(entry["destination"], "session");
            for rule in entry["rules"].as_array().unwrap() {
                assert_eq!(rule["toolName"], "Bash");
            }
        }
        let text = p.to_string();
        assert!(!text.contains("setMode") && !text.contains("addDirectories") && !text.contains("Settings"));

        // Write/Edit: the only suggestion is setMode → no button 2 [LP-02].
        let w = req(json!({"tool_name":"Write","input":{"file_path":"/w/p/a.txt","content":"x"},"tool_use_id":"toolu_w",
                           "permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}]}));
        assert_eq!(w.card.always_label, None);
        assert_eq!(w.card.options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["1", "3"]);
        assert_eq!(decision_payload(&w, &Decision::AllowSession, true, true).unwrap_err(), PayloadError::Unsupported("allowSessionHidden"));

        // suppress_always_allow_rule and the flag hide it too.
        let mut s = bash();
        s["suppress_always_allow_rule"] = json!(true);
        assert!(req(s).card.always_label.is_none());
        let off = build_request("r", &serde_json::value::to_raw_value(&bash()).unwrap(), &ctx(false));
        assert!(off.card.always_label.is_none());
    }

    #[test]
    fn sandbox_ask_uses_the_webfetch_domain_rule_and_a_uuid_tool_use_id() {
        let r = req(json!({"subtype":"can_use_tool","tool_name":"SandboxNetworkAccess","input":{"host":"example.com"},
            "description":"Allow network connection to example.com?","tool_use_id":"5b0c2a7e-1111-4222-8333-944455556666",
            "permission_suggestions":[{"type":"addRules","rules":[{"toolName":"WebFetch","ruleContent":"domain:example.com"},{"toolName":"WebFetch","ruleContent":"domain:evil.com"}],"behavior":"allow","destination":"localSettings"}]}));
        assert_eq!(r.kind, RequestKind::Tool);
        assert!(!r.card.degraded);
        assert_eq!(r.card.always_label.as_deref(), Some("example.com"));
        let p = payload(&r, Decision::AllowSession);
        assert_eq!(p["toolUseID"], "5b0c2a7e-1111-4222-8333-944455556666");
        assert_eq!(p["updatedPermissions"][0]["rules"], json!([{"toolName":"WebFetch","ruleContent":"domain:example.com"}]));
    }

    #[test]
    fn button_two_grants_only_what_its_label_says() {
        let mut v = bash();
        v["permission_suggestions"] = json!([{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"cd /x"},{"toolName":"Bash","ruleContent":"rm -rf build:*"}],"behavior":"allow","destination":"localSettings"}]);
        let r = req(v);
        assert_eq!(r.card.always_label.as_deref(), Some("cd /x"));
        let p = payload(&r, Decision::AllowSession);
        assert_eq!(p["updatedPermissions"], json!([{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"cd /x"}],"behavior":"allow","destination":"session"}]));
        let mut hidden = req(bash());
        hidden.hide_allow_session();
        assert!(hidden.card.always_label.is_none());
        assert_eq!(hidden.card.options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["1", "3"]);
        assert_eq!(decision_payload(&hidden, &Decision::AllowSession, true, true).unwrap_err(), PayloadError::Unsupported("allowSessionHidden"));
    }

    #[test]
    fn the_original_input_goes_back_byte_for_byte_and_labels_are_unambiguous() {
        let raw = r#"{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"t1","input":{"z":1,"command":"ls","a":0.30000000000000004,"n":12345678901234567890123}}"#;
        let r = build_request("r", &RawValue::from_string(raw.into()).unwrap(), &ctx(true));
        assert_eq!(r.input.as_deref().unwrap().get(), r#"{"z":1,"command":"ls","a":0.30000000000000004,"n":12345678901234567890123}"#);
        let a = decision_payload(&r, &Decision::AllowOnce, true, true).unwrap();
        assert!(a.payload.get().contains(r#""updatedInput":{"z":1,"command":"ls","a":0.30000000000000004,"n":12345678901234567890123}"#));
        let long = "x".repeat(130);
        let q = req(json!({"tool_name":"AskUserQuestion","tool_use_id":"tq","input":{"questions":[{"question":"Q?","options":[
            {"label": format!("{long}A")},{"label": format!("{long}B")},{"label":"a\u{200b}b"},{"label":"ab"}],"multiSelect":false}]}}));
        let labels = q.card.questions[0].options.clone();
        let mut uniq = labels.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), 4, "every display label is distinct: {labels:?}");
        let d = Decision::Answer { answers: vec![QuestionAnswer { question_index: 0, labels: vec![labels[1].clone()], free_text: None }] };
        let v: Value = serde_json::from_str(decision_payload(&q, &d, true, true).unwrap().payload.get()).unwrap();
        assert_eq!(v["updatedInput"]["answers"]["Q?"], json!(format!("{long}B")), "the option chosen, not its look-alike");
    }

    #[test]
    fn golden_questions_and_plans() {
        let r = req(json!({"tool_name":"AskUserQuestion","tool_use_id":"toolu_q","requires_user_interaction":true,"input":{"questions":[
            {"question":"Which colour?","header":"Colour","options":[{"label":"red","description":"r"},{"label":"blue","description":"b"}],"multiSelect":false},
            {"question":"Which fruits?","header":"Fruit","options":[{"label":"apple"},{"label":"cherry"}],"multiSelect":true},
            {"question":"Which colour?","header":"Dup","options":[{"label":"green"}],"multiSelect":false},
            {"question":"What should I call you?","header":"Name","options":[{"label":"Zed"}],"multiSelect":false}]}}));
        assert_eq!(r.card.kind, InteractionKind::Question);
        assert_eq!(r.card.questions.len(), 3, "the repeated question text is offered once (F-37)");
        assert!(r.card.detail_lines.contains(&EvidenceLine::key(KEY_DUPLICATE_QUESTION)), "…and never dropped silently [A19.3]");
        let colour = &r.card.questions[0];
        assert_eq!(colour.header.as_deref(), Some("Colour"), "the chip is shown");
        assert_eq!(colour.descriptions, [Some("r".to_string()), Some("b".to_string())], "each option's description, in option order");
        assert_eq!(colour.previews, [None, None]);
        assert_eq!(r.card.questions[1].descriptions, [None, None], "an option without a description has none");
        // Replying instead of answering: the text goes back as the tool result.
        let instead = payload(&r, Decision::DenyWithMessage { message: "let's talk about something else".into() });
        assert_eq!(instead["behavior"], "deny");
        assert_eq!(instead["message"], "let's talk about something else");
        let original = json!([
            {"question":"Which colour?","header":"Colour","options":[{"label":"red","description":"r"},{"label":"blue","description":"b"}],"multiSelect":false},
            {"question":"Which fruits?","header":"Fruit","options":[{"label":"apple"},{"label":"cherry"}],"multiSelect":true},
            {"question":"Which colour?","header":"Dup","options":[{"label":"green"}],"multiSelect":false},
            {"question":"What should I call you?","header":"Name","options":[{"label":"Zed"}],"multiSelect":false}]);
        let d = Decision::Answer {
            answers: vec![
                QuestionAnswer { question_index: 0, labels: vec!["red".into()], free_text: None },
                QuestionAnswer { question_index: 1, labels: vec!["apple".into(), "cherry".into()], free_text: None },
                QuestionAnswer { question_index: 2, labels: vec![], free_text: Some("Zed (typed)".into()) },
            ],
        };
        assert_eq!(
            payload(&r, d.clone()),
            json!({"behavior":"allow","updatedInput":{"questions":original,"answers":{
                "Which colour?":"red","Which fruits?":["apple","cherry"],"What should I call you?":"Zed (typed)"}}})
        );
        // P10 fallback: free text as a deny with the text.
        let fb = decision_payload(&r, &d, true, false).unwrap();
        let v: Value = serde_json::from_str(fb.payload.get()).unwrap();
        assert_eq!(v["behavior"], "deny");
        assert_eq!(v["message"], "Zed (typed)");

        let plan = req(json!({"tool_name":"ExitPlanMode","tool_use_id":"toolu_p","input":{"plan":"1. do\n2. done","planFilePath":"/c/plans/x.md"}}));
        assert_eq!(plan.card.kind, InteractionKind::PlanApproval);
        assert_eq!(plan.card.options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["1", "3"]);
        assert_eq!(
            payload(&plan, Decision::ApprovePlan),
            json!({"behavior":"allow","updatedInput":{"plan":"1. do\n2. done","planFilePath":"/c/plans/x.md"},"toolUseID":"toolu_p","decisionClassification":"user_temporary"})
        );
        assert_eq!(
            payload(&plan, Decision::KeepPlanning { message: "add tests".into() }),
            json!({"behavior":"deny","message":"add tests","toolUseID":"toolu_p"})
        );
        // P10 fallback: a generic tool card.
        let mut c = ctx(true);
        c.plan_answer = false;
        let generic = build_request("r", &serde_json::value::to_raw_value(&json!({"tool_name":"ExitPlanMode","tool_use_id":"t","input":{"plan":"p"}})).unwrap(), &c);
        assert_eq!(generic.card.kind, InteractionKind::ToolApproval);
    }

    // T-convo-degraded.
    #[test]
    fn unreadable_requests_degrade() {
        let r = req(json!({"tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"has space"}));
        assert!(r.card.degraded);
        assert_eq!(r.kind, RequestKind::Degraded);
        assert_eq!(r.card.options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["1", "3"]);
        let p = payload(&r, Decision::AllowOnce);
        assert_eq!(p["updatedInput"], json!({"command":"ls"}));
        assert!(p.get("toolUseID").is_none());
        let no_input = req(json!({"tool_name":"Bash","input":"ls","tool_use_id":"t"}));
        assert!(no_input.card.degraded);
        assert_eq!(no_input.card.options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["3"]);
        assert_eq!(decision_payload(&no_input, &Decision::AllowOnce, true, true).unwrap_err(), PayloadError::Unsupported("decisionKind"));
        assert_eq!(payload(&no_input, Decision::Deny)["behavior"], "deny");
        let mut c = ctx(true);
        c.sandbox_ask = false;
        let sb = build_request("r", &serde_json::value::to_raw_value(&json!({"tool_name":"SandboxNetworkAccess","tool_use_id":"t","input":{"host":"x.com"}})).unwrap(), &c);
        assert!(sb.card.degraded, "P11 fallback renders the sandbox ask degraded");
    }

    #[test]
    fn card_fields_follow_the_table() {
        let mut v = bash();
        v["default_to_no"] = json!(true);
        v["decision_reason"] = json!("\u{1b}[31mmatched ask rule\u{1b}[0m");
        v["agent_id"] = json!("a0dff261b698289f3");
        v["title"] = json!("Run a command");
        let r = req(v);
        assert!(r.card.default_to_no);
        assert_eq!(r.card.title, "Run a command");
        assert_eq!(r.card.detail_lines[0], EvidenceLine::detail_text("Create lp07-d.txt file"), "the subagent is named by its task (F-28)");
        assert!(r.card.detail_lines.contains(&EvidenceLine::detail_text("matched ask rule")), "ANSI stripped");
        assert!(
            r.card.detail_lines.iter().all(|l| !serde_json::to_string(l).unwrap().contains("a0dff261b698289f3")),
            "the opaque id is never shown"
        );
        let mut u = bash();
        u["agent_id"] = json!("a1111111111111111");
        assert_eq!(req(u).card.detail_lines[0], EvidenceLine::key(KEY_SUBAGENT), "a key, never English text [A19.3]");
        assert!(req(bash()).card.detail_lines.iter().all(|l| l.key == "card.detailText"), "vendor text only travels as card.detailText");
        let single = req(json!({"tool_name":"AskUserQuestion","tool_use_id":"tq","input":{"questions":[{"question":"A?","options":[{"label":"x"}]}]}}));
        assert!(!single.card.detail_lines.contains(&EvidenceLine::key(KEY_DUPLICATE_QUESTION)));
        // The ≤ 2 KiB cap applies to the text; ≤ 12 lines in all.
        let long = req(json!({"tool_name":"Bash","tool_use_id":"tl","input":{"command": (0..40).map(|i| format!("line {i} {}", "z".repeat(100))).collect::<Vec<_>>().join("\n")}}));
        assert!(long.card.detail_lines.len() <= DETAIL_LINES);
        let text: usize = long.card.detail_lines.iter().filter_map(|l| l.params.get("text").and_then(Value::as_str)).map(str::len).sum();
        assert!(text <= DETAIL_BYTES);
        assert_eq!(decision_payload(&r, &Decision::AllowOnce, true, true).unwrap().outcome, InteractionOutcome::Allowed);
    }
}
