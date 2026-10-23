//! A45.2 → World: the persisted UI keys World reads (`sidebarOrder`,
//! `sidebarFolds`, `splitSet`) reach it in exactly the shapes `persist.ts`
//! sends and `ui_state_set` accepts — at boot (the persisted rows, through
//! `Lifecycle::load_ui_state`) and on a later write. Owner: T42 (the T09
//! `lifecycle::wiring::apply_ui_key` reader).
//!
//! Synthetic facts only (the `world_*` rig): no vendor CLI, no vendor file.

#[path = "world_support.rs"]
mod support;

use gilde_code_lib::contract::activity::{ActivityState, RegStatus};
use gilde_code_lib::contract::common::AbsPath;
use gilde_code_lib::contract::ids::{LocationId, ProjectId, SessionUid};
use gilde_code_lib::contract::services::{LocationFacts, WorldApi as _};
use gilde_code_lib::contract::store::{EntityKind, StoreOp};
use gilde_code_lib::lifecycle::boot::Lifecycle;
use gilde_code_lib::lifecycle::wiring::apply_ui_key;
use serde_json::{json, Value};
use support::*;

fn live_session(rig: &Rig, n: u32) -> SessionUid {
    let h = claude_hint(Some(&sid(n)), Some(6000 + n));
    rig.ev(&h, alive(6000 + n as i32));
    rig.ev(&h, registry(RegStatus::Idle, rig.now(), None));
    rig.uid_of(&sid(n))
}

fn project(rig: &Rig, root: &str) -> ProjectId {
    let pid = ProjectId::derive(&format!("{root}/.git"));
    rig.sink.location(LocationFacts {
        project_id: pid.clone(),
        project_root: AbsPath::parse(root).unwrap(),
        git: true,
        location_id: LocationId::derive(root),
        location_path: AbsPath::parse(root).unwrap(),
        worktree: false,
        branch: Some("main".into()),
    });
    pid
}

fn folded(rig: &Rig, pid: &ProjectId) -> bool {
    rig.world.snapshot().projects.iter().find(|p| p.id == *pid).expect("project").folded
}

fn removed_sessions(rig: &Rig) -> Vec<String> {
    rig.pump()
        .iter()
        .flat_map(|p| p.ops.clone())
        .filter_map(|o| match o {
            StoreOp::Remove { entity_kind: EntityKind::Session, id } => Some(id),
            _ => None,
        })
        .collect()
}

/// What boot does with the persisted rows: load them (schema-checked), then
/// hand every key to World (`wiring::apply_ui_state`).
fn boot_apply(rig: &Rig, rows: Vec<(&str, Value)>) {
    let lifecycle = Lifecycle::new("0.0.0-test", "build-test");
    let gone = lifecycle.load_ui_state(rows.into_iter().map(|(k, v)| (k.to_string(), Some(v))).collect());
    assert!(gone.is_empty(), "every row is a valid A45.2 value: {gone:?}");
    for (key, value) in lifecycle.ui_state() {
        apply_ui_key(&rig.world, &key, &value);
    }
}

#[test]
fn a_persisted_split_set_and_folds_reach_world() {
    let rig = Rig::new();
    rig.subscribe();
    let a = project(&rig, "/Users/tester/alpha");
    let b = project(&rig, "/Users/tester/beta");
    let in_split = live_session(&rig, 41);
    let other = live_session(&rig, 42);
    rig.pump();

    // Exactly what persist.ts sends.
    boot_apply(
        &rig,
        vec![
            ("splitSet", json!({"uids": [in_split.as_str()], "focused": in_split.as_str()})),
            (
                "sidebarFolds",
                json!({
                    format!("p:{}", a.as_str()): false,
                    format!("p:{}", b.as_str()): true,
                    format!("s:{}", other.as_str()): false,
                    "earlier": false,
                }),
            ),
            ("sidebarOrder", json!([b.as_str(), a.as_str()])),
        ],
    );
    rig.pump();
    assert!(folded(&rig, &a), "p:<alpha> = false (collapsed) → folded");
    assert!(!folded(&rig, &b), "p:<beta> = true (expanded) → not folded");
    let order: Vec<ProjectId> = {
        let mut ps = rig.world.snapshot().projects;
        ps.sort_by_key(|p| p.order);
        ps.into_iter().map(|p| p.id).collect()
    };
    assert_eq!(order, vec![b.clone(), a.clone()], "the user's order");

    // The split set is a reference: at midnight the ended session in Split
    // stays, the other one leaves the store.
    let t = rig.now();
    rig.ev(&claude_hint(Some(&sid(41)), Some(6041)), gone(t));
    rig.ev(&claude_hint(Some(&sid(42)), Some(6042)), gone(t));
    rig.pump();
    assert_eq!(rig.state(&in_split), ActivityState::Ended);
    rig.advance(14 * 3_600_000 + 1);
    assert_eq!(removed_sessions(&rig), vec![other.as_str().to_string()]);
    assert!(rig.snapshot_uids().contains(&in_split));

    // A later write (the `ui_state_set` listener) replaces both.
    apply_ui_key(&rig.world, "sidebarFolds", &json!({ format!("p:{}", a.as_str()): true }));
    apply_ui_key(&rig.world, "splitSet", &json!({"uids": [], "focused": null}));
    assert!(!folded(&rig, &a));
    assert_eq!(removed_sessions(&rig), vec![in_split.as_str().to_string()], "released from Split, it goes");
}

#[test]
fn shapes_outside_the_schemas_set_nothing() {
    let rig = Rig::new();
    rig.subscribe();
    let a = project(&rig, "/Users/tester/alpha");
    let s = live_session(&rig, 51);
    rig.pump();
    apply_ui_key(&rig.world, "sidebarFolds", &json!({ format!("p:{}", a.as_str()): false }));
    apply_ui_key(&rig.world, "splitSet", &json!({"uids": [s.as_str()], "focused": null}));
    rig.pump();
    assert!(folded(&rig, &a));
    // The pre-A45.2 array shapes, an unknown field, a bad id: ignored, the
    // last valid values stay.
    for (key, bad) in [
        ("sidebarFolds", json!([a.as_str()])),
        ("sidebarFolds", json!({"p:not-a-project": false})),
        ("splitSet", json!([s.as_str()])),
        ("splitSet", json!({"uids": [s.as_str()], "focused": null, "note": "x"})),
        ("sidebarOrder", json!([{"id": a.as_str()}])),
    ] {
        apply_ui_key(&rig.world, key, &bad);
        apply_ui_key(&rig.world, &format!("ui.{key}"), &bad);
    }
    rig.pump();
    assert!(folded(&rig, &a), "an invalid value never clears the folds");
    let t = rig.now();
    rig.ev(&claude_hint(Some(&sid(51)), Some(6051)), gone(t));
    rig.pump();
    rig.advance(14 * 3_600_000 + 1);
    assert!(removed_sessions(&rig).is_empty(), "the Split reference is still held");
}
