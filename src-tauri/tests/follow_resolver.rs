//! The World-backed source resolver of the ContentHub (contract §4.2.4,
//! §6.2.5, §6.3, §6.4): which source a session's conversation view reads,
//! and `UNSUPPORTED{noReadableConversation}` for cursor/agy sessions started
//! elsewhere and codex without a mapping. Owner: T04.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gilde_code_lib::contract::attention::AttentionQueue;
use gilde_code_lib::contract::common::{AbsPath, Notice, Vendor};
use gilde_code_lib::contract::content::ConvoSourceKind;
use gilde_code_lib::contract::error::IpcError;
use gilde_code_lib::contract::ids::SessionUid;
use gilde_code_lib::contract::ipc::DismissWhat;
use gilde_code_lib::contract::services::{EvidenceSink, IdentityKey, IdentityLock, StorePatchSink, WorldApi};
use gilde_code_lib::contract::session::{Session, SessionDetails};
use gilde_code_lib::contract::store::StoreSnapshot;
use gilde_code_lib::contract::team::TeamAdapter;
use gilde_code_lib::follow::hub::{ResolvedSource, SourceResolver, WorldResolver};
use gilde_code_lib::follow::tail::TranscriptFormat;
use serde_json::{json, Value};

/// A World that only answers `session` and `transcript` (A10.3).
#[derive(Default)]
struct OneWorld {
    sessions: Mutex<Vec<Session>>,
    transcripts: Mutex<HashMap<SessionUid, (AbsPath, Vendor)>>,
}

#[async_trait]
impl WorldApi for OneWorld {
    fn sink(&self) -> Arc<dyn EvidenceSink> {
        unimplemented!("not used by the resolver")
    }
    fn subscribe(&self, _sink: Box<dyn StorePatchSink>) -> StoreSnapshot {
        unimplemented!("not used by the resolver")
    }
    fn snapshot(&self) -> StoreSnapshot {
        unimplemented!("not used by the resolver")
    }
    fn session(&self, uid: &SessionUid) -> Option<Session> {
        self.sessions.lock().unwrap().iter().find(|s| &s.uid == uid).cloned()
    }
    fn queue(&self) -> AttentionQueue {
        unimplemented!("not used by the resolver")
    }
    async fn identity_lock(&self, _key: IdentityKey) -> IdentityLock {
        unimplemented!("not used by the resolver")
    }
    fn mark_seen(&self, _uid: &SessionUid) -> Result<(), IpcError> {
        Ok(())
    }
    fn dismiss(&self, _uid: &SessionUid, _what: DismissWhat) -> Result<(), IpcError> {
        Ok(())
    }
    fn queue_later(&self, _uid: &SessionUid) -> Result<i64, IpcError> {
        Ok(0)
    }
    fn register_team_adapter(&self, _adapter: Arc<dyn TeamAdapter>) {}
    fn transcript(&self, uid: &SessionUid) -> Option<(AbsPath, Vendor)> {
        self.transcripts.lock().unwrap().get(uid).cloned()
    }
}

fn session(vendor: &str, control: Value, transcript: Option<&str>, control_mode: Option<&str>) -> Session {
    let mut details = serde_json::to_value(SessionDetails::default()).unwrap();
    details["transcriptPath"] = json!(transcript);
    details["controlMode"] = json!(control_mode);
    serde_json::from_value(json!({
        "uid": SessionUid::mint(), "vendor": vendor, "accountId": null, "vendorSessionId": null,
        "projectId": null, "locationId": null, "cwd": "/w/demo", "control": control,
        "activity": {"state": {"kind": "working"}, "since": 1, "confidence": "high", "evidence": null, "episode": "e_0123456789abcdef"},
        "title": null, "team": null, "startedAt": null, "endedAt": null, "lastActivityAt": null,
        "pending": [], "queued": [], "model": null, "permissionMode": null, "binary": null,
        "claudeVersion": null, "memoryBytes": null, "usageToday": null,
        "caps": {"send": "none", "interrupt": false, "forceStop": false, "approve": "none", "setModel": false,
                 "setPermissionMode": [], "continueOptions": [], "stopSession": "none", "terminalView": false,
                 "logView": false, "changes": "none", "ownerFocus": false},
        "details": details, "mergedInto": null
    }))
    .expect("a session")
}

fn watching() -> Value {
    json!({"kind": "watching", "origin": "terminal", "owner": null})
}
fn in_gilde(mode: &str) -> Value {
    json!({"kind": "inGilde", "mode": mode, "pty": null, "hostedTeam": false, "transport": "l1"})
}
fn not_running() -> Value {
    json!({"kind": "notRunning", "lastOrigin": null, "endedHow": "ended"})
}

/// Resolve `s`, with World knowing `transcript` (absolute path, vendor) for it.
fn resolve_with(s: Session, transcript: Option<(&str, Vendor)>) -> Result<ResolvedSource, IpcError> {
    let world = Arc::new(OneWorld::default());
    let uid = s.uid.clone();
    world.sessions.lock().unwrap().push(s);
    if let Some((p, v)) = transcript {
        world.transcripts.lock().unwrap().insert(uid.clone(), (AbsPath::parse(p).unwrap(), v));
    }
    WorldResolver::new(world, AbsPath::parse("/Users/tester").ok()).resolve(&uid)
}

/// Resolve `s` with World knowing its transcript at the display path (when
/// that is absolute) and the session's own vendor.
fn resolve(s: Session) -> Result<ResolvedSource, IpcError> {
    let path = s.details.transcript_path.clone().filter(|p| p.starts_with('/'));
    let vendor = s.vendor;
    match path {
        Some(p) => resolve_with(s, Some((p.as_str(), vendor))),
        None => resolve_with(s, None),
    }
}

#[test]
fn t_follow_resolver_rules() {
    // Gilde's Conversation sessions: the stream producer.
    assert!(matches!(resolve(session("claude", in_gilde("conversation"), None, None)), Ok(ResolvedSource::Producer { kind: ConvoSourceKind::Stream, note: None })));
    // Turn-based: turn memory; cursor/agy say earlier turns cannot be shown.
    assert!(matches!(resolve(session("codex", in_gilde("turnBased"), None, None)), Ok(ResolvedSource::Producer { kind: ConvoSourceKind::TurnMemory, note: None })));
    let r = resolve(session("agy", in_gilde("turnBased"), None, None)).unwrap();
    assert_eq!(r, ResolvedSource::Producer { kind: ConvoSourceKind::TurnMemory, note: Some(Notice::new("turn.earlierNotShown")) });

    // A watched claude session: the transcript World knows (A10.3), live.
    let abs = "/Users/tester/.claude/projects/-w-demo/0d4c1f6e-6b8a-4c7e-9a51-3f2b8c9d0e11.jsonl";
    let r = resolve_with(session("claude", watching(), Some("~/.claude/projects/-w-demo/0d4c1f6e-…"), None), Some((abs, Vendor::Claude))).unwrap();
    let ResolvedSource::File { path, format, base, live } = r else { panic!("{r:?}") };
    assert_eq!(path, PathBuf::from(abs), "World's absolute path, never the display string");
    assert_eq!(format, TranscriptFormat::Claude);
    assert!(live);
    assert_eq!(base.location.as_ref().map(AbsPath::as_str), Some("/w/demo"));
    // A Terminal-mode claude session in Gilde follows its transcript too (§6.2.5).
    assert!(matches!(resolve(session("claude", in_gilde("terminal"), Some("/abs/t.jsonl"), None)), Ok(ResolvedSource::File { live: true, .. })));
    // An ended one: not live (no follow note).
    assert!(matches!(resolve(session("claude", not_running(), Some("/abs/t.jsonl"), None)), Ok(ResolvedSource::File { live: false, .. })));
    // No transcript known yet: nothing (an empty view).
    assert!(matches!(resolve(session("claude", watching(), None, None)), Ok(ResolvedSource::Nothing { note: None })));

    // Codex: its rollout when mapped, else unreadable.
    assert!(matches!(resolve(session("codex", watching(), Some("/abs/rollout-x.jsonl"), None)), Ok(ResolvedSource::File { format: TranscriptFormat::CodexRollout, .. })));
    let unreadable = IpcError::unsupported("noReadableConversation");
    assert_eq!(resolve(session("codex", watching(), None, None)).unwrap_err(), unreadable);
    // cursor/agy started elsewhere: unreadable; Gilde's own ended turn-based ones: the note.
    assert_eq!(resolve(session("cursor", watching(), None, None)).unwrap_err(), unreadable);
    assert_eq!(resolve(session("agy", not_running(), None, None)).unwrap_err(), unreadable);
    assert_eq!(
        resolve(session("cursor", not_running(), None, Some("turnBased"))).unwrap(),
        ResolvedSource::Nothing { note: Some(Notice::new("turn.earlierNotShown")) }
    );
    // A10.3: the `~/…` display string is never expanded into a path; without
    // World's answer the view shows nothing (it never guesses).
    let display_only = session("claude", watching(), Some("~/.claude/projects/-w-demo/0d4c1f6e-6b8a-4c7e-9a51-3f2b8c9d0e11.jsonl"), None);
    assert_eq!(resolve_with(display_only, None).unwrap(), ResolvedSource::Nothing { note: None });
    let codex_display_only = session("codex", watching(), Some("~/.codex/sessions/2026/10/22/rollout-x.jsonl"), None);
    assert_eq!(resolve_with(codex_display_only, None).unwrap_err(), unreadable);
    // A transcript World found for another vendor is not this session's.
    let mismatch = session("claude", watching(), None, None);
    assert_eq!(resolve_with(mismatch, Some(("/abs/rollout-x.jsonl", Vendor::Codex))).unwrap(), ResolvedSource::Nothing { note: None });
}

#[test]
fn t_follow_resolver_unknown_session() {
    let world = Arc::new(OneWorld::default());
    let r = WorldResolver::new(world, None).resolve(&SessionUid::mint());
    assert_eq!(r.unwrap_err(), IpcError::not_found("session"));
}
