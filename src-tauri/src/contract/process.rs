//! ProcessRecord **[meta]** (contract §3.4.10) and the supervision kinds of
//! §3.8.
//!
//! Invariant: `leftover` is set only after the condition held for 2
//! consecutive scans; `QuitSurvivor` marks a vendor process that outlived the
//! 6 s quit bound and was adopted by the next boot sweep; masked argv is never
//! in the store (fetched by `health_process_detail`).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::common::{EpochMs, Vendor};
use crate::contract::ids::{ProcessKey, SessionUid};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ProcessRecord {
    pub key: ProcessKey,
    pub pid: i32,
    #[ts(type = "number")]
    pub start_sec: i64,
    pub vendor: Option<Vendor>,
    pub role: ProcRole,
    pub session_uid: Option<SessionUid>,
    pub gilde_owned: bool,
    /// Quantized like `memory_bytes`.
    #[ts(type = "number")]
    pub rss_bytes: u64,
    pub leftover: Option<Leftover>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ProcRole {
    Session,
    Teammate,
    ToolServer,
    /// `tmux -L claude-swarm-*`
    PaneServer,
    /// `claude daemon run`, `claude bg-pty-host`… (§5.2.2).
    VendorHelper,
    GildeHelper,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Leftover {
    pub reason: LeftoverReason,
    #[ts(type = "number")]
    pub since: EpochMs,
    pub held_scans: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum LeftoverReason {
    TeammateOfGoneLead,
    PaneServerOfDeadLead,
    ToolServerOfEndedSession,
    GildeCrashLeftover,
    QuitSurvivor,
}

/// `proc_registry.kind` (§3.8): every row follows write-before-spawn and
/// carries `parent_uid` where a session owns it. `gateway` is retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ProcKind {
    /// Terminal-mode PTYs (kept).
    Pty,
    /// The Node sidecar (kept).
    Sidecar,
    /// Conversation children.
    Convo,
    /// Hosted pane PTYs.
    Pane,
    /// `claude attach` PTYs.
    Attach,
    /// Sidecar turn runs (replaces `headless`).
    Turn,
    /// IDE opener.
    Ide,
    /// `claude stop`, `--version` probes.
    Vendorcli,
    /// `/bin/ps`, `/usr/sbin/lsof` single-pid fallbacks and the PATH probe.
    Probe,
    /// `git` invocations of the Changes view and the worktree operations
    /// (§9.3: the resolved `git`); short-lived, write-before-spawn like every
    /// other kind [A17.1].
    Git,
}

impl ProcKind {
    /// The `proc_registry.kind` column value.
    pub const fn as_str(self) -> &'static str {
        match self {
            ProcKind::Pty => "pty",
            ProcKind::Sidecar => "sidecar",
            ProcKind::Convo => "convo",
            ProcKind::Pane => "pane",
            ProcKind::Attach => "attach",
            ProcKind::Turn => "turn",
            ProcKind::Ide => "ide",
            ProcKind::Vendorcli => "vendorcli",
            ProcKind::Probe => "probe",
            ProcKind::Git => "git",
        }
    }
}
