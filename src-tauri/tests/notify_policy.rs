//! The notification policy against a fake clock (contract §8.4–§8.5, §14.2):
//! T-att-1 … T-att-8, coalescing, burst guard, reminder, withdrawal,
//! T-att-reentry, T-att-hideDetails, T-notify-denied (policy side), the wake
//! grace and the HP-6 clock jump. Pure: no OS, no timer, no notification.

use std::collections::HashMap;

use gilde_code_lib::contract::activity::{ErrorSub, PrimaryAction};
use gilde_code_lib::contract::attention::{AttentionItem, AttentionQueue, AttentionReason, AttentionSubject};
use gilde_code_lib::contract::ids::{EpisodeId, SessionUid};
use gilde_code_lib::contract::ipc::{NotifySettings, Screen, ViewState};
use gilde_code_lib::contract::store::{FocusTarget, VisibilityEvent};
use gilde_code_lib::notify::policy::*;
use gilde_code_lib::notify::{render, Locale, SessionLabels};

const LAUNCH: Now = Now { up: 5_000_000, wall: 1_792_000_000_000 };
const MIN: u64 = 60_000;

/// `ms` after launch on both clocks.
fn t(ms: u64) -> Now {
    Now { up: LAUNCH.up + ms, wall: LAUNCH.wall + ms as i64 }
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

fn with_band(mut it: It, band: u8) -> It {
    it.band = band;
    it.reason = match band {
        1 => AttentionReason::NeedsApproval,
        2 => AttentionReason::NeedsAnswer,
        3 => AttentionReason::Error { sub: ErrorSub::AgentError },
        4 => AttentionReason::NoProgress { for_secs: 600 },
        _ => AttentionReason::NewReply,
    };
    it
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

/// Every queue World hands over carries a newer rev (A48.2).
fn next_rev() -> u64 {
    static REV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    REV.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

fn queue(items: &[It]) -> AttentionQueue {
    let items: Vec<AttentionItem> = items.iter().map(to_item).collect();
    let need_you = items.iter().filter(|i| i.band <= 2 && i.later_until.is_none()).count() as u32;
    let also = items.iter().filter(|i| i.band >= 3 && i.later_until.is_none()).count() as u32;
    AttentionQueue { rev: next_rev(), items, need_you, also, paused_until: None }
}

fn leftovers_item() -> AttentionItem {
    AttentionItem {
        key: "q_leftovers".into(),
        band: 6,
        subject: AttentionSubject::Leftovers { count: 3, bytes: 1 << 30 },
        reason: AttentionReason::Leftovers,
        since: LAUNCH.wall + 1,
        waited_ms: 0,
        episode: ep(&SessionUid::mint(), 99),
        later_until: None,
        primary: PrimaryAction::CleanUp,
        watched: false,
    }
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

fn view(uids: &[&SessionUid], document_visible: bool) -> ViewState {
    ViewState {
        screen: Screen::Split,
        visible_uids: uids.iter().map(|u| (*u).clone()).collect(),
        focused_uid: None,
        zoomed_uid: None,
        team_key: None,
        document_visible,
    }
}

fn front(p: &mut Policy, frontmost: bool, occluded: bool) {
    p.visibility(VisibilityEvent { frontmost, occluded, asleep: false });
}

// -- T-att-1 --------------------------------------------------------------------

#[test]
fn t_att_1_one_notification_per_episode_plus_one_reminder() {
    let mut p = settled();
    let a = approval(&SessionUid::mint(), 1);
    let fx = p.queue(&queue(std::slice::from_ref(&a)), t(1_000));
    let first = delivered(&fx);
    assert_eq!(first.len(), 1);
    assert!(matches!(first[0].kind, PlanKind::Single { .. }));
    assert_eq!(first[0].id, id_of(&a));
    assert_eq!(first[0].target, FocusTarget::Session { uid: a.uid.clone(), focus_card: true });
    // repeats within the episode never notify
    for ms in [2_000, 5_000, 60_000, 600_000] {
        assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(ms))).is_empty(), "repeat at {ms}");
    }
    assert!(delivered(&p.tick(t(1_000 + 20 * MIN - 1))).is_empty());
    let reminder = delivered(&p.tick(t(1_000 + 20 * MIN)));
    assert_eq!(reminder.len(), 1);
    match &reminder[0].kind {
        PlanKind::Reminder { waited_ms, .. } => assert_eq!(*waited_ms, 20 * MIN),
        other => panic!("{other:?}"),
    }
    assert_eq!(reminder[0].id, first[0].id, "the reminder replaces the original");
    assert!(!reminder[0].sound, "a reminder never plays the sound");
    // at most one reminder
    assert!(delivered(&p.tick(t(1_000 + 60 * MIN))).is_empty());
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(2 * 60 * MIN))).is_empty());
    assert_eq!(p.next_deadline(t(2 * 60 * MIN)), None, "nothing left to schedule");
}

#[test]
fn t_att_1_band_three_has_no_reminder() {
    let mut p = settled();
    let e = with_band(approval(&SessionUid::mint(), 1), 3);
    assert_eq!(delivered(&p.queue(&queue(std::slice::from_ref(&e)), t(1_000))).len(), 1);
    assert!(delivered(&p.tick(t(1_000 + 40 * MIN))).is_empty());
}

// -- T-att-2 [F-04] -------------------------------------------------------------

fn visible_case(setup: impl Fn(&mut Policy, &It)) -> Vec<Planned> {
    let mut p = settled();
    let a = approval(&SessionUid::mint(), 1);
    setup(&mut p, &a);
    delivered(&p.queue(&queue(&[a]), t(1_000)))
}

#[test]
fn t_att_2_nothing_for_what_is_on_screen_while_frontmost() {
    // on screen: frontmost, not occluded, document visible, uid visible
    let none = visible_case(|p, a| {
        front(p, true, false);
        p.view_state(view(&[&a.uid], true));
    });
    assert!(none.is_empty(), "the session is on screen");
    // each condition alone makes it not on screen
    let not_front = visible_case(|p, a| {
        front(p, false, false);
        p.view_state(view(&[&a.uid], true));
    });
    let occluded = visible_case(|p, a| {
        front(p, true, true);
        p.view_state(view(&[&a.uid], true));
    });
    let hidden_doc = visible_case(|p, a| {
        front(p, true, false);
        p.view_state(view(&[&a.uid], false));
    });
    let other_tile = visible_case(|p, _| {
        front(p, true, false);
        p.view_state(view(&[&SessionUid::mint()], true));
    });
    let no_view_yet = visible_case(|p, _| front(p, true, false));
    for (name, got) in
        [("not frontmost", not_front), ("occluded", occluded), ("document hidden", hidden_doc), ("other tile", other_tile), ("no app_view_state yet", no_view_yet)]
    {
        assert_eq!(got.len(), 1, "{name}: not on screen → notify");
    }
}

#[test]
fn t_att_2_a_card_answered_in_a_visible_lead_counts_as_on_screen() {
    let mut p = settled();
    front(&mut p, true, false);
    let lead = SessionUid::mint();
    let mut member = approval(&SessionUid::mint(), 1);
    member.answer_in = Some(lead.clone());
    p.view_state(view(&[&lead], true));
    let fx = p.queue(&queue(std::slice::from_ref(&member)), t(1_000));
    assert!(delivered(&fx).is_empty());
    assert_eq!(p.outcome(&key(&member)), Some(Outcome::Suppressed(Suppressed::Visible)));
}

#[test]
fn t_att_2_no_reminder_while_the_session_is_on_screen() {
    let mut p = settled();
    let a = approval(&SessionUid::mint(), 1);
    assert_eq!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(1_000))).len(), 1);
    front(&mut p, true, false);
    p.view_state(view(&[&a.uid], true));
    assert!(delivered(&p.tick(t(1_000 + 20 * MIN))).is_empty());
    // consumed: no reminder later either
    p.view_state(view(&[], true));
    assert!(delivered(&p.tick(t(1_000 + 30 * MIN))).is_empty());
}

// -- T-att-3 --------------------------------------------------------------------

#[test]
fn t_att_3_nothing_within_10s_of_typing_in_gilde() {
    let mut p = settled();
    p.input_activity(t(5_000));
    let a = approval(&SessionUid::mint(), 1);
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(14_999))).is_empty());
    assert_eq!(p.outcome(&key(&a)), Some(Outcome::Suppressed(Suppressed::Typing)));
    let b = approval(&SessionUid::mint(), 1);
    let fx = p.queue(&queue(&[a.clone(), b.clone()]), t(15_000));
    assert_eq!(delivered(&fx).len(), 1, "10 s after the last keystroke");
    assert_eq!(delivered(&fx)[0].id, id_of(&b));
}

// -- T-att-4 --------------------------------------------------------------------

#[test]
fn t_att_4_nothing_for_bands_4_to_6_or_working_and_ready() {
    let mut p = settled();
    let np = with_band(approval(&SessionUid::mint(), 1), 4);
    let reply = with_band(approval(&SessionUid::mint(), 1), 5);
    let mut q = queue(&[np, reply]);
    q.items.push(leftovers_item());
    for ms in [1_000, 70_000, 30 * MIN, 120 * MIN] {
        q.rev = next_rev();
        assert!(p.queue(&q, t(ms)).is_empty(), "nothing at {ms}");
        assert!(p.tick(t(ms + 1)).is_empty());
    }
    // Working/Ready sessions are not in the queue at all: an empty queue is silent
    assert!(p.queue(&queue(&[]), t(130 * MIN)).is_empty());
    assert_eq!(p.next_deadline(t(130 * MIN)), None);
}

// -- T-att-5 and the wake grace -------------------------------------------------

#[test]
fn t_att_5_nothing_on_launch_for_states_that_existed_before_launch() {
    // first seen before the boot settled
    let mut p = Policy::new(LAUNCH);
    let a = approval(&SessionUid::mint(), 1);
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(2_000))).is_empty());
    assert_eq!(p.outcome(&key(&a)), Some(Outcome::Suppressed(Suppressed::PreExisting)));
    p.boot_settled();
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(3_000))).is_empty());
    assert!(delivered(&p.tick(t(30 * MIN))).is_empty(), "no reminder for it either");
    // first seen after the boot settled, but waiting since before launch
    let mut b = approval(&SessionUid::mint(), 1);
    b.since = LAUNCH.wall - 60_000;
    b.waited_ms = 64_000; // World's age of it at t(4 s): since launch − 60 s
    assert!(delivered(&p.queue(&queue(&[a.clone(), b.clone()]), t(4_000))).is_empty());
    assert_eq!(p.outcome(&key(&b)), Some(Outcome::Suppressed(Suppressed::PreExisting)));
    // a new state after the boot notifies
    let c = approval(&SessionUid::mint(), 1);
    assert_eq!(delivered(&p.queue(&queue(&[a, b, c]), t(5_000))).len(), 1);
}

#[test]
fn t_att_5_the_boot_settles_by_itself_after_15s() {
    let mut p = Policy::new(LAUNCH);
    let a = approval(&SessionUid::mint(), 1);
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(BOOT_SETTLE_FALLBACK_MS - 1))).is_empty());
    let b = approval(&SessionUid::mint(), 1);
    assert_eq!(delivered(&p.queue(&queue(&[a, b]), t(BOOT_SETTLE_FALLBACK_MS))).len(), 1);
}

#[test]
fn wake_grace_in_gilde_is_5s_from_the_wake() {
    let mut p = settled();
    p.sleep(t(10_000));
    // uptime stops in sleep: the wake reads almost the same uptime
    assert!(p.wake(t(10_001)).is_empty());
    let a = approval(&SessionUid::mint(), 1);
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(12_000))).is_empty(), "began during sleep");
    assert_eq!(p.next_deadline(t(12_000)), Some(t(10_001 + WAKE_GRACE_IN_GILDE_MS).up));
    assert!(delivered(&p.tick(t(15_000))).is_empty());
    assert_eq!(delivered(&p.tick(t(15_001))).len(), 1);
}

#[test]
fn wake_grace_watched_is_60s_from_the_wake() {
    let mut p = settled();
    p.sleep(t(10_000));
    p.wake(t(10_001));
    let mut a = watched(approval(&SessionUid::mint(), 1));
    a.waited_ms = 0;
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(12_000))).is_empty());
    // start + 60 s = 72 000 is later than wake + 60 s = 70 001
    assert!(delivered(&p.tick(t(71_999))).is_empty());
    assert_eq!(delivered(&p.tick(t(72_000))).len(), 1);
}

#[test]
fn a_queue_that_arrives_while_asleep_waits_for_the_wake_grace() {
    let mut p = settled();
    p.sleep(t(10_000));
    let a = approval(&SessionUid::mint(), 1);
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(10_200))).is_empty(), "asleep: nothing");
    assert!(delivered(&p.wake(t(10_300))).is_empty(), "the grace starts at the wake");
    assert!(delivered(&p.tick(t(15_299))).is_empty());
    assert_eq!(delivered(&p.tick(t(15_300))).len(), 1);
}

#[test]
fn nothing_is_delivered_while_asleep() {
    let mut p = settled();
    let a = watched(approval(&SessionUid::mint(), 1));
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(1_000))).is_empty());
    p.sleep(t(30_000));
    assert!(delivered(&p.tick(t(61_000))).is_empty(), "due, but asleep");
    assert_eq!(p.next_deadline(t(61_000)), None, "no timer while asleep");
    // an episode that began before sleep is not delayed further by the wake
    assert_eq!(delivered(&p.wake(t(61_500))).len(), 1);
}

// -- T-att-6 --------------------------------------------------------------------

#[test]
fn t_att_6_the_notification_layer_never_emits_a_toast() {
    // toasts are only for the user's own actions; this layer has no toast path
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for dir in ["notify", "macos"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            for needle in ["gilde://toast", "event_names::TOAST", "ToastEvent"] {
                assert!(!text.contains(needle), "{} mentions {needle}", path.display());
            }
        }
    }
}

// -- T-att-7 burst guard --------------------------------------------------------

#[test]
fn t_att_7_burst_guard_at_most_once_per_5_minutes() {
    let mut p = settled();
    let uid = SessionUid::mint();
    let mut n = 0u32;
    let mut enter = |p: &mut Policy, ms: u64| -> Vec<Planned> {
        n += 1;
        let it = approval(&uid, n);
        let out = delivered(&p.queue(&queue(&[it]), t(ms)));
        p.queue(&queue(&[]), t(ms + 500)); // answered
        out
    };
    assert_eq!(enter(&mut p, 0).len(), 1);
    assert_eq!(enter(&mut p, 16_000).len(), 1);
    assert_eq!(enter(&mut p, 32_000).len(), 1);
    // 4th entry in 60 s: bursting, and the last notification was 16 s ago
    assert!(enter(&mut p, 48_000).is_empty());
    assert!(enter(&mut p, 50_000).is_empty());
    // much later: one entry in the last minute → normal again
    let later = enter(&mut p, 6 * MIN);
    assert_eq!(later.len(), 1);
    assert!(matches!(later[0].kind, PlanKind::Single { .. }));
}

#[test]
fn t_att_7_a_bursting_session_gets_the_repeat_text_once() {
    let mut p = settled();
    let uid = SessionUid::mint();
    front(&mut p, true, false);
    p.view_state(view(&[&uid], true));
    // three entries while on screen: no notification, but they count
    for (n, ms) in [(1u32, 0u64), (2, 10_000), (3, 20_000)] {
        assert!(delivered(&p.queue(&queue(&[approval(&uid, n)]), t(ms))).is_empty());
        p.queue(&queue(&[]), t(ms + 500));
    }
    p.view_state(view(&[], true));
    let fourth = delivered(&p.queue(&queue(&[approval(&uid, 4)]), t(30_000)));
    assert_eq!(fourth.len(), 1);
    assert!(matches!(fourth[0].kind, PlanKind::Repeat { .. }));
    p.queue(&queue(&[]), t(30_500));
    assert!(delivered(&p.queue(&queue(&[approval(&uid, 5)]), t(40_000))).is_empty(), "within 5 min");
}

// -- T-att-8 --------------------------------------------------------------------

#[test]
fn t_att_8_nothing_celebrates() {
    let mut p = settled();
    let mut s = p.settings().clone();
    s.sound = true;
    p.set_settings(s);
    let a = approval(&SessionUid::mint(), 1);
    let first = delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(1_000)));
    assert!(first[0].sound, "band 1 with the sound on");
    // the turn finished (the session left the queue): a withdrawal, no delivery
    let fx = p.queue(&queue(&[]), t(2_000));
    assert!(delivered(&fx).is_empty());
    assert_eq!(withdrawn(&fx), vec![id_of(&a)]);
    // a new reply (band 5) is silent
    let reply = with_band(approval(&SessionUid::mint(), 1), 5);
    assert!(p.queue(&queue(&[reply]), t(20_000)).is_empty());
    // band 2 never plays the sound
    let answer = with_band(approval(&SessionUid::mint(), 1), 2);
    let got = delivered(&p.queue(&queue(&[answer]), t(40_000)));
    assert_eq!(got.len(), 1);
    assert!(!got[0].sound);
}

// -- coalescing -----------------------------------------------------------------

#[test]
fn coalescing_merges_a_second_event_within_15s_into_one_notification() {
    let mut p = settled();
    let a = approval(&SessionUid::mint(), 1);
    let b = with_band(approval(&SessionUid::mint(), 1), 2);
    let c = approval(&SessionUid::mint(), 1);
    assert_eq!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(1_000))).len(), 1);
    assert!(delivered(&p.queue(&queue(&[a.clone(), b.clone()]), t(5_000))).is_empty());
    assert_eq!(p.outcome(&key(&b)), Some(Outcome::Held));
    assert!(delivered(&p.queue(&queue(&[a.clone(), c.clone(), b.clone()]), t(9_000))).is_empty());
    assert_eq!(p.next_deadline(t(9_000)), Some(t(16_000).up));
    let fx = p.tick(t(16_000));
    assert_eq!(withdrawn(&fx), vec![id_of(&a)], "the window's own notification is merged");
    let many = delivered(&fx);
    assert_eq!(many.len(), 1);
    assert_eq!(many[0].id, MANY_ID);
    assert_eq!(many[0].target, FocusTarget::Home);
    match &many[0].kind {
        PlanKind::Many { keys } => assert_eq!(keys, &vec![key(&a), key(&c), key(&b)], "queue rank order"),
        other => panic!("{other:?}"),
    }
    assert_eq!(p.click_target(MANY_ID), Some(FocusTarget::Home));
    // a fourth event in the next window joins the merged notification
    let d = approval(&SessionUid::mint(), 1);
    assert!(delivered(&p.queue(&queue(&[a.clone(), c.clone(), b.clone(), d.clone()]), t(20_000))).is_empty());
    let next = delivered(&p.tick(t(31_000)));
    assert_eq!(next.len(), 1);
    match &next[0].kind {
        PlanKind::Many { keys } => assert_eq!(keys.len(), 4),
        other => panic!("{other:?}"),
    }
}

#[test]
fn coalescing_of_one_key_after_the_lead_resolved_sends_it_alone() {
    let mut p = settled();
    let a = approval(&SessionUid::mint(), 1);
    let b = approval(&SessionUid::mint(), 1);
    p.queue(&queue(std::slice::from_ref(&a)), t(0));
    assert!(delivered(&p.queue(&queue(&[a.clone(), b.clone()]), t(5_000))).is_empty());
    let fx = p.queue(&queue(std::slice::from_ref(&b)), t(6_000));
    assert_eq!(withdrawn(&fx), vec![id_of(&a)]);
    let got = delivered(&p.tick(t(15_000)));
    assert_eq!(got.len(), 1);
    assert!(matches!(&got[0].kind, PlanKind::Single { key: k, .. } if *k == key(&b)));
}

#[test]
fn the_flush_rechecks_typing_and_pause() {
    let mut p = settled();
    let a = approval(&SessionUid::mint(), 1);
    let b = approval(&SessionUid::mint(), 1);
    p.queue(&queue(std::slice::from_ref(&a)), t(0));
    p.queue(&queue(&[a.clone(), b.clone()]), t(3_000));
    p.input_activity(t(14_000));
    assert!(delivered(&p.tick(t(15_000))).is_empty());
    assert_eq!(p.outcome(&key(&b)), Some(Outcome::Suppressed(Suppressed::Typing)));
}

#[test]
fn many_carries_the_sound_only_for_a_new_band_one_key() {
    let mut p = settled();
    let mut s = p.settings().clone();
    s.sound = true;
    p.set_settings(s);
    let a = with_band(approval(&SessionUid::mint(), 1), 2);
    let b = with_band(approval(&SessionUid::mint(), 1), 2);
    p.queue(&queue(std::slice::from_ref(&a)), t(0));
    p.queue(&queue(&[a.clone(), b.clone()]), t(1_000));
    let many = delivered(&p.tick(t(15_000)));
    assert!(!many[0].sound, "bands 2 only");
    let c = approval(&SessionUid::mint(), 1);
    p.queue(&queue(&[a.clone(), b.clone(), c]), t(16_000));
    let many = delivered(&p.tick(t(30_000)));
    assert!(many[0].sound, "a new band-1 key");
}

// -- withdrawal -----------------------------------------------------------------

#[test]
fn withdrawal_when_every_key_of_a_notification_resolved() {
    let mut p = settled();
    let a = approval(&SessionUid::mint(), 1);
    let b = approval(&SessionUid::mint(), 1);
    p.queue(&queue(std::slice::from_ref(&a)), t(0));
    p.queue(&queue(&[a.clone(), b.clone()]), t(1_000));
    p.tick(t(15_000)); // merged: gilde.many = {a, b}
    assert!(withdrawn(&p.queue(&queue(std::slice::from_ref(&b)), t(20_000))).is_empty(), "b still waits");
    assert_eq!(withdrawn(&p.queue(&queue(&[]), t(21_000))), vec![MANY_ID.to_string()]);
    assert_eq!(p.click_target(MANY_ID), None);
}

// -- T-att-reentry --------------------------------------------------------------

#[test]
fn t_att_reentry_approval_to_answer_keeps_the_key() {
    // same episode id, band 1 → 2
    let mut p = settled();
    let a = approval(&SessionUid::mint(), 1);
    assert_eq!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(0))).len(), 1);
    let flipped = with_band(a.clone(), 2);
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&flipped)), t(20_000))).is_empty());
    // a new episode id that follows a direct 1 ↔ 2 flip: the same wait
    let mut next = with_band(approval(&a.uid, 2), 1);
    next.episode = ep(&a.uid, 7);
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&next)), t(40_000))).is_empty());
    assert_eq!(p.outcome(&key(&next)), Some(Outcome::Delivered));
    // its withdrawal still names the original notification
    assert_eq!(withdrawn(&p.queue(&queue(&[]), t(50_000))), vec![id_of(&a)]);
}

#[test]
fn t_att_reentry_a_new_episode_in_the_same_band_or_after_a_gap_is_new() {
    let mut p = settled();
    let uid = SessionUid::mint();
    assert_eq!(delivered(&p.queue(&queue(&[approval(&uid, 1)]), t(0))).len(), 1);
    // same band, new episode, consecutive: a new request
    assert_eq!(delivered(&p.queue(&queue(&[approval(&uid, 2)]), t(16_000))).len(), 1);
    // gap, then band 2
    p.queue(&queue(&[]), t(17_000));
    let answer = with_band(approval(&uid, 3), 2);
    assert_eq!(delivered(&p.queue(&queue(&[answer]), t(33_000))).len(), 1);
}

// -- watched (rule 3), pause, later, denied, health ------------------------------

#[test]
fn watched_sessions_wait_60s_of_episode_age_and_respect_the_setting() {
    let mut p = settled();
    let a = watched(approval(&SessionUid::mint(), 1));
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(1_000))).is_empty());
    assert_eq!(p.next_deadline(t(1_000)), Some(t(61_000).up));
    assert!(delivered(&p.tick(t(60_999))).is_empty());
    assert_eq!(delivered(&p.tick(t(61_000))).len(), 1);
    // discovered late: the age counts from `waited_ms`
    let mut b = watched(approval(&SessionUid::mint(), 1));
    b.waited_ms = 30_000;
    p.queue(&queue(&[a.clone(), b.clone()]), t(100_000));
    assert_eq!(p.next_deadline(t(100_000)), Some(t(130_000).up));
    // setting off → never
    let mut q = settled();
    q.set_settings(NotifySettings { watched_sessions: false, sound: false, show_details: false });
    let c = watched(approval(&SessionUid::mint(), 1));
    q.queue(&queue(std::slice::from_ref(&c)), t(1_000));
    assert!(delivered(&q.tick(t(10 * MIN))).is_empty());
    assert_eq!(q.outcome(&key(&c)), Some(Outcome::Suppressed(Suppressed::WatchedOff)));
}

#[test]
fn pause_suppresses_until_it_expires() {
    let mut p = settled();
    p.pause(Some(LAUNCH.wall + 60_000));
    let a = approval(&SessionUid::mint(), 1);
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(1_000))).is_empty());
    assert_eq!(p.outcome(&key(&a)), Some(Outcome::Suppressed(Suppressed::Paused)));
    assert_eq!(p.next_deadline(t(1_000)), Some(t(60_000).up), "the expiry is scheduled");
    p.tick(t(60_000));
    assert_eq!(p.paused_until(), None, "expired");
    let b = approval(&SessionUid::mint(), 1);
    assert_eq!(delivered(&p.queue(&queue(&[a, b]), t(61_000))).len(), 1);
}

#[test]
fn later_items_never_notify() {
    let mut p = settled();
    let mut a = approval(&SessionUid::mint(), 1);
    a.later_until = Some(LAUNCH.wall + 30 * MIN as i64);
    assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(1_000))).is_empty());
    assert_eq!(p.outcome(&key(&a)), Some(Outcome::Suppressed(Suppressed::Later)));
}

#[test]
fn t_notify_denied_policy_side() {
    for auth in [Authorization::Denied, Authorization::NotDetermined] {
        let mut p = settled();
        p.set_authorization(auth);
        let a = approval(&SessionUid::mint(), 1);
        assert!(delivered(&p.queue(&queue(std::slice::from_ref(&a)), t(1_000))).is_empty(), "{auth:?}");
        assert_eq!(p.outcome(&key(&a)), Some(Outcome::Suppressed(Suppressed::Denied)));
        // granted later: no backlog for what was already waiting, new ones notify
        p.set_authorization(Authorization::Granted);
        assert!(delivered(&p.tick(t(2_000))).is_empty());
        let b = approval(&SessionUid::mint(), 1);
        assert_eq!(delivered(&p.queue(&queue(&[a, b]), t(3_000))).len(), 1);
    }
}

#[test]
fn memory_pressure_alerts_on_a_rising_edge_at_most_hourly() {
    let mut p = settled();
    let first = delivered(&p.health_memory(true, t(1_000)));
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id, MEMORY_ID);
    assert!(delivered(&p.health_memory(true, t(2_000))).is_empty(), "still high: no edge");
    p.health_memory(false, t(3_000));
    assert!(delivered(&p.health_memory(true, t(4_000))).is_empty(), "within the hour");
    p.health_memory(false, t(5_000));
    assert_eq!(delivered(&p.health_memory(true, t(1_000 + 60 * MIN))).len(), 1);
    p.health_memory(false, t(2 * 60 * MIN));
    p.pause(Some(LAUNCH.wall + (3 * 60 * MIN) as i64));
    assert!(delivered(&p.health_memory(true, t(2 * 60 * MIN + 1))).is_empty(), "paused");
}

// -- HP-6: an 8 h clock jump -----------------------------------------------------

#[test]
fn hp6_an_8h_jump_produces_no_notification_burst() {
    let mut p = settled();
    let items: Vec<It> = (0..20).map(|_| watched(approval(&SessionUid::mint(), 1))).collect();
    assert!(delivered(&p.queue(&queue(&items), t(1_000))).is_empty());
    let jump = t(1_000 + 8 * 60 * MIN);
    let now = delivered(&p.tick(jump));
    assert_eq!(now.len(), 1, "one notification at the jump");
    let merged = delivered(&p.tick(Now { up: jump.up + COALESCE_MS, wall: jump.wall + COALESCE_MS as i64 }));
    assert_eq!(merged.len(), 1, "then one merged notification");
    match &merged[0].kind {
        PlanKind::Many { keys } => assert_eq!(keys.len(), 20),
        other => panic!("{other:?}"),
    }
}

// -- T-att-hideDetails [F-31] (render) --------------------------------------------

fn labels_for(uids: &[&SessionUid]) -> HashMap<SessionUid, SessionLabels> {
    let mut m = HashMap::new();
    for (i, uid) in uids.iter().enumerate() {
        m.insert(
            (*uid).clone(),
            SessionLabels {
                project: Some("checkout-service".into()),
                session: format!("payment-retry-{i}"),
                detail: None,
                title: Some("Fix the login bug PRIVATE_TITLE".into()),
                summary: Some("npm run migrate --password SECRET_ARG".into()),
            },
        );
    }
    m
}

fn single_for(uid: &SessionUid) -> Planned {
    let a = approval(uid, 1);
    Planned {
        id: id_of(&a),
        kind: PlanKind::Single { key: key(&a), band: 1, reason: AttentionReason::NeedsApproval },
        sound: false,
        target: FocusTarget::Session { uid: uid.clone(), focus_card: true },
    }
}

#[test]
fn t_att_hide_details_by_default() {
    let uid = SessionUid::mint();
    let labels = labels_for(&[&uid]);
    let n = render(&single_for(&uid), &labels, Locale::Ko, false);
    assert_eq!(n.title, "checkout-service");
    assert_eq!(n.body, "payment-retry-0 · 승인 대기");
    let en = render(&single_for(&uid), &labels, Locale::En, false);
    assert_eq!(en.body, "payment-retry-0 · Needs approval");
    for text in [&n.title, &n.body, &en.body] {
        for secret in ["SECRET_ARG", "npm", "PRIVATE_TITLE"] {
            assert!(!text.contains(secret), "{secret} leaked into {text}");
        }
    }
}

#[test]
fn t_att_hide_details_only_the_setting_adds_the_request_line() {
    let uid = SessionUid::mint();
    let labels = labels_for(&[&uid]);
    let n = render(&single_for(&uid), &labels, Locale::Ko, true);
    assert_eq!(n.body, "Fix the login bug PRIVATE_TITLE · 승인 대기\nnpm run migrate --password SECRET_ARG");
}

#[test]
fn t_att_hide_details_merged_repeat_and_reminder_never_carry_request_text() {
    let uids: Vec<SessionUid> = (0..3).map(|_| SessionUid::mint()).collect();
    let labels = labels_for(&uids.iter().collect::<Vec<_>>());
    let keys: Vec<EpisodeKey> = uids.iter().map(|u| key(&approval(u, 1))).collect();
    let many = Planned { id: MANY_ID.into(), kind: PlanKind::Many { keys: keys.clone() }, sound: false, target: FocusTarget::Home };
    let n = render(&many, &labels, Locale::En, false);
    assert_eq!(n.title, "3 sessions are waiting for you.");
    assert_eq!(n.body, "payment-retry-0, payment-retry-1, …");
    let ko = render(&many, &labels, Locale::Ko, false);
    assert_eq!(ko.title, "세션 3개가 기다리고 있습니다.");
    let repeat = Planned {
        id: "x".into(),
        kind: PlanKind::Repeat { key: keys[0].clone(), band: 1, reason: AttentionReason::NeedsApproval },
        sound: false,
        target: FocusTarget::Home,
    };
    let reminder = Planned {
        id: "y".into(),
        kind: PlanKind::Reminder { key: keys[0].clone(), band: 1, reason: AttentionReason::NeedsApproval, waited_ms: 20 * MIN },
        sound: false,
        target: FocusTarget::Home,
    };
    let r = render(&repeat, &labels, Locale::En, false);
    assert_eq!(r.body, "payment-retry-0 needs you repeatedly.");
    let rem = render(&reminder, &labels, Locale::Ko, false);
    assert_eq!(rem.body, "payment-retry-0 · 승인 대기\n아직 기다리는 중 · 20분");
    for planned in [&many, &repeat, &reminder] {
        let shown = render(planned, &labels, Locale::En, true);
        assert!(!shown.body.contains("SECRET_ARG") && !shown.title.contains("SECRET_ARG"));
    }
}

#[test]
fn rendered_text_is_sanitized() {
    let uid = SessionUid::mint();
    let mut labels = labels_for(&[&uid]);
    labels.get_mut(&uid).unwrap().project = Some("evil\u{1b}[31mproject\u{7}".into());
    labels.get_mut(&uid).unwrap().session = "a\u{202e}b".into();
    let n = render(&single_for(&uid), &labels, Locale::En, false);
    assert!(!n.title.contains('\u{1b}') && !n.title.contains('\u{7}'), "{:?}", n.title);
    assert!(!n.body.contains('\u{202e}'), "{:?}", n.body);
}

#[test]
fn missing_labels_fall_back_to_the_app_name() {
    let uid = SessionUid::mint();
    let n = render(&single_for(&uid), &HashMap::new(), Locale::En, true);
    assert_eq!(n.title, "Gilde");
    assert_eq!(n.body, "Gilde · Needs approval");
}

#[test]
fn every_band_one_to_three_reason_has_its_state_word() {
    use gilde_code_lib::contract::activity::DidntStartReason;
    use gilde_code_lib::notify::state_word;
    let cases = [
        (AttentionReason::NeedsApproval, "승인 대기"),
        (AttentionReason::NeedsAnswer, "답변 대기"),
        (AttentionReason::Error { sub: ErrorSub::UsageLimit { account_id: None, resets_at: None } }, "한도 도달"),
        (AttentionReason::Error { sub: ErrorSub::SignInNeeded }, "로그인 필요"),
        (AttentionReason::Error { sub: ErrorSub::DidntStart { reason: DidntStartReason::Timeout } }, "시작 실패"),
        (AttentionReason::Error { sub: ErrorSub::AgentError }, "에이전트 오류"),
        (AttentionReason::Crashed, "비정상 종료"),
    ];
    for (reason, word) in cases {
        assert_eq!(state_word(&reason, Locale::Ko), word);
    }
}
