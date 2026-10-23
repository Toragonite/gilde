//! Claude Conversation mode, raw stream-json duplex L1 (contract §6.1).
//!
//! Owner: T05. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.
//!
//! | module | what |
//! |---|---|
//! | [`wire`] | framing, depth check, typed frames, decision payload schema |
//! | [`handshake`] | argv/env (§6.1.1), `initialize` + allowlist + runtime compat check (§6.1.2), quota parsing |
//! | [`approvals`] | `can_use_tool` → card, every decision's wire payload (§6.1.4) |
//! | [`ledger`] | the request ledger [C12] and the Gilde id table [F-51] |
//! | [`guard`] | the silent-approval guard (§6.1.11), the AllowSession settings-file and mode-flip guards [F-01] |
//! | [`frames`] | the session core: frames → evidence, cards, classification (§6.1.5, §6.1.7) |
//! | [`driver`] | [`driver::ConvoDriver`]: spawn, pipes, the stop sequence, `SessionDriver` |
//!
//! `render::claude_stream` (also T05) turns the frames into blocks.

pub mod approvals;
pub mod driver;
pub mod frames;
pub mod guard;
pub mod handshake;
pub mod ledger;
pub mod wire;
#[cfg(feature = "l2")]
mod l2;

use std::sync::OnceLock;
use std::time::Instant;

use crate::contract::common::EpochMs;
use crate::contract::services::CompatApi;

/// The provisional-clause flags this transport reads (§1.3 "Claude flag"),
/// resolved once at spawn for the spawn-time version and never re-derived for
/// a running session (§3.10). A flag that is absent means the fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flags {
    /// P1 — `-p`, `origin` on user frames; fallback: no `-p`, no `origin`, `session_id:""`.
    pub handshake_shape: bool,
    /// P2 — `--session-id` honoured; fallback: adopt the id from `system/init`.
    pub session_id_honoured: bool,
    /// P3 — `--permission-prompts host`; fallback: flag omitted.
    pub permission_prompts_host: bool,
    /// P4 — button 2 (AllowSession, `addRules` only); fallback: hidden.
    pub allow_session: bool,
    /// P5 — `interrupt:true` on deny; fallback: Deny, then an `interrupt` request.
    pub deny_and_stop: bool,
    /// P6 — re-arm after a resume (no promise either way; cards from the
    /// `initialize` pending list are always made).
    pub resume_rearm: bool,
    /// P7 — stdin EOF cancels a pending prompt; fallback: the full stop sequence.
    pub stdin_eof_cancel: bool,
    /// P8 — an idle session exits on stdin EOF within 1.5 s.
    pub idle_stdin_exit: bool,
    /// P9 — `priority:"now"`; fallback: interrupt, wait for `result`, then send.
    pub now_priority: bool,
    /// P9 — the interrupt receipt.
    pub interrupt_receipt: bool,
    /// P9 — `session_state_changed`; fallback: phase from frames only.
    pub session_state_events: bool,
    /// P10 — ExitPlanMode card; fallback: a generic tool card.
    pub plan_answer: bool,
    /// P10 — free text as the answer value; fallback: DenyWithMessage.
    pub free_text_answer: bool,
    /// P11 — parallel subagent asks (cards stack by `request_id` either way).
    pub parallel_requests: bool,
    /// P11 — `error` reply = denial + retry (Gilde never sends it either way).
    pub error_reply: bool,
    /// P11 — the sandbox network ask shape; fallback: a degraded card.
    pub sandbox_ask: bool,
    /// P12 — `get_usage` over a live session; fallback: no live quota query.
    pub get_usage_live: bool,
    /// P13 — the guard may flip compat (open); fallback: flag only.
    pub rule_allow_marker: bool,
    /// P14 — `CLAUDE_CODE_ENTRYPOINT=gilde`; fallback: not set.
    pub entrypoint_marker: bool,
}

impl Flags {
    /// The Claude flag names, in field order (`claude-compat.json` keys).
    pub const NAMES: [&'static str; 19] = [
        "handshakeShape",
        "sessionIdHonoured",
        "permissionPromptsHost",
        "allowSession",
        "denyAndStop",
        "resumeRearm",
        "stdinEofCancel",
        "idleStdinExit",
        "nowPriority",
        "interruptReceipt",
        "sessionStateEvents",
        "planAnswer",
        "freeTextAnswer",
        "parallelRequests",
        "errorReply",
        "sandboxAsk",
        "getUsageLive",
        "ruleAllowMarker",
        "entrypointMarker",
    ];

    /// Every flag from a lookup by name.
    pub fn from_fn(f: impl Fn(&str) -> bool) -> Flags {
        Flags {
            handshake_shape: f("handshakeShape"),
            session_id_honoured: f("sessionIdHonoured"),
            permission_prompts_host: f("permissionPromptsHost"),
            allow_session: f("allowSession"),
            deny_and_stop: f("denyAndStop"),
            resume_rearm: f("resumeRearm"),
            stdin_eof_cancel: f("stdinEofCancel"),
            idle_stdin_exit: f("idleStdinExit"),
            now_priority: f("nowPriority"),
            interrupt_receipt: f("interruptReceipt"),
            session_state_events: f("sessionStateEvents"),
            plan_answer: f("planAnswer"),
            free_text_answer: f("freeTextAnswer"),
            parallel_requests: f("parallelRequests"),
            error_reply: f("errorReply"),
            sandbox_ask: f("sandboxAsk"),
            get_usage_live: f("getUsageLive"),
            rule_allow_marker: f("ruleAllowMarker"),
            entrypoint_marker: f("entrypointMarker"),
        }
    }

    /// The flags of `version` through `CompatApi::feature` (forward
    /// inheritance is the compat service's business, §3.10).
    pub fn from_compat(compat: &dyn CompatApi, version: &str) -> Flags {
        Flags::from_fn(|name| compat.feature(version, name))
    }

    /// Every flag on (`true`) or every fallback (`false`).
    pub fn all(on: bool) -> Flags {
        Flags::from_fn(|_| on)
    }
}

/// Wall clock, uptime and the local calendar day (tests pin them).
pub trait Clock: Send + Sync {
    /// UTC milliseconds.
    fn wall(&self) -> EpochMs;
    /// Milliseconds on a clock that never goes backwards (every duration
    /// threshold: `tooFresh`, the guard windows, `turn_force_stop`'s 5 s).
    fn mono_ms(&self) -> u64;
    /// The local calendar day `YYYY-MM-DD` of a wall-clock time (A10.4).
    fn day(&self, at: EpochMs) -> String;
}

/// The machine's clocks and time zone.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

fn mono_base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

impl Clock for SystemClock {
    fn wall(&self) -> EpochMs {
        crate::store::now_ms()
    }

    fn mono_ms(&self) -> u64 {
        mono_base().elapsed().as_millis() as u64
    }

    fn day(&self, at: EpochMs) -> String {
        crate::follow::tail::local_day(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_names_are_the_compat_keys() {
        let only = |n: &'static str| Flags::from_fn(move |k| k == n);
        assert!(only("allowSession").allow_session);
        assert!(!only("allowSession").deny_and_stop);
        assert!(only("entrypointMarker").entrypoint_marker);
        assert_eq!(Flags::all(true), Flags::from_fn(|k| Flags::NAMES.contains(&k)));
        for n in Flags::NAMES {
            assert!(crate::compat::features::is_claude_flag(n) || n == "ruleAllowMarker", "{n} is a Claude flag");
        }
    }
}
