//! T-world-* (identity side, §3.3): resolution rules 1–5, attach, merges
//! (the older persisted uid survives; the tombstone is `mergedInto` for
//! 10 min), rekeys (a uid is never reused for another key), Gilde's own
//! processes never shown as Watching (T-world-ownDedupe, PM-22/PM-24), the
//! teammate hold (T-world-teammateHold), and World as T03's `UidResolver`
//! (A11.4).

#[path = "world_support.rs"]
mod support;

use std::sync::Arc;

use gilde_code_lib::contract::activity::{ActivityState, RegStatus};
use gilde_code_lib::contract::common::Vendor;
use gilde_code_lib::contract::ids::AccountId;
use gilde_code_lib::contract::services::{
    IdentityHint, IdentityKey, SessionMetaFacts, SourceFacts, TeammateLabels, WorldApi,
};
use gilde_code_lib::contract::session::{Control, ControlMode, Origin};
use gilde_code_lib::contract::store::{Entity, EntityKind, StoreOp};
use gilde_code_lib::discovery::history::UidResolver;
use gilde_code_lib::world::MemoryMetaStore;
use support::*;

fn key(vsid: &str) -> IdentityKey {
    IdentityKey { vendor: Vendor::Claude, account_id: Some(account()), vendor_session_id: vsid.to_string() }
}

fn watching_rows(rig: &Rig) -> Vec<String> {
    rig.patches
        .0
        .lock()
        .iter()
        .flat_map(|p| p.ops.clone())
        .filter_map(|o| match o {
            StoreOp::Upsert { entity } => match *entity {
                Entity::Session { value } if matches!(value.control, Control::Watching { .. }) => Some(value.uid.as_str().to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn a_process_only_session_attaches_its_vendor_id() {
    let rig = Rig::new();
    rig.subscribe();
    // Rule 5: the process first (no registry row yet).
    let p = claude_hint(None, Some(7001));
    rig.ev(&p, alive(7001));
    rig.pump();
    let uids = rig.snapshot_uids();
    assert_eq!(uids.len(), 1);
    let provisional = uids[0].clone();
    // Rule 4: the same process with its id → attached, same uid.
    rig.ev(&claude_hint(Some(&sid(1)), Some(7001)), registry(RegStatus::Busy, rig.now(), None));
    rig.pump();
    assert_eq!(rig.snapshot_uids(), vec![provisional.clone()]);
    assert_eq!(rig.world.key_of(&provisional), Some(key(&sid(1))));
    assert_eq!(rig.session(&provisional).vendor_session_id.as_deref(), Some(sid(1).as_str()));
    // Rule 2: by key.
    assert_eq!(rig.uid_of(&sid(1)), provisional);
}

#[test]
fn the_older_persisted_uid_survives_and_the_tombstone_expires() {
    let meta = Arc::new(MemoryMetaStore::default());
    // A session persisted by an earlier run.
    let old = {
        let rig = Rig::with_meta(meta.clone());
        rig.uid_of(&sid(2))
    };
    let rig = Rig::with_meta(meta);
    rig.subscribe();
    // A provisional process-only session ...
    rig.ev(&claude_hint(None, Some(7002)), alive(7002));
    rig.pump();
    let provisional = rig.snapshot_uids()[0].clone();
    assert_ne!(provisional, old);
    // ... learns the persisted id: the older uid survives, the newer is a
    // tombstone the WebView remaps.
    rig.ev(&claude_hint(Some(&sid(2)), Some(7002)), registry(RegStatus::Idle, rig.now(), None));
    rig.pump();
    let tomb = rig.session(&provisional);
    assert_eq!(tomb.merged_into, Some(old.clone()));
    assert!(matches!(rig.session(&old).control, Control::Watching { .. }));
    assert_eq!(rig.world.session(&provisional).unwrap().merged_into, Some(old.clone()));
    // 10 min later the tombstone is removed.
    rig.advance(600_001);
    let p = rig.pump();
    let removed = p.iter().flat_map(|x| x.ops.clone()).any(|o| matches!(o, StoreOp::Remove { entity_kind: EntityKind::Session, id } if id == provisional.as_str()));
    assert!(removed);
}

#[test]
fn a_new_session_id_in_the_same_process_is_a_new_uid() {
    let rig = Rig::new();
    rig.subscribe();
    let h1 = claude_hint(Some(&sid(3)), Some(7003));
    rig.ev(&h1, alive(7003));
    rig.ev(&h1, registry(RegStatus::Idle, rig.now(), None));
    rig.pump();
    let first = rig.uid_of(&sid(3));
    // `/clear` in the same process: a new session id.
    let h2 = claude_hint(Some(&sid(4)), Some(7003));
    rig.ev(&h2, registry(RegStatus::Busy, rig.now(), None));
    rig.pump();
    let second = rig.uid_of(&sid(4));
    assert_ne!(first, second, "a uid is never reused for a different key");
    assert_eq!(rig.world.key_of(&first), Some(key(&sid(3))));
    assert_eq!(rig.state(&first), ActivityState::Ended);
    assert!(matches!(rig.session(&second).control, Control::Watching { .. }));
}

#[test]
fn a_conversation_copy_attaches_its_new_id() {
    let rig = Rig::new();
    rig.subscribe();
    let source = rig.uid_of(&sid(5));
    // The copy is spawned with its own uid; system/init reports the new id.
    let copy = rig.world.bind_spawn("sp-copy", None, Vendor::Claude, Some(account()));
    let t = rig.now();
    rig.ev(&spawn_hint("sp-copy", None, Some(7005)), SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t))));
    rig.ev(&spawn_hint("sp-copy", Some(&sid(6)), Some(7005)), alive(7005));
    rig.pump();
    assert_ne!(copy, source);
    assert_eq!(rig.world.key_of(&copy), Some(key(&sid(6))), "an attach, never a merge");
    assert!(rig.session(&copy).merged_into.is_none());
}

#[test]
fn t_world_own_dedupe_including_planned_rows() {
    let rig = Rig::new();
    rig.subscribe();
    // Discovery sees Gilde's own process before its Gilde facts (the PID
    // registry row is matched, the uid is not bound yet): never shown.
    let own = IdentityHint { gilde_spawn_id: Some("sp-own".into()), ..claude_hint(Some(&sid(7)), Some(7007)) };
    rig.ev(&own, alive(7007));
    let mut meta = SessionMetaFacts { origin: Some(Origin::Headless), ..SessionMetaFacts::default() };
    meta.claude_version = Some("2.1.307".into());
    rig.ev(&own, SourceFacts::Meta(Box::new(meta)));
    rig.ev(&own, registry(RegStatus::Idle, rig.now(), None));
    rig.pump();
    assert!(rig.world.snapshot().sessions.is_empty(), "not even for one scan");
    // The session actor's facts arrive: still hidden while the spawn id is
    // unbound (A20.1) ...
    let t = rig.now();
    rig.ev(&own, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t))));
    rig.pump();
    assert!(rig.world.snapshot().sessions.is_empty());
    // ... and In Gilde once T10 binds it right after the spawn returns.
    let adopted = rig.world.bind_spawn("sp-own", None, Vendor::Claude, Some(account()));
    rig.pump();
    let s = rig.world.snapshot().sessions;
    assert_eq!(s[0].uid, adopted, "the unbound row's uid is adopted");
    assert_eq!(s.len(), 1);
    assert!(matches!(s[0].control, Control::InGilde { .. }));
    // A planned row: the spawn is bound before the process exists; the
    // registry row then resolves by rule 1.
    let planned = rig.world.bind_spawn("sp-planned", None, Vendor::Claude, Some(account()));
    let ph = spawn_hint("sp-planned", Some(&sid(8)), Some(7008));
    rig.ev(&ph, registry(RegStatus::Idle, rig.now(), None));
    rig.ev(&ph, alive(7008));
    let t = rig.now();
    rig.ev(&ph, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, Some(t))));
    rig.pump();
    assert_eq!(rig.uid_of(&sid(8)), planned);
    assert!(watching_rows(&rig).is_empty(), "Gilde's own sessions are never Watching (PM-24)");
}

#[test]
fn t_world_teammate_hold() {
    let rig = Rig::new();
    rig.subscribe();
    // A teammate's update without an account (its account is its lead's).
    let teammate = IdentityHint {
        vendor: Some(Vendor::Claude),
        account_id: None,
        vendor_session_id: Some(sid(9)),
        process: None,
        gilde_spawn_id: None,
    };
    let labels = TeammateLabels {
        agent_id: Some("alpha@session-25ae1157".into()),
        agent_name: Some("alpha".into()),
        team_name: Some("session-25ae1157".into()),
        agent_color: Some("blue".into()),
        parent_session_id: Some(sid(10)),
    };
    rig.ev(&teammate, SourceFacts::Meta(Box::new(SessionMetaFacts { teammate: Some(labels.clone()), ..SessionMetaFacts::default() })));
    rig.pump();
    assert!(rig.world.snapshot().sessions.is_empty(), "held for the lead");
    // The lead appears within 10 s: the teammate resolves under its account.
    rig.advance(3_000);
    let lead = claude_hint(Some(&sid(10)), Some(7010));
    rig.ev(&lead, alive(7010));
    rig.ev(&lead, registry(RegStatus::Busy, rig.now(), None));
    rig.ev(&teammate, alive(7009));
    rig.ev(&teammate, SourceFacts::Meta(Box::new(SessionMetaFacts { teammate: Some(labels), ..SessionMetaFacts::default() })));
    rig.pump();
    let uid = rig.uid_of(&sid(9));
    assert_eq!(rig.session(&uid).account_id, Some(account()));
    // A teammate whose lead never appears is filed under account "unknown"
    // after 10 s.
    let orphan = IdentityHint { vendor_session_id: Some(sid(11)), ..teammate.clone() };
    rig.ev(&orphan, alive(7011));
    rig.advance(9_000);
    rig.pump();
    assert!(!rig.world.snapshot().sessions.iter().any(|s| s.vendor_session_id.as_deref() == Some(sid(11).as_str())));
    rig.advance(1_001);
    rig.pump();
    let filed = rig.world.snapshot().sessions.into_iter().find(|s| s.vendor_session_id.as_deref() == Some(sid(11).as_str()));
    let filed = filed.expect("filed after the hold");
    assert_eq!(filed.account_id, None);
}

#[test]
fn world_is_the_uid_resolver_and_persists_on_first_sight() {
    let meta = Arc::new(MemoryMetaStore::default());
    let rig = Rig::with_meta(meta.clone());
    let r = rig.world.uid_resolver();
    let k = key(&sid(12));
    let uid = r.uid_for(&k);
    assert_eq!(r.uid_for(&k), uid, "stable");
    assert_eq!(r.key_of(&uid), Some(k.clone()));
    assert!(meta.rows().iter().any(|row| row.uid == uid && row.vendor_session_id.as_deref() == Some(sid(12).as_str())));
    // Live evidence for that history row resolves to the same uid.
    rig.ev(&claude_hint(Some(&sid(12)), Some(7012)), alive(7012));
    rig.pump();
    assert_eq!(rig.world.snapshot().sessions[0].uid, uid);
    // Another account is another key.
    let other = IdentityKey { account_id: Some(AccountId::derive(Vendor::Claude, "/Users/tester/.claude-g")), ..k };
    assert_ne!(r.uid_for(&other), uid);
}

#[tokio::test]
async fn identity_lock_serializes_one_key() {
    let rig = Rig::new();
    let g = rig.world.identity_lock(key(&sid(13))).await;
    let w = rig.world.clone();
    let k = key(&sid(13));
    let t = tokio::spawn(async move {
        let _g = w.identity_lock(k).await;
        1
    });
    tokio::task::yield_now().await;
    assert!(!t.is_finished(), "held");
    // Another key is independent.
    let _other = rig.world.identity_lock(key(&sid(14))).await;
    drop(g);
    assert_eq!(t.await.unwrap(), 1);
}

#[test]
fn t_world_bind_after_evidence() {
    use gilde_code_lib::contract::activity::{ProcessFacts, StopCause};
    let meta = Arc::new(MemoryMetaStore::default());
    let rig = Rig::with_meta(meta);
    rig.subscribe();
    let shown = |rig: &Rig| -> Vec<String> {
        rig.patches.0.lock().iter().flat_map(|p| p.ops.clone()).filter_map(|o| match o {
            StoreOp::Upsert { entity } => match *entity {
                Entity::Session { value } => Some(value.uid.as_str().to_string()),
                _ => None,
            },
            _ => None,
        }).collect()
    };

    // Resume, evidence first. An ended session S of this run (stopped by the user).
    let s = rig.uid_of(&sid(30));
    let first = spawn_hint("sp-r0", Some(&sid(30)), Some(7600));
    rig.world.bind_spawn("sp-r0", Some(&s), Vendor::Claude, Some(account()));
    let t = rig.now();
    let mut g = gilde(t, ControlMode::Terminal, Some(t + 100));
    rig.ev(&first, SourceFacts::Gilde(g.clone()));
    rig.ev(&first, alive(7600));
    rig.pump();
    rig.advance(1_000);
    g.stop_cause = Some(StopCause::User);
    rig.ev(&first, SourceFacts::Gilde(g));
    rig.ev(&first, SourceFacts::Process(ProcessFacts::Exited { at: rig.now(), code: Some(143), signal: None }));
    rig.pump();
    assert_eq!(rig.state(&s), ActivityState::Stopped);
    // Resume: the new process is scanned before T10 binds its spawn id.
    rig.advance(1_000);
    let resumed = IdentityHint { gilde_spawn_id: Some("sp-r1".into()), ..claude_hint(None, Some(7601)) };
    rig.ev(&resumed, alive(7601));
    rig.ev(&resumed, registry(RegStatus::Idle, rig.now(), None));
    rig.pump();
    let before = rig.world.snapshot().sessions.len();
    assert_eq!(before, 1, "the hidden row is not in the store");
    let got = rig.world.bind_spawn("sp-r1", Some(&s), Vendor::Claude, Some(account()));
    assert_eq!(got, s, "bind_spawn(Some(uid)) always returns uid: Resume keeps its uid");
    rig.pump();
    assert_eq!(rig.state(&s), ActivityState::Stopped, "held until the new spawn's Gilde facts: no flash");
    let t = rig.now();
    rig.ev(&IdentityHint { gilde_spawn_id: Some("sp-r1".into()), ..claude_hint(Some(&sid(30)), Some(7601)) }, SourceFacts::Gilde(gilde(t, ControlMode::Terminal, None)));
    rig.pump();
    let sess = rig.session(&s);
    assert!(matches!(sess.control, Control::InGilde { .. }), "the absorbed evidence is kept: the new process is alive");
    assert!(matches!(sess.activity.state, ActivityState::Starting | ActivityState::Ready | ActivityState::Working));
    assert_eq!(rig.world.snapshot().sessions.len(), 1, "no duplicate row");
    // Later evidence of that spawn resolves to S (rule 1).
    rig.ev(&resumed, SourceFacts::Process(ProcessFacts::Exited { at: rig.now(), code: Some(0), signal: None }));
    rig.pump();
    assert!(matches!(rig.session(&s).control, Control::NotRunning { .. }));

    // A new session, evidence first: bind_spawn(None) adopts the row's uid.
    let fresh = IdentityHint { gilde_spawn_id: Some("sp-n1".into()), ..claude_hint(None, Some(7602)) };
    rig.ev(&fresh, alive(7602));
    rig.pump();
    assert_eq!(rig.world.snapshot().sessions.len(), 1, "hidden while unbound");
    let n = rig.world.bind_spawn("sp-n1", None, Vendor::Claude, Some(account()));
    let t = rig.now();
    rig.ev(&fresh, SourceFacts::Gilde(gilde(t, ControlMode::Conversation, None)));
    rig.pump();
    assert_eq!(rig.state(&n), ActivityState::Starting);
    assert_eq!(rig.world.snapshot().sessions.len(), 2);
    // Gilde facts that beat the bind are hidden too, then adopted.
    let early = IdentityHint { gilde_spawn_id: Some("sp-n2".into()), ..claude_hint(None, Some(7603)) };
    rig.ev(&early, SourceFacts::Gilde(gilde(rig.now(), ControlMode::Conversation, None)));
    rig.ev(&early, alive(7603));
    rig.pump();
    assert_eq!(rig.world.snapshot().sessions.len(), 2);
    let n2 = rig.world.bind_spawn("sp-n2", None, Vendor::Claude, Some(account()));
    rig.pump();
    assert_eq!(rig.state(&n2), ActivityState::Starting);

    // No visible duplicate and nothing shown as Watching in any patch.
    let ups = shown(&rig);
    let mut distinct = ups.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 3, "only S, n and n2 were ever published: {distinct:?}");
    assert!(watching_rows(&rig).is_empty());
}
