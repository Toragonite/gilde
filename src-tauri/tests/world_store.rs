//! T-world-* (store side): atomic subscribe (T-world-subscribe), patch
//! coalescing, ≤ 2,000 ops → `Resync`, no removal of referenced rows
//! (T-world-noRemoveReferenced), "ended today" at midnight and on a time-zone
//! change (T-world-endedToday), `Compat{list}` (T-world-compatList, A10.1),
//! `Integrations{state}` (T-world-integrations, A11.6), lifecycle ops
//! (T-world-lifecycleOps, A12.1), spend priority (T-world-usagePriority,
//! A10.4), the transcript path (T-world-transcriptPath, A10.3), no
//! content in `session_meta` (F-03), and accounts published with no session
//! (A24.8).

#[path = "world_support.rs"]
mod support;

use std::sync::Arc;

use gilde_code_lib::contract::activity::{ActivityState, RegStatus};
use gilde_code_lib::contract::common::{AbsPath, CompatState, Notice, Vendor};
use gilde_code_lib::contract::compat::{CanaryResult, ClaudeCompat, CompatSource};
use gilde_code_lib::contract::ids::{ProcessKey, SessionUid};
use gilde_code_lib::contract::process::{Leftover, LeftoverReason, ProcRole, ProcessRecord};
use gilde_code_lib::contract::services::{
    EvidenceSink, LifecycleOp, SessionMetaFacts, SourceFacts, UsageFacts, UsageSource, WorldApi,
};
use gilde_code_lib::contract::session::UsageToday;
use gilde_code_lib::contract::store::{
    BootStatus, EntityKind, IntegrationsState, OrchestratorPresence, QuitPhase, StoreOp,
};
use gilde_code_lib::world::{MemoryMetaStore, RefSource};
use support::*;

fn compat(v: &str, conv: CompatState) -> ClaudeCompat {
    ClaudeCompat {
        version: v.to_string(),
        conversation: conv,
        pane_hosting: CompatState::Unverified,
        source: CompatSource::Shipped,
        static_canary: CanaryResult::NotRun,
        handshake_ok: None,
        below_minimum: false,
        note: None,
        features: Default::default(),
    }
}

fn ops(patches: &[gilde_code_lib::contract::store::StorePatch]) -> Vec<StoreOp> {
    patches.iter().flat_map(|p| p.ops.clone()).collect()
}

fn live_session(rig: &Rig, n: u32) -> SessionUid {
    let h = claude_hint(Some(&sid(n)), Some(6000 + n));
    rig.ev(&h, alive(6000 + n as i32));
    rig.ev(&h, registry(RegStatus::Idle, rig.now(), None));
    rig.uid_of(&sid(n))
}

#[test]
fn t_world_subscribe_is_atomic_and_revs_chain() {
    let rig = Rig::new();
    let a = live_session(&rig, 1);
    // Ops pending before the subscribe are in the snapshot, never in a patch
    // the new subscriber would see as a gap.
    let snap = rig.subscribe();
    assert!(snap.sessions.iter().any(|s| s.uid == a));
    let b = live_session(&rig, 2);
    let p = rig.pump();
    assert_eq!(p.first().unwrap().from_rev, snap.rev);
    assert!(rig.mirror.lock().as_ref().unwrap().sessions.contains_key(b.as_str()));
    // A second subscribe replaces the first channel and starts a fresh chain.
    live_session(&rig, 3);
    let snap2 = rig.subscribe();
    assert!(snap2.rev >= p.last().unwrap().to_rev);
    assert_eq!(snap2.sessions.len(), 3);
    live_session(&rig, 4);
    let p2 = rig.pump();
    assert_eq!(p2.first().unwrap().from_rev, snap2.rev);
}

#[test]
fn t_world_subscribe_under_concurrent_evidence() {
    let rig = Arc::new(Rig::new());
    let sink: Arc<dyn EvidenceSink> = rig.world.sink();
    let writer = {
        let rig = rig.clone();
        std::thread::spawn(move || {
            for n in 0..400u32 {
                let h = claude_hint(Some(&sid(10_000 + n)), Some(20_000 + n));
                sink.evidence(gilde_code_lib::contract::services::EvidenceUpdate {
                    hint: h.clone(),
                    facts: alive(20_000 + n as i32),
                    at: rig.now(),
                });
            }
        })
    };
    rig.subscribe();
    for _ in 0..20 {
        rig.pump();
        std::thread::yield_now();
    }
    writer.join().unwrap();
    rig.pump();
    let mirror = rig.mirror.lock();
    let m = mirror.as_ref().unwrap();
    let snap = rig.world.snapshot();
    assert_eq!(m.sessions.len(), snap.sessions.len(), "the mirror equals the store: no lost or duplicated patch");
    assert_eq!(m.rev, snap.rev);
}

#[test]
fn patches_coalesce_per_entity_and_overflow_is_a_resync() {
    let rig = Rig::new();
    rig.subscribe();
    let h = claude_hint(Some(&sid(5)), Some(6005));
    rig.ev(&h, alive(6005));
    for i in 0..50 {
        rig.ev(&h, registry(if i % 2 == 0 { RegStatus::Busy } else { RegStatus::Idle }, rig.now() + i, None));
    }
    let p = rig.pump();
    assert_eq!(p.len(), 1, "one patch per flush window");
    let session_ops = p[0]
        .ops
        .iter()
        .filter(|o| matches!(o, StoreOp::Upsert { entity } if matches!(entity.as_ref(), gilde_code_lib::contract::store::Entity::Session { .. })))
        .count();
    assert_eq!(session_ops, 1, "ops for one entity collapse to the last");
    // More than 2,000 ops in one window → Resync.
    for n in 0..2_100u32 {
        let h = claude_hint(Some(&sid(100_000 + n)), Some(30_000 + n));
        rig.ev(&h, alive(30_000 + n as i32));
    }
    rig.world.flush_now();
    let all = rig.patches.0.lock().clone();
    let last = all.last().unwrap();
    assert_eq!(last.ops, vec![StoreOp::Resync]);
    // The WebView resubscribes and gets everything.
    let snap = rig.subscribe();
    assert!(snap.sessions.len() >= 2_101);
}

#[test]
fn t_world_no_remove_referenced_and_ended_today() {
    let rig = Rig::new();
    rig.subscribe();
    let viewed = live_session(&rig, 21);
    let other = live_session(&rig, 22);
    rig.world.set_refs(RefSource::View, vec![viewed.clone()]);
    rig.pump();
    // Discovery says both processes are gone: the rows turn Ended, never removed.
    let t = rig.now();
    rig.ev(&claude_hint(Some(&sid(21)), Some(6021)), gone(t));
    rig.ev(&claude_hint(Some(&sid(22)), Some(6022)), gone(t));
    let p = rig.pump();
    assert!(!ops(&p).iter().any(|o| matches!(o, StoreOp::Remove { entity_kind: EntityKind::Session, .. })));
    assert_eq!(rig.state(&viewed), ActivityState::Ended);
    assert_eq!(rig.state(&other), ActivityState::Ended);
    // Local midnight: the unreferenced "ended today" row leaves the store; the
    // referenced one stays.
    rig.advance(14 * 3_600_000 + 1);
    let p = rig.pump();
    let removed: Vec<String> = ops(&p)
        .into_iter()
        .filter_map(|o| match o {
            StoreOp::Remove { entity_kind: EntityKind::Session, id } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(removed, vec![other.as_str().to_string()]);
    assert!(rig.snapshot_uids().contains(&viewed));
    // Releasing the reference removes it.
    rig.world.set_refs(RefSource::View, vec![]);
    let p = rig.pump();
    assert!(ops(&p).iter().any(|o| matches!(o, StoreOp::Remove { entity_kind: EntityKind::Session, id } if id == viewed.as_str())));
}

#[test]
fn t_world_ended_today_on_a_time_zone_change() {
    let rig = Rig::new();
    rig.subscribe();
    // The session ends at 10:00Z = 10:00 local (offset 0); at 23:00 it still
    // ended today.
    let uid = live_session(&rig, 31);
    rig.ev(&claude_hint(Some(&sid(31)), Some(6031)), gone(rig.now()));
    rig.pump();
    rig.advance(13 * 3_600_000);
    rig.pump();
    assert!(rig.snapshot_uids().contains(&uid));
    // The Mac moves to UTC+2: it is now 01:00 on the next local day, and the
    // session ended "yesterday" there.
    rig.clock.set_offset_ms(2 * 3_600_000);
    rig.world.time_zone_changed();
    rig.pump();
    assert!(!rig.snapshot_uids().contains(&uid), "ended yesterday in the new zone");
}

#[test]
fn t_world_compat_list_only_on_change() {
    let rig = Rig::new();
    rig.subscribe();
    let list = vec![compat("2.1.306", CompatState::Verified), compat("2.1.307", CompatState::Unverified)];
    rig.sink.compat(list.clone());
    let p = rig.pump();
    assert_eq!(ops(&p).iter().filter(|o| matches!(o, StoreOp::Compat { .. })).count(), 1);
    rig.sink.compat(list.clone());
    assert!(!ops(&rig.pump()).iter().any(|o| matches!(o, StoreOp::Compat { .. })), "same list: no op");
    let mut changed = list.clone();
    changed[1].conversation = CompatState::Failed;
    rig.sink.compat(changed.clone());
    let got = ops(&rig.pump());
    assert!(got.iter().any(|o| matches!(o, StoreOp::Compat { list } if list == &changed)));
    assert_eq!(rig.world.snapshot().compat, changed);
}

#[test]
fn t_world_integrations_only_on_change() {
    let rig = Rig::new();
    rig.subscribe();
    let present = IntegrationsState { orchestrator: OrchestratorPresence::Present };
    rig.sink.integrations(present.clone());
    assert_eq!(ops(&rig.pump()), vec![StoreOp::Integrations { state: present.clone() }]);
    rig.sink.integrations(present.clone());
    assert!(rig.pump().is_empty());
    rig.sink.integrations(IntegrationsState { orchestrator: OrchestratorPresence::PresentNotRunning });
    assert_eq!(rig.pump().len(), 1);
    assert_eq!(rig.world.snapshot().integrations.orchestrator, OrchestratorPresence::PresentNotRunning);
}

#[test]
fn t_world_lifecycle_ops() {
    let rig = Rig::new();
    let snap = rig.subscribe();
    let boot = BootStatus { spawn_gate_open: true, first_discovery_done: false, agents_detected: false, memory_only: None, agents: Vec::new(), agents_scanned_at: None, recovery: None };
    rig.sink.lifecycle(LifecycleOp::Boot(boot.clone()));
    rig.sink.lifecycle(LifecycleOp::Boot(boot.clone()));
    let notices = vec![Notice::new("notice.cantReadFolders"), Notice::new("notice.memoryOnly").param("reason", "diskFull")];
    rig.sink.lifecycle(LifecycleOp::Notices(notices.clone()));
    rig.sink.lifecycle(LifecycleOp::Quit { stopped: 1, total: 3, phase: QuitPhase::Stopping });
    let rec = ProcessRecord {
        key: ProcessKey::new(777, 1_792_700_000),
        pid: 777,
        start_sec: 1_792_700_000,
        vendor: Some(Vendor::Claude),
        role: ProcRole::Teammate,
        session_uid: None,
        gilde_owned: false,
        rss_bytes: 50_000_000,
        leftover: Some(Leftover { reason: LeftoverReason::TeammateOfGoneLead, since: rig.now(), held_scans: 2 }),
    };
    rig.sink.lifecycle(LifecycleOp::LeftoverUpsert(Box::new(rec.clone())));
    let p = rig.pump();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].from_rev, snap.rev);
    let o = &p[0].ops;
    assert!(o.contains(&StoreOp::Boot { status: boot }));
    assert_eq!(o.iter().filter(|x| matches!(x, StoreOp::Boot { .. })).count(), 1);
    assert!(o.contains(&StoreOp::Notices { notices: notices.clone() }));
    assert!(o.contains(&StoreOp::Quit { stopped: 1, total: 3, phase: QuitPhase::Stopping }));
    assert!(o.iter().any(|x| matches!(x, StoreOp::Upsert { entity } if matches!(entity.as_ref(), gilde_code_lib::contract::store::Entity::Leftover { value } if value == &rec))));
    // The leftover is one band-6 queue item (Q8).
    let q = rig.world.queue();
    assert_eq!(q.items.last().unwrap().key, "q_leftovers");
    assert_eq!(q.also, 1);
    rig.sink.lifecycle(LifecycleOp::LeftoverRemove(rec.key.clone()));
    let o = ops(&rig.pump());
    assert!(o.contains(&StoreOp::Remove { entity_kind: EntityKind::Leftover, id: rec.key.as_str().to_string() }));
    assert!(rig.world.queue().items.is_empty());
    let s = rig.world.snapshot();
    assert_eq!(s.notices, notices);
    assert!(s.leftovers.is_empty());
}

fn usage(source: UsageSource, day: &str, input: u64) -> SourceFacts {
    SourceFacts::Usage(UsageFacts {
        source,
        today: UsageToday { day: day.into(), input_tokens: Some(input), output_tokens: Some(1), cached_tokens: None, cost_micros: None },
    })
}

#[test]
fn t_world_usage_priority() {
    let rig = Rig::new();
    rig.subscribe();
    let uid = live_session(&rig, 41);
    let h = claude_hint(Some(&sid(41)), Some(6041));
    rig.pump();
    // No producer: unknown, never 0.
    assert_eq!(rig.session(&uid).usage_today, None);
    rig.ev(&h, usage(UsageSource::Transcript, "2026-10-23", 10));
    rig.pump();
    assert_eq!(rig.session(&uid).usage_today.unwrap().input_tokens, Some(10));
    rig.ev(&h, usage(UsageSource::Turns, "2026-10-23", 20));
    rig.pump();
    assert_eq!(rig.session(&uid).usage_today.unwrap().input_tokens, Some(20));
    rig.ev(&h, usage(UsageSource::Stream, "2026-10-23", 30));
    rig.ev(&h, usage(UsageSource::Transcript, "2026-10-23", 99));
    rig.pump();
    assert_eq!(rig.session(&uid).usage_today.unwrap().input_tokens, Some(30), "Stream > Turns > Transcript");
    // A report for an older day than the one held is ignored (a replay).
    rig.ev(&h, usage(UsageSource::Stream, "2026-10-22", 5));
    rig.pump();
    assert_eq!(rig.session(&uid).usage_today.unwrap().input_tokens, Some(30));
    // Running totals, not deltas: a duplicate is harmless.
    rig.ev(&h, usage(UsageSource::Stream, "2026-10-23", 30));
    rig.pump();
    assert_eq!(rig.session(&uid).usage_today.unwrap().input_tokens, Some(30));
    // Another session whose only high-priority report is from yesterday shows
    // today's lower-priority total.
    let uid2 = live_session(&rig, 42);
    let h2 = claude_hint(Some(&sid(42)), Some(6042));
    rig.ev(&h2, usage(UsageSource::Stream, "2026-10-22", 500));
    rig.ev(&h2, usage(UsageSource::Transcript, "2026-10-23", 7));
    rig.pump();
    assert_eq!(rig.session(&uid2).usage_today.unwrap().input_tokens, Some(7));
}

#[test]
fn t_world_transcript_path_from_meta_never_from_details() {
    let rig = Rig::new();
    let uid = live_session(&rig, 51);
    assert_eq!(rig.world.transcript(&uid), None);
    let path = "/Users/tester/.claude/projects/-Users-tester-proj/0000003300-0000-4000-8000-000000000001.jsonl";
    rig.ev(
        &claude_hint(Some(&sid(51)), Some(6051)),
        SourceFacts::Meta(Box::new(SessionMetaFacts { transcript_path: Some(AbsPath::parse(path).unwrap()), ..SessionMetaFacts::default() })),
    );
    rig.pump();
    let (p, v) = rig.world.transcript(&uid).expect("known");
    assert_eq!((p.as_str(), v), (path, Vendor::Claude));
    // The display string is `~/…` and is never what transcript() returns.
    let shown = rig.session(&uid).details.transcript_path.unwrap();
    assert!(shown.starts_with("~/"), "{shown}");
    assert_ne!(shown, path);
    assert_eq!(rig.world.transcript(&SessionUid::mint()), None);
}

#[test]
fn session_meta_holds_no_content() {
    let meta = Arc::new(MemoryMetaStore::default());
    let rig = Rig::with_meta(meta.clone());
    let uid = live_session(&rig, 61);
    rig.ev(
        &claude_hint(Some(&sid(61)), Some(6061)),
        SourceFacts::Meta(Box::new(SessionMetaFacts {
            title: Some("CANARY-7f3a-CONTENT title of the session".into()),
            model: Some("claude-opus".into()),
            ..SessionMetaFacts::default()
        })),
    );
    rig.pump();
    assert_eq!(rig.session(&uid).title.as_deref(), Some("CANARY-7f3a-CONTENT title of the session"));
    let rows = meta.rows();
    assert!(!rows.is_empty());
    assert!(!format!("{rows:?}").contains("CANARY-7f3a"), "titles are memory only");
    // A persisted uid survives a restart (same key → same uid).
    let rig2 = Rig::with_meta(meta.clone());
    assert_eq!(rig2.uid_of(&sid(61)), uid);
}

#[test]
fn projects_and_locations_from_cwd_by_longest_prefix() {
    use gilde_code_lib::contract::ids::{LocationId, ProjectId};
    use gilde_code_lib::contract::project::{LocationKind, ProjectKind};
    use gilde_code_lib::contract::services::LocationFacts;
    let rig = Rig::new();
    rig.subscribe();
    let pid = ProjectId::derive("/Users/tester/proj/.git");
    let main = LocationFacts {
        project_id: pid.clone(),
        project_root: AbsPath::parse("/Users/tester/proj").unwrap(),
        git: true,
        location_id: LocationId::derive("/Users/tester/proj"),
        location_path: AbsPath::parse("/Users/tester/proj").unwrap(),
        worktree: false,
        branch: Some("main".into()),
    };
    let wt = LocationFacts {
        location_id: LocationId::derive("/Users/tester/proj/.gilde/worktrees/fix-auth"),
        location_path: AbsPath::parse("/Users/tester/proj/.gilde/worktrees/fix-auth").unwrap(),
        worktree: true,
        branch: Some("gilde/fix-auth".into()),
        ..main.clone()
    };
    rig.sink.location(main.clone());
    rig.sink.location(wt.clone());
    let a = live_session(&rig, 71);
    rig.ev(&claude_hint(Some(&sid(71)), Some(6071)), meta_cwd("/Users/tester/proj/src"));
    let b = live_session(&rig, 72);
    rig.ev(&claude_hint(Some(&sid(72)), Some(6072)), meta_cwd("/Users/tester/proj/.gilde/worktrees/fix-auth/lib"));
    rig.pump();
    assert_eq!(rig.session(&a).location_id, Some(main.location_id.clone()));
    assert_eq!(rig.session(&b).location_id, Some(wt.location_id.clone()), "the longest prefix wins");
    assert_eq!(rig.session(&b).project_id, Some(pid.clone()));
    let snap = rig.world.snapshot();
    let p = snap.projects.iter().find(|p| p.id == pid).unwrap();
    assert_eq!((p.name.as_str(), p.kind, p.live_sessions, p.earlier, p.is_temp), ("proj", ProjectKind::Git, 2, false, false));
    // A17.7 through T11's `worktree::naming::is_gilde_created`: a `gilde/`
    // branch under a Gilde worktree root — the in-repo root counts whatever
    // the LP-40 flag says now, and so does the sibling `<repo>-wt/`.
    let l = snap.locations.iter().find(|l| l.id == wt.location_id).unwrap();
    assert_eq!((l.kind, l.worktree_name.as_deref(), l.gilde_created), (LocationKind::Worktree, Some("fix-auth"), true));
    let loc = |path: &str, branch: Option<&str>| LocationFacts {
        location_id: LocationId::derive(path),
        location_path: AbsPath::parse(path).unwrap(),
        worktree: true,
        branch: branch.map(str::to_string),
        ..main.clone()
    };
    let cases = [
        (loc("/Users/tester/proj-wt/fix-db", Some("gilde/fix-db")), true),
        // The branch matches but the path does not.
        (loc("/Users/tester/scratch/fix-db", Some("gilde/fix-db")), false),
        (loc("/Users/tester/proj-wtx/fix-db", Some("gilde/fix-db")), false),
        // The path matches but the branch does not, or HEAD is detached.
        (loc("/Users/tester/proj-wt/feature-x", Some("feature/x")), false),
        (loc("/Users/tester/proj-wt/detached", None), false),
    ];
    for (facts, _) in &cases {
        rig.sink.location(facts.clone());
    }
    rig.pump();
    let snap = rig.world.snapshot();
    for (facts, want) in &cases {
        let got = snap.locations.iter().find(|l| l.id == facts.location_id).unwrap().gilde_created;
        assert_eq!(got, *want, "{}", facts.location_path);
    }
    // Folds from ui.sidebarFolds.
    rig.world.set_project_folds(vec![pid.clone()]);
    rig.pump();
    assert!(rig.world.snapshot().projects.iter().find(|p| p.id == pid).unwrap().folded);
    // A scratch project under /private/tmp.
    let tmp = LocationFacts {
        project_id: ProjectId::derive("/private/tmp/scratch"),
        project_root: AbsPath::parse("/private/tmp/scratch").unwrap(),
        git: false,
        location_id: LocationId::derive("/private/tmp/scratch"),
        location_path: AbsPath::parse("/private/tmp/scratch").unwrap(),
        worktree: false,
        branch: None,
    };
    rig.sink.location(tmp.clone());
    let c = live_session(&rig, 73);
    rig.ev(&claude_hint(Some(&sid(73)), Some(6073)), meta_cwd("/private/tmp/scratch"));
    rig.pump();
    let snap = rig.world.snapshot();
    let tp = snap.projects.iter().find(|p| p.id == tmp.project_id).unwrap();
    assert!(tp.is_temp);
    assert_eq!(tp.kind, ProjectKind::Folder);
    let _ = c;
    // Dense order over the projects in user order.
    rig.world.set_project_order(vec![tmp.project_id.clone(), pid.clone()]);
    rig.pump();
    let snap = rig.world.snapshot();
    let order = |id: &ProjectId| snap.projects.iter().find(|p| &p.id == id).unwrap().order;
    assert_eq!((order(&tmp.project_id), order(&pid)), (0, 1));
}

#[test]
fn the_publisher_thread_flushes_and_shuts_down() {
    use gilde_code_lib::world::{World, WorldConfig};
    struct Chan(std::sync::mpsc::Sender<gilde_code_lib::contract::store::StorePatch>);
    impl gilde_code_lib::contract::services::StorePatchSink for Chan {
        fn send(&self, p: gilde_code_lib::contract::store::StorePatch) -> bool {
            self.0.send(p).is_ok()
        }
    }
    let world = World::new(WorldConfig::system(None, Arc::new(MemoryMetaStore::default())));
    let (tx, rx) = std::sync::mpsc::channel();
    let snap = world.subscribe(Box::new(Chan(tx)));
    world.start();
    world.start(); // idempotent
    let sink = world.sink();
    let now = gilde_code_lib::discovery::now_wall_ms();
    sink.evidence(gilde_code_lib::contract::services::EvidenceUpdate { hint: claude_hint(Some(&sid(81)), Some(6081)), facts: alive(6081), at: now });
    // Event-driven wait with a generous ceiling (no wall-clock budget, A10.7).
    let p = rx.recv_timeout(std::time::Duration::from_secs(20)).expect("a patch from the publisher thread");
    assert_eq!(p.from_rev, snap.rev);
    assert!(p.ops.iter().any(|o| matches!(o, StoreOp::Upsert { .. })));
    world.shutdown();
}

#[derive(Clone, Default)]
struct LogBuf(Arc<parking_lot::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn set_queued_drives_session_queued_and_stays_in_memory() {
    use gilde_code_lib::contract::activity::{StreamFacts, TurnPhase};
    use gilde_code_lib::contract::ids::ClientMsgId;
    use gilde_code_lib::contract::session::{ControlMode, QueuedMessage};
    let canary = "CANARY-7f3a-CONTENT queued message text";
    let logs = LogBuf::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let meta = Arc::new(MemoryMetaStore::default());
    tracing::subscriber::with_default(subscriber, || {
        let rig = Rig::with_meta(meta.clone());
        rig.subscribe();
        let uid = rig.world.bind_spawn("sp-queued", None, Vendor::Claude, Some(account()));
        let h = spawn_hint("sp-queued", Some(&sid(91)), Some(6091));
        let t = rig.now();
        rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t))));
        rig.ev(&h, alive(6091));
        rig.ev(&h, SourceFacts::Stream(StreamFacts { phase: TurnPhase::Running, phase_since: t, pending: vec![], last_turn_start_at: Some(t), last_turn_end: None, last_frame_at: t }));
        rig.pump();
        assert!(rig.session(&uid).queued.is_empty());
        let m = |id: &str, at: i64, text: &str| QueuedMessage { client_msg_id: ClientMsgId::parse(id).unwrap(), text: text.into(), queued_at: at };
        let a = "11111111-1111-4111-8111-111111111111";
        let b = "22222222-2222-4222-8222-222222222222";
        // The full list, oldest first; a repeated id is kept once.
        rig.world.set_queued(&uid, vec![m(b, t + 20, "second"), m(a, t + 10, canary), m(a, t + 10, canary)]);
        let p = rig.pump();
        let upserted = p.iter().flat_map(|x| x.ops.clone()).any(|o| matches!(o, StoreOp::Upsert { entity } if matches!(entity.as_ref(), gilde_code_lib::contract::store::Entity::Session { value } if value.uid == uid && value.queued.len() == 2)));
        assert!(upserted, "a session upsert carries the queue");
        let q = rig.session(&uid).queued;
        assert_eq!(q.iter().map(|x| x.client_msg_id.as_str()).collect::<Vec<_>>(), vec![a, b]);
        assert_eq!(q[0].text, canary);
        // An empty list clears it.
        rig.world.set_queued(&uid, vec![]);
        rig.pump();
        assert!(rig.session(&uid).queued.is_empty());
        // A queued message does not outlive the process.
        rig.world.set_queued(&uid, vec![m(a, t + 30, canary)]);
        rig.pump();
        assert_eq!(rig.session(&uid).queued.len(), 1);
        rig.ev(&h, exited(rig.now(), Some(0), None));
        rig.pump();
        assert!(rig.session(&uid).queued.is_empty());
        // Unknown uids are ignored.
        rig.world.set_queued(&SessionUid::mint(), vec![m(b, t, canary)]);
        rig.pump();
    });
    let rows = format!("{:?}", meta.rows());
    assert!(!rows.is_empty() && !rows.contains("CANARY-7f3a"), "never persisted");
    let logged = String::from_utf8_lossy(&logs.0.lock()).to_string();
    assert!(!logged.contains("CANARY-7f3a"), "never logged");
}

#[test]
fn world_never_logs() {
    // World has no log call at all, so no content can reach a log from it.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/world");
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let src = std::fs::read_to_string(&p).unwrap();
                for bad in ["tracing::", "log::", "println!", "eprintln!", "dbg!"] {
                    assert!(!src.contains(bad), "{} contains `{bad}`", p.display());
                }
            }
        }
    }
}

#[test]
fn set_details_extra_replaces_clears_and_is_not_persisted() {
    use gilde_code_lib::contract::activity::{EvidenceLine, StreamFacts, TurnPhase};
    use gilde_code_lib::contract::session::ControlMode;
    use gilde_code_lib::world::DetailsExtra;
    let line = |k: &str| EvidenceLine { key: k.into(), params: serde_json::Map::new() };
    let meta = Arc::new(MemoryMetaStore::default());
    let rig = Rig::with_meta(meta.clone());
    rig.subscribe();
    let uid = rig.world.bind_spawn("sp-extra", None, Vendor::Claude, Some(account()));
    let h = spawn_hint("sp-extra", Some(&sid(95)), Some(6095));
    let t = rig.now();
    rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t))));
    rig.ev(&h, alive(6095));
    rig.ev(&h, SourceFacts::Stream(StreamFacts { phase: TurnPhase::Running, phase_since: t, pending: vec![], last_turn_start_at: Some(t), last_turn_end: None, last_frame_at: t }));
    rig.pump();
    // First call: all three facts.
    rig.world.set_details_extra(&uid, DetailsExtra {
        approval_channel_unconfirmed: Some("Bash".into()),
        version_note: Some("CANARY-7f3a spawned 2.1.306, init reports 2.1.307".into()),
        lines: vec![line("convo.notReading"), line("convo.unanswerable")],
    });
    let p = rig.pump();
    assert!(!p.is_empty(), "a session upsert");
    let s = rig.session(&uid);
    assert_eq!(s.details.approval_channel_unconfirmed.as_deref(), Some("Bash"));
    assert_eq!(s.details.version_note.as_deref(), Some("CANARY-7f3a spawned 2.1.306, init reports 2.1.307"));
    assert_eq!(s.activity.evidence.as_ref().map(|e| e.key.as_str()), Some("convo.notReading"));
    // Full replacement: a field absent from the next call is cleared.
    rig.world.set_details_extra(&uid, DetailsExtra { approval_channel_unconfirmed: None, version_note: Some("note 2".into()), lines: vec![line("convo.unanswerable")] });
    rig.pump();
    let s = rig.session(&uid);
    assert_eq!(s.details.approval_channel_unconfirmed, None);
    assert_eq!(s.details.version_note.as_deref(), Some("note 2"));
    assert_eq!(s.activity.evidence.as_ref().map(|e| e.key.as_str()), Some("convo.unanswerable"));
    // Clearing with None and an empty list.
    rig.world.set_details_extra(&uid, DetailsExtra::default());
    rig.pump();
    let s = rig.session(&uid);
    assert_eq!((s.details.approval_channel_unconfirmed, s.details.version_note, s.activity.evidence), (None, None, None));
    // World's own "Can't read updates" line comes first.
    rig.world.set_details_extra(&uid, DetailsExtra { lines: vec![line("convo.notReading")], ..DetailsExtra::default() });
    rig.ev(&h, SourceFacts::SourceDenied(true));
    rig.pump();
    assert_eq!(rig.session(&uid).activity.evidence.unwrap().key, "evidence.cantRead");
    // A session line does not outlive the process.
    rig.ev(&h, SourceFacts::SourceDenied(false));
    rig.ev(&h, exited(rig.now(), Some(0), None));
    rig.pump();
    assert_eq!(rig.session(&uid).activity.evidence, None);
    assert!(!format!("{:?}", meta.rows()).contains("CANARY-7f3a"), "never persisted");
}

fn claude_account(label: &str) -> gilde_code_lib::contract::account::Account {
    use gilde_code_lib::contract::account::{Account, AccountSource, SignIn};
    Account {
        id: account(),
        vendor: Vendor::Claude,
        config_dir: None,
        label: label.to_string(),
        is_default: true,
        source: AccountSource::Ambient,
        usable_for_new: true,
        sign_in: SignIn::SignedIn,
        quota: None,
        live_sessions: 0,
    }
}

fn account_upserts(patches: &[gilde_code_lib::contract::store::StorePatch]) -> Vec<gilde_code_lib::contract::account::Account> {
    use gilde_code_lib::contract::store::Entity;
    ops(patches)
        .into_iter()
        .filter_map(|o| match o {
            StoreOp::Upsert { entity } => match *entity {
                Entity::Account { value } => Some(value),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// A24.8 (T-wiring bug): an account reaches the store while no session
/// exists (a fresh install), an unchanged account changes nothing, and a
/// changed one is published again.
#[test]
fn t_world_accounts_publish_without_sessions() {
    let rig = Rig::new();
    let snap = rig.subscribe();
    assert!(snap.sessions.is_empty() && snap.accounts.is_empty());

    rig.sink.account(claude_account("Default"));
    let up = account_upserts(&rig.pump());
    assert_eq!(up.len(), 1, "one Upsert{{Account}} with no session in the store");
    assert_eq!((up[0].label.as_str(), up[0].live_sessions), ("Default", 0));
    assert_eq!(rig.world.snapshot().accounts, vec![claude_account("Default")]);

    let rev = rig.world.rev();
    rig.sink.account(claude_account("Default"));
    let p = rig.pump();
    assert!(account_upserts(&p).is_empty(), "an unchanged account is no op");
    assert_eq!(rig.world.rev(), rev, "nothing published");

    rig.sink.account(claude_account("Work"));
    let up = account_upserts(&rig.pump());
    assert_eq!(up.iter().map(|a| a.label.as_str()).collect::<Vec<_>>(), vec!["Work"]);
    assert_eq!(rig.world.snapshot().accounts[0].label, "Work");
}

/// A24.8: the session-driven updates still work — a live session counts on
/// its account, its end uncounts it.
#[test]
fn t_world_account_live_counts_follow_sessions() {
    let rig = Rig::new();
    rig.subscribe();
    rig.sink.account(claude_account("Default"));
    rig.pump();
    let h = claude_hint(Some(&sid(81)), Some(6081));
    rig.ev(&h, alive(6081));
    rig.ev(&h, registry(RegStatus::Idle, rig.now(), None));
    let up = account_upserts(&rig.pump());
    assert_eq!(up.last().map(|a| a.live_sessions), Some(1));
    assert_eq!(rig.world.snapshot().accounts[0].live_sessions, 1);
    rig.ev(&h, exited(rig.now(), Some(0), None));
    let up = account_upserts(&rig.pump());
    assert_eq!(up.last().map(|a| a.live_sessions), Some(0));
    assert_eq!(rig.world.snapshot().accounts[0].live_sessions, 0);
}
