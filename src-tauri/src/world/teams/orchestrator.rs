//! The orchestrator mapping, read-only (§7.3.1).
//!
//! Owner: T02. Input: the sidecar's orchestrator observer snapshots
//! (`orch.snapshot { generation, rev, team }`, the vendored `TeamState`
//! shape), latest-wins by `(generation, rev)` per team. Mapping:
//! * a team per orchestrator team id (`TeamKey` from `"orchestrator"`, the
//!   account and the team id); no lead member (the lead is the session that
//!   runs the orchestrator);
//! * members carry their vendor (`gemini` → agy) and the `Orchestrator`
//!   backend; a member's session row is the live process of its running turn
//!   (matched by pid; the process carries the `CCO_TEAM_MEMBER` marker);
//!   without a session row its state comes from the snapshot (`running` →
//!   working, `blocked` → error, otherwise ready; `stopped`/`ended` → gone);
//! * board tasks are real cards (`source: Orchestrator`); dependencies only
//!   when the feed exposes them (it does not: none are invented);
//! * no in-flight count (unknown, P38); no actions in v1 (ux-ia Q18).
//!
//! Titles of board tasks are **[content]**: sanitized, memory only.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use crate::contract::activity::TeamFileFacts;
use crate::contract::common::{EpochMs, Vendor};
use crate::contract::display::TextCap;
use crate::contract::ids::{AccountId, MemberKey, RequestId, SessionUid, TaskKey, TeamKey};
use crate::contract::interaction::Decision;
use crate::contract::session::MemberRole;
use crate::contract::team::{
    ActOutcome, ApprovalRoute, Correlation, Durability, GoneReason, IdleReason, LeadBinding, Member, MemberBackend,
    MemberSpec, Presence, StopHow, Support, Task, TaskOp, TaskSource, TaskStatus, TaskWrite, TeamAdapter,
    TeamCapabilities, TeamEvent, TeamEventSink, TeamSource,
};
use crate::render::sanitize::sanitize_display_text;
use crate::world::teams::assembly::{TeamDetails, TeamHooks};

/// The vendored `TEAM_ID` shape (`tm` + base36).
fn valid_team_id(s: &str) -> bool {
    s.len() >= 11 && s.len() <= 22 && s.starts_with("tm") && s[2..].bytes().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
}

/// The descriptor of an orchestrator team (§7.1.2 table, column 3).
pub fn orchestrator_caps() -> TeamCapabilities {
    TeamCapabilities {
        observe_members: Support::Yes,
        observe_member_state: Support::Yes,
        observe_messages: Support::Yes,
        observe_in_flight: Support::Unknown,
        observe_tasks: Support::Yes,
        observe_approvals: Support::No,
        observe_questions: Support::No,
        act_send: Vec::new(),
        act_broadcast: Vec::new(),
        act_spawn: Vec::new(),
        act_stop: Vec::new(),
        act_answer_approval: ApprovalRoute::None,
        act_task_write: TaskWrite::None,
        correlation_report: Correlation::Unknown,
        durability_task_board: Durability::Durable,
        durability_message_log: Durability::Durable,
        fidelity_note: None,
    }
}

fn vendor_of(provider: &str) -> Option<Vendor> {
    match provider {
        "claude" => Some(Vendor::Claude),
        "codex" => Some(Vendor::Codex),
        "cursor" => Some(Vendor::Cursor),
        "gemini" | "agy" => Some(Vendor::Agy),
        _ => None,
    }
}

fn status_of(state: &str) -> Option<TaskStatus> {
    Some(match state {
        "open" => TaskStatus::Open,
        "claimed" | "blocked" | "review" => TaskStatus::InProgress,
        "done" => TaskStatus::Done,
        "cancelled" => TaskStatus::Deleted,
        _ => return None,
    })
}

fn str_of<'a>(v: &'a Value, k: &str, max: usize) -> Option<&'a str> {
    v.get(k)?.as_str().filter(|s| !s.is_empty() && s.len() <= max)
}

fn ms_of(v: &Value, k: &str) -> Option<EpochMs> {
    v.get(k)?.as_i64().filter(|x| *x > 0 && *x <= crate::contract::common::MAX_SAFE_INTEGER)
}

#[derive(Debug, Default)]
struct OTeam {
    key: Option<TeamKey>,
    version: (u64, u64),
    members: HashMap<String, (Member, Option<SessionUid>)>,
    member_facts: HashMap<MemberKey, TeamFileFacts>,
    tasks: HashMap<String, Task>,
    seen: Option<String>,
    ended: bool,
    in_flight_sent: bool,
}

/// The orchestrator adapter (v1, read-only).
pub struct OrchestratorAdapter {
    hooks: Arc<dyn TeamHooks>,
    sink: RwLock<Option<Arc<dyn TeamEventSink>>>,
    teams: Mutex<HashMap<String, OTeam>>,
}

impl std::fmt::Debug for OrchestratorAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestratorAdapter").field("teams", &self.teams.lock().len()).finish()
    }
}

impl OrchestratorAdapter {
    pub fn new(hooks: Arc<dyn TeamHooks>) -> OrchestratorAdapter {
        OrchestratorAdapter { hooks, sink: RwLock::new(None), teams: Mutex::new(HashMap::new()) }
    }

    /// Apply one `orch.snapshot` (latest-wins by `(generation, rev)`).
    /// Returns false when the snapshot was older or not a team.
    pub fn apply_snapshot(&self, account: Option<&AccountId>, generation: u64, rev: u64, team: &Value, now: EpochMs) -> bool {
        let Some(id) = str_of(team, "id", 32).filter(|s| valid_team_id(s)) else { return false };
        // Session lookups before taking the adapter lock (they lock World).
        let pids: Vec<(String, i32)> = team
            .get("members")
            .and_then(Value::as_array)
            .map(|ms| {
                ms.iter()
                    .filter_map(|m| {
                        let name = str_of(m, "name", 64)?.to_string();
                        let pid = m.get("turn")?.get("pid")?.as_i64()?;
                        i32::try_from(pid).ok().filter(|p| *p > 0).map(|p| (name, p))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let uids: HashMap<String, SessionUid> =
            pids.into_iter().filter_map(|(n, p)| self.hooks.uid_by_pid(p).map(|u| (n, u))).collect();
        let mut events = Vec::new();
        let details;
        {
            let mut teams = self.teams.lock();
            let t = teams.entry(id.to_string()).or_default();
            if t.key.is_some() && (generation, rev) <= t.version {
                return false;
            }
            t.version = (generation, rev);
            let key = t.key.get_or_insert_with(|| TeamKey::derive(TeamSource::Orchestrator.as_str(), account, id)).clone();
            let name = str_of(team, "name", 64)
                .map(|n| sanitize_display_text(n, TextCap::Chars(64)).text)
                .unwrap_or_else(|| id.to_string());
            if t.seen.as_deref() != Some(name.as_str()) {
                events.push(TeamEvent::TeamSeen {
                    key: key.clone(),
                    name: name.clone(),
                    raw_id: id.to_string(),
                    project: None,
                    lead: None,
                    binding: LeadBinding::None,
                });
                t.seen = Some(name);
            }
            if !t.in_flight_sent {
                events.push(TeamEvent::InFlight { team: key.clone(), count: None });
                t.in_flight_sent = true;
            }
            for m in team.get("members").and_then(Value::as_array).into_iter().flatten() {
                let Some(mname) = str_of(m, "name", 64) else { continue };
                let state = str_of(m, "state", 32).unwrap_or("idle");
                let mk = MemberKey::derive(&key, &format!("{mname}@{id}"), None);
                let prev = t.members.get(mname).cloned();
                let session_uid = uids.get(mname).cloned().or_else(|| prev.as_ref().and_then(|(_, u)| u.clone()));
                let presence = match state {
                    "stopped" => Presence::Gone { reason: GoneReason::ProcessExited, at: now },
                    "ended" => Presence::Gone { reason: GoneReason::Unknown, at: now },
                    _ => Presence::Present,
                };
                let presence = match (&presence, prev.as_ref().map(|(p, _)| &p.presence)) {
                    (Presence::Gone { reason, .. }, Some(Presence::Gone { at, .. })) => Presence::Gone { reason: *reason, at: *at },
                    _ => presence,
                };
                let member = Member {
                    key: mk.clone(),
                    team_key: key.clone(),
                    session_uid: session_uid.clone(),
                    name: sanitize_display_text(mname, TextCap::Chars(64)).text,
                    agent_id: Some(format!("{mname}@{id}")),
                    role: MemberRole::Member,
                    colour: None,
                    vendor: str_of(m, "provider", 16).and_then(vendor_of).unwrap_or(Vendor::Claude),
                    backend: MemberBackend::Orchestrator,
                    joined_at: None,
                    presence,
                    pane: None,
                    plan_mode_required: false,
                    current_task: None,
                };
                if prev.as_ref().map(|(p, _)| p) != Some(&member) {
                    events.push(TeamEvent::MemberUpserted(member.clone()));
                }
                // State for members without a session row.
                let facts = TeamFileFacts {
                    is_active: Some(state == "running" || state == "starting"),
                    in_roster: true,
                    permission_request: None,
                    last_idle: (state == "blocked").then_some((now, IdleReason::Failed)),
                };
                if t.member_facts.get(&mk) != Some(&facts) {
                    events.push(TeamEvent::MemberFacts { member: mk.clone(), facts: facts.clone() });
                    t.member_facts.insert(mk.clone(), facts);
                }
                t.members.insert(mname.to_string(), (member, session_uid));
            }
            for b in team.get("board").and_then(Value::as_array).into_iter().flatten() {
                let Some(tid) = str_of(b, "id", 16) else { continue };
                let Some(status) = str_of(b, "state", 16).and_then(status_of) else { continue };
                let owner = str_of(b, "assignee", 64).map(|a| MemberKey::derive(&key, &format!("{a}@{id}"), None));
                let updated = ms_of(b, "updatedAt").unwrap_or(now);
                let mut task = t.tasks.get(tid).cloned().unwrap_or_else(|| Task {
                    key: TaskKey::derive(&key, tid),
                    team_key: key.clone(),
                    native_id: Some(tid.to_string()),
                    title: None,
                    inferred_k: None,
                    status: TaskStatus::Open,
                    station: crate::contract::team::Station::ToDo,
                    owner: None,
                    blocked_by: Vec::new(),
                    blocks: Vec::new(),
                    created_at: ms_of(b, "createdAt"),
                    started_at: None,
                    completed_at: None,
                    first_seen_in_progress_at: None,
                    source: TaskSource::Orchestrator,
                    inferred: false,
                });
                let before = task.clone();
                task.title = str_of(b, "title", 4096).map(|s| sanitize_display_text(s, TextCap::Chars(200)).text);
                task.owner = owner;
                if task.status != status {
                    match status {
                        TaskStatus::InProgress => {
                            task.started_at.get_or_insert(updated);
                        }
                        TaskStatus::Done => {
                            task.completed_at.get_or_insert(updated);
                        }
                        _ => {}
                    }
                    task.status = status;
                }
                if task != before || !t.tasks.contains_key(tid) {
                    events.push(TeamEvent::TaskUpserted(task.clone()));
                }
                t.tasks.insert(tid.to_string(), task);
            }
            let status = str_of(team, "status", 16).unwrap_or("active");
            if status == "ended" && !t.ended {
                t.ended = true;
                events.push(TeamEvent::TeamEnded { key: key.clone(), at: ms_of(team, "endedAt").unwrap_or(now) });
            }
            details = (
                key,
                TeamDetails {
                    external_host: None,
                    pane_hosting: Some(crate::contract::team::PaneHostingState::NotApplicable),
                    roster_stale: false,
                    started_at: ms_of(team, "createdAt"),
                    config_mtime: None,
                    pane_tiles: Vec::new(),
                },
            );
        }
        self.hooks.team_details(&details.0, TeamSource::Orchestrator, details.1);
        let sink = self.sink.read().clone();
        if let Some(s) = sink {
            for e in events {
                s.emit(e);
            }
        }
        true
    }
}

impl TeamAdapter for OrchestratorAdapter {
    fn source(&self) -> TeamSource {
        TeamSource::Orchestrator
    }
    fn capabilities(&self, _team: &TeamKey) -> TeamCapabilities {
        orchestrator_caps()
    }
    fn subscribe(&self, sink: Arc<dyn TeamEventSink>) {
        *self.sink.write() = Some(sink);
    }
    fn send(&self, _team: &TeamKey, _to: &MemberKey, _text: &str) -> ActOutcome {
        ActOutcome::Unsupported { reason: "orchestratorTeam".into() }
    }
    fn broadcast(&self, _team: &TeamKey, _text: &str) -> ActOutcome {
        ActOutcome::Unsupported { reason: "orchestratorTeam".into() }
    }
    fn spawn(&self, _team: &TeamKey, _spec: &MemberSpec) -> ActOutcome {
        ActOutcome::Unsupported { reason: "orchestratorTeam".into() }
    }
    fn stop(&self, _team: &TeamKey, _member: &MemberKey, _how: StopHow) -> ActOutcome {
        ActOutcome::Unsupported { reason: "orchestratorTeam".into() }
    }
    fn answer_approval(&self, _team: &TeamKey, _req: &RequestId, _d: &Decision) -> ActOutcome {
        ActOutcome::Unsupported { reason: "orchestratorTeam".into() }
    }
    fn task_write(&self, _team: &TeamKey, _op: TaskOp) -> ActOutcome {
        ActOutcome::Unsupported { reason: "orchestratorTeam".into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_id_shape_and_mappings() {
        assert!(valid_team_id("tmabc123def4"));
        assert!(!valid_team_id("tm"));
        assert!(!valid_team_id("xx../etc/passwd"));
        assert_eq!(vendor_of("gemini"), Some(Vendor::Agy));
        assert_eq!(status_of("cancelled"), Some(TaskStatus::Deleted));
        assert_eq!(status_of("merged"), None);
        let c = orchestrator_caps();
        assert!(c.act_send.is_empty() && c.act_task_write == TaskWrite::None);
    }
}
