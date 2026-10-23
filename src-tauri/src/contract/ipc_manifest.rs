//! The IPC manifest (contract §4.1 rule 2, C1): every command with its request,
//! response, error codes, idempotency key, rate class and capability file, and
//! the four global events (§4.3).
//!
//! **Self-contained on purpose**: `build.rs` includes this file by path to feed
//! the same list to `tauri_build::AppManifest::commands`, so it may use only
//! `core`. From it are generated: `commands/registry.rs` (the
//! `generate_handler!` list), `capabilities/main.json` (`allow-<command>` for
//! exactly the main list), `src/lib/gen/commands.ts` and `src/lib/gen/events.ts`
//! (`scripts/gen-ipc.mjs`). A command that is not here is not invocable.

/// Token bucket classes (§4.1 rule 6). Exceeding returns `BUSY{retryAfterMs}`,
/// except `Probe` on `quota_refresh`, which returns the cached value with
/// `stale:true`. A45.3: `convo_subscribe`/`convo_unsubscribe` and
/// `pty_attach`/`pty_detach` are `Interactive` (screen flips mount and unmount
/// views in bursts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateClass {
    /// 50/s, burst 100.
    Interactive,
    /// 10/s per session.
    Turn,
    /// 5 per 10 s.
    Spawn,
    /// 20/s.
    Query,
    /// 1 per 60 s per account or per command.
    Probe,
    /// 10/s per command.
    Default,
}

impl RateClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            RateClass::Interactive => "interactive",
            RateClass::Turn => "turn",
            RateClass::Spawn => "spawn",
            RateClass::Query => "query",
            RateClass::Probe => "probe",
            RateClass::Default => "default",
        }
    }
}

/// Which capability file grants the command (§9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityFile {
    /// `capabilities/main.json`, every build.
    Main,
    /// `capabilities/debug.json`, debug builds only.
    Debug,
}

impl CapabilityFile {
    pub const fn as_str(self) -> &'static str {
        match self {
            CapabilityFile::Main => "main",
            CapabilityFile::Debug => "debug",
        }
    }
}

/// The idempotency key a command carries (§4.1 rule 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idempotency {
    None,
    /// Remembered 60 s → the same response.
    ClientRequestId,
    /// Remembered for the session's life → the first outcome.
    ClientMsgId,
    /// Outcome semantics of §4.2.3 [C12].
    RequestId,
    /// `requestId` + `promptRev`.
    RequestIdPromptRev,
    /// A single-use `confirmToken` from the matching `*_preview` (60 s,
    /// bound to a digest of the previewed state; `CONFLICT{changed}` if it differs).
    ConfirmToken,
}

impl Idempotency {
    pub const fn as_str(self) -> &'static str {
        match self {
            Idempotency::None => "none",
            Idempotency::ClientRequestId => "clientRequestId",
            Idempotency::ClientMsgId => "clientMsgId",
            Idempotency::RequestId => "requestId",
            Idempotency::RequestIdPromptRev => "requestId+promptRev",
            Idempotency::ConfirmToken => "confirmToken",
        }
    }
}

/// A documented error beyond the common ones: the code and the documented
/// values of its parameter (`cap`, `reason`, `changed`, `entity`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorSpec {
    pub code: &'static str,
    pub params: &'static [&'static str],
}

const fn e(code: &'static str, params: &'static [&'static str]) -> ErrorSpec {
    ErrorSpec { code, params }
}

/// Codes every command may return (§4.2 preamble).
pub const COMMON_ERRORS: &[&str] = &["INVALID_ARGUMENT", "NOT_FOUND", "INTERNAL", "BUSY"];

/// Wave 0 (contract §15.1 H1): every command answers
/// `UNSUPPORTED{cap: "notBuilt"}` until its owner builds it.
pub const WAVE0_STUB_ERROR: ErrorSpec = e("UNSUPPORTED", &["notBuilt"]);

/// Default request body cap: 1 MiB (§4.1 rule 7).
pub const BODY_DEFAULT: usize = 1024 * 1024;
/// `pty_write`: 1.4 MiB of base64.
pub const BODY_PTY_WRITE: usize = 1_468_006;
/// `turn_send`: 300 KiB.
pub const BODY_TURN_SEND: usize = 300 * 1024;

/// The response marker for a raw-bytes response (`pty_snapshot`).
pub const RAW_RESPONSE: &str = "raw";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// `snake_case` `domain_verb`.
    pub name: &'static str,
    /// Contract section.
    pub section: &'static str,
    /// The command module under `src-tauri/src/commands/` that implements it.
    pub module: &'static str,
    /// The build task that owns the implementation (§15.2).
    pub owner: &'static str,
    /// Request type (a contract type name).
    pub req: &'static str,
    /// Response type, or [`RAW_RESPONSE`].
    pub res: &'static str,
    /// `Some(T)` for the commands that also take a `Channel<T>`.
    pub channel: Option<&'static str>,
    /// Documented errors beyond [`COMMON_ERRORS`].
    pub errors: &'static [ErrorSpec],
    pub idempotency: Idempotency,
    pub rate: RateClass,
    pub capability: CapabilityFile,
    /// Payload cap checked before deserialization.
    pub max_body_bytes: usize,
}

const BASE: CommandSpec = CommandSpec {
    name: "",
    section: "",
    module: "",
    owner: "",
    req: "",
    res: "",
    channel: None,
    errors: &[],
    idempotency: Idempotency::None,
    rate: RateClass::Default,
    capability: CapabilityFile::Main,
    max_body_bytes: BODY_DEFAULT,
};

/// Every v1 command (§4.2), in contract order.
pub const COMMANDS: &[CommandSpec] = &[
    // -- §4.2.1 App, store, UI state ----------------------------------------
    CommandSpec { name: "app_bootstrap", section: "4.2.1", module: "app", owner: "T09", req: "AppBootstrapReq", res: "AppBootstrapRes", ..BASE },
    CommandSpec { name: "app_mark", section: "4.2.1", module: "app", owner: "T09", req: "AppMarkReq", res: "AppMarkRes", ..BASE },
    CommandSpec { name: "app_input_activity", section: "4.2.1", module: "notify", owner: "T12", req: "AppInputActivityReq", res: "AppInputActivityRes", rate: RateClass::Interactive, ..BASE },
    CommandSpec { name: "app_view_state", section: "4.2.1", module: "notify", owner: "T12", req: "ViewState", res: "AppViewStateRes", rate: RateClass::Interactive, ..BASE },
    CommandSpec { name: "store_subscribe", section: "4.2.1", module: "store", owner: "T09", req: "StoreSubscribeReq", res: "StoreSnapshot", channel: Some("StorePatch"), ..BASE },
    CommandSpec { name: "ui_state_set", section: "4.2.1", module: "store", owner: "T09", req: "UiStateSetReq", res: "UiStateSetRes", rate: RateClass::Interactive, ..BASE },
    CommandSpec { name: "app_quit_preview", section: "4.2.1", module: "quit", owner: "T09", req: "AppQuitPreviewReq", res: "AppQuitPreviewRes", ..BASE },
    CommandSpec { name: "app_quit", section: "4.2.1", module: "quit", owner: "T09", req: "AppQuitReq", res: "AppQuitRes", errors: &[e("CONFLICT", &["changed"])], idempotency: Idempotency::ConfirmToken, ..BASE },
    CommandSpec { name: "app_quit_cancel", section: "4.2.1", module: "quit", owner: "T09", req: "AppQuitCancelReq", res: "AppQuitCancelRes", ..BASE },
    CommandSpec { name: "continue_all", section: "4.2.1", module: "quit", owner: "T09", req: "ContinueAllReq", res: "ContinueAllRes", errors: &[e("NOT_READY", &["bootSweep"])], rate: RateClass::Spawn, ..BASE },
    CommandSpec { name: "quit_record_dismiss", section: "4.2.1", module: "quit", owner: "T09", req: "QuitRecordDismissReq", res: "QuitRecordDismissRes", ..BASE },
    CommandSpec { name: "agents_rescan", section: "4.2.1", module: "app", owner: "T09", req: "AgentsRescanReq", res: "AgentsRescanRes", rate: RateClass::Probe, ..BASE },
    CommandSpec { name: "app_diagnostics", section: "4.2.1", module: "app", owner: "T09", req: "AppDiagnosticsReq", res: "AppDiagnosticsRes", capability: CapabilityFile::Debug, ..BASE },
    // -- §4.2.2 Sessions ------------------------------------------------------
    CommandSpec { name: "newsession_options", section: "4.2.2", module: "session", owner: "T10", req: "NewsessionOptionsReq", res: "NewSessionOptions", ..BASE },
    CommandSpec { name: "project_pick", section: "4.2.2", module: "project", owner: "T11", req: "ProjectPickReq", res: "ProjectPickRes", errors: &[e("REFUSED", &["notADirectory"]), e("PERMISSION_DENIED", &[])], ..BASE },
    CommandSpec {
        name: "session_start", section: "4.2.2", module: "session", owner: "T10", req: "SessionStartReq", res: "SessionStartRes",
        errors: &[
            e("UNSUPPORTED", &["vendorNotInstalled", "modeNotAllowed", "teamNotAllowed", "conversationUnavailable", "conversationMinVersion"]),
            e("REFUSED", &["signInNeeded", "bypassNotAllowed", "worktreeGate", "accountNotConfirmed"]),
            e("CONFLICT", &["worktreeExists"]),
            e("NOT_READY", &["bootSweep", "agentsNotDetected"]),
            e("DISK_FULL", &[]),
            e("SPAWN_FAILED", &[]),
            e("TIMEOUT", &[]),
            e("PERMISSION_DENIED", &["file"]),
        ],
        idempotency: Idempotency::ClientRequestId, rate: RateClass::Spawn, ..BASE
    },
    CommandSpec {
        name: "session_continue", section: "4.2.2", module: "session", owner: "T10", req: "SessionContinueReq", res: "SessionContinueRes",
        errors: &[
            e("SESSION_LIVE_ELSEWHERE", &[]),
            e("REFUSED", &["idNotOnDisk", "notResumable", "heldByBackground"]),
            e("CONFLICT", &["alreadyOwned"]),
            e("UNSUPPORTED", &["continueOption"]),
            e("NOT_READY", &["bootSweep", "stoppingPrevious"]),
            e("DISK_FULL", &[]),
            e("SPAWN_FAILED", &[]),
        ],
        idempotency: Idempotency::ClientRequestId, rate: RateClass::Spawn, ..BASE
    },
    CommandSpec { name: "session_retry_as_terminal", section: "4.2.2", module: "session", owner: "T10", req: "SessionRetryAsTerminalReq", res: "SessionRetryAsTerminalRes", errors: &[e("REFUSED", &["notEligible"])], idempotency: Idempotency::ClientRequestId, rate: RateClass::Spawn, ..BASE },
    CommandSpec { name: "session_stop_preview", section: "4.2.2", module: "session", owner: "T10", req: "SessionStopPreviewReq", res: "SessionStopPreviewRes", errors: &[e("UNSUPPORTED", &["notLive"])], ..BASE },
    CommandSpec { name: "session_stop", section: "4.2.2", module: "session", owner: "T10", req: "SessionStopReq", res: "SessionStopRes", errors: &[e("CONFLICT", &["changed"]), e("REFUSED", &["identityUnverified"])], idempotency: Idempotency::ConfirmToken, ..BASE },
    CommandSpec { name: "session_mark_seen", section: "4.2.2", module: "session", owner: "T10", req: "SessionMarkSeenReq", res: "SessionMarkSeenRes", ..BASE },
    CommandSpec { name: "session_dismiss", section: "4.2.2", module: "session", owner: "T10", req: "SessionDismissReq", res: "SessionDismissRes", errors: &[e("REFUSED", &["notInState"])], ..BASE },
    CommandSpec { name: "session_models", section: "4.2.2", module: "session", owner: "T10", req: "SessionModelsReq", res: "SessionModelsRes", errors: &[e("UNSUPPORTED", &["setModel"])], ..BASE },
    CommandSpec { name: "session_technical", section: "4.2.2", module: "session", owner: "T10", req: "SessionTechnicalReq", res: "SessionTechnicalRes", errors: &[e("UNSUPPORTED", &["notLive"])], rate: RateClass::Query, ..BASE },
    // -- §4.2.3 Turns and interactions --------------------------------------------
    CommandSpec {
        name: "turn_send", section: "4.2.3", module: "turn", owner: "T10", req: "TurnSendReq", res: "TurnSendRes",
        errors: &[e("UNSUPPORTED", &["send"]), e("CONFLICT", &["queueFull"]), e("REFUSED", &["screenPromptPending"]), e("BUSY", &["stdinFull"])],
        idempotency: Idempotency::ClientMsgId, rate: RateClass::Turn, max_body_bytes: BODY_TURN_SEND, ..BASE
    },
    CommandSpec { name: "turn_cancel_queued", section: "4.2.3", module: "turn", owner: "T10", req: "TurnCancelQueuedReq", res: "TurnCancelQueuedRes", errors: &[e("NOT_FOUND", &["message"])], rate: RateClass::Turn, ..BASE },
    CommandSpec { name: "turn_interrupt", section: "4.2.3", module: "turn", owner: "T10", req: "TurnInterruptReq", res: "TurnInterruptRes", errors: &[e("UNSUPPORTED", &["interrupt"])], rate: RateClass::Turn, ..BASE },
    CommandSpec { name: "turn_force_stop", section: "4.2.3", module: "turn", owner: "T10", req: "TurnForceStopReq", res: "TurnForceStopRes", errors: &[e("REFUSED", &["tooEarly"]), e("UNSUPPORTED", &["forceStop"])], rate: RateClass::Turn, ..BASE },
    CommandSpec { name: "turn_retry", section: "4.2.3", module: "turn", owner: "T10", req: "TurnRetryReq", res: "TurnRetryRes", errors: &[e("REFUSED", &["notInError"])], idempotency: Idempotency::ClientMsgId, rate: RateClass::Turn, ..BASE },
    CommandSpec { name: "session_set_model", section: "4.2.3", module: "session", owner: "T10", req: "SessionSetModelReq", res: "SessionSetModelRes", errors: &[e("UNSUPPORTED", &["setModel"]), e("TIMEOUT", &[]), e("VENDOR_ERROR", &[])], ..BASE },
    CommandSpec { name: "session_set_permission_mode", section: "4.2.3", module: "session", owner: "T10", req: "SessionSetPermissionModeReq", res: "SessionSetPermissionModeRes", errors: &[e("UNSUPPORTED", &["setPermissionMode"]), e("REFUSED", &["needsLaunchFlag"]), e("TIMEOUT", &[]), e("VENDOR_ERROR", &[])], ..BASE },
    CommandSpec {
        name: "interaction_answer", section: "4.2.3", module: "interaction", owner: "T10", req: "InteractionAnswerReq", res: "InteractionAnswerRes",
        errors: &[e("NOT_FOUND", &["request"]), e("UNSUPPORTED", &["allowSessionHidden", "decisionKind"]), e("REFUSED", &["watching", "defaultToNo"]), e("CONFLICT", &["tooFresh"])],
        idempotency: Idempotency::RequestId, rate: RateClass::Turn, ..BASE
    },
    CommandSpec {
        name: "screen_prompt_key", section: "4.2.3", module: "interaction", owner: "T10", req: "ScreenPromptKeyReq", res: "ScreenPromptKeyRes",
        errors: &[e("STALE_PROMPT", &[]), e("UNSUPPORTED", &["key", "allowWithNote"]), e("REFUSED", &["setupPrompt"])],
        idempotency: Idempotency::RequestIdPromptRev, rate: RateClass::Turn, ..BASE
    },
    CommandSpec { name: "interaction_detail", section: "4.2.3", module: "interaction", owner: "T10", req: "InteractionDetailReq", res: "InteractionDetailRes", rate: RateClass::Query, ..BASE },
    // -- §4.2.4 Content channels -----------------------------------------------------
    CommandSpec { name: "convo_subscribe", section: "4.2.4", module: "convo", owner: "T04", req: "ConvoSubscribeReq", res: "ConvoSubscribeRes", channel: Some("ConvoEnvelope"), errors: &[e("UNSUPPORTED", &["noReadableConversation"])], rate: RateClass::Interactive, ..BASE },
    CommandSpec { name: "convo_unsubscribe", section: "4.2.4", module: "convo", owner: "T04", req: "ConvoUnsubscribeReq", res: "ConvoUnsubscribeRes", rate: RateClass::Interactive, ..BASE },
    CommandSpec { name: "convo_load_older", section: "4.2.4", module: "convo", owner: "T04", req: "ConvoLoadOlderReq", res: "ConvoLoadOlderRes", rate: RateClass::Query, ..BASE },
    CommandSpec { name: "pty_attach", section: "4.2.4", module: "pty", owner: "T06", req: "PtyAttachReq", res: "PtyInfo", channel: Some("PtyChunk"), errors: &[e("UNSUPPORTED", &["noPty"])], rate: RateClass::Interactive, ..BASE },
    CommandSpec { name: "pty_snapshot", section: "4.2.4", module: "pty", owner: "T06", req: "PtySnapshotReq", res: RAW_RESPONSE, ..BASE },
    CommandSpec { name: "pty_write", section: "4.2.4", module: "pty", owner: "T06", req: "PtyWriteReq", res: "PtyWriteRes", rate: RateClass::Interactive, max_body_bytes: BODY_PTY_WRITE, ..BASE },
    CommandSpec { name: "pty_paste", section: "4.2.4", module: "pty", owner: "T06", req: "PtyPasteReq", res: "PtyPasteRes", rate: RateClass::Interactive, ..BASE },
    CommandSpec { name: "pty_resize", section: "4.2.4", module: "pty", owner: "T06", req: "PtyResizeReq", res: "PtyResizeRes", rate: RateClass::Interactive, ..BASE },
    CommandSpec { name: "pty_detach", section: "4.2.4", module: "pty", owner: "T06", req: "PtyDetachReq", res: "PtyDetachRes", rate: RateClass::Interactive, ..BASE },
    // -- §4.2.5 Changes, IDE, links, owners --------------------------------------------
    CommandSpec { name: "changes_get", section: "4.2.5", module: "changes", owner: "T11", req: "ChangesGetReq", res: "ChangesView", errors: &[e("UNSUPPORTED", &["notGit"]), e("GIT_FAILED", &["repoProgram"]), e("TIMEOUT", &[]), e("SPAWN_FAILED", &[]), e("NOT_READY", &["bootSweep"]), e("DISK_FULL", &[])], rate: RateClass::Query, ..BASE },
    CommandSpec { name: "git_commit_preview", section: "4.2.5", module: "changes", owner: "T11", req: "GitCommitPreviewReq", res: "GitCommitPreviewRes", errors: &[e("REFUSED", &["ownerAlive"]), e("UNSUPPORTED", &["notGit"]), e("GIT_FAILED", &["repoProgram"]), e("TIMEOUT", &[]), e("SPAWN_FAILED", &[]), e("NOT_READY", &["bootSweep"]), e("DISK_FULL", &[])], ..BASE },
    CommandSpec { name: "git_commit", section: "4.2.5", module: "changes", owner: "T11", req: "GitCommitReq", res: "GitCommitRes", errors: &[e("CONFLICT", &["changed"]), e("GIT_FAILED", &["lockLeft", "repoProgram"]), e("REFUSED", &["ownerAlive"]), e("TIMEOUT", &[]), e("SPAWN_FAILED", &[]), e("NOT_READY", &["bootSweep"]), e("DISK_FULL", &[])], idempotency: Idempotency::ConfirmToken, ..BASE },
    CommandSpec { name: "git_merge_preview", section: "4.2.5", module: "changes", owner: "T11", req: "GitMergePreviewReq", res: "GitMergePreviewRes", errors: &[e("UNSUPPORTED", &["notGit", "notAWorktree"]), e("REFUSED", &["ownerAlive", "noBaseBranch", "mainDirty", "conflicts"]), e("GIT_FAILED", &["repoProgram"]), e("TIMEOUT", &[]), e("SPAWN_FAILED", &[]), e("NOT_READY", &["bootSweep"]), e("DISK_FULL", &[])], ..BASE },
    CommandSpec { name: "git_merge", section: "4.2.5", module: "changes", owner: "T11", req: "GitMergeReq", res: "GitMergeRes", errors: &[e("REFUSED", &["mainDirty", "conflicts", "ownerAlive", "noBaseBranch"]), e("CONFLICT", &["changed"]), e("GIT_FAILED", &["lockLeft", "repoProgram"]), e("UNSUPPORTED", &["notGit", "notAWorktree"]), e("TIMEOUT", &[]), e("SPAWN_FAILED", &[]), e("NOT_READY", &["bootSweep"]), e("DISK_FULL", &[])], idempotency: Idempotency::ConfirmToken, ..BASE },
    CommandSpec { name: "worktree_discard_preview", section: "4.2.5", module: "worktree", owner: "T11", req: "WorktreeDiscardPreviewReq", res: "WorktreeDiscardPreviewRes", errors: &[e("UNSUPPORTED", &["notGit", "notAWorktree"]), e("REFUSED", &["ownerAlive", "sessionsLive", "operationInProgress", "nestedRepository", "notGildeCreated", "lockedByUser"]), e("GIT_FAILED", &["lockLeft", "repoProgram"]), e("TIMEOUT", &[]), e("SPAWN_FAILED", &[]), e("NOT_READY", &["bootSweep"]), e("DISK_FULL", &[])], ..BASE },
    CommandSpec { name: "worktree_discard", section: "4.2.5", module: "worktree", owner: "T11", req: "WorktreeDiscardReq", res: "WorktreeDiscardRes", errors: &[e("CONFLICT", &["changed"]), e("REFUSED", &["unacknowledgedLoss", "ownerAlive", "sessionsLive", "operationInProgress", "nestedRepository", "notGildeCreated", "lockedByUser"]), e("UNSUPPORTED", &["notGit", "notAWorktree"]), e("GIT_FAILED", &["lockLeft", "repoProgram"]), e("TIMEOUT", &[]), e("SPAWN_FAILED", &[]), e("NOT_READY", &["bootSweep"]), e("DISK_FULL", &[]), e("PERMISSION_DENIED", &["file"])], idempotency: Idempotency::ConfirmToken, ..BASE },
    CommandSpec { name: "ide_open", section: "4.2.5", module: "ide", owner: "T11", req: "IdeOpenReq", res: "IdeOpenRes", errors: &[e("REFUSED", &["outsideRoots"]), e("UNSUPPORTED", &["diffNotSupportedByEditor"]), e("NOT_READY", &["editorNotChosen", "bootSweep"]), e("SPAWN_FAILED", &[]), e("GIT_FAILED", &["repoProgram"]), e("TIMEOUT", &[]), e("DISK_FULL", &[]), e("PERMISSION_DENIED", &["file"])], ..BASE },
    CommandSpec { name: "ide_reveal", section: "4.2.5", module: "ide", owner: "T11", req: "IdeRevealReq", res: "IdeRevealRes", ..BASE },
    CommandSpec { name: "ide_editors", section: "4.2.5", module: "ide", owner: "T11", req: "IdeEditorsReq", res: "IdeEditorsRes", ..BASE },
    CommandSpec { name: "link_open", section: "4.2.5", module: "ide", owner: "T11", req: "LinkOpenReq", res: "LinkOpenRes", errors: &[e("INVALID_ARGUMENT", &["url"]), e("SPAWN_FAILED", &[])], ..BASE },
    CommandSpec { name: "owner_focus", section: "4.2.5", module: "owner", owner: "T12", req: "OwnerFocusReq", res: "OwnerFocusRes", errors: &[e("UNSUPPORTED", &["noOwner"])], ..BASE },
    // -- §4.2.6 Teams -----------------------------------------------------------------
    CommandSpec { name: "team_send", section: "4.2.6", module: "team", owner: "T07", req: "TeamSendReq", res: "TeamSendRes", errors: &[e("UNSUPPORTED", &["viaLeadOnly", "orchestratorTeam", "memberGone"])], idempotency: Idempotency::ClientMsgId, rate: RateClass::Turn, ..BASE },
    CommandSpec { name: "team_member_close_preview", section: "4.2.6", module: "team", owner: "T07", req: "TeamMemberClosePreviewReq", res: "TeamMemberClosePreviewRes", errors: &[e("UNSUPPORTED", &["notAPane"])], ..BASE },
    CommandSpec { name: "team_member_close", section: "4.2.6", module: "team", owner: "T07", req: "TeamMemberCloseReq", res: "TeamMemberCloseRes", errors: &[e("CONFLICT", &["changed"])], idempotency: Idempotency::ConfirmToken, ..BASE },
    CommandSpec { name: "team_message_body", section: "4.2.6", module: "team", owner: "T07", req: "TeamMessageBodyReq", res: "TeamMessageBodyRes", rate: RateClass::Query, ..BASE },
    CommandSpec { name: "team_messages_page", section: "4.2.6", module: "team", owner: "T07", req: "TeamMessagesPageReq", res: "TeamMessagesPageRes", rate: RateClass::Query, ..BASE },
    CommandSpec { name: "team_task_title", section: "4.2.6", module: "team", owner: "T07", req: "TeamTaskTitleReq", res: "TeamTaskTitleRes", ..BASE },
    // -- §4.2.7 Accounts, quota, health --------------------------------------------------
    CommandSpec { name: "account_add", section: "4.2.7", module: "accounts", owner: "T08", req: "AccountAddReq", res: "AccountAddRes", errors: &[e("REFUSED", &["notAConfigDir", "alreadyAdded"])], ..BASE },
    CommandSpec { name: "account_confirm", section: "4.2.7", module: "accounts", owner: "T08", req: "AccountConfirmReq", res: "AccountConfirmRes", errors: &[e("REFUSED", &["notDiscovered"])], ..BASE },
    CommandSpec { name: "account_remove", section: "4.2.7", module: "accounts", owner: "T08", req: "AccountRemoveReq", res: "AccountRemoveRes", errors: &[e("REFUSED", &["inUse", "notUserAdded"])], ..BASE },
    CommandSpec { name: "account_set_default", section: "4.2.7", module: "accounts", owner: "T08", req: "AccountSetDefaultReq", res: "AccountSetDefaultRes", errors: &[e("REFUSED", &["notUsable"])], ..BASE },
    CommandSpec { name: "account_set_label", section: "4.2.7", module: "accounts", owner: "T08", req: "AccountSetLabelReq", res: "AccountSetLabelRes", ..BASE },
    CommandSpec { name: "account_sign_in", section: "4.2.7", module: "accounts", owner: "T08", req: "AccountSignInReq", res: "AccountSignInRes", errors: &[e("UNSUPPORTED", &["vendorNotInstalled"]), e("NOT_READY", &[]), e("SPAWN_FAILED", &[])], idempotency: Idempotency::ClientRequestId, rate: RateClass::Spawn, ..BASE },
    CommandSpec { name: "quota_refresh", section: "4.2.7", module: "accounts", owner: "T08", req: "QuotaRefreshReq", res: "QuotaRefreshRes", errors: &[e("SIDECAR_DOWN", &[]), e("NOT_READY", &["sidecarStarting"])], rate: RateClass::Probe, ..BASE },
    CommandSpec { name: "health_processes", section: "4.2.7", module: "health", owner: "T09", req: "HealthProcessesReq", res: "HealthProcessesRes", rate: RateClass::Query, ..BASE },
    CommandSpec { name: "health_process_detail", section: "4.2.7", module: "health", owner: "T09", req: "HealthProcessDetailReq", res: "HealthProcessDetailRes", rate: RateClass::Query, ..BASE },
    CommandSpec { name: "health_stop_preview", section: "4.2.7", module: "health", owner: "T09", req: "HealthStopPreviewReq", res: "HealthStopPreviewRes", ..BASE },
    CommandSpec { name: "health_stop", section: "4.2.7", module: "health", owner: "T09", req: "HealthStopReq", res: "HealthStopRes", errors: &[e("CONFLICT", &["changed"])], idempotency: Idempotency::ConfirmToken, ..BASE },
    CommandSpec { name: "undo_cancel", section: "4.2.7", module: "health", owner: "T09", req: "UndoCancelReq", res: "UndoCancelRes", errors: &[e("NOT_FOUND", &["token"])], ..BASE },
    // -- §4.2.8 History, attention, notifications, settings --------------------------------
    CommandSpec { name: "history_query", section: "4.2.8", module: "history", owner: "T03", req: "HistoryQueryReq", res: "HistoryQueryRes", rate: RateClass::Query, ..BASE },
    CommandSpec { name: "history_preview", section: "4.2.8", module: "history", owner: "T03", req: "HistoryPreviewReq", res: "HistoryPreviewRes", errors: &[e("UNSUPPORTED", &["titleOnly"])], rate: RateClass::Query, ..BASE },
    CommandSpec { name: "history_row", section: "4.2.8", module: "history", owner: "T03", req: "HistoryRowReq", res: "HistoryRow", rate: RateClass::Query, ..BASE },
    CommandSpec { name: "queue_later", section: "4.2.8", module: "notify", owner: "T12", req: "QueueLaterReq", res: "QueueLaterRes", errors: &[e("NOT_FOUND", &["queueItem"])], ..BASE },
    CommandSpec { name: "notify_pause", section: "4.2.8", module: "notify", owner: "T12", req: "NotifyPauseReq", res: "NotifyPauseRes", ..BASE },
    CommandSpec { name: "notify_resume", section: "4.2.8", module: "notify", owner: "T12", req: "NotifyResumeReq", res: "NotifyResumeRes", ..BASE },
    CommandSpec { name: "notify_permission_request", section: "4.2.8", module: "notify", owner: "T12", req: "NotifyPermissionRequestReq", res: "NotifyPermissionRequestRes", ..BASE },
    CommandSpec { name: "notify_test", section: "4.2.8", module: "notify", owner: "T12", req: "NotifyTestReq", res: "NotifyTestRes", rate: RateClass::Probe, ..BASE },
    CommandSpec { name: "settings_get", section: "4.2.8", module: "settings", owner: "T09", req: "SettingsGetReq", res: "Settings", ..BASE },
    CommandSpec { name: "settings_set", section: "4.2.8", module: "settings", owner: "T09", req: "SettingsSetReq", res: "Settings", ..BASE },
    CommandSpec { name: "settings_set_executable", section: "4.2.8", module: "settings", owner: "T09", req: "SettingsSetExecutableReq", res: "SettingsSetExecutableRes", ..BASE },
];

/// One global Tauri event (§4.3). Only these four exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventSpec {
    pub name: &'static str,
    /// Payload type (a contract type name).
    pub payload: &'static str,
    /// The TS helper name suffix (`on<Ident>`).
    pub ident: &'static str,
    pub when: &'static str,
}

pub const EVENTS: &[EventSpec] = &[
    EventSpec { name: "gilde://focus-request", payload: "FocusRequestEvent", ident: "FocusRequest", when: "notification click, single-instance relaunch, dock menu" },
    EventSpec { name: "gilde://quit-requested", payload: "QuitRequestedEvent", ident: "QuitRequested", when: "⌘Q / menu Quit while sessions run in Gilde (the exit is prevented until app_quit)" },
    EventSpec { name: "gilde://toast", payload: "ToastEvent", ident: "Toast", when: "outcomes of the user's own actions only" },
    EventSpec { name: "gilde://visibility", payload: "VisibilityEvent", ident: "Visibility", when: "window occlusion / app activation / sleep-wake" },
];

/// The core permissions `capabilities/main.json` grants besides the commands (§9.2).
pub const CORE_PERMISSIONS: &[&str] = &[
    "core:event:allow-listen",
    "core:event:allow-unlisten",
    "core:window:allow-start-dragging",
    "core:window:allow-set-title",
];

/// Look a command up by name.
pub fn command(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// The Tauri permission identifier of a command: `allow-<name>` with `_`
/// slugified to `-`, exactly as tauri-build autogenerates it.
pub fn permission_id(name: &str) -> String {
    let mut s = String::with_capacity(name.len() + 6);
    s.push_str("allow-");
    for ch in name.chars() {
        s.push(if ch == '_' { '-' } else { ch });
    }
    s
}
