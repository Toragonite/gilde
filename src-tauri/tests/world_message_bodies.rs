//! T27 (contract A27): team message bodies on demand.
//!
//! * A27.1 — `World::message_locator`: the team, both ends' sessions and
//!   native names, the observed time and status, and the Claude adapter's
//!   transcript evidence (the sender's `SendMessage` tool_use id, the
//!   recipient's delivering row and block);
//! * A27.2 — `follow::hub::MessageBodyReader`: the recipient's delivering
//!   turn block, else the sender's `SendMessage` tool_use, through
//!   `safe_read`, ≤ 8 MiB scanned backwards per file, the text cut to 16 KiB
//!   on a grapheme boundary;
//! * A27.3 — `follow::hub::MessageBodySource` behind T07's LRU, wired by the
//!   composition root (T-wiring: the command is live through the real IPC).
//!
//! Privacy is the acceptance core: bodies are **[content]** — a canary body
//! never reaches a store row, a patch, a `session_meta` row, a log line (at
//! TRACE) or an `IpcError`.
//!
//! Fixtures are synthesized from the R5 §2 / R6 §5 row shapes into temp
//! dirs (never real transcripts); no vendor CLI, no vendor directory.
//!
//! Owner: T02 (T27).

#[path = "world_support.rs"]
mod support;

mod fixtures;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use unicode_segmentation::UnicodeSegmentation;

use gilde_code_lib::commands::team::MessageBodies;
use gilde_code_lib::contract::common::{AbsPath, EpochMs};
use gilde_code_lib::contract::ids::{MessageKey, ProcessKey, SessionUid, TeamKey};
use gilde_code_lib::contract::ipc::{MessageBodySource as BodyFrom, TeamMessageBodyRes};
use gilde_code_lib::contract::services::{EvidenceSink, EvidenceUpdate, IdentityHint, SessionMetaFacts, SourceFacts, WorldApi};
use gilde_code_lib::contract::display::TextCap;
use gilde_code_lib::contract::team::{DeliveryStatus, LeadBinding, MessageKind};
use gilde_code_lib::discovery::claude::teams::{LeadJoin, TasksRead, TeamConfig, TeamDirRead, TeamMemberRow};
use gilde_code_lib::discovery::reconcile::{LiveMember, TeamObservation, TeamObserver};
use gilde_code_lib::discovery::safe_read::FileId;
use gilde_code_lib::follow::hub::{
    MessageBodyReader, MessageBodySource, BODY_SCAN_BUDGET, BODY_TEXT_CAP, BODY_WINDOW_MS,
};
use gilde_code_lib::follow::tail::RowSink;
use gilde_code_lib::render::transcript::row_facts;
use gilde_code_lib::world::{DeliveredBlock, MessageLocator, World};
use support::*;

/// Every test here creates temp dirs, and the wiring test points `TMPDIR`
/// at its hermetic tree while it runs: one test at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.blocking_lock()
}

const DIR: &str = "session-27aa0001";
const MIB: usize = 1024 * 1024;

fn lead_sid() -> String {
    sid(0x27a0)
}
fn alpha_sid() -> String {
    sid(0x27a1)
}

fn team_key() -> TeamKey {
    TeamKey::derive("claudeNative", Some(&account()), DIR)
}

/// RFC 3339 UTC with milliseconds, as the CLI writes `timestamp`.
fn rfc3339(ms: EpochMs) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil from days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

// ---------------------------------------------------------------------------
// synthesized rows (R5 §2.2, R6 §5 LP-16 shapes)
// ---------------------------------------------------------------------------

/// The sender's `SendMessage` tool_use row and its result row
/// (`toolUseResult{success, message, msg_id?, routing{…}}`).
fn send_rows(at: EpochMs, tool_use_id: &str, to: &str, body: &str, summary: &str, msg_id: Option<&str>) -> Vec<String> {
    let call = json!({
        "type": "assistant", "uuid": format!("a-{tool_use_id}"), "timestamp": rfc3339(at), "isSidechain": false,
        "sessionId": lead_sid(),
        "message": {
            "id": format!("msg_{tool_use_id}"), "type": "message", "role": "assistant", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": tool_use_id, "name": "SendMessage",
                         "input": {"to": to, "message": body, "summary": summary}}],
            "usage": {"input_tokens": 3, "output_tokens": 5}
        }
    });
    let mut result = json!({
        "type": "user", "uuid": format!("r-{tool_use_id}"), "timestamp": rfc3339(at + 40), "isSidechain": false,
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id,
                                                  "content": format!("Message sent to {to}'s inbox")}]},
        "toolUseResult": {"success": true, "message": format!("Message sent to {to}'s inbox"),
                          "routing": {"sender": "team-lead", "target": format!("@{to}"), "targetColor": "blue",
                                      "summary": summary, "content": body}}
    });
    if let Some(m) = msg_id {
        result["toolUseResult"]["msg_id"] = json!(m);
    }
    vec![call.to_string(), result.to_string()]
}

/// A recipient's turn-opening user row: one `<teammate-message>` block per
/// delivered message, in arrival order (R5 §2.2).
fn delivered_row(at: EpochMs, uuid: &str, blocks: &[(&str, &str, Option<&str>)]) -> String {
    let text = blocks
        .iter()
        .map(|(from, body, summary)| {
            let s = summary.map(|s| format!(" summary=\"{s}\"")).unwrap_or_default();
            format!("<teammate-message teammate_id=\"{from}\" color=\"blue\"{s}>\n{body}\n</teammate-message>")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    json!({"type": "user", "uuid": uuid, "timestamp": rfc3339(at), "isSidechain": false,
           "message": {"role": "user", "content": text}})
    .to_string()
}

/// An assistant text row of about `bytes` bytes (no message markers).
fn filler_row(at: EpochMs, n: usize, bytes: usize) -> String {
    json!({"type": "assistant", "uuid": format!("f-{n}"), "timestamp": rfc3339(at),
           "message": {"id": format!("msg_f{n}"), "role": "assistant",
                       "content": [{"type": "text", "text": "y".repeat(bytes)}]}})
    .to_string()
}

/// `count` filler rows of 64 KiB.
fn filler(at: EpochMs, count: usize) -> Vec<String> {
    (0..count).map(|i| filler_row(at, i, 64 * 1024)).collect()
}

// ---------------------------------------------------------------------------
// a native team with a lead and one pane member, both with transcripts
// ---------------------------------------------------------------------------

struct Scene {
    rig: Rig,
    _dir: tempfile::TempDir,
    lead_path: PathBuf,
    alpha_path: PathBuf,
}

fn member_row(name: &str, backend: &str, pane: &str, joined: EpochMs) -> TeamMemberRow {
    TeamMemberRow {
        agent_id: format!("{name}@{DIR}"),
        name: name.into(),
        agent_type: Some(if name == "team-lead" { "team-lead".into() } else { "general-purpose".into() }),
        model: None,
        color: Some("blue".into()),
        backend_type: Some(backend.into()),
        tmux_pane_id: Some(pane.into()),
        cwd: None,
        joined_at: Some(joined),
        is_active: None,
        plan_mode_required: Some(false),
        mode: None,
    }
}

fn observation(now: EpochMs) -> TeamObservation {
    let created = now - 60_000;
    TeamObservation {
        account: account(),
        dir: TeamDirRead {
            dir_name: DIR.into(),
            config: Some(TeamConfig {
                name: Some("bodies".into()),
                created_at: Some(created),
                lead_agent_id: Some(format!("team-lead@{DIR}")),
                lead_session_id: Some("27aa0001".into()),
                members: vec![
                    member_row("team-lead", "in-process", "leader", created),
                    member_row("alpha", "tmux", "%1", created + 10_000),
                ],
                members_truncated: false,
            }),
            config_id: Some(FileId { size: 1, mtime_ms: created, inode: 1, dev: 1 }),
            roster_stale: false,
            torn: 0,
            inboxes: Default::default(),
            inboxes_skipped: 0,
        },
        join: LeadJoin { binding: LeadBinding::Exact, lead_session_id: Some(lead_sid()), lead_pid: Some(7200) },
        lead_alive: true,
        live_members: vec![LiveMember {
            agent_id: format!("alpha@{DIR}"),
            agent_name: "alpha".into(),
            process: ProcessKey::new(7201, 1_792_700_000),
            pid: 7201,
            session_id: Some(alpha_sid()),
        }],
        tasks: TasksRead::default(),
        in_process: vec![],
        visible: true,
        at: now,
    }
}

fn hint(vsid: &str, pid: u32) -> IdentityHint {
    claude_hint(Some(vsid), Some(pid))
}

fn meta_path(path: &Path) -> SourceFacts {
    SourceFacts::Meta(Box::new(SessionMetaFacts {
        transcript_path: Some(AbsPath::parse(path.to_str().unwrap()).unwrap()),
        ..SessionMetaFacts::default()
    }))
}

/// Feed one session's evidence and the team observation into `sink`/`world`.
fn seed(world: &World, sink: &Arc<dyn EvidenceSink>, now: EpochMs, lead_path: &Path, alpha_path: &Path) {
    for (vsid, pid, path) in [(lead_sid(), 7200, lead_path), (alpha_sid(), 7201, alpha_path)] {
        let h = hint(&vsid, pid);
        sink.evidence(EvidenceUpdate { hint: h.clone(), facts: alive(pid as i32), at: now });
        sink.evidence(EvidenceUpdate { hint: h, facts: meta_path(path), at: now });
    }
    world.claude_team_adapter().team(observation(now));
}

fn scene() -> Scene {
    let rig = Rig::new();
    rig.subscribe();
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (lead_path, alpha_path) = (base.join("lead.jsonl"), base.join("alpha.jsonl"));
    seed(&rig.world, &rig.sink, rig.now(), &lead_path, &alpha_path);
    rig.pump();
    Scene { rig, _dir: dir, lead_path, alpha_path }
}

/// What the follower would hand the adapter for these lines (T04's own
/// structure parser: the same path as in the app).
fn observe_rows(world: &World, vsid: &str, path: &Path, lines: &[String]) {
    let facts: Vec<_> = lines.iter().filter_map(|l| row_facts(l.as_bytes()).0).collect();
    world.claude_team_adapter().rows(path, &claude_hint(Some(vsid), None), &facts);
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn append_lines(path: &Path, lines: &[String]) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all((lines.join("\n") + "\n").as_bytes()).unwrap();
}

impl Scene {
    fn world(&self) -> Arc<World> {
        Arc::new(self.rig.world.clone())
    }

    fn uid(&self, vsid: &str) -> SessionUid {
        self.rig.uid_of(vsid)
    }

    /// The lead sends `body` to alpha (both rows observed and written).
    fn lead_sends(&self, at: EpochMs, tool_use_id: &str, body: &str, summary: &str, msg_id: Option<&str>) -> Vec<String> {
        let rows = send_rows(at, tool_use_id, "alpha", body, summary, msg_id);
        observe_rows(&self.rig.world, &lead_sid(), &self.lead_path, &rows);
        rows
    }

    /// Alpha's turn-opening row delivers lead blocks (observed, not written).
    fn alpha_receives(&self, at: EpochMs, uuid: &str, blocks: &[(&str, &str, Option<&str>)]) -> String {
        let row = delivered_row(at, uuid, blocks);
        observe_rows(&self.rig.world, &alpha_sid(), &self.alpha_path, std::slice::from_ref(&row));
        row
    }

    fn body(&self, key: &MessageKey) -> TeamMessageBodyRes {
        MessageBodySource::new(self.world()).body(key).expect("World holds the message")
    }

    fn reader(&self) -> MessageBodyReader {
        let api: Arc<dyn WorldApi> = self.world();
        MessageBodyReader::new(api)
    }
}

fn found(res: &TeamMessageBodyRes) -> (Option<&str>, Option<&str>, BodyFrom) {
    (res.text.as_deref(), res.summary.as_deref(), res.source)
}

// ---------------------------------------------------------------------------
// A27.1 — the locator
// ---------------------------------------------------------------------------

#[test]
fn t_body_locator_carries_both_ends_the_time_and_the_transcript_evidence() {
    let _s = serial();
    let s = scene();
    let t = s.rig.now();
    s.lead_sends(t, "toolu_01", "check the parser", "parser check", Some("m-1"));
    let key = MessageKey::derive(&team_key(), "m-1");
    let loc = s.rig.world.message_locator(&key).expect("located");
    assert_eq!(loc.team, team_key());
    assert_eq!((loc.kind, loc.status, loc.at), (MessageKind::Plain, DeliveryStatus::Sent, t));
    assert_eq!(loc.sender.as_ref(), Some(&s.uid(&lead_sid())));
    assert_eq!(loc.recipient.as_ref(), Some(&s.uid(&alpha_sid())));
    assert_eq!((loc.sender_name.as_deref(), loc.recipient_name.as_deref()), (Some("team-lead"), Some("alpha")));
    assert_eq!(loc.tool_use_id.as_deref(), Some("toolu_01"), "the sender's SendMessage id");
    assert_eq!(loc.delivered, None, "not delivered yet");

    s.alpha_receives(t + 5_000, "o-1", &[("team-lead", "check the parser", Some("parser check"))]);
    let loc = s.rig.world.message_locator(&key).unwrap();
    assert_eq!(loc.status, DeliveryStatus::Delivered);
    assert_eq!(loc.delivered, Some(DeliveredBlock { row_uuid: "o-1".into(), index: 0 }), "the delivering row and block");
    assert_eq!(loc.tool_use_id.as_deref(), Some("toolu_01"), "kept");

    // No content in the locator: ids and times only.
    let dbg = format!("{loc:?}");
    assert!(!dbg.contains("check the parser") && !dbg.contains("parser check"), "{dbg}");

    assert!(s.rig.world.message_locator(&MessageKey::derive(&team_key(), "m-none")).is_none(), "unknown key");
}

// ---------------------------------------------------------------------------
// A27.2 — each source, unavailable
// ---------------------------------------------------------------------------

#[test]
fn t_body_from_the_recipient_transcript() {
    let _s = serial();
    let s = scene();
    let t = s.rig.now();
    let sent = s.lead_sends(t, "toolu_01", "first message", "first", Some("m-1"));
    // Two blocks in one delivering row (a batch): the recorded index picks ours.
    s.lead_sends(t + 1_000, "toolu_02", "second message", "second", Some("m-2"));
    let row = s.alpha_receives(
        t + 9_000,
        "o-1",
        &[("team-lead", "first message", Some("first")), ("team-lead", "second message", Some("second"))],
    );
    write_lines(&s.lead_path, &sent);
    write_lines(&s.alpha_path, &[filler_row(t, 0, 512), row, filler_row(t + 10_000, 1, 512)]);
    let res = s.body(&MessageKey::derive(&team_key(), "m-2"));
    assert_eq!(found(&res), (Some("second message"), Some("second"), BodyFrom::RecipientTranscript));
    let res = s.body(&MessageKey::derive(&team_key(), "m-1"));
    assert_eq!(found(&res), (Some("first message"), Some("first"), BodyFrom::RecipientTranscript));
}

#[test]
fn t_body_from_the_sender_tool_use() {
    let _s = serial();
    let s = scene();
    let t = s.rig.now();
    // Sent while alpha is busy: alpha's transcript has no block yet (R6 §5
    // LP-17a), so the body comes from the lead's SendMessage input.
    let mut lead = s.lead_sends(t, "toolu_01", "look at db/seed.ts", "seed review", Some("m-1"));
    lead.extend(s.lead_sends(t + 2_000, "toolu_02", "another one", "other", Some("m-2")));
    write_lines(&s.lead_path, &lead);
    // An earlier block from the lead in alpha's transcript is another
    // message: a message still on its way never takes its body.
    let old = delivered_row(t - 30_000, "o-old", &[("team-lead", "an older message", None)]);
    write_lines(&s.alpha_path, &[old]);
    let res = s.body(&MessageKey::derive(&team_key(), "m-1"));
    assert_eq!(found(&res), (Some("look at db/seed.ts"), Some("seed review"), BodyFrom::SenderToolUse));

    // Delivered, but the recipient's transcript is gone: the sender's call.
    s.alpha_receives(t + 9_000, "o-1", &[("team-lead", "look at db/seed.ts", Some("seed review"))]);
    assert_eq!(s.rig.world.message_locator(&MessageKey::derive(&team_key(), "m-1")).unwrap().status, DeliveryStatus::Delivered);
    std::fs::remove_file(&s.alpha_path).unwrap();
    let res = s.body(&MessageKey::derive(&team_key(), "m-1"));
    assert_eq!(found(&res), (Some("look at db/seed.ts"), Some("seed review"), BodyFrom::SenderToolUse));
}

#[test]
fn t_body_unavailable_when_neither_transcript_has_it() {
    let _s = serial();
    let s = scene();
    let t = s.rig.now();
    s.lead_sends(t, "toolu_01", "lost body", "lost", Some("m-1"));
    s.alpha_receives(t + 3_000, "o-1", &[("team-lead", "lost body", Some("lost"))]);
    let key = MessageKey::derive(&team_key(), "m-1");
    let unavailable = TeamMessageBodyRes { text: None, summary: None, source: BodyFrom::Unavailable };
    // (a) No files at all.
    assert_eq!(s.body(&key), unavailable);
    // (b) Files without the rows.
    write_lines(&s.lead_path, &filler(t, 2));
    write_lines(&s.alpha_path, &filler(t, 2));
    assert_eq!(s.body(&key), unavailable);
    // (c) A directory and a FIFO where the transcripts should be: refused by
    // safe_read (regular files only), never blocking.
    std::fs::remove_file(&s.lead_path).unwrap();
    std::fs::remove_file(&s.alpha_path).unwrap();
    std::fs::create_dir(&s.lead_path).unwrap();
    let c = std::ffi::CString::new(s.alpha_path.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path; mkfifo has no other preconditions.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert_eq!(s.body(&key), unavailable);
    // (d) A symlink to a real transcript is not followed (O_NOFOLLOW).
    std::fs::remove_file(&s.alpha_path).unwrap();
    let real = s.alpha_path.with_file_name("real.jsonl");
    write_lines(&real, &[delivered_row(t + 3_000, "o-1", &[("team-lead", "lost body", Some("lost"))])]);
    std::os::unix::fs::symlink(&real, &s.alpha_path).unwrap();
    assert_eq!(s.body(&key), unavailable);
    // (e) A message World does not hold: no source answer at all.
    assert!(MessageBodySource::new(s.world()).body(&MessageKey::derive(&team_key(), "m-none")).is_none());
}

#[test]
fn t_body_never_opens_a_refused_name() {
    let _s = serial();
    let rig = Rig::new();
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    // World knows these paths, but the never-open filter refuses them by name.
    let lead = base.join("oauth-token.jsonl");
    let alpha = base.join("history.jsonl");
    seed(&rig.world, &rig.sink, rig.now(), &lead, &alpha);
    let t = rig.now();
    let rows = send_rows(t, "toolu_01", "alpha", "secret-ish", "s", Some("m-1"));
    observe_rows(&rig.world, &lead_sid(), &lead, &rows);
    write_lines(&lead, &rows);
    write_lines(&alpha, &[delivered_row(t + 1_000, "o-1", &[("team-lead", "secret-ish", Some("s"))])]);
    rig.pump();
    gilde_code_lib::discovery::safe_read::reset_stats();
    let res = MessageBodySource::new(Arc::new(rig.world.clone())).body(&MessageKey::derive(&team_key(), "m-1")).unwrap();
    assert_eq!(res.source, BodyFrom::Unavailable);
    assert_eq!(gilde_code_lib::discovery::safe_read::stats().opened, 0, "nothing was opened");
}

// ---------------------------------------------------------------------------
// the 16 KiB cut and the 8 MiB scan bound
// ---------------------------------------------------------------------------

#[test]
fn t_body_text_is_cut_at_16_kib_on_a_grapheme_boundary() {
    let _s = serial();
    let s = scene();
    let t = s.rig.now();
    // A family emoji is one grapheme of 25 bytes (4 scalars + 3 ZWJ): a
    // byte cut at 16 KiB would split one.
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
    assert_eq!((family.len(), family.graphemes(true).count()), (25, 1));
    let body: String = ["한글 문장입니다. ", family].concat().repeat(900);
    assert!(body.len() > 2 * 16 * 1024);
    let sent = s.lead_sends(t, "toolu_01", &body, "long", Some("m-1"));
    let row = s.alpha_receives(t + 1_000, "o-1", &[("team-lead", &body, Some("long"))]);
    write_lines(&s.lead_path, &sent);
    write_lines(&s.alpha_path, &[row]);
    const CAP: usize = 16 * 1024;
    assert_eq!(BODY_TEXT_CAP, TextCap::Bytes(CAP));
    let key = MessageKey::derive(&team_key(), "m-1");
    let from_recipient = s.body(&key);
    std::fs::remove_file(&s.alpha_path).unwrap();
    let from_sender = s.reader().read(&s.rig.world.message_locator(&key).unwrap());
    for (res, from) in [(from_recipient, BodyFrom::RecipientTranscript), (from_sender, BodyFrom::SenderToolUse)] {
        assert_eq!(res.source, from);
        let text = res.text.unwrap();
        assert!(text.len() <= CAP, "{} bytes", text.len());
        assert!(text.len() > CAP - 64, "cut near the cap, not far below it");
        let cut = text.strip_suffix('…').expect("a cut is marked with …");
        assert!(body.starts_with(cut), "a prefix of the body");
        assert!(body.grapheme_indices(true).any(|(i, _)| i == cut.len()), "the cut is a grapheme boundary");
        assert_eq!(res.summary.as_deref(), Some("long"));
    }
}

#[test]
fn rfc3339_of_t0() {
    assert_eq!(rfc3339(T0), "2026-10-23T10:00:00.000Z");
    assert_eq!(rfc3339(T0 + 3_723_045), "2026-10-23T11:02:03.045Z");
    assert_eq!(gilde_code_lib::render::transcript::parse_rfc3339_ms(&rfc3339(T0 + 1)), Some(T0 + 1));
}

#[test]
fn t_body_scan_reads_at_most_8_mib_per_file() {
    let _s = serial();
    let s = scene();
    let t = s.rig.now();
    let sent = s.lead_sends(t, "toolu_01", "deep body", "deep", Some("m-1"));
    let row = s.alpha_receives(t + 1_000, "o-1", &[("team-lead", "deep body", Some("deep"))]);
    let key = MessageKey::derive(&team_key(), "m-1");
    let loc = s.rig.world.message_locator(&key).unwrap();
    // 7 MiB after the rows: still found.
    let mut lead = sent.clone();
    lead.extend(filler(t + 2_000, 7 * 16));
    write_lines(&s.lead_path, &lead);
    let mut alpha = vec![row.clone()];
    alpha.extend(filler(t + 2_000, 7 * 16));
    write_lines(&s.alpha_path, &alpha);
    let (res, scan) = s.reader().read_scanned(&loc);
    assert_eq!(found(&res), (Some("deep body"), Some("deep"), BodyFrom::RecipientTranscript));
    assert!(scan.recipient_bytes > 7 * MIB as u64 && scan.recipient_bytes <= BODY_SCAN_BUDGET);
    // 2 MiB more: both rows lie beyond the bound — unavailable, each file
    // read for exactly the budget and no more.
    append_lines(&s.lead_path, &filler(t + 3_000, 2 * 16));
    append_lines(&s.alpha_path, &filler(t + 3_000, 2 * 16));
    let (res, scan) = s.reader().read_scanned(&loc);
    assert_eq!(res.source, BodyFrom::Unavailable);
    assert_eq!((scan.recipient_bytes, scan.sender_bytes), (BODY_SCAN_BUDGET, BODY_SCAN_BUDGET));
    // One line larger than the budget at the end of the recipient's file:
    // the scan stops at the bound (never buffers the line whole) and the
    // sender's call — within the bound again — answers.
    write_lines(&s.lead_path, &sent);
    append_lines(&s.alpha_path, &[filler_row(t + 4_000, 99, 9 * MIB)]);
    let (res, scan) = s.reader().read_scanned(&loc);
    assert_eq!(found(&res), (Some("deep body"), Some("deep"), BodyFrom::SenderToolUse));
    assert_eq!(scan.recipient_bytes, BODY_SCAN_BUDGET);
    assert!(scan.sender_bytes < 64 * 1024);
}

// ---------------------------------------------------------------------------
// matching without exact ids (sender + time window)
// ---------------------------------------------------------------------------

/// A locator as World builds it for a message seen without any transcript
/// evidence (e.g. from an inbox entry).
fn bare_locator(s: &Scene, key: MessageKey, at: EpochMs, status: DeliveryStatus) -> MessageLocator {
    MessageLocator {
        team: team_key(),
        key,
        kind: MessageKind::Plain,
        status,
        at,
        sender: Some(s.uid(&lead_sid())),
        sender_name: Some("team-lead".into()),
        recipient: Some(s.uid(&alpha_sid())),
        recipient_name: Some("alpha".into()),
        tool_use_id: None,
        delivered: None,
    }
}

#[test]
fn t_body_time_window_match_is_unique_or_nothing() {
    let _s = serial();
    let s = scene();
    let t = s.rig.now();
    let key = MessageKey::derive(&team_key(), "inbox-only");
    let r = s.reader();
    // Recipient: the nearest lead block within 120 s.
    write_lines(
        &s.alpha_path,
        &[
            delivered_row(t - 100_000, "o-0", &[("team-lead", "too early? no: in window", None)]),
            delivered_row(t + 3_000, "o-1", &[("team-lead", "the near one", None)]),
            delivered_row(t + 3_000 + BODY_WINDOW_MS + 1, "o-2", &[("team-lead", "outside the window", None)]),
            delivered_row(t + 1_000, "o-3", &[("beta", "another sender", None)]),
        ],
    );
    let res = r.read(&bare_locator(&s, key.clone(), t, DeliveryStatus::Delivered));
    assert_eq!(found(&res), (Some("the near one"), None, BodyFrom::RecipientTranscript));
    // Two lead blocks in the nearest row: ambiguous → not the recipient's.
    write_lines(&s.alpha_path, &[delivered_row(t + 3_000, "o-1", &[("team-lead", "one", None), ("team-lead", "two", None)])]);
    // Sender: two calls to alpha, one strictly nearer.
    let mut lead = send_rows(t + 500, "toolu_a", "alpha", "the nearer call", "a", None);
    lead.extend(send_rows(t + 5_000, "toolu_b", "alpha", "the farther call", "b", None));
    lead.extend(send_rows(t + 100, "toolu_c", "beta", "to someone else", "c", None));
    write_lines(&s.lead_path, &lead);
    let res = r.read(&bare_locator(&s, key.clone(), t, DeliveryStatus::Delivered));
    assert_eq!(found(&res), (Some("the nearer call"), Some("a"), BodyFrom::SenderToolUse));
    // Equidistant calls: ambiguous on both sides → unavailable.
    let mut lead = send_rows(t - 2_000, "toolu_a", "alpha", "before", "a", None);
    lead.extend(send_rows(t + 2_000, "toolu_b", "alpha", "after", "b", None));
    write_lines(&s.lead_path, &lead);
    assert_eq!(r.read(&bare_locator(&s, key.clone(), t, DeliveryStatus::Delivered)).source, BodyFrom::Unavailable);
    // A call whose result names another msg_id is another message.
    write_lines(&s.lead_path, &send_rows(t + 500, "toolu_x", "alpha", "someone else's", "x", Some("m-other")));
    assert_eq!(r.read(&bare_locator(&s, key.clone(), t, DeliveryStatus::Sent)).source, BodyFrom::Unavailable);
    // ... and the one whose result names this message's msg_id is exact,
    // also far outside the window.
    let ours = MessageKey::derive(&team_key(), "m-ours");
    let mut lead = send_rows(t - 3_600_000, "toolu_o", "alpha", "ours, an hour ago", "o", Some("m-ours"));
    lead.extend(send_rows(t + 500, "toolu_x", "alpha", "someone else's", "x", Some("m-other")));
    write_lines(&s.lead_path, &lead);
    let res = r.read(&bare_locator(&s, ours, t, DeliveryStatus::Sent));
    assert_eq!(found(&res), (Some("ours, an hour ago"), Some("o"), BodyFrom::SenderToolUse));
    // A recipient block that carries a msg_id attribute matches by key only.
    write_lines(&s.lead_path, &[]);
    let tagged = json!({"type": "user", "uuid": "o-9", "timestamp": rfc3339(t + 1_000),
        "message": {"role": "user", "content":
            "<teammate-message teammate_id=\"team-lead\" msg_id=\"m-other\">not ours</teammate-message>\n<teammate-message teammate_id=\"team-lead\" msg_id=\"m-tag\">ours by msg_id</teammate-message>"}})
    .to_string();
    write_lines(&s.alpha_path, &[tagged]);
    let res = r.read(&bare_locator(&s, MessageKey::derive(&team_key(), "m-tag"), t - 3_600_000, DeliveryStatus::Sent));
    assert_eq!(found(&res), (Some("ours by msg_id"), None, BodyFrom::RecipientTranscript));
    assert_eq!(r.read(&bare_locator(&s, key, t, DeliveryStatus::Delivered)).source, BodyFrom::Unavailable, "tagged blocks of other messages never match by time");
}

#[test]
fn t_body_recorded_row_must_still_agree() {
    let _s = serial();
    let s = scene();
    let t = s.rig.now();
    s.lead_sends(t, "toolu_01", "the real body", "real", Some("m-1"));
    s.alpha_receives(t + 1_000, "o-1", &[("team-lead", "the real body", Some("real"))]);
    let key = MessageKey::derive(&team_key(), "m-1");
    // The file now holds another row under that uuid whose block 0 is an
    // idle notification of another sender: never taken; the sender's call
    // answers instead.
    let changed = delivered_row(t + 1_000, "o-1", &[("beta", r#"{"type":"idle_notification","from":"beta"}"#, None)]);
    write_lines(&s.alpha_path, &[changed]);
    write_lines(&s.lead_path, &send_rows(t, "toolu_01", "alpha", "the real body", "real", Some("m-1")));
    let res = s.body(&key);
    assert_eq!(found(&res), (Some("the real body"), Some("real"), BodyFrom::SenderToolUse));
}

#[test]
fn t_body_malformed_and_hostile_rows_are_skipped() {
    let _s = serial();
    let s = scene();
    let t = s.rig.now();
    let sent = s.lead_sends(t, "toolu_01", "survives", "ok", Some("m-1"));
    let row = s.alpha_receives(t + 1_000, "o-1", &[("team-lead", "survives", Some("ok"))]);
    // Newer garbage: torn JSON, invalid UTF-8, a lone surrogate, a wrapper
    // that never closes, ANSI/bidi in a body, an unterminated last line.
    let mut alpha = vec![row];
    alpha.push(r#"{"type":"user","uuid":"o-2","message":{"content":"<teammate-message teammate_id=\"team-lead\">torn"#.into());
    alpha.push(r#"{"type":"user","uuid":"o-3","timestamp":"bad","message":{"role":"user","content":"\ud800 <teammate-message teammate_id=\"team-lead\">x"}}"#.into());
    write_lines(&s.alpha_path, &alpha);
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(&s.alpha_path).unwrap();
        f.write_all(b"\xff\xfe{\"type\":\"user\"}\n{\"type\":\"user\",\"uuid\":\"o-4\"").unwrap();
    }
    write_lines(&s.lead_path, &sent);
    let res = s.body(&MessageKey::derive(&team_key(), "m-1"));
    assert_eq!(found(&res), (Some("survives"), Some("ok"), BodyFrom::RecipientTranscript));
    // Escape sequences and bidi overrides never reach the WebView.
    let hostile = "red \u{1b}[31mtext\u{1b}[0m \u{202e}reversed\u{202c} \u{7}bell";
    s.lead_sends(t + 2_000, "toolu_02", hostile, "\u{1b}]0;title\u{7}sum", Some("m-2"));
    let row = s.alpha_receives(t + 3_000, "o-5", &[("team-lead", hostile, Some("plain"))]);
    write_lines(&s.alpha_path, &[row]);
    let res = s.body(&MessageKey::derive(&team_key(), "m-2"));
    let text = res.text.unwrap();
    assert!(!text.contains('\u{1b}') && !text.contains('\u{202e}') && !text.contains('\u{7}'), "{text:?}");
    assert!(text.contains("text") && text.contains("reversed"));
}

// ---------------------------------------------------------------------------
// privacy: the canary
// ---------------------------------------------------------------------------

const CANARY: &str = "CANARY-T27-5b1d0c9e";

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

fn capture<R>(f: impl FnOnce() -> R) -> (R, String) {
    let buf = LogBuf::default();
    let sub = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(buf.clone())
        .finish();
    let r = tracing::subscriber::with_default(sub, f);
    let logs = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
    (r, logs)
}

#[test]
fn t_body_canary_never_reaches_a_store_row_a_patch_a_meta_row_or_a_log() {
    let _s = serial();
    let (s, logs) = capture(|| {
        let s = scene();
        let t = s.rig.now();
        let body = format!("please rotate the {CANARY} key");
        let summary = format!("summary {CANARY}");
        let sent = s.lead_sends(t, "toolu_01", &body, &summary, Some("m-1"));
        let row = s.alpha_receives(t + 1_000, "o-1", &[("team-lead", &body, Some(&summary))]);
        // A malformed row carrying the canary is skipped silently.
        let torn = format!(r#"{{"type":"user","uuid":"o-2","message":{{"content":"<teammate-message teammate_id=\"team-lead\">{CANARY}"#);
        write_lines(&s.alpha_path, &[row, torn]);
        write_lines(&s.lead_path, &sent);
        let key = MessageKey::derive(&team_key(), "m-1");
        let res = s.body(&key);
        assert_eq!(res.source, BodyFrom::RecipientTranscript);
        assert!(res.text.as_deref().unwrap().contains(CANARY) && res.summary.as_deref().unwrap().contains(CANARY), "the canary is real");
        std::fs::remove_file(&s.alpha_path).unwrap();
        let res = s.body(&key);
        assert_eq!(res.source, BodyFrom::SenderToolUse);
        assert!(res.text.as_deref().unwrap().contains(CANARY));
        s.rig.pump();
        s
    });
    assert!(logs.contains("team message body lookup"), "the capture saw the lookups:\n{logs}");
    assert!(!logs.contains(CANARY), "a log line carries the body:\n{logs}");
    let snap = serde_json::to_string(&s.rig.world.snapshot()).unwrap();
    assert!(!snap.contains(CANARY), "the store holds the body");
    for p in s.rig.patches.0.lock().iter() {
        assert!(!serde_json::to_string(p).unwrap().contains(CANARY), "a patch carries the body");
    }
    for row in s.rig.meta.rows() {
        assert!(!format!("{row:?}").contains(CANARY), "a session_meta row carries the body");
    }
    let loc = s.rig.world.message_locator(&MessageKey::derive(&team_key(), "m-1")).unwrap();
    assert!(!format!("{loc:?}").contains(CANARY));
}

// ---------------------------------------------------------------------------
// T-wiring: the command is live through the composition root, with the LRU
// ---------------------------------------------------------------------------

mod wired {
    use super::*;
    use crate::fixtures::Hermetic;
    use gilde_code_lib::commands::registry;
    use gilde_code_lib::commands::store::LocalStore;
    use gilde_code_lib::lifecycle::boot::{self, SetupOutcome, SetupOutput};
    use gilde_code_lib::lifecycle::wiring::{self, Core, Options, Wired};
    use gilde_code_lib::paths::AppPaths;
    use gilde_code_lib::sidecar::SidecarRuntime;
    use gilde_code_lib::state::Services;
    use gilde_code_lib::store::now_ms;
    use gilde_code_lib::supervise::health::HealthState;
    use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};
    use tauri::webview::InvokeRequest;
    use tauri::WebviewWindow;

    fn invoke(w: &WebviewWindow<MockRuntime>, cmd: &str, req: Value) -> Result<Value, Value> {
        get_ipc_response(
            w,
            InvokeRequest {
                cmd: cmd.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost".parse().unwrap(),
                body: tauri::ipc::InvokeBody::Json(json!({ "req": req })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|b| b.deserialize::<Value>().unwrap_or(Value::Null))
    }

    /// The composition root on a hermetic home, with a mock app whose IPC
    /// runs the real `team_message_body` command (the managed `TeamState`
    /// and `Arc<World>`).
    struct Ctx {
        wired: Arc<Wired>,
        w: WebviewWindow<MockRuntime>,
        home: PathBuf,
        _app: tauri::App<MockRuntime>,
        _core: Core,
        _out: Box<SetupOutput>,
    }

    impl Drop for Ctx {
        fn drop(&mut self) {
            self.wired.shutdown();
        }
    }

    impl Ctx {
        fn call_raw(&self, req: Value) -> Result<Value, Value> {
            tokio::task::block_in_place(|| invoke(&self.w, "team_message_body", req))
        }

        fn call(&self, key: &MessageKey) -> Result<Value, Value> {
            self.call_raw(json!({ "messageKey": key.as_str() }))
        }

        fn world(&self) -> Arc<World> {
            self.wired.world.clone()
        }

        /// A native team in the wired World; its transcripts' paths under
        /// the hermetic home (the files are not written).
        fn team(&self, now: EpochMs) -> (PathBuf, PathBuf) {
            let base = self.home.join("t27");
            std::fs::create_dir_all(&base).unwrap();
            let (lead_path, alpha_path) = (base.join("lead.jsonl"), base.join("alpha.jsonl"));
            let world = self.world();
            seed(&world, &world.sink(), now, &lead_path, &alpha_path);
            (lead_path, alpha_path)
        }
    }

    async fn with_wired<F, Fut>(f: F)
    where
        F: FnOnce(Ctx) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _s = SERIAL.lock().await;
        let h = Hermetic::new();
        let _env = h.enter();
        let home = std::fs::canonicalize(h.home()).unwrap();
        let out = match boot::setup_core(AppPaths::for_home(&home), "0.0.0-test", "build-test").await.expect("setup") {
            SetupOutcome::Ready(out) => out,
            SetupOutcome::SecondInstance { .. } => panic!("no other instance holds the lock"),
        };
        // A38.2: detection is pinned to the fixture dir (no login shell, no real CLI).
        out.config.patch(h.detection_pins()).expect("detection pins");
        out.config.flush().expect("flush the pins");
        out.lifecycle.window_visible(now_ms());
        boot::run_boot_sweep(&out.lifecycle, &out.supervisor).await;
        let core = Core {
            paths: out.paths.clone(),
            db: out.db.clone(),
            config: out.config.clone(),
            lifecycle: out.lifecycle.clone(),
            supervisor: out.supervisor.clone(),
            services: Services::new(),
            health: HealthState::new(),
        };
        let local = LocalStore::new();
        core.lifecycle.feed.set_local_store(local.clone());
        let (platform, _recorded) =
            wiring::fakes::platform(SidecarRuntime::explicit("/nonexistent/node", "/nonexistent/sidecar.cjs"));
        let (discovery, procs) = wiring::fakes::hermetic_discovery(&home);
        let wired = Arc::new(
            wiring::wire(
                &core,
                platform,
                Options {
                    discovery: Some(discovery),
                    procs: Some(procs),
                    follow_watch: false,
                    background: false,
                    runtime: tokio::runtime::Handle::current(),
                },
            )
            .await
            .expect("wired"),
        );
        let app = mock_builder().invoke_handler(registry::handler()).build(tauri::generate_context!(test = true)).unwrap();
        wiring::manage_core(app.handle(), &core, local);
        wiring::manage_all(app.handle(), &wired);
        let w = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
        f(Ctx { wired, w, home, _app: app, _core: core, _out: out }).await;
    }

    /// T-wiring-messageBodies (A27.3): the composition root registers the
    /// body source, so `team_message_body` answers from the transcripts
    /// (never `notBuilt`); a repeat is served from T07's LRU (memory only);
    /// an error never carries a body.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn t_wiring_team_message_body_is_live_with_the_lru() {
        with_wired(|c| async move {
            // Live: an unknown message is NOT_FOUND, never UNSUPPORTED{notBuilt}.
            let unknown = MessageKey::derive(&team_key(), "m-none");
            let err = c.call(&unknown).unwrap_err();
            assert_eq!(err["code"], "NOT_FOUND", "{err}");

            let world = c.world();
            let now = now_ms();
            let (lead_path, alpha_path) = c.team(now);
            let body = format!("wired body {CANARY}");
            let sent = send_rows(now, "toolu_01", "alpha", &body, "wired", Some("m-1"));
            observe_rows(&world, &lead_sid(), &lead_path, &sent);
            let row = delivered_row(now + 1_000, "o-1", &[("team-lead", &body, Some("wired"))]);
            observe_rows(&world, &alpha_sid(), &alpha_path, std::slice::from_ref(&row));
            write_lines(&lead_path, &sent);
            write_lines(&alpha_path, &[row]);
            world.flush_now();
            let key = MessageKey::derive(&team_key(), "m-1");
            assert!(world.snapshot().messages.iter().any(|m| m.key == key), "the message is in the store");

            let res = c.call(&key).expect("a body");
            assert_eq!(res["source"], "recipientTranscript", "{res}");
            assert_eq!(res["text"].as_str(), Some(body.as_str()));
            assert_eq!(res["summary"].as_str(), Some("wired"));

            // LRU: the transcripts are gone, the repeat is served from memory.
            std::fs::remove_file(&lead_path).unwrap();
            std::fs::remove_file(&alpha_path).unwrap();
            let again = c.call(&key).expect("from the LRU");
            assert_eq!(again, res, "served from the in-memory LRU");

            // Errors carry no body (a malformed key and an unknown one).
            let bad = c.call_raw(json!({ "messageKey": format!("g_{CANARY}") })).unwrap_err();
            assert_eq!(bad["code"], "INVALID_ARGUMENT", "{bad}");
            for e in [bad, c.call(&unknown).unwrap_err()] {
                let s = e.to_string();
                assert!(!s.contains("wired body") && !s.contains(&body), "an error carries the body: {s}");
            }
            assert!(!serde_json::to_string(&world.snapshot()).unwrap().contains(CANARY), "the store holds no body");
        })
        .await;
    }

    /// A30.7 (1): an `unavailable` answer is not cached — once the
    /// transcript has the message, the next call returns its body (and that
    /// body is cached).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn t_wiring_an_unavailable_body_is_not_cached() {
        with_wired(|c| async move {
            let world = c.world();
            let now = now_ms();
            let (lead_path, _alpha_path) = c.team(now);
            let sent = send_rows(now, "toolu_01", "alpha", "on its way", "way", Some("m-1"));
            observe_rows(&world, &lead_sid(), &lead_path, &sent);
            world.flush_now();
            let key = MessageKey::derive(&team_key(), "m-1");
            // Neither transcript is on disk yet.
            let first = c.call(&key).expect("an answer");
            assert_eq!(first["source"], "unavailable", "{first}");
            assert_eq!(first["text"], Value::Null);
            // The sender's transcript appears: read again, not the cached miss.
            write_lines(&lead_path, &sent);
            let second = c.call(&key).expect("an answer");
            assert_eq!(second["source"], "senderToolUse", "{second}");
            assert_eq!(second["text"].as_str(), Some("on its way"));
            // A body is cached as before.
            std::fs::remove_file(&lead_path).unwrap();
            assert_eq!(c.call(&key).expect("from the LRU"), second);
        })
        .await;
    }

    /// A30.7 (2): the key is checked against World (up to 5,000 messages per
    /// team), not the store's newest 500 — every message `team_messages_page`
    /// shows can be read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn t_wiring_a_message_older_than_the_store_is_found() {
        with_wired(|c| async move {
            let world = c.world();
            let now = now_ms();
            let (lead_path, _alpha_path) = c.team(now);
            let mut rows = Vec::new();
            for i in 0..505i64 {
                rows.extend(send_rows(now + i * 10, &format!("toolu_{i:04}"), "alpha", &format!("body {i}"), "s", Some(&format!("m-{i}"))));
            }
            observe_rows(&world, &lead_sid(), &lead_path, &rows);
            write_lines(&lead_path, &rows);
            world.flush_now();
            let in_store: Vec<MessageKey> =
                world.snapshot().messages.into_iter().filter(|m| m.team_key == team_key()).map(|m| m.key).collect();
            assert_eq!(in_store.len(), 500, "the store keeps the newest 500");
            let oldest = MessageKey::derive(&team_key(), "m-0");
            assert!(!in_store.contains(&oldest), "the oldest is not in the store");
            let (page, _) = world.team_messages_page(&team_key(), now + 100_000, 500);
            assert!(page.len() == 500 && world.team_messages_page(&team_key(), now + 50, 10).0.iter().any(|m| m.key == oldest), "team_messages_page still shows it");
            let res = c.call(&oldest).expect("World holds it");
            assert_eq!(res["source"], "senderToolUse", "{res}");
            assert_eq!(res["text"].as_str(), Some("body 0"));
            assert_eq!(c.call(&MessageKey::derive(&team_key(), "m-9999")).unwrap_err()["code"], "NOT_FOUND");
        })
        .await;
    }

    /// Records where each read ran: `Handle::block_on` works on the blocking
    /// pool and panics on a thread that drives async tasks.
    struct PoolProbe(Mutex<Vec<bool>>);

    impl MessageBodies for PoolProbe {
        fn body(&self, _key: &MessageKey) -> Option<TeamMessageBodyRes> {
            let blocking_pool = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tokio::runtime::Handle::current().block_on(async {});
            }))
            .is_ok();
            self.0.lock().unwrap().push(blocking_pool);
            Some(TeamMessageBodyRes { text: Some("probe".into()), summary: None, source: BodyFrom::SenderToolUse })
        }
    }

    /// A30.7 (3): the transcript read runs on the blocking pool, never on a
    /// thread of the async runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn t_wiring_the_body_read_runs_on_the_blocking_pool() {
        with_wired(|c| async move {
            let world = c.world();
            let now = now_ms();
            let (lead_path, _alpha_path) = c.team(now);
            observe_rows(&world, &lead_sid(), &lead_path, &send_rows(now, "toolu_01", "alpha", "x", "s", Some("m-1")));
            world.flush_now();
            let probe = Arc::new(PoolProbe(Mutex::new(Vec::new())));
            c.wired.team.set_bodies(probe.clone());
            let res = c.call(&MessageKey::derive(&team_key(), "m-1")).expect("an answer");
            assert_eq!(res["text"].as_str(), Some("probe"));
            assert_eq!(*probe.0.lock().unwrap(), vec![true], "read once, on the blocking pool");
        })
        .await;
    }
}
