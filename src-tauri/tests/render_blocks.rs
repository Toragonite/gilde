//! Golden blocks from the transcript fixtures (contract §6.4, §3.7, §14.2
//! T-render-*), the shared Claude message → block helper with both id
//! schemes, tool summaries (§3.7.3), and the agent-mesh renderer (§6.3).
//! Owner: T04.
//!
//! Re-bless the goldens after an intended change with
//! `GILDE_BLESS=1 cargo test --test render_blocks`.

mod fixtures;
#[path = "fixtures/transcripts/support.rs"]
mod support;

use gilde_code_lib::contract::activity::ErrorSub;
use gilde_code_lib::contract::common::AbsPath;
use gilde_code_lib::contract::content::{
    Block, BlockKind, BlockPatch, ChangeKind, SystemNote, ToolStatus, ToolVerb, UserOrigin,
};
use gilde_code_lib::contract::error::VendorErrorClass;
use gilde_code_lib::contract::ids::ViewId;
use gilde_code_lib::contract::services::ContentUpdate;
use gilde_code_lib::contract::sidecar::{TurnDoneStatus, TurnEvent};
use gilde_code_lib::contract::team::MessageKind;
use gilde_code_lib::follow::hub::ContentHub;
use gilde_code_lib::follow::tail::{Follower, TranscriptFormat};
use gilde_code_lib::render::blocks::{claude_blocks, claude_tool_results, BlockIdScheme, RenderContext};
use gilde_code_lib::render::mesh::MeshTurnRenderer;
use gilde_code_lib::render::tools::{claude_tool_summary, PathBase};
use serde_json::json;
use support::*;

/// Every block of a fixture as a view sees it (one Reset with a large tail).
fn render(rel: &str, format: TranscriptFormat) -> Vec<Block> {
    let env = fixtures::Hermetic::new();
    let path = copy_fixture(rel, &env.tmp_dir());
    let hub = ContentHub::new_manual(Follower::manual(manual_config()), Some(Fixed::file(&path, format, false)));
    let c = Collect::new();
    hub.subscribe(&uid(), &ViewId::parse("golden").unwrap(), Some(500), c.clone()).unwrap();
    reset_blocks(&c.events()[0])
}

fn golden(rel: &str, blocks: &[Block]) {
    let path = fixture(rel);
    let got = serde_json::to_string_pretty(blocks).unwrap() + "\n";
    if std::env::var_os("GILDE_BLESS").is_some() {
        std::fs::write(&path, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing golden {rel}; run with GILDE_BLESS=1"));
    assert_eq!(got, want, "golden {rel} differs (GILDE_BLESS=1 re-blesses)");
}

#[test]
fn t_render_golden_claude_transcript() {
    let blocks = render("claude/basic.jsonl", TranscriptFormat::Claude);
    golden("claude/basic.blocks.json", &blocks);

    // The facts the golden pins, spelled out.
    let by_id = |id: &str| blocks.iter().find(|b| b.id.as_str() == id).unwrap_or_else(|| panic!("no block {id}"));
    assert!(matches!(&by_id("u-0001").kind, BlockKind::UserText { origin: UserOrigin::Human, .. }));
    assert!(matches!(&by_id("r-0001").kind, BlockKind::Thinking { seconds: Some(2), text: Some(t) } if t == "Let me look at the file."));
    assert!(matches!(&by_id("r-0003").kind, BlockKind::ToolCall { summary, status: ToolStatus::Ok, .. }
        if summary.verb == ToolVerb::Read && summary.object.as_deref() == Some("src/main.rs")));
    let BlockKind::ToolCall { file_change: Some(fc), status: ToolStatus::Ok, .. } = &by_id("r-0004").kind else { panic!() };
    assert_eq!((fc.path_rel.as_str(), fc.added, fc.removed, fc.change), ("src/main.rs", 1, 1, ChangeKind::Modified));
    assert!(matches!(&by_id("r-0005").kind, BlockKind::ToolCall { status: ToolStatus::Denied, result_preview: Some(p), summary, .. }
        if !p.contains('\u{1b}') && summary.object.as_deref() == Some("cargo build")));
    assert!(matches!(&by_id("s-0001").kind, BlockKind::System { note: SystemNote::ApiRetry { attempt: 2 }, .. }));
    assert!(matches!(&by_id("r-0006").kind, BlockKind::AssistantText { markdown, .. } if markdown == "Fixed the typo in src/main.rs."));
    assert!(matches!(&by_id("u-0005").kind, BlockKind::UserText { text, .. } if text == "Now write a README please"));
    let BlockKind::ToolCall { summary, file_change: Some(fc), .. } = &by_id("r-0007").kind else { panic!() };
    assert_eq!((summary.verb, fc.change, fc.added), (ToolVerb::Create, ChangeKind::Added, 3));
    assert!(matches!(&by_id("u-0007").kind, BlockKind::System { note: SystemNote::Interrupted, .. }));
    assert!(matches!(&by_id("s-0003").kind, BlockKind::System { note: SystemNote::CompactBoundary, .. }));
    assert!(matches!(&by_id("u-0008").kind, BlockKind::UserText { origin: UserOrigin::Teammate, text, .. } if text == "Please run the tests."));
    assert!(matches!(&by_id("a-0002").kind, BlockKind::UserText { origin: UserOrigin::Peer, text, .. } if text == "Status?"));
    assert!(matches!(&by_id("r-0008").kind, BlockKind::System { note: SystemNote::Error { sub: ErrorSub::UsageLimit { .. } }, .. }));
    assert!(matches!(&by_id("r-0009").kind, BlockKind::AssistantText { markdown, .. } if markdown == "\u{FFFD} broken"));
    assert!(matches!(&by_id("r-0010").kind, BlockKind::Unknown { type_name } if type_name == "container_upload"));
    // Metadata rows, the meta user row, results and unknown row types are not blocks.
    assert!(blocks.iter().all(|b| !["u-0002", "u-0003", "u-0004", "u-0006", "u-0009", "a-0001"].contains(&b.id.as_str())));
    assert_eq!(blocks.len(), 17);
    assert!(blocks.windows(2).all(|w| w[0].seq < w[1].seq), "file order");
}

#[test]
fn t_render_golden_codex_rollout() {
    let blocks = render("codex/rollout-basic.jsonl", TranscriptFormat::CodexRollout);
    golden("codex/rollout-basic.blocks.json", &blocks);
    let kinds: Vec<&str> = blocks
        .iter()
        .map(|b| match &b.kind {
            BlockKind::UserText { .. } => "user",
            BlockKind::AssistantText { .. } => "assistant",
            BlockKind::Thinking { .. } => "thinking",
            BlockKind::ToolCall { .. } => "tool",
            BlockKind::System { .. } => "system",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["user", "thinking", "tool", "tool", "tool", "assistant", "tool", "system"], "response_item rows never duplicate");
    let BlockKind::ToolCall { summary, file_change: Some(fc), .. } = &blocks[3].kind else { panic!() };
    assert_eq!((summary.verb, fc.path_rel.as_str(), fc.added, fc.change), (ToolVerb::Create, "NOTES.md", 2, ChangeKind::Added));
    assert!(matches!(&blocks[2].kind, BlockKind::ToolCall { status: ToolStatus::Ok, summary, .. } if summary.object.as_deref() == Some("bash -lc ls")));
    assert!(matches!(&blocks[6].kind, BlockKind::ToolCall { status: ToolStatus::Error, .. }));
    assert!(matches!(&blocks[7].kind, BlockKind::System { note: SystemNote::Interrupted, .. }));
}

#[test]
fn t_render_claude_blocks_stream_scheme() {
    // The helper as T05 uses it for a complete `assistant` frame.
    let ctx = RenderContext { location: AbsPath::parse("/w/demo").ok(), home: AbsPath::parse("/Users/tester").ok(), at: 5, next_seq: 0 };
    let msg = json!({"id": "msg_S", "role": "assistant", "content": [
        {"type": "thinking", "thinking": "plan"},
        {"type": "text", "text": "Running it."},
        {"type": "tool_use", "id": "toolu_S", "name": "Bash", "input": {"command": "ls /Users/tester/x"}}
    ]});
    let b = claude_blocks(&msg, BlockIdScheme::Stream { message_id: "msg_S" }, &ctx);
    let ids: Vec<&str> = b.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, ["msg_S#0", "msg_S#1", "msg_S#2"]);
    assert!(b.iter().all(|b| b.at == 5));
    // The same message as transcript rows: one row uuid per block.
    let row = json!({"role": "assistant", "content": [{"type": "text", "text": "Running it."}]});
    assert_eq!(claude_blocks(&row, BlockIdScheme::Transcript { row_uuid: "6f1e" }, &ctx)[0].id.as_str(), "6f1e");
    // A user frame with a result yields no block, only the result.
    let user = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_S", "content": [{"type": "text", "text": "a\nb"}]}]});
    assert!(claude_blocks(&user, BlockIdScheme::Stream { message_id: "u1" }, &ctx).is_empty());
    assert_eq!(claude_tool_results(&user)[0].preview.as_deref(), Some("a\nb"));
    // A protocol frame from a teammate: no body text.
    let frame = json!({"role": "user", "content": "<teammate-message teammate_id=\"alpha\">{\"type\":\"shutdown_request\",\"reason\":\"secret\"}</teammate-message>"});
    let b = claude_blocks(&frame, BlockIdScheme::Transcript { row_uuid: "t1" }, &ctx);
    assert!(matches!(&b[0].kind, BlockKind::System { note: SystemNote::TeamEvent { message_kind: MessageKind::ShutdownRequest, member: Some(m) }, text: None } if m == "alpha"));
}

#[test]
fn t_render_tool_summary_table() {
    let base = PathBase { location: Some("/w/demo"), home: Some("/Users/tester") };
    let s = |n: &str, i: serde_json::Value| claude_tool_summary(n, &i, base);
    assert_eq!(s("Read", json!({"file_path": "/Users/tester/notes/x.md"})).object.as_deref(), Some("~/notes/x.md"));
    assert_eq!(s("Bash", json!({"command": format!("echo {}", "x".repeat(400))})).object.unwrap().chars().count(), 200);
    assert_eq!(s("Glob", json!({"pattern": "**/*.rs"})).verb, ToolVerb::Search);
    assert_eq!(s("WebFetch", json!({"url": "not a url"})).object, None);
    assert_eq!(s("Task", json!({"subagent_type": "reviewer"})).object.as_deref(), Some("reviewer"));
    assert_eq!(s("ExitPlanMode", json!({"plan": "…"})).object, None);
    assert_eq!(s("mcp__docs__search", json!({})).object.as_deref(), Some("docs · search"));
    assert_eq!(s(&"T".repeat(100), json!({})).tool.chars().count(), 64);
}

#[test]
fn t_render_mesh_turn() {
    let mut r = MeshTurnRenderer::new("turn-1");
    let mut updates = vec![r.user_prompt("Summarize the repo", None, 1)];
    for ev in [
        TurnEvent::Text { delta: "Looking".into() },
        TurnEvent::Text { delta: " now.\u{1b}]8;;http://x\u{1b}".into() },
        TurnEvent::Text { delta: "\\".into() },
        TurnEvent::Tool { name: "shell".into(), detail_preview: Some("ls -la\nmore".into()) },
        TurnEvent::Text { delta: "Done.".into() },
        TurnEvent::Done { status: TurnDoneStatus::Stopped, conversation_id: "c".into(), usage: None, cost_usd: None },
    ] {
        updates.extend(r.on_event(&ev, 2));
    }
    updates.extend(r.on_event(&TurnEvent::Error { class: VendorErrorClass::Quota, message: "limit".into() }, 3));
    let ids: Vec<String> = updates
        .iter()
        .filter_map(|u| match u {
            ContentUpdate::Appended(b) => Some(b.id.as_str().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["turn-1#0", "turn-1#1", "turn-1#2", "turn-1#3", "turn-1#4", "turn-1#5"]);
    let finalized: Vec<&Block> = updates
        .iter()
        .filter_map(|u| match u {
            ContentUpdate::Updated { patch: BlockPatch::Finalized { block }, .. } => Some(&**block),
            _ => None,
        })
        .collect();
    assert!(matches!(&finalized[0].kind, BlockKind::AssistantText { markdown, streaming: false, .. } if markdown == "Looking now."));
    assert!(updates.iter().any(|u| matches!(u, ContentUpdate::Appended(Block { kind: BlockKind::ToolCall { status: ToolStatus::Unknown, summary, .. }, .. })
        if summary.verb == ToolVerb::Run && summary.object.as_deref() == Some("ls -la"))));
    assert!(updates.iter().any(|u| matches!(u, ContentUpdate::Appended(Block { kind: BlockKind::System { note: SystemNote::Interrupted, .. }, .. }))));
    assert!(updates.iter().any(|u| matches!(u, ContentUpdate::Appended(Block { kind: BlockKind::System { note: SystemNote::Error { sub: ErrorSub::UsageLimit { .. } }, .. }, .. }))));
}
