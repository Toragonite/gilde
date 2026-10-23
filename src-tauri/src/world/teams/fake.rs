//! The fake adapter with the all-Native descriptor (§7.3.4); `cfg(test)`/`showcase` only.
//!
//! Owner: T02. Scripted events and the all-`Native` descriptor of the v1.x
//! emulated engine (§7.3.3), so Board, Floor and Needs you are proven against
//! a fully capable team before that engine exists. Never registered in a
//! release build (the `mod` line is behind `cfg(any(test, feature =
//! "showcase"))`).
//!
//! It satisfies the §7.1.4 rules by construction: it has no I/O at all; every
//! act returns `Done` (the effect — a native act — is the act itself) and is
//! recorded, and its effect is *observed* only when the script emits the
//! corresponding event; missing data stays `None`/`Unknown`.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::contract::ids::{MemberKey, RequestId, TeamKey};
use crate::contract::interaction::Decision;
use crate::contract::team::{
    ActOutcome, MemberSpec, StopHow, TaskOp, TeamAdapter, TeamCapabilities, TeamEvent, TeamEventSink, TeamSource,
};

/// One recorded act.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeAct {
    Send { team: TeamKey, to: MemberKey, text_len: usize },
    Broadcast { team: TeamKey, text_len: usize },
    Spawn { team: TeamKey, name: String },
    Stop { team: TeamKey, member: MemberKey, how: StopHow },
    AnswerApproval { team: TeamKey, req: RequestId },
    TaskWrite { team: TeamKey },
}

/// A scripted team adapter.
#[derive(Default)]
pub struct FakeTeamAdapter {
    sink: Mutex<Option<Arc<dyn TeamEventSink>>>,
    script: Mutex<Vec<TeamEvent>>,
    acts: Mutex<Vec<FakeAct>>,
    source: Option<TeamSource>,
}

impl std::fmt::Debug for FakeTeamAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeTeamAdapter").field("pending", &self.script.lock().len()).finish()
    }
}

impl FakeTeamAdapter {
    pub fn new() -> FakeTeamAdapter {
        FakeTeamAdapter::default()
    }

    /// An adapter that reports another source (default `Emulated`).
    pub fn with_source(source: TeamSource) -> FakeTeamAdapter {
        FakeTeamAdapter { source: Some(source), ..FakeTeamAdapter::default() }
    }

    /// Queue events; [`FakeTeamAdapter::play`] emits them.
    pub fn script(&self, events: Vec<TeamEvent>) {
        self.script.lock().extend(events);
    }

    /// Emit every queued event (in order), with no lock held.
    pub fn play(&self) {
        let events: Vec<TeamEvent> = std::mem::take(&mut *self.script.lock());
        let sink = self.sink.lock().clone();
        if let Some(s) = sink {
            for e in events {
                s.emit(e);
            }
        }
    }

    /// Emit one event now.
    pub fn emit(&self, event: TeamEvent) {
        let sink = self.sink.lock().clone();
        if let Some(s) = sink {
            s.emit(event);
        }
    }

    pub fn acts(&self) -> Vec<FakeAct> {
        self.acts.lock().clone()
    }

    fn record(&self, a: FakeAct) -> ActOutcome {
        self.acts.lock().push(a);
        ActOutcome::Done
    }
}

impl TeamAdapter for FakeTeamAdapter {
    fn source(&self) -> TeamSource {
        self.source.unwrap_or(TeamSource::Emulated)
    }
    fn capabilities(&self, _team: &TeamKey) -> TeamCapabilities {
        TeamCapabilities::all_native()
    }
    fn subscribe(&self, sink: Arc<dyn TeamEventSink>) {
        *self.sink.lock() = Some(sink);
    }
    fn send(&self, team: &TeamKey, to: &MemberKey, text: &str) -> ActOutcome {
        self.record(FakeAct::Send { team: team.clone(), to: to.clone(), text_len: text.len() })
    }
    fn broadcast(&self, team: &TeamKey, text: &str) -> ActOutcome {
        self.record(FakeAct::Broadcast { team: team.clone(), text_len: text.len() })
    }
    fn spawn(&self, team: &TeamKey, spec: &MemberSpec) -> ActOutcome {
        self.record(FakeAct::Spawn { team: team.clone(), name: spec.name.clone() })
    }
    fn stop(&self, team: &TeamKey, member: &MemberKey, how: StopHow) -> ActOutcome {
        self.record(FakeAct::Stop { team: team.clone(), member: member.clone(), how })
    }
    fn answer_approval(&self, team: &TeamKey, req: &RequestId, _d: &Decision) -> ActOutcome {
        self.record(FakeAct::AnswerApproval { team: team.clone(), req: req.clone() })
    }
    fn task_write(&self, team: &TeamKey, _op: TaskOp) -> ActOutcome {
        self.record(FakeAct::TaskWrite { team: team.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::common::Vendor;
    use crate::contract::ids::{MessageKey, SessionUid};
    use crate::contract::services::WorldApi;
    use crate::contract::session::MemberRole;
    use crate::contract::team::{
        DeliveryStatus, LeadBinding, Member, MemberBackend, MessageKind, Party, Presence, Support, TeamMessage,
    };
    use crate::world::{ManualClock, MemoryMetaStore, World, WorldConfig, WorldSettings};

    fn world() -> World {
        World::new(WorldConfig {
            clock: Arc::new(ManualClock::new(1_792_749_600_000, 1_000_000)),
            meta_store: Arc::new(MemoryMetaStore::default()),
            settings: WorldSettings::default(),
            home: None,
        })
    }

    fn member(team: &TeamKey, name: &str, role: MemberRole, joined: i64) -> Member {
        Member {
            key: MemberKey::derive(team, &format!("{name}@fake"), Some(joined)),
            team_key: team.clone(),
            session_uid: None,
            name: name.into(),
            agent_id: Some(format!("{name}@fake")),
            role,
            colour: None,
            vendor: Vendor::Claude,
            backend: MemberBackend::Emulated,
            joined_at: Some(joined),
            presence: Presence::Present,
            pane: None,
            plan_mode_required: false,
            current_task: None,
        }
    }

    #[test]
    fn the_fake_team_is_fully_capable_and_acts_are_observed_not_acknowledged() {
        let w = world();
        let fake = Arc::new(FakeTeamAdapter::new());
        w.register_team_adapter(fake.clone());
        let team = TeamKey::derive("emulated", None, "fake-1");
        let lead = member(&team, "lead", MemberRole::Lead, 1);
        let a = member(&team, "a", MemberRole::Member, 2);
        fake.script(vec![
            TeamEvent::TeamSeen {
                key: team.clone(),
                name: "fake".into(),
                raw_id: "fake-1".into(),
                project: None,
                lead: Some(lead.key.clone()),
                binding: LeadBinding::None,
            },
            TeamEvent::MemberUpserted(lead.clone()),
            TeamEvent::MemberUpserted(a.clone()),
            TeamEvent::InFlight { team: team.clone(), count: Some(3) },
        ]);
        fake.play();
        let snap = w.snapshot();
        assert_eq!(snap.teams.len(), 1);
        let t = &snap.teams[0];
        assert_eq!(t.caps, TeamCapabilities::all_native());
        assert_eq!(t.caps.observe_tasks, Support::Yes);
        assert_eq!(t.progress.in_flight, Some(3));
        assert_eq!(snap.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["lead", "a"]);
        // An act is Done and recorded; nothing appears until it is observed.
        assert_eq!(fake.send(&team, &a.key, "hello"), ActOutcome::Done);
        assert_eq!(fake.acts(), vec![FakeAct::Send { team: team.clone(), to: a.key.clone(), text_len: 5 }]);
        assert!(w.snapshot().messages.is_empty());
        let m = TeamMessage {
            key: MessageKey::derive(&team, "m1"),
            team_key: team.clone(),
            from: Party::User,
            to: Party::Member { member: a.key.clone() },
            at: 1_792_749_600_000,
            kind: MessageKind::Plain,
            status: DeliveryStatus::Sent,
            in_reply_to: None,
            derived: false,
            idle_reason: None,
            has_summary: false,
        };
        fake.emit(TeamEvent::MessageObserved(m.clone()));
        fake.emit(TeamEvent::MessageStatus { key: m.key.clone(), status: DeliveryStatus::Delivered });
        let msgs = w.snapshot().messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].status, DeliveryStatus::Delivered);
        // Gone members stay visible.
        fake.emit(TeamEvent::MemberGone { key: a.key.clone(), reason: crate::contract::team::GoneReason::ShutdownApproved, at: 5 });
        let gone = w.snapshot().members.into_iter().find(|x| x.key == a.key).unwrap();
        assert!(matches!(gone.presence, Presence::Gone { .. }));
        let _ = SessionUid::mint();
    }
}
