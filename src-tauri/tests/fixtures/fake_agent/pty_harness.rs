//! Shared harness of the PTY integration tests (`tests/pty_fidelity.rs`,
//! `tests/pty_prompts.rs`), included with `#[path]` — not part of the
//! `fake_agent` binary.
//!
//! Every PTY is opened through the real [`PtyManager`] over the real
//! supervisor (`SupervisorApi`, in-memory registry): the §9.3 allowlist, the
//! durable row, `GILDE_SPAWN_ID`. Children get the hermetic environment only
//! (`HOME`, `GILDE_HOME`, a `PATH` of fixture binaries, `DISABLE_AUTOUPDATER=1`,
//! `AGENT_CLI_CREDENTIAL_STORE=memory`); no vendor CLI is reachable. Waits are
//! event-polled with generous ceilings — never a wall-clock budget (A10.7).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use parking_lot::Mutex;

use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::contract::activity::ExitInfo;
use gilde_code_lib::contract::common::{AbsPath, EpochMs};
use gilde_code_lib::contract::content::PtyChunk;
use gilde_code_lib::contract::ids::PtyId;
use gilde_code_lib::contract::process::ProcKind;
use gilde_code_lib::contract::services::{PtyExitSink, PtyHandle, PtyOpenSpec, SupervisorApi};
use gilde_code_lib::pty::{ChunkSink, PtyManager, PtyOptions};
use gilde_code_lib::store::Db;
use gilde_code_lib::supervise::registry::Supervisor;

/// The ceiling of every event-polled wait: generous on a loaded machine.
pub const CEILING: Duration = Duration::from_secs(30);

pub struct Harness {
    pub hermetic: crate::fixtures::Hermetic,
    pub db: Db,
    pub supervisor: Arc<Supervisor>,
    pub manager: Arc<PtyManager>,
    pub cwd: PathBuf,
}

impl Harness {
    pub async fn new() -> Harness {
        let hermetic = crate::fixtures::Hermetic::new();
        let db = Db::open_in_memory().expect("db");
        let config = Arc::new(
            ConfigStore::open_at(&hermetic.root().join("config.json")).expect("config"),
        );
        let supervisor = Arc::new(
            Supervisor::open_instance(&db, config, "0.0.0-test")
                .await
                .expect("open_instance"),
        );
        let api: Arc<dyn SupervisorApi> = supervisor.clone();
        let manager = Arc::new(PtyManager::with_api(api));
        let cwd = hermetic.root().join("project");
        std::fs::create_dir_all(&cwd).expect("cwd");
        Harness {
            hermetic,
            db,
            supervisor,
            manager,
            cwd,
        }
    }

    /// Put `fake_agent` on the hermetic `PATH` as `name` and allowlist it for
    /// the PTY kinds (as detection would for a resolved vendor binary, §9.3).
    pub fn install(&self, name: &str) -> AbsPath {
        let fixture = crate::fixtures::fake_agent_path();
        let path = self.hermetic.bin_dir().join(name);
        if !path.exists() {
            self.hermetic.install_fixture(name, &fixture);
        }
        gilde_code_lib::supervise::spawn::allow_executable(
            &path,
            &[
                gilde_code_lib::store::models::ProcKind::Pty,
                gilde_code_lib::store::models::ProcKind::Attach,
            ],
        );
        AbsPath::parse(path.to_string_lossy().into_owned()).expect("abs path")
    }

    /// A PTY spec for an installed fixture.
    pub fn spec(&self, argv0: &AbsPath, args: &[&str]) -> PtyOpenSpec {
        PtyOpenSpec {
            kind: ProcKind::Pty,
            argv0: argv0.clone(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: AbsPath::parse(self.cwd.to_string_lossy().into_owned()).expect("cwd"),
            env: self.hermetic.env(),
            cols: 120,
            rows: 40,
            parent_uid: None,
        }
    }

    /// `/bin/sh -c <script>` — the hosted-pane kind (§7.4.5); the shell is
    /// allowlisted for panes only.
    pub fn shell(&self, script: &str) -> PtyOpenSpec {
        PtyOpenSpec {
            kind: ProcKind::Pane,
            argv0: AbsPath::parse("/bin/sh").expect("sh"),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: AbsPath::parse(self.cwd.to_string_lossy().into_owned()).expect("cwd"),
            env: self.hermetic.env(),
            cols: 120,
            rows: 40,
            parent_uid: None,
        }
    }

    pub async fn open(&self, spec: PtyOpenSpec) -> (PtyHandle, Arc<ExitRecorder>) {
        self.open_with(spec, PtyOptions::default()).await
    }

    pub async fn open_with(&self, spec: PtyOpenSpec, options: PtyOptions) -> (PtyHandle, Arc<ExitRecorder>) {
        let exits = Arc::new(ExitRecorder::default());
        let handle = self
            .manager
            .open_with(spec, exits.clone(), options)
            .await
            .expect("open");
        (handle, exits)
    }

    /// The visible grid of a PTY, joined.
    pub fn screen(&self, pty: &PtyId) -> String {
        self.manager
            .status_engine()
            .screen(pty.as_str())
            .map(|s| s.lines.join("\n"))
            .unwrap_or_default()
    }

    /// A file in the temp tree (e.g. `FAKE_AGENT_INPUT_LOG`).
    pub fn temp_file(&self, name: &str) -> PathBuf {
        self.hermetic.tmp_dir().join(name)
    }
}

/// Records every exit the manager reports.
#[derive(Default)]
pub struct ExitRecorder {
    pub exits: Mutex<Vec<(PtyId, ExitInfo, EpochMs)>>,
}

impl PtyExitSink for ExitRecorder {
    fn exited(&self, pty: &PtyId, exit: ExitInfo, at: EpochMs) {
        self.exits.lock().push((pty.clone(), exit, at));
    }
}

impl ExitRecorder {
    pub fn first(&self) -> Option<ExitInfo> {
        self.exits.lock().first().map(|(_, e, _)| *e)
    }
}

/// A view that keeps every chunk.
#[derive(Default)]
pub struct Collect {
    pub chunks: Mutex<Vec<PtyChunk>>,
    /// While true the view "is wedged": it accepts nothing (and says so).
    pub wedged: std::sync::atomic::AtomicBool,
}

impl ChunkSink for Collect {
    fn send(&self, chunk: PtyChunk) -> bool {
        if self.wedged.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.chunks.lock().push(chunk);
        true
    }
}

impl Collect {
    /// `(offset, bytes)` of every data chunk, in delivery order.
    pub fn data(&self) -> Vec<(u64, Vec<u8>)> {
        self.chunks
            .lock()
            .iter()
            .filter_map(|c| match c {
                PtyChunk::Data { offset, b64 } => Some((
                    *offset,
                    base64::engine::general_purpose::STANDARD.decode(b64).expect("b64"),
                )),
                _ => None,
            })
            .collect()
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.data().into_iter().flat_map(|(_, b)| b).collect()
    }

    pub fn text(&self) -> String {
        self.bytes().iter().map(|b| *b as char).collect()
    }

    pub fn resyncs(&self) -> usize {
        self.chunks
            .lock()
            .iter()
            .filter(|c| matches!(c, PtyChunk::Resync { .. }))
            .count()
    }

    pub fn detached(&self) -> usize {
        self.chunks
            .lock()
            .iter()
            .filter(|c| matches!(c, PtyChunk::Detached { .. }))
            .count()
    }
}

/// Poll `f` (every 20 ms) until it holds or `ceiling` passes.
pub async fn wait_for(ceiling: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + ceiling;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub fn read_log(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// True while `pid` exists (signal 0 on a process the test started).
pub fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}
