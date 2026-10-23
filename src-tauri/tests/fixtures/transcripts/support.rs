//! Test support for the render/follow suites (owner T04): fixture paths, a
//! collecting view channel, a fixed source resolver, evidence and row sinks.
//! Included with `#[path = "fixtures/transcripts/support.rs"] mod support;`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use gilde_code_lib::contract::common::AbsPath;
use gilde_code_lib::contract::content::{Block, ConvoEnvelope, ConvoEvent};
use gilde_code_lib::contract::error::IpcError;
use gilde_code_lib::contract::ids::SessionUid;
use gilde_code_lib::contract::services::{EvidenceSink, EvidenceUpdate, IdentityHint, LocationFacts};
use gilde_code_lib::contract::store::SourceStatus;
use gilde_code_lib::follow::hub::{ConvoChannel, ResolvedSource, SendOutcome, SourceResolver};
use gilde_code_lib::follow::tail::{FollowConfig, RowSink, TranscriptFormat};
use gilde_code_lib::render::transcript::{ContentBase, RowFacts};

/// `src-tauri/tests/fixtures/transcripts/<rel>`.
pub fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcripts").join(rel)
}

/// Copy a fixture into `dir` (tests append to and replace the copy).
pub fn copy_fixture(rel: &str, dir: &Path) -> PathBuf {
    let name = Path::new(rel).file_name().unwrap();
    let dest = dir.join(name);
    std::fs::copy(fixture(rel), &dest).unwrap();
    dest
}

/// A follower config for tests: no FSEvents, reads whenever asked.
pub fn manual_config() -> FollowConfig {
    FollowConfig {
        poll_interval: std::time::Duration::from_millis(0),
        min_read_interval: std::time::Duration::from_millis(0),
        missing_retry: std::time::Duration::from_millis(0),
        watch: false,
        ..FollowConfig::default()
    }
}

/// A view channel that records envelopes; `capacity` simulates backpressure.
#[derive(Default)]
pub struct Collect {
    pub got: Mutex<Vec<ConvoEnvelope>>,
    pub capacity: Mutex<Option<usize>>,
    pub closed: std::sync::atomic::AtomicBool,
}

impl Collect {
    pub fn new() -> Arc<Collect> {
        Arc::new(Collect::default())
    }
    pub fn events(&self) -> Vec<ConvoEvent> {
        self.got.lock().unwrap().iter().map(|e| e.event.clone()).collect()
    }
    pub fn envelopes(&self) -> Vec<ConvoEnvelope> {
        self.got.lock().unwrap().clone()
    }
    pub fn take(&self) -> Vec<ConvoEvent> {
        self.got.lock().unwrap().drain(..).map(|e| e.event).collect()
    }
    pub fn set_capacity(&self, cap: Option<usize>) {
        *self.capacity.lock().unwrap() = cap;
    }
}

impl ConvoChannel for Collect {
    fn send(&self, envelope: ConvoEnvelope) -> SendOutcome {
        if self.closed.load(Ordering::SeqCst) {
            return SendOutcome::Closed;
        }
        let mut got = self.got.lock().unwrap();
        if let Some(cap) = *self.capacity.lock().unwrap() {
            if got.len() >= cap {
                return SendOutcome::Full;
            }
        }
        got.push(envelope);
        SendOutcome::Sent
    }
}

/// A resolver that answers the same source for every uid, counting calls.
pub struct Fixed {
    pub source: Mutex<Result<ResolvedSource, IpcError>>,
    pub calls: AtomicUsize,
}

impl Fixed {
    pub fn file(path: &Path, format: TranscriptFormat, live: bool) -> Arc<Fixed> {
        Arc::new(Fixed {
            source: Mutex::new(Ok(ResolvedSource::File {
                path: path.to_path_buf(),
                format,
                base: ContentBase { location: AbsPath::parse("/w/demo").ok(), home: AbsPath::parse("/Users/tester").ok() },
                live,
            })),
            calls: AtomicUsize::new(0),
        })
    }
    pub fn answer(r: Result<ResolvedSource, IpcError>) -> Arc<Fixed> {
        Arc::new(Fixed { source: Mutex::new(r), calls: AtomicUsize::new(0) })
    }
}

impl SourceResolver for Fixed {
    fn resolve(&self, _uid: &SessionUid) -> Result<ResolvedSource, IpcError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.source.lock().unwrap().clone()
    }
}

/// Collects evidence updates.
#[derive(Default)]
pub struct Evidence {
    pub got: Mutex<Vec<EvidenceUpdate>>,
}

impl EvidenceSink for Evidence {
    fn evidence(&self, update: EvidenceUpdate) {
        self.got.lock().unwrap().push(update);
    }
    fn location(&self, _facts: LocationFacts) {}
    fn source_status(&self, _status: SourceStatus) {}
    fn account(&self, _account: gilde_code_lib::contract::account::Account) {}
}

/// Collects per-row structure facts.
#[derive(Default)]
pub struct Rows {
    pub got: Mutex<Vec<RowFacts>>,
}

impl RowSink for Rows {
    fn rows(&self, _path: &Path, _hint: &IdentityHint, rows: &[RowFacts]) {
        self.got.lock().unwrap().extend_from_slice(rows);
    }
}

/// The blocks of a `Reset` event.
pub fn reset_blocks(e: &ConvoEvent) -> Vec<Block> {
    match e {
        ConvoEvent::Reset { blocks, .. } => blocks.clone(),
        other => panic!("expected Reset, got {other:?}"),
    }
}

pub fn uid() -> SessionUid {
    SessionUid::mint()
}

/// Append raw bytes to a file.
pub fn append(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

/// A minimal Claude assistant text row.
pub fn assistant_row(uuid: &str, msg: &str, text: &str, ts: &str) -> String {
    serde_json::json!({
        "type": "assistant", "uuid": uuid, "timestamp": ts, "sessionId": "0d4c1f6e-6b8a-4c7e-9a51-3f2b8c9d0e11",
        "message": {"id": msg, "role": "assistant", "content": [{"type": "text", "text": text}],
                    "usage": {"input_tokens": 1, "output_tokens": 2}}
    })
    .to_string()
        + "\n"
}

/// A minimal Claude user prompt row.
pub fn user_row(uuid: &str, text: &str, ts: &str) -> String {
    serde_json::json!({"type": "user", "uuid": uuid, "timestamp": ts, "message": {"role": "user", "content": text}}).to_string() + "\n"
}
