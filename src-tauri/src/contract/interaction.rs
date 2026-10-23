//! PendingInteraction **[content]** (contract §3.4.5) and the decisions that
//! answer it (§4.2.3) [C12].
//!
//! Invariants: `options` keys are unique and in `^[1-9]$|^esc$`; for
//! `ToolApproval` the keys are exactly `1` (AllowOnce), optionally `2`
//! (AllowSession, iff `always_label.is_some()`), `3` (Deny) — numbers never
//! shift when 2 is hidden; for `WatchedApproval`/`WatchedQuestion` `options` is
//! empty (the only action is Go to owner). The full tool input is **not** in
//! the store; `interaction_detail` fetches it on demand (≤ 64 KiB).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::activity::EvidenceLine;
use crate::contract::common::{Confidence, EpochMs};
use crate::contract::content::ToolSummary;
use crate::contract::ids::{MemberKey, RequestId, SessionUid};
use crate::contract::validate::{self as v, Checked, Invalid, Validate};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct PendingInteraction {
    pub request_id: RequestId,
    pub kind: InteractionKind,
    #[ts(type = "number")]
    pub received_at: EpochMs,
    /// Set when a lead's prompt is a teammate's request (§7.2.6).
    pub on_behalf_of: Option<MemberKey>,
    /// The session whose screen/card answers it, if not this one.
    pub answer_in: Option<SessionUid>,
    /// ≤ 200 chars; title/description/display_name or the generic sentence.
    pub title: String,
    pub tool: Option<ToolSummary>,
    /// ≤ 12 lines, ≤ 2 KiB of text in total, sanitized. Every line is an i18n
    /// key + params the WebView renders; vendor text is
    /// `card.detailText` with `params.text` (never a bare string, never English
    /// in the payload) [A19.7].
    pub detail_lines: Vec<EvidenceLine>,
    /// Keys the card offers.
    pub options: Vec<InteractionOption>,
    /// Button 2 label; `None` ⇒ button 2 hidden.
    pub always_label: Option<String>,
    /// ⌘⏎ inert, focus on Deny.
    pub default_to_no: bool,
    /// Gilde could not fully parse the request (§6.1.9).
    pub degraded: bool,
    /// Screen prompts: digest of the prompt lines (TOCTOU guard, §6.2.4).
    pub prompt_rev: Option<String>,
    pub confidence: Confidence,
    /// Questions only.
    pub multi_select: bool,
    /// Question kind.
    pub questions: Vec<QuestionView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum InteractionKind {
    ToolApproval,
    Question,
    PlanApproval,
    ScreenPrompt,
    /// Unnumbered TUI menu, never answered by Gilde (§6.2.4).
    SetupPrompt,
    WatchedApproval,
    WatchedQuestion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct InteractionOption {
    /// "1".."9" | "esc"
    pub key: String,
    /// ≤ 120
    pub label: String,
    pub role: OptionRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum OptionRole {
    AllowOnce,
    AllowSession,
    Deny,
    Answer,
    Other,
    /// A setup prompt's "trust this folder" option; choosing it runs the
    /// clicked trust action of A6 (≤ 3 single arrow keys with a re-read, then
    /// one Enter, or abort) [A25.1].
    TrustFolder,
    /// A screen-prompt option the user may amend with a note before answering
    /// (Tab to amend, "Allow with a note", §6.2.4) [A25.1].
    AllowWithNote,
}

/// One question of an AskUserQuestion card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct QuestionView {
    /// ≤ 400
    pub text: String,
    /// Each label ≤ 120.
    pub options: Vec<String>,
    pub multi_select: bool,
}

/// Whether an option key has the §3.4.5 form.
pub fn is_option_key(k: &str) -> bool {
    k == "esc" || (k.len() == 1 && (b'1'..=b'9').contains(&k.as_bytes()[0]))
}

// ---------------------------------------------------------------------------
// decisions (§4.2.3)
// ---------------------------------------------------------------------------

/// The answer to a structured card.
///
/// Value rules: `labels` non-empty unless `free_text` is present; more than one
/// label only for `multi_select` questions; `question_index` < number of
/// questions; `Answer` only for `Question` cards, `ApprovePlan`/`KeepPlanning`
/// only for `PlanApproval` cards, `AllowSession` only when `always_label` is set
/// (else `UNSUPPORTED{allowSessionHidden}`), otherwise
/// `INVALID_ARGUMENT{field:"decision", rule:"kindForCard"}` (checked against
/// the card by the session actor: [`Decision::fits`]). A `Deny` needs no
/// message from the WebView; the core supplies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Decision {
    AllowOnce,
    /// Only when the card shows button 2: forwards only the `addRules`
    /// suggestions (frozen [R6 §3 LP-02]).
    AllowSession,
    /// message = "The user denied this action."
    Deny,
    /// 1..=8 KiB; Claude sees it as the tool result [R4 §1.6].
    DenyWithMessage { message: String },
    /// No message: the CLI does not deliver it with `interrupt:true`.
    DenyAndStop,
    /// AskUserQuestion; free text = the answer value (frozen [R6 §3 LP-06]).
    Answer { answers: Vec<QuestionAnswer> },
    /// Leaves plan mode (frozen [R6 §3 LP-06]).
    ApprovePlan,
    /// 1..=8 KiB
    KeepPlanning { message: String },
}

/// The kind of a decision (`interaction_answer.previous`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum DecisionKind {
    AllowOnce,
    AllowSession,
    Deny,
    DenyWithMessage,
    DenyAndStop,
    Answer,
    ApprovePlan,
    KeepPlanning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct QuestionAnswer {
    pub question_index: u8,
    /// Each must equal an offered label.
    pub labels: Vec<String>,
    /// ≤ 8 KiB
    pub free_text: Option<String>,
}

const DECISION_TEXT_MAX: usize = 8 * 1024;

impl Decision {
    pub fn kind(&self) -> DecisionKind {
        match self {
            Decision::AllowOnce => DecisionKind::AllowOnce,
            Decision::AllowSession => DecisionKind::AllowSession,
            Decision::Deny => DecisionKind::Deny,
            Decision::DenyWithMessage { .. } => DecisionKind::DenyWithMessage,
            Decision::DenyAndStop => DecisionKind::DenyAndStop,
            Decision::Answer { .. } => DecisionKind::Answer,
            Decision::ApprovePlan => DecisionKind::ApprovePlan,
            Decision::KeepPlanning { .. } => DecisionKind::KeepPlanning,
        }
    }

    /// The card-dependent rules of §4.2.3, checked by the session actor
    /// against the pending card (step (b) needs the card, so it runs after the
    /// existence check but still before any side effect).
    ///
    /// Returns `Err(Unsupported("allowSessionHidden"))` or the
    /// `kindForCard` / per-answer violations.
    pub fn fits(&self, card: &PendingInteraction) -> Result<(), DecisionMisfit> {
        let kind_ok = match self {
            Decision::Answer { .. } => card.kind == InteractionKind::Question,
            Decision::ApprovePlan | Decision::KeepPlanning { .. } => {
                card.kind == InteractionKind::PlanApproval
            }
            Decision::AllowOnce
            | Decision::Deny
            | Decision::DenyWithMessage { .. }
            | Decision::DenyAndStop => card.kind == InteractionKind::ToolApproval,
            Decision::AllowSession => card.kind == InteractionKind::ToolApproval,
        };
        if !kind_ok {
            return Err(DecisionMisfit::Invalid(Invalid::new("decision", "kindForCard")));
        }
        if matches!(self, Decision::AllowSession) && card.always_label.is_none() {
            return Err(DecisionMisfit::Unsupported("allowSessionHidden"));
        }
        if let Decision::Answer { answers } = self {
            for (i, a) in answers.iter().enumerate() {
                let at = |rule| DecisionMisfit::Invalid(Invalid::new(format!("decision.answers[{i}]"), rule));
                let Some(q) = card.questions.get(a.question_index as usize) else {
                    return Err(DecisionMisfit::Invalid(Invalid::new(
                        format!("decision.answers[{i}].questionIndex"),
                        "range",
                    )));
                };
                if a.labels.len() > 1 && !q.multi_select {
                    return Err(at("maxItems"));
                }
                for (j, l) in a.labels.iter().enumerate() {
                    if !q.options.iter().any(|o| o == l) {
                        return Err(DecisionMisfit::Invalid(Invalid::new(
                            format!("decision.answers[{i}].labels[{j}]"),
                            "pattern",
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Why a decision does not fit its card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionMisfit {
    /// → `INVALID_ARGUMENT{field, rule}`.
    Invalid(Invalid),
    /// → `UNSUPPORTED{cap}`.
    Unsupported(&'static str),
}

impl Validate for Decision {
    fn validate(&self) -> Checked {
        match self {
            Decision::DenyWithMessage { message } | Decision::KeepPlanning { message } => {
                v::text_range("message", message, 1, DECISION_TEXT_MAX)
            }
            Decision::Answer { answers } => {
                v::items_range("answers", answers, 1, 64)?;
                v::each("answers", answers)
            }
            _ => Ok(()),
        }
    }
}

impl Validate for QuestionAnswer {
    fn validate(&self) -> Checked {
        let free = self.free_text.as_deref().filter(|t| !t.is_empty());
        if self.labels.is_empty() && free.is_none() {
            return Err(Invalid::new("labels", "minItems"));
        }
        v::max_items("labels", &self.labels, 64)?;
        for (i, l) in self.labels.iter().enumerate() {
            v::required_str(&format!("labels[{i}]"), l, 4 * 120)?;
        }
        v::unique("labels", &self.labels)?;
        v::optional_str("freeText", &self.free_text, |f, s| v::text(f, s, DECISION_TEXT_MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(kind: InteractionKind) -> PendingInteraction {
        PendingInteraction {
            request_id: RequestId::parse("r1").unwrap(),
            kind,
            received_at: 1,
            on_behalf_of: None,
            answer_in: None,
            title: String::new(),
            tool: None,
            detail_lines: vec![],
            options: vec![],
            always_label: None,
            default_to_no: false,
            degraded: false,
            prompt_rev: None,
            confidence: Confidence::High,
            multi_select: false,
            questions: vec![QuestionView {
                text: "q".into(),
                options: vec!["a".into(), "b".into()],
                multi_select: false,
            }],
        }
    }

    #[test]
    fn decision_wire_shape() {
        let d: Decision = serde_json::from_str(r#"{"kind":"denyWithMessage","message":"no"}"#).unwrap();
        assert_eq!(d.kind(), DecisionKind::DenyWithMessage);
        assert!(serde_json::from_str::<Decision>(r#"{"kind":"nope"}"#).is_err());
        // serde alone accepts an unknown field on a unit variant; the request
        // pipeline (`decode_request`) rejects it.
        let req = serde_json::json!({"kind": "deny", "x": 1});
        let e = crate::contract::validate::decode_request::<Decision>(&req, 1 << 20).unwrap_err();
        assert_eq!(e.params["rule"], "shape");
        let ok = serde_json::json!({"kind": "denyWithMessage", "message": "no"});
        assert!(crate::contract::validate::decode_request::<Decision>(&ok, 1 << 20).is_ok());
    }

    #[test]
    fn kind_for_card_rules() {
        let tool = card(InteractionKind::ToolApproval);
        assert_eq!(
            Decision::ApprovePlan.fits(&tool),
            Err(DecisionMisfit::Invalid(Invalid::new("decision", "kindForCard")))
        );
        assert_eq!(
            Decision::AllowSession.fits(&tool),
            Err(DecisionMisfit::Unsupported("allowSessionHidden"))
        );
        let q = card(InteractionKind::Question);
        let two = Decision::Answer {
            answers: vec![QuestionAnswer {
                question_index: 0,
                labels: vec!["a".into(), "b".into()],
                free_text: None,
            }],
        };
        assert!(matches!(two.fits(&q), Err(DecisionMisfit::Invalid(_))));
        let bad_index = Decision::Answer {
            answers: vec![QuestionAnswer {
                question_index: 3,
                labels: vec!["a".into()],
                free_text: None,
            }],
        };
        assert!(matches!(bad_index.fits(&q), Err(DecisionMisfit::Invalid(_))));
    }

    #[test]
    fn answer_needs_a_label_or_free_text() {
        let empty = QuestionAnswer {
            question_index: 0,
            labels: vec![],
            free_text: Some(String::new()),
        };
        assert_eq!(empty.validate().unwrap_err().rule, "minItems");
        let free = QuestionAnswer {
            question_index: 0,
            labels: vec![],
            free_text: Some("x".into()),
        };
        assert!(free.validate().is_ok());
    }
}
