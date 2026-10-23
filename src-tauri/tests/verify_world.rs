//! G6 probes over World and the follow/notify services (contract §14.5,
//! A31.3): SP-8's transcript half (oversized transcript lines in a followed
//! conversation) and HP-6 (clock jumps and sleep). Real World, the real
//! ContentHub/Follower, the real NotifyService with fake sinks, the real
//! composition root for the wake path. Each probe prints one `PROBE` line.
//!
//! Owner: T33.

mod fixtures;
#[path = "fixtures/verify/mod.rs"]
mod verify;
#[path = "world_support.rs"]
mod support;

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::json;

use gilde_code_lib::contract::activity::{ActivityState, OpenToolUse, RegStatus, TranscriptFacts};
use gilde_code_lib::contract::common::AbsPath;
use gilde_code_lib::contract::content::{BlockKind, ConvoEnvelope, ConvoEvent, SystemNote};
use gilde_code_lib::contract::ids::{SessionUid, ViewId};
use gilde_code_lib::contract::services::{SessionMetaFacts, SourceFacts, WorldApi};
use gilde_code_lib::contract::store::{FocusTarget, VisibilityEvent};
use gilde_code_lib::follow::hub::{ContentHub, ConvoChannel, SendOutcome, WorldResolver};
use gilde_code_lib::follow::tail::{FollowConfig, Follower, LINE_CAP};
use gilde_code_lib::macos::dock::DockSink;
use gilde_code_lib::macos::sleep::{SystemEvent, SystemEvents};
use gilde_code_lib::macos::visibility::{VisibilityEmitter, VisibilityHub};
use gilde_code_lib::notify::policy::COALESCE_MS;
use gilde_code_lib::notify::{
    Authorization, ClickHandler, Clock, FocusEmitter, Notification, NotificationSink, NotifyConfig, NotifyDeps,
    NotifyService, Now, WorldLabels,
};
use gilde_code_lib::world::{ManualClock, WorldClock};

use support::*;
use verify::{with_wired, Probe};

const MIB: usize = 1024 * 1024;
const HOUR: u64 = 3_600_000;

// ---------------------------------------------------------------------------
// SP-8 (transcript half) — oversized transcript lines in a followed view
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Collect(Mutex<Vec<ConvoEnvelope>>);

impl ConvoChannel for Collect {
    fn send(&self, envelope: ConvoEnvelope) -> SendOutcome {
        self.0.lock().push(envelope);
        SendOutcome::Sent
    }
}

impl Collect {
    fn take(&self) -> Vec<ConvoEvent> {
        std::mem::take(&mut *self.0.lock()).into_iter().map(|e| e.event).collect()
    }
}

/// An assistant text row whose line (without `\n`) is exactly `len` bytes.
fn row_of_len(uuid: &str, len: usize) -> String {
    let make = |text: &str| {
        json!({"type": "assistant", "uuid": uuid, "timestamp": "2026-10-23T10:00:00.000Z",
               "message": {"id": format!("msg_{uuid}"), "role": "assistant", "content": [{"type": "text", "text": text}]}})
        .to_string()
    };
    let base = make("").len();
    assert!(len > base, "row too short");
    let line = make(&"y".repeat(len - base));
    assert_eq!(line.len(), len);
    line
}

fn small_row(uuid: &str, text: &str) -> String {
    json!({"type": "assistant", "uuid": uuid, "timestamp": "2026-10-23T10:00:01.000Z",
           "message": {"id": format!("msg_{uuid}"), "role": "assistant", "content": [{"type": "text", "text": text}]}})
    .to_string()
}

fn append(path: &std::path::Path, line: &str) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(line.as_bytes()).unwrap();
    f.write_all(b"\n").unwrap();
}

/// What a view got for one row: `RowTooLarge`, or the parsed block kind.
fn kind_of(ev: &[ConvoEvent], uuid: &str) -> String {
    let blocks: Vec<&gilde_code_lib::contract::content::Block> = ev
        .iter()
        .flat_map(|e| match e {
            ConvoEvent::Appended { block } => vec![block],
            ConvoEvent::Reset { blocks, .. } | ConvoEvent::Older { blocks, .. } => blocks.iter().collect(),
            _ => vec![],
        })
        .collect();
    let too_large = blocks.iter().filter(|b| matches!(b.kind, BlockKind::System { note: SystemNote::RowTooLarge, .. })).count();
    match blocks.iter().find(|b| b.id.as_str() == uuid) {
        Some(b) => match &b.kind {
            BlockKind::AssistantText { markdown, .. } => format!("parsed (AssistantText, {} B shown, truncated:{})", markdown.len(), b.truncated),
            BlockKind::System { note: SystemNote::RowTooLarge, .. } => "RowTooLarge".into(),
            other => format!("{other:?}").chars().take(40).collect(),
        },
        None if too_large > 0 => "RowTooLarge".into(),
        None => "nothing".into(),
    }
}

/// SP-8 (transcript): rows of 2 MiB, exactly the 4 MiB cap, cap + 1 and
/// 6 MiB appended to a followed Claude transcript (forward path), then a
/// normal row (the view stays alive), then a fresh view over the same file
/// (backward path).
#[test]
fn probe_sp8_transcript_rows_over_the_cap() {
    let mut p = Probe::new("SP-8");
    assert_eq!(LINE_CAP, 4 * MIB);
    let rig = Rig::new();
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    let path = dir.join("0d4c1f6e-6b8a-4c7e-9a51-3f2b8c9d0e11.jsonl");
    std::fs::write(&path, small_row("r-0", "hello") + "\n").unwrap();
    let h = claude_hint(Some("0d4c1f6e-6b8a-4c7e-9a51-3f2b8c9d0e11"), Some(7400));
    rig.ev(&h, alive(7400));
    rig.ev(&h, SourceFacts::Meta(Box::new(SessionMetaFacts { transcript_path: Some(AbsPath::parse(path.to_str().unwrap()).unwrap()), ..SessionMetaFacts::default() })));
    let uid = rig.uid_of("0d4c1f6e-6b8a-4c7e-9a51-3f2b8c9d0e11");
    let api: Arc<dyn WorldApi> = Arc::new(rig.world.clone());
    let follower = Follower::manual(FollowConfig {
        poll_interval: std::time::Duration::from_millis(0),
        min_read_interval: std::time::Duration::from_millis(0),
        missing_retry: std::time::Duration::from_millis(0),
        watch: false,
        ..FollowConfig::default()
    });
    let hub = ContentHub::new_manual(follower.clone(), Some(Arc::new(WorldResolver::new(api, AbsPath::parse(dir.to_str().unwrap()).ok()))));
    let view = Arc::new(Collect::default());
    hub.subscribe(&uid, &ViewId::parse("probe-1".to_string()).unwrap(), Some(50), view.clone()).expect("subscribed");
    view.take();
    let rows: Vec<(&str, String, usize)> = vec![
        ("2MiB", "r-2m".into(), 2 * MIB),
        ("=cap(4MiB)", "r-cap".into(), LINE_CAP),
        ("cap+1", "r-cap1".into(), LINE_CAP + 1),
        ("6MiB", "r-6m".into(), 6 * MIB),
    ];
    for (label, uuid, len) in &rows {
        append(&path, &row_of_len(uuid, *len));
        // A33.2: reading continues after every row — the next ordinary row
        // arrives in the same read.
        let next = format!("{uuid}-next");
        append(&path, &small_row(&next, "after"));
        follower.run_once();
        let ev = view.take();
        let observed = kind_of(&ev, uuid);
        let continued = ev.iter().any(|e| matches!(e, ConvoEvent::Appended { block } if block.id.as_str() == next));
        assert!(continued, "{label}: reading continues: {ev:?}");
        let variant = format!("transcript line {label} (forward)");
        match *label {
            "cap+1" | "6MiB" => {
                assert_eq!(observed, "RowTooLarge", "{label}: {ev:?}");
                p.saw(variant, format!("{observed}; the next row read"));
            }
            "=cap(4MiB)" => {
                assert!(observed.starts_with("parsed"), "exactly the cap is parsed: {observed}");
                p.saw(variant, format!("{observed}; the next row read"));
            }
            _ => {
                // A33.2: §14.5's "2 MiB" is an erratum — the cap is 4 MiB.
                assert!(observed.starts_with("parsed"), "under the cap is parsed: {observed}");
                p.saw(format!("{variant} (A33.2: under the 4 MiB cap)"), format!("{observed}; the next row read"));
            }
        }
    }
    // Alive: the next ordinary row arrives in the same view.
    append(&path, &small_row("r-after", "still here"));
    follower.run_once();
    let ev = view.take();
    assert!(ev.iter().any(|e| matches!(e, ConvoEvent::Appended { block } if block.id.as_str() == "r-after")), "{ev:?}");
    p.saw("next row", "Appended (view alive)");
    // A fresh view reads backwards from the end (≤ 4 MiB initial load).
    let fresh = Arc::new(Collect::default());
    hub.subscribe(&uid, &ViewId::parse("probe-2".to_string()).unwrap(), Some(50), fresh.clone()).expect("subscribed");
    let ev = fresh.take();
    let ConvoEvent::Reset { blocks, has_older, .. } = &ev[0] else { panic!("{ev:?}") };
    assert!(blocks.iter().any(|b| b.id.as_str() == "r-after"), "the newest row is shown");
    let tl = blocks.iter().filter(|b| matches!(b.kind, BlockKind::System { note: SystemNote::RowTooLarge, .. })).count();
    p.saw("fresh view (backward)", format!("Reset with {} blocks, {tl} RowTooLarge, has_older:{has_older}", blocks.len()));
    p.finish();
}

// ---------------------------------------------------------------------------
// HP-6 — clock jumps and sleep
// ---------------------------------------------------------------------------

/// The notify clock reads World's manual clock (one time line for both).
struct SharedClock(Arc<ManualClock>);
impl Clock for SharedClock {
    fn now(&self) -> Now {
        Now { up: self.0.up_ms(), wall: self.0.wall_ms() }
    }
}

#[derive(Default)]
struct Sink {
    delivered: Mutex<Vec<Notification>>,
}

#[async_trait]
impl NotificationSink for Sink {
    fn deliver(&self, n: &Notification) -> bool {
        self.delivered.lock().push(n.clone());
        true
    }
    fn withdraw(&self, _ids: &[String]) {}
    fn supports_withdrawal(&self) -> bool {
        true
    }
    fn authorization(&self) -> Authorization {
        Authorization::Granted
    }
    async fn request_authorization(&self) -> bool {
        true
    }
    fn set_click_handler(&self, _handler: ClickHandler) {}
}

#[derive(Default)]
struct Dock;
impl DockSink for Dock {
    fn set_badge(&self, _count: u32) {}
}

struct NoFocus;
impl FocusEmitter for NoFocus {
    fn focus_request(&self, _target: FocusTarget) {}
}

struct NoVis;
impl VisibilityEmitter for NoVis {
    fn emit(&self, _event: VisibilityEvent) {}
}

fn no_progress(rig: &Rig, uids: &[SessionUid]) -> usize {
    uids.iter().filter(|u| matches!(rig.state(u), ActivityState::NoProgress { .. })).count()
}

/// HP-6: with 10 working and 10 watched-approval sessions, (a) a system
/// sleep of 8 h (wall clock only) and (b) an 8 h jump of both clocks between
/// ticks: no No-progress storm after the sleep, at most one notification
/// plus one merged one for the jump; (c) a wake through the composition root
/// runs a discovery reconcile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_hp6_clock_jumps_and_sleep() {
    let mut p = Probe::new("HP-6");
    let rig = Rig::new();
    rig.subscribe();
    let sink = Arc::new(Sink::default());
    let system = SystemEvents::new();
    let visibility = VisibilityHub::new(Arc::new(NoVis));
    visibility.follow(&system);
    let api: Arc<dyn WorldApi> = Arc::new(rig.world.clone());
    let service = NotifyService::start(
        NotifyDeps {
            sink: sink.clone(),
            dock: Arc::new(Dock),
            visibility,
            system: system.clone(),
            labels: Arc::new(WorldLabels::new(api)),
            focus: Arc::new(NoFocus),
            clock: Arc::new(SharedClock(rig.clock.clone())),
        },
        NotifyConfig { launch: Some(SharedClock(rig.clock.clone()).now()) },
    );
    service.boot_settled();
    rig.world.set_notify(service.clone());
    // Past the launch grace.
    rig.advance(5 * 60_000);
    let now = rig.now();
    let mut working = Vec::new();
    let mut waiting = Vec::new();
    for i in 0..10u32 {
        let h = claude_hint(Some(&sid(0x6000 + i)), Some(7600 + i));
        rig.ev(&h, alive(7600 + i as i32));
        rig.ev(&h, registry(RegStatus::Busy, now, None));
        rig.ev(&h, SourceFacts::Transcript(transcript(now, Some(now - 5_000), None)));
        working.push(rig.uid_of(&sid(0x6000 + i)));
        let h = claude_hint(Some(&sid(0x6100 + i)), Some(7700 + i));
        rig.ev(&h, alive(7700 + i as i32));
        rig.ev(&h, registry(RegStatus::Waiting, now, Some("permission prompt")));
        rig.ev(
            &h,
            SourceFacts::Transcript(TranscriptFacts {
                open_tool_use: Some(OpenToolUse { tool_use_id: format!("toolu_{i}"), tool_name: "Bash".into(), at: now, is_question: false }),
                ..transcript(now, Some(now - 5_000), None)
            }),
        );
        waiting.push(rig.uid_of(&sid(0x6100 + i)));
    }
    rig.pump();
    service.settle().await;
    rig.advance(COALESCE_MS + 1_000);
    rig.pump();
    service.poke();
    service.settle().await;
    assert!(working.iter().all(|u| matches!(rig.state(u), ActivityState::Working)), "working before");
    let before = sink.delivered.lock().len();
    p.saw("setup", format!("10 working, 10 watched approvals, {before} notification(s) delivered"));

    // (a) A system sleep of 8 h: the wall clock moves, the uptime does not.
    system.publish(SystemEvent::WillSleep);
    rig.clock.sleep(8 * HOUR);
    system.publish(SystemEvent::DidWake);
    rig.pump();
    service.poke();
    service.settle().await;
    rig.advance(COALESCE_MS + 1_000);
    rig.pump();
    service.poke();
    service.settle().await;
    let np = no_progress(&rig, &working);
    let after_sleep = sink.delivered.lock().len() - before;
    assert_eq!(np, 0, "no No-progress after a sleep");
    assert!(after_sleep <= 2, "no notification burst after a sleep: {after_sleep}");
    p.saw("sleep 8h (wall only)", format!("NoProgress {np}/10; {after_sleep} new notification(s)"));

    // (b) Both clocks jump 8 h between two ticks (no tick in between).
    let base = sink.delivered.lock().len();
    rig.advance(8 * HOUR);
    rig.pump();
    service.poke();
    service.settle().await;
    let at_jump = sink.delivered.lock().len() - base;
    rig.advance(COALESCE_MS + 1_000);
    rig.pump();
    service.poke();
    service.settle().await;
    let total = sink.delivered.lock().len() - base;
    let np = no_progress(&rig, &working);
    assert!(at_jump <= 1 && total <= 2, "no notification burst: {at_jump} at the jump, {total} in all");
    p.saw(
        "both clocks +8h",
        format!("NoProgress {np}/10 (awake 8 h without a row); {at_jump} notification(s) at the jump, {total} incl. the merged one"),
    );
    drop(service);

    // (c) A wake through the composition root runs a discovery reconcile.
    with_wired(
        |_| {},
        |c| async move {
            let reconciles = |c: &verify::Ctx| {
                let d = c.wired.discovery.clone();
                let r = d.read().as_ref().and_then(|d| d.sync(std::time::Duration::from_secs(30))).map(|x| x.reconciles);
                r.expect("discovery answers")
            };
            let first = tokio::task::block_in_place(|| reconciles(&c));
            let idle = tokio::task::block_in_place(|| reconciles(&c));
            assert_eq!(idle, first, "a sync alone runs no reconcile (control)");
            c.wired.system.publish(SystemEvent::WillSleep);
            c.wired.system.publish(SystemEvent::DidWake);
            let after = tokio::task::block_in_place(|| reconciles(&c));
            assert!(after > first, "a reconcile ran after the wake ({first} → {after})");
            p.saw("DidWake (composition root)", format!("discovery reconciles {first} → {after} (control sync: {idle})"));
            p.finish();
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// A35.1 — the Conversation driver's mode report reaches the session row
// ---------------------------------------------------------------------------

fn meta_mode(m: gilde_code_lib::contract::common::PermissionMode) -> SourceFacts {
    SourceFacts::Meta(Box::new(SessionMetaFacts { permission_mode: Some(m), ..SessionMetaFacts::default() }))
}

/// A35.1: `World::set_permission_mode` overrides the metadata while Gilde
/// runs the session in Conversation mode — `None` (a mode Gilde does not
/// know, A33.3) stays `None` even when a later metadata fact names a mode,
/// until the next report; a Terminal session keeps the metadata value.
#[test]
fn the_conversation_drivers_mode_report_wins_while_it_runs_the_session() {
    use gilde_code_lib::contract::common::{PermissionMode, Vendor};
    use gilde_code_lib::contract::session::ControlMode;
    let rig = Rig::new();
    let t = rig.now();
    let convo = rig.world.bind_spawn("sp-convo", None, Vendor::Claude, Some(account()));
    let h = spawn_hint("sp-convo", Some(&sid(0x35a1)), Some(7351));
    rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t))));
    rig.ev(&h, alive(7351));
    rig.ev(&h, meta_mode(PermissionMode::Plan));
    rig.pump();
    assert_eq!(rig.session(&convo).permission_mode, Some(PermissionMode::Plan), "the metadata before any report");
    rig.world.set_permission_mode(&convo, None);
    assert_eq!(rig.session(&convo).permission_mode, None, "an unknown mode: none, not plan");
    rig.ev(&h, meta_mode(PermissionMode::Plan));
    assert_eq!(rig.session(&convo).permission_mode, None, "a later metadata fact does not bring the old mode back");
    rig.world.set_permission_mode(&convo, Some(PermissionMode::AcceptEdits));
    assert_eq!(rig.session(&convo).permission_mode, Some(PermissionMode::AcceptEdits), "the next report");
    // A Terminal session: the report never overrides its metadata.
    let term = rig.world.bind_spawn("sp-term", None, Vendor::Claude, Some(account()));
    let h = spawn_hint("sp-term", Some(&sid(0x35a2)), Some(7352));
    rig.ev(&h, SourceFacts::Gilde(gilde(t, ControlMode::Terminal, Some(t))));
    rig.ev(&h, alive(7352));
    rig.ev(&h, meta_mode(PermissionMode::Default));
    rig.world.set_permission_mode(&term, None);
    assert_eq!(rig.session(&term).permission_mode, Some(PermissionMode::Default), "not a Conversation session: the metadata");
}
