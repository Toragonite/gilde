//! Continue, copy, attach and Continue all (contract §6.5) — never resume a
//! live conversation in place.
//!
//! Owner: T10.
//!
//! Rule 1 (re-check inside the command): after taking World's per-
//! `IdentityKey` lock, a fresh liveness check runs for the vendor id
//! (registry rows of every watched config dir + an argv scan). A live process
//! anywhere refuses a Resume with `SESSION_LIVE_ELSEWHERE` (the UI offers the
//! copy) — **except** this uid's own crash leftover (`GildeCrashLeftover`,
//! identity-fenced by the boot sweep): Continue waits for it to exit (≤ 12 s,
//! the row says `home.stoppingPrevious`), then resumes; a copy of one's own
//! leftover is never offered [F-12]. The lock is held through the spawn until
//! the process is registered (§6.0), so two Continues of one conversation can
//! never both spawn; a second one finds the first's actor and answers
//! `CONFLICT{alreadyOwned}`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::contract::activity::EvidenceLine;
use crate::contract::common::{AbsPath, PermissionMode, Vendor};
use crate::contract::error::IpcError;
use crate::contract::ids::{ClientRequestId, QuitRecordId, SessionUid};
use crate::contract::ipc::{ContinueAllRes, ContinueFailure, ContinueHow, SessionContinueReq};
use crate::contract::process::LeftoverReason;
use crate::contract::services::IdentityKey;
use crate::contract::session::{Control, ControlMode};
use crate::convo::frames::AbandonedAsk;
use crate::discovery::procs::{Alive, ProcSource};
use crate::store::models::QuitItemRow;
use crate::store::Db;

use super::manager::{DriverDetails, LiveProcess, Liveness, QuitItems, SessionsManager};
use super::options::permission_modes;
use super::start::{mode_allowed, Launch, LaunchRequest};

fn mode_of_item(s: &str) -> Option<ControlMode> {
    match s {
        "conversation" => Some(ControlMode::Conversation),
        "terminal" => Some(ControlMode::Terminal),
        "turnBased" => Some(ControlMode::TurnBased),
        _ => None,
    }
}

fn permission_of_item(s: &str) -> Option<PermissionMode> {
    [
        PermissionMode::Default,
        PermissionMode::AcceptEdits,
        PermissionMode::Plan,
        PermissionMode::DontAsk,
        PermissionMode::Auto,
        PermissionMode::BypassPermissions,
    ]
    .into_iter()
    .find(|m| m.as_cli() == s)
}

/// The vendor's default mode for a continue (§6.5: Conversation default for
/// claude when allowed — the launch falls back to Terminal otherwise).
fn default_mode(vendor: Vendor) -> ControlMode {
    match vendor {
        Vendor::Claude => ControlMode::Conversation,
        _ => ControlMode::TurnBased,
    }
}

/// §3.2: a cursor/agy id must exist on disk before Gilde resumes an id it did
/// not create in this run (an unknown id silently starts a new chat [R2 C7/C8]).
/// `stat` only, nothing is opened.
pub fn id_on_disk(vendor: Vendor, home: &Path, cwd: Option<&Path>, id: &str) -> bool {
    match vendor {
        Vendor::Cursor => {
            let Some(cwd) = cwd.and_then(|c| c.to_str()) else { return false };
            if !(8..=64).contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                return false;
            }
            use md5::Digest as _;
            let digest = hex::encode(md5::Md5::digest(cwd.as_bytes()));
            std::fs::symlink_metadata(home.join(".cursor/chats").join(digest).join(id).join("meta.json")).is_ok()
        }
        Vendor::Agy => {
            if id.len() != 36 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                return false;
            }
            std::fs::symlink_metadata(home.join(".gemini/antigravity-cli/conversations").join(format!("{id}.db"))).is_ok()
        }
        Vendor::Claude | Vendor::Codex => true,
    }
}

impl SessionsManager {
    /// `session_continue` (§4.2.2, §6.5). `item` = the quit-record row when
    /// Continue all resumes it (its mode, model and permission mode).
    pub(crate) async fn continue_one(&self, req: SessionContinueReq, item: Option<QuitItemRow>) -> Result<SessionUid, IpcError> {
        self.spawn_gate().await?;
        let session = self.deps.world.session(&req.uid).ok_or_else(|| IpcError::not_found("session"))?;
        let vendor = session.vendor;
        // (b) value rules.
        if let Some(pm) = req.permission_mode {
            // Bypass is chosen only in the New-session sheet (§4.2.2, PM-03).
            if pm == PermissionMode::BypassPermissions || !permission_modes(vendor).contains(&pm) {
                return Err(IpcError::invalid_argument("permissionMode", "range"));
            }
        }
        let model = req.model.clone().filter(|m| !m.is_empty());
        if let Some(m) = &model {
            if !self.model_known(vendor, m) {
                return Err(IpcError::invalid_argument("model", "range"));
            }
        }
        if let Some(cm) = req.control_mode {
            if !mode_allowed(vendor, cm) {
                return Err(IpcError::unsupported("continueOption"));
            }
        }
        // (c) existence.
        let target_account = match &req.how {
            ContinueHow::OtherAccount { account_id } => {
                let a = self
                    .deps
                    .accounts
                    .all()
                    .await
                    .into_iter()
                    .find(|a| &a.id == account_id)
                    .ok_or_else(|| IpcError::not_found("account"))?;
                Some(a)
            }
            _ => None,
        };
        // (d) capability: what this session can continue as.
        let claude_only = matches!(req.how, ContinueHow::Fork | ContinueHow::Attach | ContinueHow::OtherAccount { .. });
        if claude_only && vendor != Vendor::Claude {
            return Err(IpcError::unsupported("continueOption"));
        }
        if let Some(a) = &target_account {
            if a.vendor != vendor || !a.usable_for_new || Some(&a.id) == session.account_id.as_ref() {
                return Err(IpcError::unsupported("continueOption"));
            }
        }
        let vsid = session
            .vendor_session_id
            .clone()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| IpcError::refused("notResumable"))?;
        let own = self.actor(&req.uid);
        let own_live = match &own {
            Some(a) => a.is_live().await,
            None => false,
        };
        let in_gilde = matches!(session.control, Control::InGilde { .. });
        match &req.how {
            ContinueHow::OtherAccount { .. } => {
                // §6.5: Gilde first stops its own live process, then copies.
                if own_live {
                    if let Some(a) = &own {
                        a.stop(crate::contract::activity::StopCause::User).await?;
                    }
                } else if in_gilde {
                    return Err(IpcError::conflict("alreadyOwned"));
                }
            }
            _ => {
                if own_live || in_gilde {
                    return Err(IpcError::conflict("alreadyOwned"));
                }
            }
        }

        let account = match target_account.clone() {
            Some(a) => a,
            None => {
                let id = session.account_id.clone().ok_or_else(|| IpcError::refused("notResumable"))?;
                self.deps
                    .accounts
                    .all()
                    .await
                    .into_iter()
                    .find(|a| a.id == id)
                    .ok_or_else(|| IpcError::not_found("account"))?
            }
        };
        let cwd = session.cwd.clone().ok_or_else(|| IpcError::refused("notResumable"))?;

        // Rule 1: the identity lock from the re-check through the spawn.
        let key = IdentityKey { vendor, account_id: session.account_id.clone(), vendor_session_id: vsid.clone() };
        let _lock = self.deps.world.identity_lock(key).await;
        // A Continue that waited on the lock finds the first one's session.
        if let Some(a) = self.actor(&req.uid) {
            if a.is_live().await && !matches!(req.how, ContinueHow::OtherAccount { .. }) {
                return Err(IpcError::conflict("alreadyOwned"));
            }
        }
        let live = self.deps.liveness.find(vendor, &vsid).await?;
        let (mine, others) = self.split_own_leftovers(&req.uid, live);
        if !mine.is_empty() {
            self.wait_own_leftovers(&req.uid, &mine).await?;
        }
        match &req.how {
            ContinueHow::Resume => {
                if others.iter().any(|p| p.background) {
                    return Err(IpcError::refused("heldByBackground"));
                }
                if !others.is_empty() {
                    return Err(IpcError::session_live_elsewhere());
                }
                if matches!(vendor, Vendor::Cursor | Vendor::Agy) && !self.created_here(&vsid) {
                    let home = self.deps.env.home().ok_or_else(|| IpcError::refused("idNotOnDisk"))?;
                    if !id_on_disk(vendor, Path::new(home.as_str()), Some(Path::new(cwd.as_str())), &vsid) {
                        return Err(IpcError::refused("idNotOnDisk"));
                    }
                }
            }
            ContinueHow::Attach => {
                if !others.iter().any(|p| p.background) {
                    return Err(IpcError::unsupported("continueOption"));
                }
            }
            ContinueHow::Fork | ContinueHow::OtherAccount { .. } => {}
        }

        let previous = own.as_ref().map(|a| a.info());
        let team = item.as_ref().is_some_and(|i| i.team_lead)
            || previous.as_ref().is_some_and(|p| p.hosted_team || p.launch.as_ref().is_some_and(|l| l.team));
        let mode = match &req.how {
            ContinueHow::Attach => ControlMode::Terminal,
            _ if team => ControlMode::Terminal,
            _ => req
                .control_mode
                .or_else(|| item.as_ref().and_then(|i| mode_of_item(&i.control_mode)))
                .or_else(|| previous.as_ref().map(|p| p.mode))
                .or(session.details.control_mode)
                .filter(|m| mode_allowed(vendor, *m))
                .unwrap_or_else(|| default_mode(vendor)),
        };
        let permission_mode = req
            .permission_mode
            .or_else(|| item.as_ref().and_then(|i| i.permission_mode.as_deref()).and_then(permission_of_item))
            .or(session.permission_mode)
            .filter(|m| *m != PermissionMode::BypassPermissions || previous.as_ref().is_some_and(|p| p.launched_bypass))
            .unwrap_or(PermissionMode::Default);
        let kind = match &req.how {
            ContinueHow::Resume => Launch::Resume { id: vsid.clone() },
            ContinueHow::Fork => Launch::Fork { id: vsid.clone() },
            ContinueHow::Attach => Launch::Attach { id: vsid.clone() },
            ContinueHow::OtherAccount { .. } => {
                let (path, _) = self.deps.world.transcript(&req.uid).ok_or_else(|| IpcError::refused("notResumable"))?;
                Launch::OtherAccount { transcript: path }
            }
        };
        // OtherAccount is a copy by transcript path: Conversation only (§6.1.1).
        let mode = if matches!(kind, Launch::OtherAccount { .. }) { ControlMode::Conversation } else { mode };
        let new_uid = matches!(kind, Launch::Fork { .. } | Launch::OtherAccount { .. });
        let uid = if new_uid { SessionUid::mint() } else { req.uid.clone() };
        let launch = LaunchRequest {
            vendor,
            account,
            project_id: session.project_id.clone(),
            location_id: session.location_id.clone(),
            cwd,
            model: model
                .or_else(|| item.as_ref().and_then(|i| i.model.clone()))
                .or(session.model.clone())
                .filter(|m| !m.is_empty()),
            permission_mode,
            mode,
            team: team && matches!(kind, Launch::Resume { .. }),
            prompt: None,
            kind: kind.clone(),
            fallback_to_terminal: !matches!(kind, Launch::OtherAccount { .. }),
            // F-05 (A24.3): the transcript ends in a tool call nobody answered.
            abandoned: self
                .deps
                .binder
                .open_tool_use(&req.uid)
                .map(|o| AbandonedAsk { tool_use_id: o.tool_use_id, tool_name: o.tool_name }),
        };
        let started = self.launch(uid.clone(), launch, None).await?;
        if new_uid {
            // A24.2: World records (and persists) the copy's parent.
            self.deps.binder.set_parent(&started, &req.uid);
        }
        Ok(started)
    }

    /// Whether a vendor id belongs to a session this app run started.
    fn created_here(&self, vsid: &str) -> bool {
        self.actors.lock().values().any(|a| a.sink().hint().vendor_session_id.as_deref() == Some(vsid))
    }

    /// Split the live processes into this uid's own crash leftovers and
    /// everything else. A43.1: the boot sweep's own record
    /// (`reaper::is_own_crash_leftover`) decides at once — before the health
    /// scan published anything — and World's `GildeCrashLeftover` records
    /// still count; the debounced health record is never the only source.
    fn split_own_leftovers(&self, uid: &SessionUid, live: Vec<LiveProcess>) -> (Vec<LiveProcess>, Vec<LiveProcess>) {
        let leftovers = self.deps.world.snapshot().leftovers;
        let is_own = |p: &LiveProcess| {
            crate::supervise::reaper::is_own_crash_leftover(uid.as_str(), p.pid, p.start_sec)
                || leftovers.iter().any(|r| {
                    r.session_uid.as_ref() == Some(uid)
                        && r.leftover.as_ref().is_some_and(|l| l.reason == LeftoverReason::GildeCrashLeftover)
                        && r.pid == p.pid
                        && (r.start_sec - p.start_sec).abs() <= 1
                })
        };
        live.into_iter().partition(is_own)
    }

    /// §6.5 rule 1 [F-12]: wait ≤ 12 s for this uid's own leftovers to exit
    /// (the undo window's SIGTERM plus the CLI's own finish); the row says
    /// `home.stoppingPrevious` meanwhile. Still alive → `NOT_READY` (retry
    /// later); never a copy of one's own leftover.
    async fn wait_own_leftovers(&self, uid: &SessionUid, mine: &[LiveProcess]) -> Result<(), IpcError> {
        self.deps.binder.set_details_extra(
            uid,
            DriverDetails { lines: vec![EvidenceLine::key("home.stoppingPrevious")], ..DriverDetails::default() },
        );
        let deadline = tokio::time::Instant::now() + self.deps.timing.own_leftover_wait;
        let gone = loop {
            if mine.iter().all(|p| !self.deps.liveness.alive(p.pid, p.start_sec)) {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(self.deps.timing.liveness_poll).await;
        };
        self.deps.binder.set_details_extra(uid, DriverDetails::default());
        if gone {
            Ok(())
        } else {
            Err(IpcError::not_ready("stoppingPrevious"))
        }
    }

    /// Continue all (§6.5 rule 4, `continue_all`): items in quit order, at most
    /// 3 spawns in flight, each with rule 1; a failure is listed for its item
    /// and never aborts the rest; teams resume the lead only (pane members are
    /// not session items of a quit record).
    pub(crate) async fn continue_record(&self, record: &QuitRecordId, uids: Option<Vec<SessionUid>>) -> Result<ContinueAllRes, IpcError> {
        let items = self.deps.quit_items.items(record).await?;
        // A38.9 (§6.5): `None` = every item; `Some(list)` = only those (an empty
        // list continues nothing).
        let wanted = uids;
        let items: Vec<QuitItemRow> = items
            .into_iter()
            .filter(|i| wanted.as_ref().is_none_or(|w| w.iter().any(|u| u.as_str() == i.uid)))
            .collect();
        let me = self.me.upgrade().ok_or_else(|| IpcError::internal("sessions-gone"))?;
        let sem = Arc::new(tokio::sync::Semaphore::new(self.deps.timing.continue_all_parallel.max(1)));
        let mut tasks = Vec::new();
        for item in items {
            let Ok(uid) = SessionUid::parse(item.uid.clone()) else { continue };
            let permit = sem.clone().acquire_owned().await.map_err(|_| IpcError::internal("continue-all"))?;
            let me = me.clone();
            let task_uid = uid.clone();
            let task = tokio::spawn(async move {
                let _permit = permit;
                let req = SessionContinueReq {
                    client_request_id: ClientRequestId::parse(uuid::Uuid::new_v4().to_string()).expect("a v4 uuid"),
                    uid: task_uid,
                    how: ContinueHow::Resume,
                    control_mode: None,
                    model: None,
                    permission_mode: None,
                };
                me.continue_one(req, Some(item)).await
            });
            tasks.push((uid, task));
        }
        let mut res = ContinueAllRes { started: Vec::new(), failed: Vec::new() };
        for (uid, task) in tasks {
            match task.await {
                Ok(Ok(started)) => res.started.push(started),
                Ok(Err(error)) => res.failed.push(ContinueFailure { uid, error }),
                Err(_) => res.failed.push(ContinueFailure { uid, error: IpcError::internal("continue-all-task") }),
            }
        }
        Ok(res)
    }
}

// ---------------------------------------------------------------------------
// production adapters
// ---------------------------------------------------------------------------

/// Whether `argv` (after argv[0]) names `id` as the session it serves:
/// `--resume <id>` / `--resume=<id>`, `--session-id <id>`, `--conversation
/// <id>`, `resume <id>` (codex), `attach <id>` (claude).
pub fn argv_names(argv: &[String], id: &str) -> bool {
    let rest = argv.get(1..).unwrap_or(&[]);
    rest.iter().enumerate().any(|(i, a)| {
        let next = rest.get(i + 1).map(String::as_str);
        match a.as_str() {
            "--resume" | "--session-id" | "--conversation" | "resume" | "attach" => next == Some(id),
            other => other.strip_prefix("--resume=").or_else(|| other.strip_prefix("--session-id=")) == Some(id),
        }
    })
}

/// The production [`Liveness`]: the Claude registry of every watched config
/// dir (`<cfg>/sessions/<pid>.json`, read through discovery's `safe_read`
/// with its four process checks) and an argv scan of the vendor CLIs that
/// are running (libproc), never cached.
pub struct ScanLiveness {
    config_dirs: Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>,
    procs: Arc<dyn ProcSource>,
}

impl ScanLiveness {
    pub fn new(config_dirs: Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>, procs: Arc<dyn ProcSource>) -> ScanLiveness {
        ScanLiveness { config_dirs, procs }
    }

    fn scan(&self, vendor: Vendor, id: &str) -> Vec<LiveProcess> {
        use crate::discovery::claude::registry::{list_registry, read_registry, verdict, RegKind, Verdict};
        use crate::discovery::safe_read::Root;
        let mut out: Vec<LiveProcess> = Vec::new();
        if vendor == Vendor::Claude {
            for dir in (self.config_dirs)() {
                let Ok(root) = Root::new(&dir.join("sessions")) else { continue };
                let Ok((files, _)) = list_registry(&root) else { continue };
                for f in files {
                    let Ok((parsed, _)) = read_registry(&root, &f.path, f.pid, std::time::Duration::from_millis(75)) else {
                        continue;
                    };
                    if !parsed.as_ref().is_ok_and(|r| r.session_id == id) {
                        continue;
                    }
                    if let Verdict::Live { record, start_sec, .. } = verdict(parsed, f.pid, self.procs.as_ref()) {
                        out.push(LiveProcess {
                            pid: record.pid,
                            start_sec,
                            background: record.kind == Some(RegKind::Bg),
                        });
                    }
                }
            }
        }
        for pid in self.procs.list_pids() {
            if out.iter().any(|p| p.pid == pid) || self.procs.alive(pid) != Alive::Yes {
                continue;
            }
            let Some(args) = self.procs.args(pid) else { continue };
            let (Some(exec), Some(argv)) = (args.exec_path.as_deref(), args.argv.as_deref()) else { continue };
            if crate::discovery::procs::classify(exec, Some(argv)).vendor != Some(vendor) || !argv_names(argv, id) {
                continue;
            }
            if let Some(info) = self.procs.bsd_info(pid) {
                out.push(LiveProcess { pid, start_sec: info.start_sec, background: false });
            }
        }
        out
    }
}

#[async_trait]
impl Liveness for ScanLiveness {
    async fn find(&self, vendor: Vendor, vendor_session_id: &str) -> Result<Vec<LiveProcess>, IpcError> {
        let me = ScanLiveness { config_dirs: self.config_dirs.clone(), procs: self.procs.clone() };
        let id = vendor_session_id.to_string();
        tokio::task::spawn_blocking(move || me.scan(vendor, &id))
            .await
            .map_err(|_| IpcError::internal("liveness"))
    }

    fn alive(&self, pid: i32, start_sec: i64) -> bool {
        self.procs.alive(pid) == Alive::Yes && self.procs.bsd_info(pid).is_some_and(|i| (i.start_sec - start_sec).abs() <= 1)
    }
}

/// The production [`QuitItems`] over `state.db` (T09's quit records).
pub struct DbQuitItems {
    db: Db,
}

impl DbQuitItems {
    pub fn new(db: Db) -> DbQuitItems {
        DbQuitItems { db }
    }
}

#[async_trait]
impl QuitItems for DbQuitItems {
    async fn items(&self, record: &QuitRecordId) -> Result<Vec<QuitItemRow>, IpcError> {
        let id = record.as_str().to_string();
        self.db
            .read(move |conn| crate::store::dao::quit_record::items(conn, &id))
            .await
            .map_err(|e| IpcError::from(&e))
    }
}

/// An `AbsPath` as a `Path`.
pub fn as_path(p: &AbsPath) -> &Path {
    Path::new(p.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn argv_names_every_resume_spelling_and_nothing_else() {
        let id = "0ff2360d-9ebf-4152-ad44-5fb0754c7823";
        assert!(argv_names(&v(&["claude", "--resume", id]), id));
        assert!(argv_names(&v(&["claude", "--permission-mode", "plan", "--resume", id, "--fork-session"]), id));
        assert!(argv_names(&v(&["claude", &format!("--resume={id}")]), id));
        assert!(argv_names(&v(&["claude", "--session-id", id]), id));
        assert!(argv_names(&v(&["codex", "resume", id]), id));
        assert!(argv_names(&v(&["agy", "--conversation", id]), id));
        assert!(argv_names(&v(&["claude", "attach", id]), id));
        assert!(!argv_names(&v(&["claude", "--", id]), id), "a prompt that equals the id is not a resume");
        assert!(!argv_names(&v(&["claude", "--resume"]), id));
        assert!(!argv_names(&v(&[id, "x"]), id), "argv[0] is never a resume");
    }

    #[test]
    fn cursor_and_agy_ids_are_checked_on_disk_by_stat() {
        let home = tempfile::tempdir().unwrap();
        let cwd = Path::new("/Users/someone/proj");
        let id = "0ff2360d-9ebf-4152-ad44-5fb0754c7823";
        assert!(!id_on_disk(Vendor::Cursor, home.path(), Some(cwd), id));
        assert!(!id_on_disk(Vendor::Agy, home.path(), None, id));
        use md5::Digest as _;
        let digest = hex::encode(md5::Md5::digest(cwd.to_str().unwrap().as_bytes()));
        let chat = home.path().join(".cursor/chats").join(digest).join(id);
        std::fs::create_dir_all(&chat).unwrap();
        std::fs::write(chat.join("meta.json"), b"{}").unwrap();
        assert!(id_on_disk(Vendor::Cursor, home.path(), Some(cwd), id));
        let conv = home.path().join(".gemini/antigravity-cli/conversations");
        std::fs::create_dir_all(&conv).unwrap();
        std::fs::write(conv.join(format!("{id}.db")), b"").unwrap();
        assert!(id_on_disk(Vendor::Agy, home.path(), None, id));
        assert!(!id_on_disk(Vendor::Agy, home.path(), None, "../../etc/passwd"));
        assert!(id_on_disk(Vendor::Codex, home.path(), None, id), "codex ids need no on-disk check");
    }

    #[test]
    fn quit_item_spellings_parse() {
        assert_eq!(mode_of_item("turnBased"), Some(ControlMode::TurnBased));
        assert_eq!(mode_of_item("nope"), None);
        assert_eq!(permission_of_item("acceptEdits"), Some(PermissionMode::AcceptEdits));
    }
}
