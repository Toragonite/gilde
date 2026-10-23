//! §4.2.1 App, store, UI state.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use super::empty_structs;
use crate::contract::compat::ClaudeCompat;
use crate::contract::error::IpcError;
use crate::contract::ids::{ConfirmToken, MemberKey, ProjectId, QuitRecordId, SessionUid, TeamKey};
use crate::contract::store::TeamViewMode;
use crate::contract::validate::{self as v, Checked, Invalid, Validate, ROOT};

empty_structs!(req:
    /// `app_bootstrap` request.
    AppBootstrapReq,
    /// `app_input_activity` request (carries no content).
    AppInputActivityReq,
    /// `store_subscribe` request (+ `Channel<StorePatch>`).
    StoreSubscribeReq,
    /// `app_quit_preview` request.
    AppQuitPreviewReq,
    /// `app_quit_cancel` request.
    AppQuitCancelReq,
    /// `agents_rescan` request.
    AgentsRescanReq,
    /// `app_diagnostics` request (debug capability only).
    AppDiagnosticsReq,
);

empty_structs!(res:
    AppMarkRes,
    AppInputActivityRes,
    AppViewStateRes,
    UiStateSetRes,
    AppQuitRes,
    AppQuitCancelRes,
    QuitRecordDismissRes,
);

// -- app_bootstrap -----------------------------------------------------------

/// Never fails after B4 (only `INTERNAL`); ≤ 50 ms, memory only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AppBootstrapRes {
    /// MUST equal the WebView's embedded build id (§4.7).
    pub build_id: String,
    pub version: String,
    pub locale: Locale,
    pub theme: Theme,
    pub launch: LaunchInfo,
    /// `UiKey` → JSON, from `settings_kv` `ui.*`.
    pub ui_state: BTreeMap<String, Value>,
    pub platform: Platform,
    pub features: AppFeatures,
    pub compat: Vec<ClaudeCompat>,
}

/// The resolved UI locale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Locale {
    Ko,
    En,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Validate for Theme {}

/// [ux-ia §2.7, M2, M10]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum LaunchInfo {
    FirstLaunch,
    Restore,
    HomeAfter8h,
    AfterQuit { record_id: QuitRecordId, count: u32 },
    AfterCrash { record_id: QuitRecordId, count: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Platform {
    pub macos_major: u32,
    pub arm64: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AppFeatures {
    pub orchestrator: bool,
}

// -- app_mark ----------------------------------------------------------------

/// A repeated mark is ignored; logged with the boot timer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AppMarkReq {
    pub mark: AppMark,
    /// 0..=600000
    pub perf_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum AppMark {
    FirstPaint,
    Interactive,
}

impl Validate for AppMarkReq {
    fn validate(&self) -> Checked {
        v::range("perfMs", self.perf_ms, 0, 600_000)
    }
}

// -- app_view_state ----------------------------------------------------------

/// The `app_view_state` request [F-04]: sent on every change of screen, visible
/// tiles, focus or zoom, on `visibilitychange`, and after a reload. Memory
/// only; until the first call after launch every session counts as not visible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ViewState {
    pub screen: Screen,
    /// ≤ 32: the open session, every visible Split/Team tile.
    pub visible_uids: Vec<SessionUid>,
    pub focused_uid: Option<SessionUid>,
    pub zoomed_uid: Option<SessionUid>,
    pub team_key: Option<TeamKey>,
    pub document_visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Screen {
    Home,
    Session,
    Split,
    Team,
    Floor,
    History,
    Accounts,
    Settings,
}

impl Validate for ViewState {
    fn validate(&self) -> Checked {
        v::max_items("visibleUids", &self.visible_uids, 32)?;
        v::each("visibleUids", &self.visible_uids)?;
        v::nested("focusedUid", &self.focused_uid)?;
        v::nested("zoomedUid", &self.zoomed_uid)?;
        v::nested("teamKey", &self.team_key)
    }
}

// -- ui_state_set ------------------------------------------------------------

/// `ui_state_set.value` cap: 64 KiB of compact JSON (§4.2.1).
pub const UI_VALUE_MAX_BYTES: usize = 64 * 1024;
/// `ui_state_set.value` nesting cap (§4.2.1).
pub const UI_VALUE_MAX_DEPTH: usize = 16;
/// The prefix of the per-team layout keys.
pub const UI_TEAM_LAYOUT_PREFIX: &str = "teamLayout:";
/// A45.2: at most this many `teamLayout:<TeamKey>` rows are kept; the least
/// recently written one goes first.
pub const UI_TEAM_LAYOUT_MAX_ROWS: usize = 64;
/// A45.2: the whole UI state (keys + compact JSON values) stays at or under
/// this many bytes.
pub const UI_STATE_MAX_BYTES: usize = 256 * 1024;
/// A45.2: `sidebarOrder` ids / `sidebarFolds` entries.
pub const UI_SIDEBAR_MAX_ENTRIES: usize = 500;
/// A45.2: `splitSet.uids` (Split shows 2–6 tiles).
pub const UI_SPLIT_MAX_UIDS: usize = 6;
/// A45.2 / A30.3: `teamLayout.tileOrder`.
pub const UI_TILE_ORDER_MAX: usize = 64;
/// A30.3: `teamLayout.leadRatio` bounds.
pub const UI_LEAD_RATIO_MIN: f64 = 0.2;
pub const UI_LEAD_RATIO_MAX: f64 = 0.8;

/// `UiKey ∈ {sidebarOrder, sidebarFolds, splitSet, teamLayout:<TeamKey>,
/// lastView, homeMode, floorPrefs}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(transparent)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct UiKey(String);

impl UiKey {
    pub const FIXED: [&'static str; 6] = [
        "sidebarOrder",
        "sidebarFolds",
        "splitSet",
        "lastView",
        "homeMode",
        "floorPrefs",
    ];

    pub fn is_valid_str(s: &str) -> bool {
        Self::FIXED.contains(&s)
            || s.strip_prefix(UI_TEAM_LAYOUT_PREFIX)
                .is_some_and(TeamKey::is_valid_str)
    }

    /// Whether `s` is a `teamLayout:<TeamKey>` key.
    pub fn is_team_layout_str(s: &str) -> bool {
        s.strip_prefix(UI_TEAM_LAYOUT_PREFIX).is_some_and(TeamKey::is_valid_str)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Validate for UiKey {
    fn validate(&self) -> Checked {
        if Self::is_valid_str(&self.0) {
            Ok(())
        } else {
            Err(Invalid::new("", "pattern"))
        }
    }
}

/// `{ key: UiKey, value: Json }`; value ≤ 64 KiB, JSON depth ≤ 16, and (A45.2)
/// exactly the shape of its key ([`validate_ui_value`]); persisted in
/// `settings_kv`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct UiStateSetReq {
    pub key: UiKey,
    pub value: Value,
}

impl Validate for UiStateSetReq {
    fn validate(&self) -> Checked {
        v::nested("key", &self.key)?;
        if v::json_len(&self.value) > UI_VALUE_MAX_BYTES {
            return Err(Invalid::new("value", "maxLength"));
        }
        if v::json_depth(&self.value) > UI_VALUE_MAX_DEPTH {
            return Err(Invalid::new("value", "depth"));
        }
        validate_ui_value(self.key.as_str(), &self.value).map_err(|e| e.under("value"))
    }
}

/// The bytes one UI entry counts toward [`UI_STATE_MAX_BYTES`].
pub fn ui_entry_bytes(key: &str, value: &Value) -> usize {
    key.len() + v::json_len(value)
}

/// A45.2: the value schema of each `UiKey`. Every object denies unknown
/// fields, every string is an id or an enum value that matches its pattern —
/// a value can carry no free text. Also applied to persisted rows at load
/// time (a row that fails is dropped, never served). Violations name the
/// path inside the value (`$` for the value itself) and never echo a key the
/// caller chose.
pub fn validate_ui_value(key: &str, value: &Value) -> Checked {
    if v::json_len(value) > UI_VALUE_MAX_BYTES {
        return Err(Invalid::new(ROOT, "maxLength"));
    }
    if v::json_depth(value) > UI_VALUE_MAX_DEPTH {
        return Err(Invalid::new(ROOT, "depth"));
    }
    match key {
        "lastView" => typed::<UiLastView>(value)?.validate(),
        "splitSet" => typed::<UiSplitSet>(value)?.validate(),
        "homeMode" => typed::<UiHomeMode>(value).map(|_| ()),
        "floorPrefs" => typed::<UiFloorPrefs>(value).map(|_| ()),
        "sidebarOrder" => {
            let ids = typed::<Vec<ProjectId>>(value)?;
            v::max_items("", &ids, UI_SIDEBAR_MAX_ENTRIES)?;
            for (i, id) in ids.iter().enumerate() {
                v::nested(&format!("[{i}]"), id)?;
            }
            v::unique("", &ids)
        }
        "sidebarFolds" => {
            let folds = typed::<BTreeMap<String, bool>>(value)?;
            if folds.len() > UI_SIDEBAR_MAX_ENTRIES {
                return Err(Invalid::new(ROOT, "maxItems"));
            }
            if folds.keys().all(|k| is_fold_key(k)) {
                Ok(())
            } else {
                Err(Invalid::new(ROOT, "pattern"))
            }
        }
        k if UiKey::is_team_layout_str(k) => typed::<UiTeamLayout>(value)?.validate(),
        _ => Err(Invalid::new(ROOT, "pattern")),
    }
}

/// Deserialize `value` into the schema `T` and refuse anything the schema
/// would silently drop (an extra key next to a unit variant, a positional
/// array for an object): `shape` at the value itself.
fn typed<T: DeserializeOwned + Serialize>(value: &Value) -> Result<T, Invalid> {
    let shape = || Invalid::new(ROOT, "shape");
    let parsed: T = serde_json::from_value(value.clone()).map_err(|_| shape())?;
    let back = serde_json::to_value(&parsed).map_err(|_| shape())?;
    if v::ignored_key(value, &back, "").is_some() {
        return Err(shape());
    }
    Ok(parsed)
}

/// A sidebar fold key (`src/sidebar/tree.ts`): `earlier`, `p:<ProjectId>`,
/// `e:<ProjectId>`, `t:<TeamKey>` or `s:<SessionUid>`.
pub fn is_fold_key(k: &str) -> bool {
    if k == "earlier" {
        return true;
    }
    match k.split_once(':') {
        Some(("p" | "e", id)) => ProjectId::is_valid_str(id),
        Some(("t", id)) => TeamKey::is_valid_str(id),
        Some(("s", id)) => SessionUid::is_valid_str(id),
        _ => false,
    }
}

/// `lastView`: the main view (`MainView` of `src/store/uiSlice.ts`) — the
/// screen and its ids only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "screen", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum UiLastView {
    Home,
    Session { uid: SessionUid, sub: UiSessionSub },
    Split,
    Team { team_key: TeamKey, mode: TeamViewMode },
    Floor,
    History,
    Accounts { section: UiAccountsSection },
    Settings,
}

impl Validate for UiLastView {
    fn validate(&self) -> Checked {
        match self {
            UiLastView::Session { uid, .. } => v::nested("uid", uid),
            UiLastView::Team { team_key, .. } => v::nested("teamKey", team_key),
            _ => Ok(()),
        }
    }
}

/// The Session screen's sub-view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UiSessionSub {
    Conversation,
    Changes,
    Terminal,
}

/// The Accounts screen's section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UiAccountsSection {
    Accounts,
    Health,
}

/// `splitSet`: ≤ 6 distinct uids and the focused one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiSplitSet {
    pub uids: Vec<SessionUid>,
    #[serde(default)]
    pub focused: Option<SessionUid>,
}

impl Validate for UiSplitSet {
    fn validate(&self) -> Checked {
        v::max_items("uids", &self.uids, UI_SPLIT_MAX_UIDS)?;
        v::each("uids", &self.uids)?;
        v::unique("uids", &self.uids)?;
        v::nested("focused", &self.focused)
    }
}

/// `homeMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UiHomeMode {
    Focus,
    Classic,
}

/// `floorPrefs`: three flags (`src/floor/prefs.ts`); absent = the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiFloorPrefs {
    #[serde(default)]
    pub list: Option<bool>,
    #[serde(default)]
    pub traffic: Option<bool>,
    #[serde(default)]
    pub legend: Option<bool>,
}

/// `teamLayout:<TeamKey>` (`TeamLayout` of `src/store/uiSlice.ts`, A30.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiTeamLayout {
    pub mode: TeamViewMode,
    #[serde(default)]
    pub focused: Option<MemberKey>,
    #[serde(default)]
    pub zoomed: Option<MemberKey>,
    #[serde(default)]
    pub lead_ratio: Option<f64>,
    #[serde(default)]
    pub tile_order: Option<Vec<MemberKey>>,
}

impl Validate for UiTeamLayout {
    fn validate(&self) -> Checked {
        v::nested("focused", &self.focused)?;
        v::nested("zoomed", &self.zoomed)?;
        if let Some(r) = self.lead_ratio {
            if !(r.is_finite() && (UI_LEAD_RATIO_MIN..=UI_LEAD_RATIO_MAX).contains(&r)) {
                return Err(Invalid::new("leadRatio", "range"));
            }
        }
        if let Some(order) = &self.tile_order {
            v::max_items("tileOrder", order, UI_TILE_ORDER_MAX)?;
            v::each("tileOrder", order)?;
            v::unique("tileOrder", order)?;
        }
        Ok(())
    }
}

// -- quit ----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AppQuitPreviewRes {
    pub sessions: Vec<QuitRow>,
    /// Sessions running elsewhere (not affected).
    pub elsewhere: u32,
    pub teams: Vec<QuitTeamRow>,
    pub confirm_token: ConfirmToken,
}

/// [ux-ia §3.16]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct QuitRow {
    pub uid: SessionUid,
    pub project_id: Option<ProjectId>,
    pub waiting: Option<WaitingKind>,
    pub team_lead: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum WaitingKind {
    Approval,
    Answer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct QuitTeamRow {
    pub team_key: TeamKey,
    pub members: u32,
}

/// `CONFLICT{changed}` if sessions started or ended since the preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AppQuitReq {
    pub confirm_token: ConfirmToken,
}

impl Validate for AppQuitReq {
    fn validate(&self) -> Checked {
        v::nested("confirmToken", &self.confirm_token)
    }
}

/// `null` (or an empty list, §3.1 rule 4) = all items; starts in quit order,
/// ≤ 3 in flight; failures never abort the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ContinueAllReq {
    pub record_id: QuitRecordId,
    pub uids: Option<Vec<SessionUid>>,
}

impl Validate for ContinueAllReq {
    fn validate(&self) -> Checked {
        v::nested("recordId", &self.record_id)?;
        if let Some(uids) = &self.uids {
            v::each("uids", uids)?;
            v::unique("uids", uids)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ContinueAllRes {
    pub started: Vec<SessionUid>,
    pub failed: Vec<ContinueFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ContinueFailure {
    pub uid: SessionUid,
    pub error: IpcError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct QuitRecordDismissReq {
    pub record_id: QuitRecordId,
}

impl Validate for QuitRecordDismissReq {
    fn validate(&self) -> Checked {
        v::nested("recordId", &self.record_id)
    }
}

// -- agents_rescan, app_diagnostics -------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AgentsRescanRes {
    /// Always `true`.
    pub scheduled: bool,
}

/// Never content; paths under the home dir only as `~/…` (§9.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AppDiagnosticsRes {
    #[ts(type = "Record<string, number>")]
    pub counts: BTreeMap<String, u64>,
    pub versions: BTreeMap<String, String>,
    pub compat: Vec<ClaudeCompat>,
    #[ts(type = "Record<string, number>")]
    pub timings: BTreeMap<String, u64>,
    #[ts(type = "Record<string, number>")]
    pub error_codes: BTreeMap<String, u64>,
}
