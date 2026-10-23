//! New-session options with the compat and account rules (contract §4.2.2,
//! §3.10, ux-ia §3.12 and §5.8), and the production accounts and
//! `project_prefs` adapters.
//!
//! Owner: T10.
//!
//! `newsession_options` answers with what is known within 1 s (§4.2.2): the
//! detected vendors (or, while detection runs, none plus the
//! `newsession.findingAgents` notice), the usable accounts, the vendor's live
//! model list when the helper answers in time (else the static fallback),
//! the modes the installed claude version allows (Conversation is omitted for
//! a `Failed` or below-2.1.268 version, with its notice), the permission
//! modes the vendor accepts, the project's locations, the team switch and the
//! defaults from `project_prefs` and the §5.8 account rule.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::OptionalExtension as _;

use crate::accounts::AccountsService;
use crate::contract::account::{Account, Quota};
use crate::contract::common::{CompatState, Notice, PermissionMode, Vendor};
use crate::contract::compat::ClaudeCompat;
use crate::contract::error::IpcError;
use crate::contract::ids::{AccountId, ProjectId};
use crate::contract::ipc::{
    AccountOption, LocationOption, ModelOption, NewSessionOptions, NewsessionOptionsReq, StartDefaults,
    StartLocation, TeamMode,
};
use crate::contract::project::LocationKind;
use crate::contract::services::WorldApi;
use crate::contract::session::ControlMode;
use crate::contract::sidecar::{AccountEnv, ProbeParams};
use crate::store::Db;

use super::manager::{AccountsPort, PrefsStore, ProjectPrefs, SessionsManager};
use super::start::{conversation_refusal, mode_allowed};

/// The minimum claude version for Conversation mode [C3].
pub const CONVERSATION_MIN_VERSION: (u32, u32, u32) = (2, 1, 268);
/// An account at or above this share of any window is "near its limit" (§5.8).
pub const NEAR_LIMIT_PCT: u8 = 90;

/// The permission modes each vendor accepts (ux-ia §3.12 "Permissions shows
/// only the choices that vendor accepts"): claude all of R4 §1.9
/// (`bypassPermissions` only at start); codex the modes the helper maps to
/// its sandbox (T08's `TURN_PERMISSION_MAP`); cursor and agy none but the
/// vendor default.
pub fn permission_modes(vendor: Vendor) -> Vec<PermissionMode> {
    match vendor {
        Vendor::Claude => vec![
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::DontAsk,
            PermissionMode::Auto,
            PermissionMode::BypassPermissions,
        ],
        Vendor::Codex => vec![PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::Plan, PermissionMode::Auto],
        Vendor::Cursor | Vendor::Agy => vec![PermissionMode::Default],
    }
}

/// The static model fallback (§4.2.2 "live list or static fallback"): the
/// aliases the claude CLI resolves itself; other vendors have none, so any id
/// that passes the pattern rule is accepted until the live list is known.
pub struct StaticModels;

impl StaticModels {
    pub fn for_vendor(vendor: Vendor) -> &'static [&'static str] {
        match vendor {
            Vendor::Claude => &["opus", "sonnet", "haiku"],
            _ => &[],
        }
    }

    fn options(vendor: Vendor) -> Vec<ModelOption> {
        Self::for_vendor(vendor)
            .iter()
            .map(|id| ModelOption { id: (*id).to_string(), label: None, is_default: false })
            .collect()
    }
}

/// `x.y.z` → numbers (a suffix after the patch is ignored).
fn version_tuple(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c: String = it.next()?.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some((a, b, c.parse().ok()?))
}

/// The compat record of an installed claude version (a version the table
/// does not know is Unverified; below 2.1.268 it is below the minimum).
pub(crate) fn compat_of(compat: &dyn crate::contract::services::CompatApi, version: Option<&str>) -> Option<ClaudeCompat> {
    let v = version?;
    if let Some(c) = compat.lookup(v) {
        return Some(c);
    }
    let below = version_tuple(v).is_some_and(|t| t < CONVERSATION_MIN_VERSION);
    Some(ClaudeCompat {
        version: v.to_string(),
        conversation: CompatState::Unverified,
        pane_hosting: CompatState::Unverified,
        source: crate::contract::compat::CompatSource::Shipped,
        static_canary: crate::contract::compat::CanaryResult::NotRun,
        handshake_ok: None,
        below_minimum: below,
        note: None,
        features: Default::default(),
    })
}

fn max_pct(a: &Account) -> Option<u8> {
    a.quota.as_ref().and_then(|q| q.windows.iter().filter_map(|w| w.used_pct).max())
}

fn near_limit(a: &Account) -> bool {
    max_pct(a).is_some_and(|p| p >= NEAR_LIMIT_PCT)
}

fn earliest_reset(accounts: &[&Account]) -> Option<i64> {
    accounts
        .iter()
        .filter_map(|a| a.quota.as_ref())
        .flat_map(|q| q.windows.iter())
        .filter(|w| w.used_pct.is_some_and(|p| p >= NEAR_LIMIT_PCT))
        .filter_map(|w| w.resets_at)
        .min()
}

/// The automatic default rule of ux-ia §5.8: this project's last account,
/// unless it is at ≥ 90 % of any window — then the same vendor's account with
/// the most headroom (accounts with no known number rank after those with
/// one) and a one-line notice; if all are near, nothing is switched and the
/// notice names the earliest reset.
pub fn default_account(vendor: Vendor, accounts: &[Account], last: Option<&AccountId>) -> (Option<AccountId>, Option<Notice>) {
    let usable: Vec<&Account> = accounts.iter().filter(|a| a.vendor == vendor && a.usable_for_new).collect();
    let base = last
        .and_then(|id| usable.iter().find(|a| &a.id == id).copied())
        .or_else(|| usable.iter().find(|a| a.is_default).copied())
        .or_else(|| usable.first().copied());
    let Some(base) = base else { return (None, None) };
    if !near_limit(base) {
        return (Some(base.id.clone()), None);
    }
    let mut others: Vec<&Account> = usable.iter().copied().filter(|a| a.id != base.id && !near_limit(a)).collect();
    // Known headroom first (lowest share), unknown after.
    others.sort_by_key(|a| (max_pct(a).is_none(), max_pct(a).unwrap_or(0)));
    match others.first() {
        Some(best) => {
            let mut n = Notice::new("accounts.switched").param("a", base.label.clone()).param("b", best.label.clone());
            n.params.insert("p".into(), serde_json::Value::from(max_pct(base).unwrap_or(NEAR_LIMIT_PCT)));
            (Some(best.id.clone()), Some(n))
        }
        None => {
            let mut n = Notice::new("accounts.allNear").param("vendor", vendor.as_str());
            if let Some(at) = earliest_reset(&usable) {
                n.params.insert("time".into(), serde_json::Value::from(at));
            }
            (Some(base.id.clone()), Some(n))
        }
    }
}

fn push_notice(list: &mut Vec<Notice>, n: Notice) {
    if !list.iter().any(|x| x.key == n.key) {
        list.push(n);
    }
}

impl SessionsManager {
    /// The live model list last seen for a vendor (the `model` value rule).
    pub(crate) fn models_seen(&self, vendor: Vendor) -> Vec<String> {
        self.models_seen.lock().get(&vendor).cloned().unwrap_or_default()
    }

    /// `newsession_options` (§4.2.2).
    pub(crate) async fn new_session_options(&self, req: &NewsessionOptionsReq) -> Result<NewSessionOptions, IpcError> {
        let started = tokio::time::Instant::now();
        let budget = self.deps.timing.options_wait;
        let snapshot = self.deps.world.snapshot();
        let project = snapshot
            .projects
            .iter()
            .find(|p| p.id == req.project_id)
            .cloned()
            .ok_or_else(|| IpcError::not_found("project"))?;
        let mut notices: Vec<Notice> = Vec::new();
        let prefs = self.deps.prefs.get(&project.id).await.unwrap_or_default();

        let vendors = match tokio::time::timeout(budget, self.deps.binaries.installed()).await {
            Ok(Ok(v)) => v,
            _ => {
                push_notice(&mut notices, Notice::new("newsession.findingAgents"));
                Vec::new()
            }
        };
        let installed = |v: Vendor| vendors.iter().any(|o| o.vendor == v && o.installed);
        let vendor = req
            .vendor
            .or(prefs.last_vendor.filter(|v| installed(*v)))
            .or_else(|| Vendor::ALL.iter().copied().find(|v| installed(*v)))
            .unwrap_or(Vendor::Claude);

        let all_accounts = self.deps.accounts.all().await;
        let accounts: Vec<AccountOption> = all_accounts
            .iter()
            .filter(|a| a.usable_for_new && req.vendor.is_none_or(|v| a.vendor == v))
            .map(|a| AccountOption {
                account_id: a.id.clone(),
                vendor: a.vendor,
                label: a.label.clone(),
                is_default: a.is_default,
                sign_in: a.sign_in,
            })
            .collect();
        let (default_account, account_notice) = default_account(vendor, &all_accounts, prefs.last_account_id.as_ref());
        if let Some(n) = account_notice {
            push_notice(&mut notices, n);
        }

        // Modes: Conversation only on a claude version that allows it (§3.10).
        let claude_version = vendors.iter().find(|o| o.vendor == Vendor::Claude).and_then(|o| o.version.clone());
        let claude_compat = compat_of(self.deps.compat.as_ref(), claude_version.as_deref());
        let mut modes: Vec<ControlMode> = [ControlMode::Conversation, ControlMode::Terminal, ControlMode::TurnBased]
            .into_iter()
            .filter(|m| mode_allowed(vendor, *m))
            .collect();
        if vendor == Vendor::Claude {
            if let Some(cap) = conversation_refusal(claude_compat.as_ref()) {
                modes.retain(|m| *m != ControlMode::Conversation);
                let key = if cap == "conversationMinVersion" { "notice.conversationMinVersion" } else { "notice.conversationUnavailable" };
                push_notice(&mut notices, Notice::new(key));
            }
            if let Some(v) = &claude_version {
                for n in self.deps.compat.notices(v) {
                    push_notice(&mut notices, n);
                }
            }
        }

        // Models: the helper's live list within what is left of the 1 s.
        let mut models = StaticModels::options(vendor);
        if let (Some(sidecar), Some(account)) = (&self.deps.sidecar, default_account.clone()) {
            let left = budget.saturating_sub(started.elapsed());
            let probe = ProbeParams { vendor, account_id: account };
            if let Ok(Ok(list)) = tokio::time::timeout(left, sidecar.models(probe)).await {
                if !list.is_empty() {
                    self.models_seen.lock().insert(vendor, list.iter().map(|m| m.id.clone()).collect());
                    models = list.into_iter().map(|m| ModelOption { id: m.id, label: m.label, is_default: m.is_default }).collect();
                }
            }
        }
        if models.is_empty() {
            let seen = self.models_seen(vendor);
            models = seen.into_iter().map(|id| ModelOption { id, label: None, is_default: false }).collect();
        }

        let locations: Vec<LocationOption> = snapshot
            .locations
            .iter()
            .filter(|l| l.project_id == project.id && l.exists)
            .map(|l| LocationOption {
                location: if l.kind == LocationKind::Worktree {
                    StartLocation::ExistingWorktree { location_id: l.id.clone() }
                } else {
                    StartLocation::Here { location_id: Some(l.id.clone()) }
                },
                branch: l.branch.clone(),
                worktree_name: l.worktree_name.clone(),
            })
            .collect();

        let team_allowed = vendor == Vendor::Claude && installed(Vendor::Claude);
        let panes = self.deps.panehost.is_some()
            && claude_compat.as_ref().is_none_or(|c| c.pane_hosting != CompatState::Failed);
        let team_mode = if panes { TeamMode::Panes } else { TeamMode::InProcess };

        let default_mode = prefs
            .last_control_mode
            .filter(|m| modes.contains(m) && prefs.last_vendor == Some(vendor))
            .or_else(|| modes.first().copied());
        let default_model = prefs
            .last_model
            .clone()
            .filter(|m| prefs.last_vendor == Some(vendor) && models.iter().any(|o| &o.id == m));
        let main = snapshot
            .locations
            .iter()
            .find(|l| l.project_id == project.id && l.path == project.root)
            .map(|l| StartLocation::Here { location_id: Some(l.id.clone()) })
            .unwrap_or(StartLocation::Here { location_id: None });
        let default_location = match prefs.last_location_kind.as_deref() {
            Some("newWorktree" | "existingWorktree") => None,
            _ => Some(main),
        };

        Ok(NewSessionOptions {
            vendors,
            accounts,
            models,
            modes,
            permission_modes: permission_modes(vendor),
            locations,
            team_allowed,
            team_mode,
            defaults: StartDefaults {
                vendor: Some(vendor),
                account_id: default_account,
                model: default_model,
                control_mode: default_mode,
                permission_mode: Some(PermissionMode::Default),
                location: default_location,
            },
            notices,
        })
    }
}

// ---------------------------------------------------------------------------
// production adapters
// ---------------------------------------------------------------------------

/// The production [`AccountsPort`] over T08's `AccountsService`.
///
/// A live session's quota reading and a signed-out handshake go to the
/// accounts service (A24.1: `observe_quota` / `observe_signed_out`), which
/// keeps them as its own state and publishes them, so its next publish never
/// reverts them. No probe process starts (§4.2.7).
pub struct ServiceAccounts {
    svc: Arc<AccountsService>,
    world: Arc<dyn WorldApi>,
}

impl ServiceAccounts {
    pub fn new(svc: Arc<AccountsService>, world: Arc<dyn WorldApi>) -> ServiceAccounts {
        ServiceAccounts { svc, world }
    }

    /// The World this adapter reads accounts against (kept for callers).
    pub fn world(&self) -> &Arc<dyn WorldApi> {
        &self.world
    }
}

#[async_trait]
impl AccountsPort for ServiceAccounts {
    async fn all(&self) -> Vec<Account> {
        self.svc.accounts().await
    }

    async fn env_for(&self, id: &AccountId) -> Option<AccountEnv> {
        self.svc.env_for(id).await
    }

    fn observed_quota(&self, id: &AccountId, quota: Quota) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let svc = self.svc.clone();
        let id = id.clone();
        rt.spawn(async move { svc.observe_quota(&id, quota).await });
    }

    fn signed_out(&self, id: &AccountId) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let svc = self.svc.clone();
        let id = id.clone();
        rt.spawn(async move { svc.observe_signed_out(&id).await });
    }
}

fn vendor_of(s: &str) -> Option<Vendor> {
    Vendor::ALL.iter().copied().find(|v| v.as_str() == s)
}

fn mode_str(m: ControlMode) -> &'static str {
    match m {
        ControlMode::Conversation => "conversation",
        ControlMode::Terminal => "terminal",
        ControlMode::TurnBased => "turnBased",
    }
}

fn mode_of(s: &str) -> Option<ControlMode> {
    [ControlMode::Conversation, ControlMode::Terminal, ControlMode::TurnBased]
        .into_iter()
        .find(|m| mode_str(*m) == s)
}

/// The production [`PrefsStore`] over `state.db` `project_prefs` (§3.9, meta only).
pub struct DbPrefs {
    db: Db,
    clock: super::manager::WallClock,
}

impl DbPrefs {
    pub fn new(db: Db, clock: super::manager::WallClock) -> DbPrefs {
        DbPrefs { db, clock }
    }
}

#[async_trait]
impl PrefsStore for DbPrefs {
    async fn get(&self, project: &ProjectId) -> Option<ProjectPrefs> {
        let id = project.as_str().to_string();
        let row = self
            .db
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT last_vendor, last_account_id, last_model, last_control_mode, last_location_kind
                           FROM project_prefs WHERE project_id = ?1",
                        [id],
                        |r| {
                            Ok((
                                r.get::<_, Option<String>>(0)?,
                                r.get::<_, Option<String>>(1)?,
                                r.get::<_, Option<String>>(2)?,
                                r.get::<_, Option<String>>(3)?,
                                r.get::<_, Option<String>>(4)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .await
            .ok()
            .flatten()?;
        Some(ProjectPrefs {
            last_vendor: row.0.as_deref().and_then(vendor_of),
            last_account_id: row.1.and_then(|a| AccountId::parse(a).ok()),
            last_model: row.2,
            last_control_mode: row.3.as_deref().and_then(mode_of),
            last_location_kind: row.4,
        })
    }

    async fn put(&self, project: &ProjectId, prefs: ProjectPrefs) {
        let id = project.as_str().to_string();
        let now = (self.clock)();
        let res = self
            .db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO project_prefs
                       (project_id, last_vendor, last_account_id, last_model, last_control_mode, last_location_kind, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(project_id) DO UPDATE SET
                       last_vendor = excluded.last_vendor,
                       last_account_id = excluded.last_account_id,
                       last_model = excluded.last_model,
                       last_control_mode = excluded.last_control_mode,
                       last_location_kind = excluded.last_location_kind,
                       updated_at = excluded.updated_at",
                    rusqlite::params![
                        id,
                        prefs.last_vendor.map(|v| v.as_str()),
                        prefs.last_account_id.as_ref().map(|a| a.as_str().to_string()),
                        prefs.last_model,
                        prefs.last_control_mode.map(mode_str),
                        prefs.last_location_kind,
                        now,
                    ],
                )?;
                Ok(())
            })
            .await;
        if let Err(e) = res {
            let e = IpcError::from(&e);
            let (code, key, _) = e.log_fields();
            tracing::warn!(target: "sessions", code, key, "could not record the project's start preferences");
        }
    }
}

/// An in-memory [`PrefsStore`] (memory-only mode and tests).
#[derive(Default)]
pub struct MemoryPrefs(parking_lot::Mutex<HashMap<ProjectId, ProjectPrefs>>);

#[async_trait]
impl PrefsStore for MemoryPrefs {
    async fn get(&self, project: &ProjectId) -> Option<ProjectPrefs> {
        self.0.lock().get(project).cloned()
    }

    async fn put(&self, project: &ProjectId, prefs: ProjectPrefs) {
        self.0.lock().insert(project.clone(), prefs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::account::{AccountSource, QuotaWindow, QuotaWindowKind, SignIn};

    fn acct(id: &str, label: &str, default: bool, pct: Option<u8>) -> Account {
        Account {
            id: AccountId::parse(id).unwrap(),
            vendor: Vendor::Claude,
            config_dir: None,
            label: label.to_string(),
            is_default: default,
            source: AccountSource::User,
            usable_for_new: true,
            sign_in: SignIn::SignedIn,
            quota: pct.map(|p| Quota {
                windows: vec![QuotaWindow { kind: QuotaWindowKind::FiveHour, label: None, used_pct: Some(p), resets_at: Some(1_000 + i64::from(p)) }],
                fetched_at: 1,
                stale: false,
                unavailable: None,
            }),
            live_sessions: 0,
        }
    }

    #[test]
    fn the_last_account_is_kept_below_the_limit() {
        let a = acct("a_0000000000000001", "Work", false, Some(38));
        let b = acct("a_0000000000000002", "Personal", true, Some(10));
        let (id, notice) = default_account(Vendor::Claude, &[a.clone(), b], Some(&a.id));
        assert_eq!(id, Some(a.id));
        assert!(notice.is_none());
    }

    #[test]
    fn a_near_limit_account_switches_to_the_most_headroom_with_one_line() {
        let personal = acct("a_0000000000000001", "Personal", true, Some(91));
        let work = acct("a_0000000000000002", "Work", false, Some(38));
        let unknown = acct("a_0000000000000003", "Spare", false, None);
        let (id, notice) = default_account(Vendor::Claude, &[personal.clone(), unknown, work.clone()], Some(&personal.id));
        assert_eq!(id, Some(work.id), "known headroom ranks before unknown");
        let n = notice.unwrap();
        assert_eq!(n.key, "accounts.switched");
        assert_eq!(n.params["a"], "Personal");
        assert_eq!(n.params["b"], "Work");
        assert_eq!(n.params["p"], 91);
    }

    #[test]
    fn all_near_switches_nothing_and_names_the_earliest_reset() {
        let a = acct("a_0000000000000001", "A", true, Some(95));
        let b = acct("a_0000000000000002", "B", false, Some(92));
        let (id, notice) = default_account(Vendor::Claude, &[a.clone(), b], None);
        assert_eq!(id, Some(a.id));
        let n = notice.unwrap();
        assert_eq!(n.key, "accounts.allNear");
        assert_eq!(n.params["vendor"], "claude");
        assert_eq!(n.params["time"], 1_092);
    }

    #[test]
    fn unconfirmed_accounts_are_never_the_default() {
        let mut d = acct("a_0000000000000001", "Discovered", true, None);
        d.usable_for_new = false;
        d.source = AccountSource::Discovered;
        let (id, _) = default_account(Vendor::Claude, &[d], None);
        assert_eq!(id, None);
    }

    #[test]
    fn versions_below_the_minimum_are_detected_without_a_table_entry() {
        assert!(version_tuple("2.1.267").unwrap() < CONVERSATION_MIN_VERSION);
        assert!(version_tuple("2.1.268").unwrap() >= CONVERSATION_MIN_VERSION);
        assert!(version_tuple("2.1.306-beta").unwrap() >= CONVERSATION_MIN_VERSION);
        assert!(version_tuple("x").is_none());
    }

    #[test]
    fn bypass_is_offered_for_claude_only() {
        assert!(permission_modes(Vendor::Claude).contains(&PermissionMode::BypassPermissions));
        for v in [Vendor::Codex, Vendor::Cursor, Vendor::Agy] {
            assert!(!permission_modes(v).contains(&PermissionMode::BypassPermissions));
            assert!(permission_modes(v).contains(&PermissionMode::Default));
        }
    }
}
