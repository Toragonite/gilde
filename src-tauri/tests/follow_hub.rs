//! The ContentHub (contract §4.2.4, §6.1.8, §6.4): subscription eviction
//! (T-follow-evict), the first event is `Reset`, lagging views, delta
//! coalescing, the producer protocol, transcript follow end to end, load
//! older, and the failure paths of the `convo_*` commands. Owner: T04.

mod fixtures;
#[path = "fixtures/transcripts/support.rs"]
mod support;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gilde_code_lib::contract::common::Notice;
use gilde_code_lib::contract::content::{
    Block, BlockKind, BlockPatch, ConvoEvent, ConvoSourceKind, ResetReason, ToolStatus,
};
use gilde_code_lib::contract::error::IpcError;
use gilde_code_lib::contract::ids::{BlockId, SubId, ViewId};
use gilde_code_lib::contract::services::{ContentHubApi, ContentUpdate};
use gilde_code_lib::follow::hub::{ContentHub, ResolvedSource, LAG_EVENTS, MAX_SUBSCRIPTIONS};
use gilde_code_lib::follow::tail::{Follower, TranscriptFormat};
use gilde_code_lib::render::blocks::block_id;
use support::*;

fn view(n: usize) -> ViewId {
    ViewId::parse(format!("view-{n}")).unwrap()
}

fn text_block(id: &str, text: &str, streaming: bool) -> Block {
    Block { id: block_id(id), seq: 0, at: 1_000, kind: BlockKind::AssistantText { markdown: text.into(), streaming, agent: None }, truncated: false }
}

fn producer_hub() -> Arc<ContentHub> {
    ContentHub::new_manual(
        Follower::manual(manual_config()),
        Some(Fixed::answer(Ok(ResolvedSource::Producer { kind: ConvoSourceKind::Stream, note: None }))),
    )
}

#[test]
fn t_follow_evict_ninth_subscription() {
    let hub = producer_hub();
    let chans: Vec<Arc<Collect>> = (0..=MAX_SUBSCRIPTIONS).map(|_| Collect::new()).collect();
    let mut subs = Vec::new();
    for (i, c) in chans.iter().enumerate() {
        let res = hub.subscribe(&uid(), &view(i), None, c.clone()).unwrap();
        assert_eq!(res.source, ConvoSourceKind::Stream);
        subs.push(res.sub_id);
    }
    assert_eq!(hub.subscription_count(), MAX_SUBSCRIPTIONS);
    // Every view got Reset first (seq 0); the least recently subscribed was evicted.
    for c in &chans {
        let env = c.envelopes();
        assert_eq!(env[0].seq, 0);
        assert!(matches!(env[0].event, ConvoEvent::Reset { reason: ResetReason::Initial, .. }));
    }
    let first = chans[0].envelopes();
    assert_eq!(first.len(), 2);
    assert_eq!(first[1].event, ConvoEvent::Evicted);
    assert_eq!(first[1].seq, 1);
    assert_eq!(first[1].sub_id, subs[0]);
    for c in &chans[1..] {
        assert!(!c.events().contains(&ConvoEvent::Evicted));
    }
    // Unsubscribe is idempotent, also for the evicted one.
    hub.unsubscribe(&subs[0]);
    hub.unsubscribe(&subs[1]);
    hub.unsubscribe(&subs[1]);
    assert_eq!(hub.subscription_count(), MAX_SUBSCRIPTIONS - 1);
}

#[test]
fn t_follow_evict_same_session_keeps_content() {
    // A leaked view of the same session is evicted; the new view still sees the content.
    let env = fixtures::Hermetic::new();
    let path = copy_fixture("claude/eleven-rows.jsonl", &env.tmp_dir());
    let hub = ContentHub::new_manual(Follower::manual(manual_config()), Some(Fixed::file(&path, TranscriptFormat::Claude, true)));
    let u = uid();
    let mut chans = Vec::new();
    for i in 0..=MAX_SUBSCRIPTIONS {
        let c = Collect::new();
        hub.subscribe(&u, &view(i), Some(5), c.clone()).unwrap();
        chans.push(c);
    }
    let last = reset_blocks(&chans[MAX_SUBSCRIPTIONS].events()[0]);
    assert_eq!(last.len(), 5);
    assert_eq!(hub.follower().followed().len(), 1, "one tail for the file");
}

#[test]
fn t_follow_producer_protocol_and_coalescing() {
    let hub = producer_hub();
    let u = uid();
    // Before any view: content is kept, deltas are folded.
    hub.publish(&u, ContentUpdate::Source { source: ConvoSourceKind::Stream, note: None });
    hub.publish(&u, ContentUpdate::Appended(text_block("msg_1#0", "Hel", true)));
    hub.publish(&u, ContentUpdate::Updated { block_id: block_id("msg_1#0"), patch: BlockPatch::TextDelta { append: "lo".into() } });
    assert!(!hub.has_subscribers(&u));
    let c = Collect::new();
    let res = hub.subscribe(&u, &view(1), None, c.clone()).unwrap();
    assert_eq!(res.source, ConvoSourceKind::Stream);
    assert!(hub.has_subscribers(&u));
    let blocks = reset_blocks(&c.take()[0]);
    assert!(matches!(&blocks[0].kind, BlockKind::AssistantText { markdown, .. } if markdown == "Hello"));

    // Deltas are coalesced per view until the 50 ms window passes.
    for piece in [" wor", "ld", "\u{1b}[1m!"] {
        hub.publish(&u, ContentUpdate::Updated { block_id: block_id("msg_1#0"), patch: BlockPatch::TextDelta { append: piece.into() } });
    }
    assert!(c.events().is_empty(), "held for coalescing");
    hub.flush_deltas(Instant::now());
    assert!(c.events().is_empty(), "the window has not passed");
    hub.flush_deltas(Instant::now() + Duration::from_millis(60));
    let ev = c.take();
    assert_eq!(ev, vec![ConvoEvent::Updated { block_id: block_id("msg_1#0"), patch: BlockPatch::TextDelta { append: " world!".into() } }]);

    // A non-delta event flushes pending deltas first (order is kept).
    hub.publish(&u, ContentUpdate::Updated { block_id: block_id("msg_1#0"), patch: BlockPatch::TextDelta { append: " Bye".into() } });
    let fin = text_block("msg_1#0", "Hello world! Bye.", false);
    hub.publish(&u, ContentUpdate::Updated { block_id: block_id("msg_1#0"), patch: BlockPatch::Finalized { block: Box::new(fin) } });
    let ev = c.take();
    assert_eq!(ev.len(), 2);
    assert!(matches!(&ev[0], ConvoEvent::Updated { patch: BlockPatch::TextDelta { append }, .. } if append == " Bye"));
    assert!(matches!(&ev[1], ConvoEvent::Updated { patch: BlockPatch::Finalized { block }, .. } if !matches!(block.kind, BlockKind::AssistantText { streaming: true, .. })));

    // 16 KiB of deltas go out without waiting.
    hub.publish(&u, ContentUpdate::Appended(text_block("msg_2#0", "", true)));
    c.take();
    hub.publish(&u, ContentUpdate::Updated { block_id: block_id("msg_2#0"), patch: BlockPatch::TextDelta { append: "x".repeat(16 * 1024) } });
    assert_eq!(c.take().len(), 1);

    // An Appended with an existing id replaces that block; seqs increase.
    hub.publish(&u, ContentUpdate::Appended(text_block("msg_1#0", "replaced", false)));
    let ev = c.take();
    assert!(matches!(&ev[0], ConvoEvent::Updated { patch: BlockPatch::Finalized { .. }, .. }));
    hub.publish(&u, ContentUpdate::Appended(text_block("msg_3#0", "third", false)));
    let ev = c.take();
    let ConvoEvent::Appended { block } = &ev[0] else { panic!("{ev:?}") };
    assert!(block.seq > blocks[0].seq);

    // Ended reaches the view.
    hub.publish(&u, ContentUpdate::Ended);
    assert_eq!(c.take(), vec![ConvoEvent::Ended]);
}

#[test]
fn t_follow_envelope_seq_is_contiguous() {
    let hub = producer_hub();
    let u = uid();
    let c = Collect::new();
    hub.subscribe(&u, &view(1), None, c.clone()).unwrap();
    for i in 0..10 {
        hub.publish(&u, ContentUpdate::Appended(text_block(&format!("m{i}#0"), "t", false)));
    }
    let seqs: Vec<u64> = c.envelopes().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (0..11).collect::<Vec<u64>>());
}

#[test]
fn t_follow_lagging_view_gets_reset() {
    let hub = producer_hub();
    let u = uid();
    let c = Collect::new();
    hub.subscribe(&u, &view(1), Some(10), c.clone()).unwrap();
    c.set_capacity(Some(1)); // the view stops taking events
    for i in 0..(LAG_EVENTS + 5) {
        hub.publish(&u, ContentUpdate::Appended(text_block(&format!("m{i}#0"), "t", false)));
    }
    c.set_capacity(None);
    hub.flush_deltas(Instant::now());
    let ev = c.events();
    assert!(matches!(ev[0], ConvoEvent::Reset { reason: ResetReason::Initial, .. }));
    let lagging: Vec<&ConvoEvent> = ev.iter().filter(|e| matches!(e, ConvoEvent::Reset { reason: ResetReason::Lagging, .. })).collect();
    assert_eq!(lagging.len(), 1, "one Reset{{Lagging}} instead of the backlog");
    let ConvoEvent::Reset { blocks, has_older, .. } = lagging[0] else { unreachable!() };
    assert_eq!(blocks.len(), 10, "the current tail");
    assert!(*has_older);
    assert!(ev.len() < LAG_EVENTS, "the backlog was dropped ({} events)", ev.len());
}

#[test]
fn t_follow_closed_channel_ends_subscription() {
    let hub = producer_hub();
    let u = uid();
    let c = Collect::new();
    hub.subscribe(&u, &view(1), None, c.clone()).unwrap();
    c.closed.store(true, Ordering::SeqCst);
    hub.publish(&u, ContentUpdate::Appended(text_block("m#0", "t", false)));
    assert_eq!(hub.subscription_count(), 0);
}

#[test]
fn t_follow_transcript_end_to_end() {
    let env = fixtures::Hermetic::new();
    let path = copy_fixture("claude/basic.jsonl", &env.tmp_dir());
    let follower = Follower::manual(manual_config());
    let resolver = Fixed::file(&path, TranscriptFormat::Claude, true);
    let hub = ContentHub::new_manual(follower.clone(), Some(resolver.clone()));
    let u = uid();
    let c = Collect::new();
    let res = hub.subscribe(&u, &view(1), Some(3), c.clone()).unwrap();
    assert_eq!(res.source, ConvoSourceKind::Transcript);
    let note = res.note.expect("live transcript views carry the follow note");
    assert_eq!(note.key, "details.followNote");
    let ev = c.take();
    let ConvoEvent::Reset { blocks, has_older, reason } = &ev[0] else { panic!() };
    assert_eq!(*reason, ResetReason::Initial);
    assert_eq!(blocks.len(), 3);
    assert!(*has_older);
    assert!(blocks.windows(2).all(|w| w[0].seq < w[1].seq));

    // Load older, twice, then by an id that is no longer in memory.
    hub.load_older(&res.sub_id, &blocks[0].id, 4).unwrap();
    let ev = c.take();
    let ConvoEvent::Older { blocks: older, has_older } = &ev[0] else { panic!("{ev:?}") };
    assert!(*has_older);
    assert!(!older.is_empty() && older.len() <= 5);
    assert!(older.last().unwrap().seq < blocks[0].seq, "older blocks sort before");
    hub.load_older(&res.sub_id, &older[0].id, 200).unwrap();
    let ev = c.take();
    let ConvoEvent::Older { blocks: oldest, has_older } = &ev[0] else { panic!() };
    assert!(!*has_older, "the whole file was read");
    assert!(matches!(&oldest[0].kind, BlockKind::UserText { text, .. } if text.starts_with("Please read")));

    // Appends arrive as Appended; a result row finalizes its tool call.
    let call = serde_json::json!({"type": "assistant", "uuid": "r-new", "timestamp": "2026-10-22T03:30:00.000Z",
        "message": {"id": "msg_N", "role": "assistant", "content": [{"type": "tool_use", "id": "toolu_N", "name": "Grep", "input": {"pattern": "fn main"}}]}});
    let result = serde_json::json!({"type": "user", "uuid": "u-new", "timestamp": "2026-10-22T03:30:05.000Z",
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_N", "content": "src/main.rs:1"}]}});
    append(&path, format!("{call}\n").as_bytes());
    follower.run_once();
    let ev = c.take();
    assert!(matches!(&ev[0], ConvoEvent::Appended { block } if block.id.as_str() == "r-new"));
    assert!(matches!(&ev[1], ConvoEvent::Source { source: ConvoSourceKind::Transcript, note: Some(n) } if n.key == "details.followNote"));
    append(&path, format!("{result}\n").as_bytes());
    follower.run_once();
    let ev = c.take();
    assert!(matches!(&ev[0], ConvoEvent::Updated { block_id, patch: BlockPatch::Finalized { block } }
        if block_id.as_str() == "r-new" && matches!(&block.kind, BlockKind::ToolCall { status: ToolStatus::Ok, result_preview: Some(p), .. } if p == "src/main.rs:1")));

    // A truncation resets the view.
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(0).unwrap();
    append(&path, user_row("u-z", "fresh start", "2026-10-22T04:00:00.000Z").as_bytes());
    follower.run_once();
    let ev = c.take();
    let ConvoEvent::Reset { blocks, reason, .. } = &ev[0] else { panic!("{ev:?}") };
    assert_eq!(*reason, ResetReason::Truncated);
    assert_eq!(blocks.len(), 1);

    // The last view leaving releases the tail.
    hub.unsubscribe(&res.sub_id);
    assert!(follower.followed().is_empty());
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn t_render_block_id_source_switch() {
    // T-render-blockId [F-06]: eleven rows of one message.id stay eleven
    // blocks, and a source change resets the view (never mixes id schemes).
    let env = fixtures::Hermetic::new();
    let path = copy_fixture("claude/eleven-rows.jsonl", &env.tmp_dir());
    let hub = ContentHub::new_manual(Follower::manual(manual_config()), Some(Fixed::file(&path, TranscriptFormat::Claude, false)));
    let u = uid();
    let c = Collect::new();
    let res = hub.subscribe(&u, &view(1), Some(500), c.clone()).unwrap();
    assert_eq!(res.note, None, "no follow note for a session that is not live");
    let blocks = reset_blocks(&c.take()[0]);
    let calls: Vec<&Block> = blocks.iter().filter(|b| matches!(b.kind, BlockKind::ToolCall { .. })).collect();
    assert_eq!(calls.len(), 11);
    let ids: Vec<&str> = calls.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, (1..=11).map(|i| format!("r-e{i:02}")).collect::<Vec<_>>());
    assert!(calls.iter().all(|b| matches!(b.kind, BlockKind::ToolCall { status: ToolStatus::Ok, .. })));

    // The session is continued in Gilde: the stream producer takes over.
    hub.publish(&u, ContentUpdate::Source { source: ConvoSourceKind::Stream, note: None });
    let ev = c.take();
    assert_eq!(ev[0], ConvoEvent::Reset { blocks: vec![], has_older: false, reason: ResetReason::Replaced });
    assert_eq!(ev[1], ConvoEvent::Source { source: ConvoSourceKind::Stream, note: None });
    assert!(hub.follower().followed().is_empty(), "the transcript is no longer followed for this view");
    hub.publish(&u, ContentUpdate::Appended(text_block("msg_X#0", "live", true)));
    assert!(matches!(&c.take()[0], ConvoEvent::Appended { block } if block.id.as_str() == "msg_X#0"));

    // An Appended without a Source on a transcript view also switches.
    let u2 = uid();
    let c2 = Collect::new();
    hub.subscribe(&u2, &view(2), None, c2.clone()).unwrap();
    c2.take();
    hub.publish(&u2, ContentUpdate::Appended(text_block("msg_Y#0", "x", false)));
    let ev = c2.take();
    assert!(matches!(ev[0], ConvoEvent::Reset { reason: ResetReason::Replaced, .. }));
    assert!(matches!(ev[2], ConvoEvent::Appended { .. }));
}

#[test]
fn t_follow_errors() {
    let hub = producer_hub();
    let missing = SubId::parse("v_0123456789abcdef").unwrap();
    let e = hub.load_older(&missing, &block_id("x"), 10).unwrap_err();
    assert_eq!(e, IpcError::not_found("subscription"));
    let c = Collect::new();
    let res = hub.subscribe(&uid(), &view(1), None, c).unwrap();
    let e = hub.load_older(&res.sub_id, &BlockId::parse("nope").unwrap(), 10).unwrap_err();
    assert_eq!(e, IpcError::not_found("block"));

    let unreadable = ContentHub::new_manual(Follower::manual(manual_config()), Some(Fixed::answer(Err(IpcError::unsupported("noReadableConversation")))));
    let e = unreadable.subscribe(&uid(), &view(1), None, Collect::new()).unwrap_err();
    assert_eq!(e, IpcError::unsupported("noReadableConversation"));
    assert_eq!(unreadable.subscription_count(), 0);

    // A transcript path that cannot be opened is unreadable, not a crash.
    let env = fixtures::Hermetic::new();
    let gone = env.tmp_dir().join("gone.jsonl");
    let hub2 = ContentHub::new_manual(Follower::manual(manual_config()), Some(Fixed::file(&gone, TranscriptFormat::Claude, true)));
    let e = hub2.subscribe(&uid(), &view(1), None, Collect::new()).unwrap_err();
    assert_eq!(e, IpcError::unsupported("noReadableConversation"));
    assert!(hub2.follower().followed().is_empty());

    // Nothing readable yet: an empty view with the note.
    let note = Some(Notice::new("turn.earlierNotShown"));
    let hub3 = ContentHub::new_manual(Follower::manual(manual_config()), Some(Fixed::answer(Ok(ResolvedSource::Nothing { note: note.clone() }))));
    let c = Collect::new();
    let res = hub3.subscribe(&uid(), &view(1), None, c.clone()).unwrap();
    assert_eq!(res.source, ConvoSourceKind::None);
    assert_eq!(res.note, note);
    assert_eq!(c.events(), vec![ConvoEvent::Reset { blocks: vec![], has_older: false, reason: ResetReason::Initial }]);
}

#[test]
fn t_follow_memory_bound() {
    let hub = producer_hub();
    let u = uid();
    for i in 0..2_100 {
        hub.publish(&u, ContentUpdate::Appended(text_block(&format!("m{i}#0"), "t", false)));
    }
    let c = Collect::new();
    let res = hub.subscribe(&u, &view(1), Some(500), c.clone()).unwrap();
    let ConvoEvent::Reset { blocks, has_older, .. } = &c.take()[0] else { panic!() };
    assert_eq!(blocks.len(), 500);
    assert!(*has_older, "more blocks than the tail are kept in memory");
    assert_eq!(blocks.last().unwrap().id.as_str(), "m2099#0");
    // Only 2,000 are kept: paging back ends at m100, and a stream cannot reload more.
    let mut before = blocks[0].id.clone();
    let mut total = 0;
    loop {
        hub.load_older(&res.sub_id, &before, 200).unwrap();
        let ev = c.take();
        let ConvoEvent::Older { blocks, has_older } = &ev[0] else { panic!("{ev:?}") };
        total += blocks.len();
        if !*has_older {
            assert_eq!(blocks[0].id.as_str(), "m100#0");
            break;
        }
        before = blocks[0].id.clone();
    }
    assert_eq!(total, 1_500);
}

// ---------------------------------------------------------------------------
// A50.1 — a swapped transcript is unavailable, never its bytes (LV-1)
// ---------------------------------------------------------------------------

mod a50 {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use gilde_code_lib::contract::attention::AttentionQueue;
    use gilde_code_lib::contract::common::{AbsPath, Vendor};
    use gilde_code_lib::contract::content::ConvoSourceKind;
    use gilde_code_lib::contract::error::IpcError;
    use gilde_code_lib::contract::ids::{MessageKey, SessionUid, TeamKey};
    use gilde_code_lib::contract::ipc::{DismissWhat, MessageBodySource as BodyFrom};
    use gilde_code_lib::contract::services::{EvidenceSink, IdentityKey, IdentityLock, StorePatchSink, WorldApi};
    use gilde_code_lib::contract::session::Session;
    use gilde_code_lib::contract::store::StoreSnapshot;
    use gilde_code_lib::contract::team::{DeliveryStatus, MessageKind, TeamAdapter};
    use gilde_code_lib::contract::ErrorCode;
    use gilde_code_lib::follow::hub::{ContentHub, MessageBodyReader};
    use gilde_code_lib::follow::tail::{Follower, TranscriptFormat};
    use gilde_code_lib::world::MessageLocator;

    use super::view;
    use crate::support::*;

    const CANARY: &str = "A50-HUB-CANARY";
    const SID: &str = "0d4c1f6e-6b8a-4c7e-9a51-3f2b8c9d0e11";

    struct Tree {
        _env: crate::fixtures::Hermetic,
        cfg: PathBuf,
        projects: PathBuf,
        outside: PathBuf,
    }

    /// `<cfg>/projects/-w-proj/<SID>.jsonl` holding `inside`, a dir outside
    /// the root holding `<SID>.jsonl` with the canary, and a never-open file
    /// with the canary.
    fn tree(inside: &str, outside: &str) -> Tree {
        let env = crate::fixtures::Hermetic::new();
        let home = std::fs::canonicalize(env.home()).unwrap();
        let cfg = home.join(".claude");
        let projects = cfg.join("projects");
        std::fs::create_dir_all(projects.join("-w-proj")).unwrap();
        std::fs::write(projects.join("-w-proj").join(format!("{SID}.jsonl")), inside).unwrap();
        let out = home.join("outside");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join(format!("{SID}.jsonl")), outside).unwrap();
        std::fs::write(cfg.join(".credentials.json"), outside).unwrap();
        Tree { _env: env, cfg, projects, outside: out }
    }

    impl Tree {
        /// The project dir swapped for a symlink out of the root.
        fn dir_link(&self) -> PathBuf {
            std::os::unix::fs::symlink(&self.outside, self.projects.join("-w-out")).unwrap();
            self.projects.join("-w-out").join(format!("{SID}.jsonl"))
        }

        /// A transcript name hard-linked to `.credentials.json`.
        fn hard_link(&self) -> PathBuf {
            let p = self.projects.join("-w-proj/55555555-5555-4555-8555-555555555555.jsonl");
            std::fs::hard_link(self.cfg.join(".credentials.json"), &p).unwrap();
            p
        }

        fn ok(&self) -> PathBuf {
            self.projects.join("-w-proj").join(format!("{SID}.jsonl"))
        }
    }

    #[test]
    fn a_swapped_history_transcript_is_unavailable_never_its_bytes() {
        let ts = "2026-10-23T10:00:00.000Z";
        let t = tree(&user_row("u-1", "inside text", ts), &user_row("u-1", CANARY, ts));
        for (what, path) in [("project dir symlinked out", t.dir_link()), ("hard link to .credentials.json", t.hard_link())] {
            let hub = ContentHub::new_manual(Follower::manual(manual_config()), Some(Fixed::file(&path, TranscriptFormat::Claude, false)));
            let c = Collect::new();
            let err = hub.subscribe(&uid(), &view(1), None, c.clone()).expect_err(what);
            assert_eq!((err.code, err.params["cap"].as_str()), (ErrorCode::Unsupported, Some("noReadableConversation")), "{what}");
            assert!(!format!("{:?}", c.events()).contains(CANARY), "{what}: bytes reached the view");
            assert!(hub.follower().followed().is_empty(), "{what}: nothing is followed");
        }
        // Control: the transcript in place is read.
        let hub = ContentHub::new_manual(Follower::manual(manual_config()), Some(Fixed::file(&t.ok(), TranscriptFormat::Claude, false)));
        let c = Collect::new();
        let res = hub.subscribe(&uid(), &view(2), None, c.clone()).expect("the transcript in place");
        assert_eq!(res.source, ConvoSourceKind::Transcript);
        assert!(format!("{:?}", c.events()).contains("inside text"));
    }

    /// A World that only answers `transcript` (the body reader's one call).
    #[derive(Default)]
    struct Transcripts(Mutex<HashMap<SessionUid, (AbsPath, Vendor)>>);

    #[async_trait]
    impl WorldApi for Transcripts {
        fn sink(&self) -> Arc<dyn EvidenceSink> {
            unimplemented!("not used by the reader")
        }
        fn subscribe(&self, _sink: Box<dyn StorePatchSink>) -> StoreSnapshot {
            unimplemented!("not used by the reader")
        }
        fn snapshot(&self) -> StoreSnapshot {
            unimplemented!("not used by the reader")
        }
        fn session(&self, _uid: &SessionUid) -> Option<Session> {
            None
        }
        fn queue(&self) -> AttentionQueue {
            unimplemented!("not used by the reader")
        }
        async fn identity_lock(&self, _key: IdentityKey) -> IdentityLock {
            unimplemented!("not used by the reader")
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
            self.0.lock().unwrap().get(uid).cloned()
        }
    }

    /// The lead's `SendMessage` call with tool_use id `toolu_a50`.
    fn send_call(body: &str) -> String {
        serde_json::json!({
            "type": "assistant", "uuid": "a-1", "timestamp": "2026-10-23T10:00:00.000Z", "isSidechain": false,
            "message": {"id": "msg_1", "type": "message", "role": "assistant", "stop_reason": "tool_use",
                        "content": [{"type": "tool_use", "id": "toolu_a50", "name": "SendMessage",
                                     "input": {"to": "alpha", "message": body, "summary": "s"}}]}
        })
        .to_string()
            + "\n"
    }

    fn body_from(path: &Path) -> (Option<String>, BodyFrom) {
        let world = Arc::new(Transcripts::default());
        let sender = SessionUid::mint();
        world.0.lock().unwrap().insert(sender.clone(), (AbsPath::parse(path.to_str().unwrap()).unwrap(), Vendor::Claude));
        let team = TeamKey::derive("claudeNative", None, "session-a50");
        let loc = MessageLocator {
            team: team.clone(),
            key: MessageKey::derive(&team, "m-1"),
            kind: MessageKind::Plain,
            status: DeliveryStatus::Sent,
            at: 1_792_740_000_000,
            sender: Some(sender),
            sender_name: Some("team-lead".into()),
            recipient: None,
            recipient_name: Some("alpha".into()),
            tool_use_id: Some("toolu_a50".into()),
            delivered: None,
        };
        let res = MessageBodyReader::new(world).read(&loc);
        (res.text, res.source)
    }

    #[test]
    fn a_message_body_is_never_read_through_a_swapped_transcript() {
        let t = tree(&send_call("inside body"), &send_call(CANARY));
        for (what, path) in [("project dir symlinked out", t.dir_link()), ("hard link to .credentials.json", t.hard_link())] {
            let (text, source) = body_from(&path);
            assert_eq!((text, source), (None, BodyFrom::Unavailable), "{what}");
        }
        assert_eq!(body_from(&t.ok()), (Some("inside body".into()), BodyFrom::SenderToolUse), "control: in place");
    }
}
