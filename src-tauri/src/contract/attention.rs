//! The attention queue **[content]** (it embeds titles) (contract §3.4.9, §8.1).
//!
//! Computed once in Rust; every surface reads it as delivered (D7). Invariants:
//! each session at most once, in its highest band; a team never appears as its
//! own item; `need_you`/`also` equal the counts every surface shows (sidebar
//! header, `☰ n`, dock badge = `need_you`, Floor counter).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::activity::{ErrorSub, PrimaryAction};
use crate::contract::common::EpochMs;
use crate::contract::ids::{EpisodeId, MemberKey, SessionUid};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AttentionQueue {
    #[ts(type = "number")]
    pub rev: u64,
    /// Ranked (§8.1).
    pub items: Vec<AttentionItem>,
    /// Bands 1–2, excluding Later-ed items.
    pub need_you: u32,
    /// Bands 3–6, excluding Later-ed items.
    pub also: u32,
    /// `None` = not paused; [`PAUSED_UNTIL_RESUME`] = paused until the user
    /// resumes ("until I resume") [A16.2].
    #[ts(type = "number | null")]
    pub paused_until: Option<EpochMs>,
}

/// `paused_until` value for "until I resume": the largest JavaScript `Date`
/// (8.64e15 ms, exactly representable in both i64 and an f64), so the WebView
/// can compare it like any other time; anything ≥ this is shown as "until you
/// resume" [A16.2].
pub const PAUSED_UNTIL_RESUME: EpochMs = 8_640_000_000_000_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AttentionItem {
    /// "q_" + uid, or "q_leftovers".
    pub key: String,
    /// 1..=6
    pub band: u8,
    pub subject: AttentionSubject,
    pub reason: AttentionReason,
    /// Wall clock, for absolute display.
    #[ts(type = "number")]
    pub since: EpochMs,
    /// Uptime-based age at publish time; the UI adds its own monotonic elapsed
    /// since receipt.
    #[ts(type = "number")]
    pub waited_ms: u64,
    pub episode: EpisodeId,
    #[ts(type = "number | null")]
    pub later_until: Option<EpochMs>,
    pub primary: PrimaryAction,
    /// Owner elsewhere [M8].
    pub watched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum AttentionSubject {
    Session {
        uid: SessionUid,
        on_behalf_of: Option<MemberKey>,
        answer_in: Option<SessionUid>,
    },
    Leftovers {
        count: u32,
        #[ts(type = "number")]
        bytes: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum AttentionReason {
    NeedsApproval,
    NeedsAnswer,
    Error { sub: ErrorSub },
    Crashed,
    NoProgress { for_secs: u32 },
    NewReply,
    Leftovers,
}
