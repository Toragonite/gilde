//! A48 — the second review's minors (contract §8.4–§8.5, §9.1, §9.7). The
//! reviewer's `g7c_notify.rs` (G7c-11, G7c-12) ported as passing tests, plus
//! the variants: details on and off, two sessions of one project, a late
//! older queue (the policy and World's hand-off), bidi marks and zero-width
//! characters in names, and a wall clock jumping ±8 h around Later.
//!
//! Owner: T44 (T12's tests). Pure policy or the real World with recording
//! fakes: no OS, no timer, no notification is posted.

#[path = "world_support.rs"]
mod support;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use gilde_code_lib::contract::activity::{
    ActivityState, ErrorSub, OpenToolUse, PrimaryAction, RegStatus, TranscriptFacts,
};
use gilde_code_lib::contract::attention::{
    AttentionItem, AttentionQueue, AttentionReason, AttentionSubject,
};
use gilde_code_lib::contract::common::{AbsPath, EpochMs};
use gilde_code_lib::contract::ids::{EpisodeId, LocationId, ProjectId, SessionUid};
use gilde_code_lib::contract::ipc::{PauseUntil, ViewState};
use gilde_code_lib::contract::services::{
    EvidenceSink, LocationFacts, NotifyApi, SessionMetaFacts, SourceFacts, WorldApi,
};
use gilde_code_lib::contract::session::{Control, ControlMode};
use gilde_code_lib::contract::store::{FocusTarget, VisibilityEvent};
use gilde_code_lib::notify::policy::*;
use gilde_code_lib::notify::{render, uids_of, LabelSource, Locale, SessionLabels, WorldLabels};
use gilde_code_lib::world::WorldClock;
use gilde_code_lib::worktree::naming::slug_from_prompt;

use support::*;

const LAUNCH: Now = Now { up: 5_000_000, wall: 1_792_000_000_000 };
const MIN: u64 = 60_000;
const HOUR: i64 = 3_600_000;

// ---------------------------------------------------------------------------
// the reviewer's helpers (from notify_policy.rs)
// ---------------------------------------------------------------------------

/// `ms` after launch on both clocks.
fn t(ms: u64) -> Now {
    Now { up: LAUNCH.up + ms, wall: LAUNCH.wall + ms as i64 }
}

/// `ms` after launch on the uptime clock, with the wall clock moved by `jump`.
fn jumped(ms: u64, jump: i64) -> Now {
    Now { up: LAUNCH.up + ms, wall: LAUNCH.wall + ms as i64 + jump }
}

fn settled() -> Policy {
    let mut p = Policy::new(LAUNCH);
    p.boot_settled();
    p
}

#[derive(Clone)]
struct It {
    uid: SessionUid,
    episode: EpisodeId,
    band: u8,
    reason: AttentionReason,
    watched: bool,
    since: i64,
    waited_ms: u64,
    answer_in: Option<SessionUid>,
    later_until: Option<i64>,
}

fn ep(uid: &SessionUid, n: u32) -> EpisodeId {
    EpisodeId::derive(uid, "s", i64::from(n))
}

fn approval(uid: &SessionUid, n: u32) -> It {
    It {
        uid: uid.clone(),
        episode: ep(uid, n),
        band: 1,
        reason: AttentionReason::NeedsApproval,
        watched: false,
        since: LAUNCH.wall + 1,
        waited_ms: 0,
        answer_in: None,
        later_until: None,
    }
}

fn watched(mut it: It) -> It {
    it.watched = true;
    it
}

fn to_item(it: &It) -> AttentionItem {
    AttentionItem {
        key: format!("q_{}", it.uid.as_str()),
        band: it.band,
        subject: AttentionSubject::Session { uid: it.uid.clone(), on_behalf_of: None, answer_in: it.answer_in.clone() },
        reason: it.reason.clone(),
        since: it.since,
        waited_ms: it.waited_ms,
        episode: it.episode.clone(),
        later_until: it.later_until,
        primary: PrimaryAction::Answer,
        watched: it.watched,
    }
}

fn queue_rev(items: &[It], rev: u64) -> AttentionQueue {
    let items: Vec<AttentionItem> = items.iter().map(to_item).collect();
    let need_you = items.iter().filter(|i| i.band <= 2 && i.later_until.is_none()).count() as u32;
    let also = items.iter().filter(|i| i.band >= 3 && i.later_until.is_none()).count() as u32;
    AttentionQueue { rev, items, need_you, also, paused_until: None }
}

fn delivered(fx: &[Effect]) -> Vec<Planned> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::Deliver(p) => Some(p.clone()),
            Effect::Withdraw(_) => None,
        })
        .collect()
}

fn withdrawn(fx: &[Effect]) -> Vec<String> {
    fx.iter()
        .flat_map(|e| match e {
            Effect::Withdraw(ids) => ids.clone(),
            Effect::Deliver(_) => Vec::new(),
        })
        .collect()
}

fn key(it: &It) -> EpisodeKey {
    EpisodeKey { uid: it.uid.clone(), episode: it.episode.clone() }
}

fn id_of(it: &It) -> String {
    episode_id_of(&key(it))
}

// ---------------------------------------------------------------------------
// G7c-11 / A48.2 — the policy applies queues in rev order
// ---------------------------------------------------------------------------

/// G7c-11 (ported as is): World computes each queue under its lock and hands
/// it over after releasing it, so rev N+1 can reach the policy before rev N.
/// The stale queue must neither withdraw the delivered notification nor make
/// the episode a fresh key that notifies again (§8.5 rule 1).
#[test]
fn g7c_11_a_stale_queue_never_withdraws_or_renotifies_an_episode() {
    let mut p = settled();
    let a = approval(&SessionUid::mint(), 1);
    let mut deliveries = delivered(&p.queue(&queue_rev(std::slice::from_ref(&a), 2), t(1_000))).len();
    // rev 1 (computed before `a` entered) arrives late
    let stale = p.queue(&queue_rev(&[], 1), t(1_010));
    let withdrew = withdrawn(&stale);
    deliveries += delivered(&p.queue(&queue_rev(std::slice::from_ref(&a), 3), t(1_020))).len();
    for ms in [5_000u64, 16_100, 20_000, 40_000] {
        deliveries += delivered(&p.tick(t(ms))).len();
    }
    assert!(
        withdrew.is_empty() && deliveries == 1,
        "an out-of-order queue (rev 1 after rev 2) withdrew {withdrew:?} and the episode was delivered {deliveries} times"
    );
}

/// The mirror case: the newer queue resolved the episode (withdrawn); the
/// older queue that still lists it, arriving late — or a duplicate of the
/// last rev — re-delivers nothing.
#[test]
fn a48_2_a_late_older_queue_never_redelivers_a_resolved_episode() {
    let mut p = settled();
    let a = approval(&SessionUid::mint(), 1);
    assert_eq!(delivered(&p.queue(&queue_rev(std::slice::from_ref(&a), 5), t(1_000))).len(), 1);
    assert_eq!(withdrawn(&p.queue(&queue_rev(&[], 6), t(2_000))), vec![id_of(&a)], "resolved: withdrawn");
    for rev in [6u64, 5, 4, 0] {
        assert!(p.queue(&queue_rev(std::slice::from_ref(&a), rev), t(3_000)).is_empty(), "rev {rev} after rev 6");
    }
    for ms in [5_000u64, 20_000, 21 * MIN, 60 * MIN] {
        assert!(delivered(&p.tick(t(ms))).is_empty(), "nothing at {ms}");
    }
    assert_eq!(p.last_rev(), Some(6));
    assert_eq!(p.outcome(&key(&a)), None, "the key stays resolved");
    // a newer queue with a new episode of the session notifies as usual
    let again = approval(&a.uid, 2);
    assert_eq!(delivered(&p.queue(&queue_rev(std::slice::from_ref(&again), 7), t(30 * MIN))).len(), 1);
}

// ---------------------------------------------------------------------------
// A48.4 — Later in the policy never compares with the wall clock
// ---------------------------------------------------------------------------

/// A watched approval Later-ed before its 60 s decision, then the wall clock
/// jumps +8 h and another queue arrives: the item is still Later-ed (World
/// decides Later on the uptime clock), so it never notifies.
#[test]
fn a48_4_later_survives_a_forward_wall_jump_in_the_policy() {
    for jump in [8 * HOUR, -8 * HOUR] {
        let mut p = settled();
        let mut a = watched(approval(&SessionUid::mint(), 1));
        let other = approval(&SessionUid::mint(), 1);
        assert!(p.queue(&queue_rev(std::slice::from_ref(&a), 1), t(1_000)).is_empty(), "watched: 60 s grace");
        // `queue_later` at 10 s: World marks the item until wall + 30 min
        a.later_until = Some(t(10_000).wall + 30 * MIN as i64);
        p.queue(&queue_rev(std::slice::from_ref(&a), 2), t(10_000));
        // the wall clock jumps; another session's change brings a new queue
        p.queue(&queue_rev(&[a.clone(), other.clone()], 3), jumped(20_000, jump));
        let mut named = Vec::new();
        for ms in [61_000u64, 2 * MIN, 29 * MIN] {
            for planned in delivered(&p.tick(jumped(ms, jump))) {
                named.extend(uids_of(&planned));
            }
        }
        assert_eq!(p.outcome(&key(&a)), Some(Outcome::Suppressed(Suppressed::Later)), "wall {jump:+} ms");
        assert!(named.contains(&other.uid), "wall {jump:+} ms: the other session notifies");
        assert!(!named.contains(&a.uid), "wall {jump:+} ms: the Later-ed session notified");
    }
}

/// §8.5 rule 5 under a wall-clock jump (found by the ±8 h variant above):
/// "existed before launch" needs the episode to be older than the launch on
/// both clocks. A wall clock set back 8 h after the launch makes a new
/// episode's `since` older than the launch wall time, yet it began after the
/// launch on the uptime clock: it notifies. An episode that began before the
/// launch on both clocks still never does.
#[test]
fn a48_4_a_wall_clock_set_back_never_silences_new_episodes() {
    let mut p = settled();
    let mut fresh = approval(&SessionUid::mint(), 1);
    fresh.since = jumped(60_000, -8 * HOUR).wall; // World's wall reading, after the jump
    fresh.waited_ms = 0;
    let fx = p.queue(&queue_rev(std::slice::from_ref(&fresh), 1), jumped(60_000, -8 * HOUR));
    assert_eq!(delivered(&fx).len(), 1, "a new episode after the jump notifies");
    let mut old = approval(&SessionUid::mint(), 1);
    old.since = LAUNCH.wall - 60_000;
    old.waited_ms = 70_000 + 60_000; // its age at t(70 s): it began 60 s before the launch
    p.queue(&queue_rev(&[fresh.clone(), old.clone()], 2), t(70_000));
    assert_eq!(p.outcome(&key(&old)), Some(Outcome::Suppressed(Suppressed::PreExisting)));
}

/// The other half: an episode that began during a long sleep is old on the
/// uptime clock (World converts its wall `since`) but not on the wall clock:
/// it is not "existed before launch" — it gets the wake grace (§8.5 rule 5).
#[test]
fn a48_4_an_episode_that_began_during_a_long_sleep_still_notifies_after_the_wake_grace() {
    let mut p = settled();
    p.sleep(t(60_000));
    // 8 h asleep: the uptime clock stands still, the wall clock moves on
    let wake = jumped(60_000, 8 * HOUR);
    let fx = p.wake(wake);
    assert!(delivered(&fx).is_empty());
    let mut a = approval(&SessionUid::mint(), 1);
    a.since = LAUNCH.wall + 2 * HOUR; // began 6 h before the wake, during the sleep
    a.waited_ms = 6 * HOUR as u64; // its age: older than the launch on the uptime clock
    assert!(delivered(&p.queue(&queue_rev(std::slice::from_ref(&a), 1), wake)).is_empty(), "not at once after a wake");
    assert_eq!(p.outcome(&key(&a)), Some(Outcome::Waiting));
    let later = jumped(60_000 + WAKE_GRACE_IN_GILDE_MS, 8 * HOUR);
    assert_eq!(delivered(&p.tick(later)).len(), 1, "notified after the wake grace");
}

// ---------------------------------------------------------------------------
// the real World: labels, ordinals, names
// ---------------------------------------------------------------------------

const ROOT: &str = "/Users/tester/payments";

fn abs(p: &str) -> AbsPath {
    AbsPath::parse(p).unwrap()
}

/// Registers the project at `root` (git) and, when `worktree` is set, a
/// worktree `<root>/.gilde/worktrees/<worktree>` on branch `branch`.
fn project(rig: &Rig, root: &str, worktree: Option<(&str, &str)>) -> Option<String> {
    let main = LocationFacts {
        project_id: ProjectId::derive(&format!("{root}/.git")),
        project_root: abs(root),
        git: true,
        location_id: LocationId::derive(root),
        location_path: abs(root),
        worktree: false,
        branch: Some("main".into()),
    };
    rig.sink.location(main.clone());
    let (name, branch) = worktree?;
    let path = format!("{root}/.gilde/worktrees/{name}");
    rig.sink.location(LocationFacts {
        location_id: LocationId::derive(&path),
        location_path: abs(&path),
        worktree: true,
        branch: Some(branch.into()),
        ..main
    });
    Some(path)
}

/// A watched claude session `n` in `cwd`, started at `started`, with an
/// optional title (a first prompt).
fn session(rig: &Rig, n: u32, cwd: &str, started: EpochMs, title: Option<&str>) -> SessionUid {
    let h = claude_hint(Some(&sid(n)), Some(7000 + n));
    rig.ev(&h, alive(7000 + n as i32));
    rig.ev(&h, registry(RegStatus::Idle, rig.now(), None));
    rig.ev(
        &h,
        SourceFacts::Meta(Box::new(SessionMetaFacts {
            cwd: Some(abs(cwd)),
            started_at: Some(started),
            title: title.map(str::to_string),
            ..SessionMetaFacts::default()
        })),
    );
    rig.uid_of(&sid(n))
}

fn labels_of(rig: &Rig, uids: &[SessionUid]) -> HashMap<SessionUid, SessionLabels> {
    rig.pump();
    let api: Arc<dyn WorldApi> = Arc::new(rig.world.clone());
    WorldLabels::new(api).labels(uids)
}

fn single(uid: &SessionUid) -> Planned {
    Planned {
        id: format!("gilde.e.{}", uid.as_str()),
        kind: PlanKind::Single {
            key: EpisodeKey { uid: uid.clone(), episode: ep(uid, 1) },
            band: 1,
            reason: AttentionReason::NeedsApproval,
        },
        sound: false,
        target: FocusTarget::Session { uid: uid.clone(), focus_card: true },
    }
}

fn many(uids: &[&SessionUid]) -> Planned {
    Planned {
        id: MANY_ID.into(),
        kind: PlanKind::Many { keys: uids.iter().map(|u| EpisodeKey { uid: (*u).clone(), episode: ep(u, 1) }).collect() },
        sound: false,
        target: FocusTarget::Home,
    }
}

const PROMPT: &str = "rotate leaked stripe secret key in prod config";

/// The prompt's words that appear in `text` as whole words.
fn prompt_words_in(text: &str) -> Vec<&'static str> {
    let words: Vec<&str> = text.split(|c: char| !c.is_alphanumeric()).collect();
    PROMPT.split_whitespace().filter(|w| words.contains(w)).collect()
}

/// G7c-12 (ported, through the real World): a Gilde worktree is named by
/// the slug of the first five prompt words (§4.2.2); with "Show details in
/// notifications" off, none of them reaches the notification (§9.7).
#[test]
fn g7c_12_details_off_never_puts_prompt_words_in_a_notification() {
    let slug = slug_from_prompt(PROMPT);
    assert!(!prompt_words_in(&slug).is_empty(), "the slug carries prompt words: {slug}");
    let rig = Rig::new();
    let wt = project(&rig, ROOT, Some((&slug, &format!("gilde/{slug}")))).unwrap();
    let a = session(&rig, 1, &format!("{wt}/src"), T0 - 60_000, Some(PROMPT));
    let labels = labels_of(&rig, std::slice::from_ref(&a));
    for locale in [Locale::En, Locale::Ko] {
        let n = render(&single(&a), &labels, locale, false);
        let leaked = prompt_words_in(&format!("{} {}", n.title, n.body));
        assert!(leaked.is_empty(), "details off, yet {:?} / {:?} carries prompt words {leaked:?}", n.title, n.body);
    }
    let n = render(&single(&a), &labels, Locale::En, false);
    assert_eq!((n.title.as_str(), n.body.as_str()), ("payments", "payments · claude · Needs approval"));
    // the reviewer's literal shape: the slug as the richer label, the prompt as
    // the title — details off shows neither
    let literal: HashMap<SessionUid, SessionLabels> = [(
        a.clone(),
        SessionLabels {
            project: Some("payments".into()),
            session: "payments · claude".into(),
            detail: Some(slug.clone()),
            title: Some(PROMPT.into()),
            summary: Some("rm -rf secrets/".into()),
        },
    )]
    .into_iter()
    .collect();
    let n = render(&single(&a), &literal, Locale::En, false);
    assert!(prompt_words_in(&n.body).is_empty() && !n.body.contains("secrets"), "{:?}", n.body);
}

/// A48.1: details on keeps the richer label — the title, else the worktree
/// name — and adds the request line; details off shows the content-free
/// label only, for single, merged, repeat and reminder notifications.
#[test]
fn a48_1_details_on_keeps_the_richer_label_details_off_never() {
    let slug = slug_from_prompt(PROMPT);
    let rig = Rig::new();
    let wt = project(&rig, ROOT, Some((&slug, &format!("gilde/{slug}")))).unwrap();
    let titled = session(&rig, 1, &wt, T0 - 120_000, Some(PROMPT));
    let untitled = session(&rig, 2, &wt, T0 - 60_000, None);
    let labels = labels_of(&rig, &[titled.clone(), untitled.clone()]);
    assert_eq!(labels[&titled].detail.as_deref(), Some(slug.as_str()), "the worktree name is kept for details on");

    let on = render(&single(&titled), &labels, Locale::En, true);
    assert_eq!(on.body, format!("{PROMPT} · Needs approval"), "details on: the title, as before");
    let on = render(&single(&untitled), &labels, Locale::En, true);
    assert_eq!(on.body, format!("{slug} · Needs approval"), "details on, no title: the worktree name, as before");

    let off = render(&single(&titled), &labels, Locale::Ko, false);
    assert_eq!(off.body, "payments · claude 1 · 승인 대기");
    let off = render(&single(&untitled), &labels, Locale::En, false);
    assert_eq!(off.body, "payments · claude 2 · Needs approval");

    let key = EpisodeKey { uid: titled.clone(), episode: ep(&titled, 1) };
    let repeat = Planned {
        id: "r".into(),
        kind: PlanKind::Repeat { key: key.clone(), band: 1, reason: AttentionReason::NeedsApproval },
        sound: false,
        target: FocusTarget::Home,
    };
    let reminder = Planned {
        id: "m".into(),
        kind: PlanKind::Reminder { key, band: 1, reason: AttentionReason::NeedsApproval, waited_ms: 20 * MIN },
        sound: false,
        target: FocusTarget::Home,
    };
    for planned in [&single(&titled), &many(&[&titled, &untitled]), &repeat, &reminder] {
        for locale in [Locale::En, Locale::Ko] {
            let n = render(planned, &labels, locale, false);
            let text = format!("{} {}", n.title, n.body);
            assert!(prompt_words_in(&text).is_empty() && !text.contains(&slug), "details off: {text:?}");
        }
    }
}

/// A48.1: two sessions of one project share "project · vendor", so each
/// gets an ordinal by start time; a session that ended (and waits for
/// nothing) is not counted, a session of another project gets none.
#[test]
fn a48_1_two_sessions_of_one_project_get_ordinals() {
    let rig = Rig::new();
    project(&rig, ROOT, None);
    project(&rig, "/Users/tester/billing", None);
    // started first but ended normally: not counted
    let ended = session(&rig, 9, ROOT, T0 - 600_000, None);
    rig.ev(&claude_hint(Some(&sid(9)), Some(7009)), exited(rig.now(), Some(0), None));
    let later = session(&rig, 2, &format!("{ROOT}/web"), T0 - 60_000, None);
    let earlier = session(&rig, 1, ROOT, T0 - 120_000, None);
    let other = session(&rig, 3, "/Users/tester/billing", T0 - 30_000, None);
    let labels = labels_of(&rig, &[earlier.clone(), later.clone(), other.clone(), ended.clone()]);
    let s = rig.session(&ended);
    assert!(matches!(s.control, Control::NotRunning { .. }), "{:?}", s.control);
    assert!(gilde_code_lib::world::derive::band(&s.activity.state).is_none_or(|b| b > 3), "{:?}", s.activity.state);

    assert_eq!(labels[&earlier].session, "payments · claude 1");
    assert_eq!(labels[&later].session, "payments · claude 2");
    assert_eq!(labels[&other].session, "billing · claude");
    // the ended one, if it were ever labelled, counts itself
    assert_eq!(labels[&ended].session, "payments · claude 1");
    let n = render(&many(&[&later, &earlier]), &labels, Locale::En, false);
    assert_eq!(n.body, "payments · claude 2, payments · claude 1");
    // a third live session starting later never renumbers the first two
    let third = session(&rig, 4, ROOT, T0, None);
    let labels = labels_of(&rig, &[earlier.clone(), later.clone(), third.clone()]);
    assert_eq!(
        [&labels[&earlier].session, &labels[&later].session, &labels[&third].session],
        ["payments · claude 1", "payments · claude 2", "payments · claude 3"]
    );
}

/// A crashed session waits in band 3 though it no longer runs: it still
/// counts, so two crashed sessions of one project stay distinguishable.
#[test]
fn a48_1_crashed_sessions_keep_their_ordinals() {
    let rig = Rig::new();
    project(&rig, ROOT, None);
    let a = session(&rig, 1, ROOT, T0 - 120_000, None);
    let b = session(&rig, 2, ROOT, T0 - 60_000, None);
    for (n, uid) in [(1u32, &a), (2, &b)] {
        let h = claude_hint(Some(&sid(n)), Some(7000 + n));
        // Gilde's own terminal child (R2 crashes are Gilde's children only)
        rig.ev(&h, SourceFacts::Gilde(gilde(T0 - 200_000, ControlMode::Terminal, Some(T0 - 190_000))));
        rig.ev(&h, exited(rig.now(), None, Some(9)));
        rig.pump();
        let s = rig.session(uid);
        assert!(matches!(s.activity.state, ActivityState::Crashed { .. }), "{:?}", s.activity.state);
    }
    let labels = labels_of(&rig, &[a.clone(), b.clone()]);
    assert_eq!((labels[&a].session.as_str(), labels[&b].session.as_str()), ("payments · claude 1", "payments · claude 2"));
}

/// A48.3: LRM/RLM/ALM and zero-width characters in a project folder, a
/// worktree folder and a branch never reach a label or a notification.
#[test]
fn a48_3_bidi_marks_and_zero_width_characters_leave_names() {
    const HIDDEN: [char; 7] = ['\u{200E}', '\u{200F}', '\u{061C}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'];
    let rig = Rig::new();
    // A45.1: a path carrying bidi/zero-width characters is refused at the
    // type, so such characters can no longer reach a name through a path;
    // they still can through a branch name and a title, tested below.
    assert!(AbsPath::parse("/Users/tester/pay\u{200B}men\u{200F}ts\u{FEFF}").is_err(), "A45.1 refuses hidden characters in paths");
    let root = "/Users/tester/payments";
    let wt = project(&rig, root, Some(("fix-auth", "gilde/fix\u{200C}-auth\u{200F}\u{061C}"))).unwrap();
    let a = session(&rig, 1, &wt, T0 - 60_000, Some("ti\u{200F}tle"));
    let labels = labels_of(&rig, std::slice::from_ref(&a));
    let snap = rig.world.snapshot();
    let p = snap.projects.iter().find(|p| p.root.as_str() == root).expect("the project");
    assert_eq!(p.name, "payments");
    let l = snap.locations.iter().find(|l| l.path.as_str() == wt).expect("the worktree");
    assert_eq!((l.worktree_name.as_deref(), l.branch.as_deref()), (Some("fix-auth"), Some("gilde/fix-auth")));
    let lab = &labels[&a];
    assert_eq!((lab.project.as_deref(), lab.session.as_str(), lab.detail.as_deref()), (Some("payments"), "payments · claude", Some("fix-auth")));
    for show in [false, true] {
        let n = render(&single(&a), &labels, Locale::En, show);
        let text = format!("{}{}", n.title, n.body);
        assert!(!text.chars().any(|c| HIDDEN.contains(&c)), "details {show}: {text:?}");
    }
    assert_eq!(render(&single(&a), &labels, Locale::En, true).body, "title · Needs approval", "RLM gone from the title too");
}

// ---------------------------------------------------------------------------
// A48.2 — World hands queues over in rev order
// ---------------------------------------------------------------------------

/// Records each queue when its `queue_changed` call returns; optionally calls
/// back into World from inside the first call.
#[derive(Default)]
struct Ordered {
    seen: Mutex<Vec<(u64, Option<EpochMs>)>>,
    reenter: Mutex<Option<Arc<dyn EvidenceSink>>>,
}

#[async_trait]
impl NotifyApi for Ordered {
    fn view_state(&self, _state: ViewState) {}
    fn input_activity(&self) {}
    fn queue_changed(&self, queue: &AttentionQueue) {
        let sink = self.reenter.lock().take();
        if let Some(sink) = sink {
            // a NotifyApi that mutates World from its callback (the pause)
            sink.notify_paused(Some(T0 + 1));
        }
        std::thread::yield_now();
        self.seen.lock().push((queue.rev, queue.paused_until));
    }
    fn visibility(&self) -> VisibilityEvent {
        VisibilityEvent { frontmost: false, occluded: false, asleep: false }
    }
    fn pause(&self, _until: PauseUntil) -> Option<EpochMs> {
        None
    }
    fn resume(&self) {}
    async fn permission_request(&self) -> bool {
        false
    }
    async fn test(&self) -> bool {
        false
    }
}

fn assert_in_order(seen: &[(u64, Option<EpochMs>)], world: &gilde_code_lib::world::World) {
    assert!(seen.windows(2).all(|w| w[0].0 < w[1].0), "queues out of rev order: {:?}", seen.iter().map(|s| s.0).collect::<Vec<_>>());
    let last = seen.last().expect("queues were handed over");
    let q = world.queue();
    assert_eq!(*last, (q.rev, q.paused_until), "the newest queue is the last one handed over");
}

/// Eight threads change the queue concurrently through every path (the sink,
/// the publisher step, `flush_now`): `queue_changed` sees strictly rising revs
/// and ends with World's current queue.
#[test]
fn a48_2_world_hands_queues_over_in_rev_order() {
    let rig = Rig::new();
    let rec = Arc::new(Ordered::default());
    rig.world.set_notify(rec.clone());
    let sink = rig.world.sink();
    std::thread::scope(|s| {
        for th in 0..8i64 {
            let sink = sink.clone();
            s.spawn(move || {
                for i in 0..200i64 {
                    sink.notify_paused(Some(T0 + th * 1_000 + i + 1));
                }
            });
        }
        s.spawn(|| {
            for _ in 0..200 {
                rig.world.flush_now();
                rig.world.pump();
            }
        });
    });
    let seen = rec.seen.lock().clone();
    assert_in_order(&seen, &rig.world);
}

/// A `NotifyApi` that changes World from inside `queue_changed` neither
/// deadlocks nor sees the newer queue before the older call returned.
#[test]
fn a48_2_a_notify_api_that_calls_back_into_world_is_served_in_order() {
    let rig = Rig::new();
    let rec = Arc::new(Ordered::default());
    *rec.reenter.lock() = Some(rig.world.sink());
    rig.world.set_notify(rec.clone());
    rig.sink.notify_paused(Some(T0 + 5));
    let seen = rec.seen.lock().clone();
    assert_eq!(seen.len(), 2, "both queues handed over: {seen:?}");
    assert_eq!(seen[0].1, Some(T0 + 5));
    assert_eq!(seen[1].1, Some(T0 + 1));
    assert_in_order(&seen, &rig.world);
}

// ---------------------------------------------------------------------------
// A48.4 — Later keeps its 30 minutes of uptime across wall-clock jumps
// ---------------------------------------------------------------------------

/// A watched approval waiting in band 1 (permission prompt + open tool use).
fn approval_session(rig: &Rig, n: u32) -> SessionUid {
    let now = rig.now();
    let h = claude_hint(Some(&sid(n)), Some(7000 + n));
    rig.ev(&h, alive(7000 + n as i32));
    rig.ev(&h, registry(RegStatus::Waiting, now, Some("permission prompt")));
    rig.ev(
        &h,
        SourceFacts::Transcript(TranscriptFacts {
            open_tool_use: Some(OpenToolUse { tool_use_id: format!("toolu_{n}"), tool_name: "Bash".into(), at: now, is_question: false }),
            ..transcript(now, Some(now - 5_000), None)
        }),
    );
    rig.pump();
    rig.uid_of(&sid(n))
}

fn later_until(rig: &Rig, uid: &SessionUid) -> Option<EpochMs> {
    rig.pump();
    rig.world.queue().items.into_iter().find(|i| matches!(&i.subject, AttentionSubject::Session { uid: u, .. } if u == uid)).and_then(|i| i.later_until)
}

/// World's Later lasts 30 min of uptime whatever the wall clock does, and the
/// policy fed with World's queues never notifies the Later-ed item — not
/// even when a queue arrives right after a +8 h jump.
#[test]
fn a48_4_later_keeps_its_30_minutes_across_wall_jumps() {
    for jump in [8 * HOUR, -8 * HOUR] {
        let rig = Rig::new();
        rig.subscribe();
        let clock = || Now { up: rig.clock.up_ms(), wall: rig.clock.wall_ms() };
        let mut p = Policy::new(clock());
        p.boot_settled();
        let feed = |p: &mut Policy| -> Vec<Planned> {
            let queues: Vec<AttentionQueue> = std::mem::take(&mut *rig.notify.queues.lock());
            let mut out = Vec::new();
            for q in queues {
                out.extend(delivered(&p.queue(&q, clock())));
            }
            out.extend(delivered(&p.tick(clock())));
            out
        };
        rig.advance(MIN);
        let a = approval_session(&rig, 1);
        assert!(matches!(rig.state(&a), ActivityState::NeedsApproval), "{:?}", rig.state(&a));
        let mut got = feed(&mut p);
        rig.advance(10_000);
        rig.world.queue_later(&a).unwrap();
        assert!(later_until(&rig, &a).is_some());
        got.extend(feed(&mut p));
        // the wall clock jumps; another session enters, so a new queue arrives
        rig.clock.set_wall(rig.now() + jump);
        rig.advance(5_000);
        let b = approval_session(&rig, 2);
        got.extend(feed(&mut p));
        assert!(later_until(&rig, &a).is_some(), "wall {jump:+} ms: Later was cut");
        // past the 60 s watched grace and up to 29 min of uptime: still Later
        for _ in 0..28 {
            rig.advance(MIN);
            got.extend(feed(&mut p));
        }
        assert!(later_until(&rig, &a).is_some(), "wall {jump:+} ms: Later ended before 30 min of uptime");
        let named = |got: &[Planned]| got.iter().flat_map(uids_of).collect::<Vec<_>>();
        assert!(named(&got).contains(&b), "wall {jump:+} ms: the other session notifies");
        assert!(!named(&got).contains(&a), "wall {jump:+} ms: the Later-ed session notified");
        // 30 min of uptime after the Later: it ends, neither stretched nor cut
        rig.advance(2 * MIN);
        assert_eq!(later_until(&rig, &a), None, "wall {jump:+} ms: Later was stretched");
        got.extend(feed(&mut p));
        assert!(!named(&got).contains(&a), "Later-ed → never (§8.4)");
    }
}

/// Band 3 for completeness: an error item is decided once, like the others.
#[test]
fn a48_2_old_and_new_queues_for_an_error_item() {
    let mut p = settled();
    let mut e = approval(&SessionUid::mint(), 1);
    e.band = 3;
    e.reason = AttentionReason::Error { sub: ErrorSub::AgentError };
    assert_eq!(delivered(&p.queue(&queue_rev(std::slice::from_ref(&e), 10), t(1_000))).len(), 1);
    assert!(withdrawn(&p.queue(&queue_rev(&[], 9), t(1_100))).is_empty());
    assert!(delivered(&p.queue(&queue_rev(std::slice::from_ref(&e), 11), t(1_200))).is_empty());
}
