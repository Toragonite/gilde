//! The G6 probe kit's shared harness (contract §14.5, A31.3). Owner: T33.
//!
//! Include from a `tests/verify_*.rs` file with
//!
//! ```ignore
//! mod fixtures;                                   // the T01 harness (Hermetic)
//! #[path = "fixtures/verify/mod.rs"]
//! mod verify;
//! ```
//!
//! * [`Probe`]: every probe test prints exactly one line
//!   `PROBE <id>: <observed>` — on success, and (from `Drop`) on failure, so
//!   the lead's record always has a line per probe. A variant whose observed
//!   result differs from the §14.5 "Expected" column is recorded with
//!   [`Probe::mismatch`]: it is **not** asserted (a mismatch is a finding for
//!   the lead, never fixed in the kit) and shows as `MISMATCH(...)` in the line.
//! * [`bare_app`] / [`invoke`]: Tauri's mock runtime with the generated
//!   context (the shipped capability ACL) and the real command registry, as
//!   `ipc_contract.rs` does; [`with_wired`]: the same IPC over the real
//!   composition root (`lifecycle::wiring`, real World, fakes only).
//! * [`Logs`]: a process-wide TRACE log capture (every thread), for "logged"
//!   and "never in logs" checks.
#![allow(dead_code)]

use std::fmt::Display;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::WebviewWindow;

use gilde_code_lib::commands::registry;
use gilde_code_lib::commands::store::LocalStore;
use gilde_code_lib::lifecycle::boot::{self, SetupOutcome, SetupOutput};
use gilde_code_lib::lifecycle::wiring::{self, Core, Options, Wired};
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::sidecar::SidecarRuntime;
use gilde_code_lib::state::Services;
use gilde_code_lib::store::now_ms;
use gilde_code_lib::supervise::health::HealthState;

// ---------------------------------------------------------------------------
// the PROBE line
// ---------------------------------------------------------------------------

/// One probe's evidence: its variants in order, printed as one line.
pub struct Probe {
    id: &'static str,
    parts: Vec<String>,
    mismatches: usize,
    printed: bool,
}

impl Probe {
    pub fn new(id: &'static str) -> Probe {
        Probe { id, parts: Vec::new(), mismatches: 0, printed: false }
    }

    /// A variant whose observed result is the §14.5 expectation (the caller
    /// asserted it).
    pub fn saw(&mut self, variant: impl Display, observed: impl Display) -> &mut Probe {
        self.parts.push(format!("{variant} → {observed}"));
        self
    }

    /// A variant whose observed result differs from §14.5's "Expected": a
    /// finding, recorded and not asserted.
    pub fn mismatch(&mut self, variant: impl Display, observed: impl Display, expected: impl Display) -> &mut Probe {
        self.mismatches += 1;
        self.parts.push(format!("{variant} → {observed} MISMATCH(§14.5 expects {expected})"));
        self
    }

    pub fn mismatches(&self) -> usize {
        self.mismatches
    }

    /// Print the line (once).
    pub fn finish(mut self) {
        self.print("");
    }

    fn print(&mut self, prefix: &str) {
        if self.printed {
            return;
        }
        self.printed = true;
        let body = if self.parts.is_empty() { "(no variant ran)".to_string() } else { self.parts.join("; ") };
        println!("PROBE {}: {prefix}{body}", self.id);
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.print("FAILED after: ");
        } else {
            self.print("");
        }
    }
}

/// A short form of an IPC answer: `OK`, or `CODE{param:value,…}`.
pub fn short(r: &Result<Value, Value>) -> String {
    match r {
        Ok(_) => "OK".into(),
        Err(e) => {
            let code = e.get("code").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| {
                // Not an IpcError: Tauri's own refusal (e.g. the ACL) — a string.
                format!("not invocable({})", e.as_str().unwrap_or("?").chars().take(60).collect::<String>())
            });
            let params = e
                .get("params")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, v)| format!("{k}:{}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            if params.is_empty() {
                code
            } else {
                format!("{code}{{{params}}}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// IPC through Tauri's mock runtime with the shipped ACL
// ---------------------------------------------------------------------------

/// `invoke(cmd, { req })` from the `main` window, as the WebView sends it.
pub fn invoke(w: &WebviewWindow<MockRuntime>, cmd: &str, req: Value) -> Result<Value, Value> {
    invoke_raw(w, cmd, json!({ "req": req }))
}

/// `invoke(cmd, args)` with the raw args object.
pub fn invoke_raw(w: &WebviewWindow<MockRuntime>, cmd: &str, args: Value) -> Result<Value, Value> {
    get_ipc_response(
        w,
        InvokeRequest {
            cmd: cmd.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    )
    .map(|b| b.deserialize::<Value>().unwrap_or(Value::Null))
}

/// A mock app with the real registry and the generated context, no managed
/// state (commands answer from their contract checks or `notBuilt`).
pub fn bare_app() -> (tauri::App<MockRuntime>, WebviewWindow<MockRuntime>) {
    let app = mock_builder().invoke_handler(registry::handler()).build(tauri::generate_context!(test = true)).unwrap();
    let w = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
    (app, w)
}

/// The composition root on a hermetic home (fakes only), with every state
/// managed into a mock app whose IPC runs the real commands.
pub struct Ctx {
    pub wired: Arc<Wired>,
    pub w: WebviewWindow<MockRuntime>,
    pub home: PathBuf,
    pub core: Core,
    pub recorded: wiring::fakes::Recorded,
    pub procs: Arc<gilde_code_lib::discovery::procs::FakeProcs>,
    _app: tauri::App<MockRuntime>,
    _out: Box<SetupOutput>,
}

impl Drop for Ctx {
    fn drop(&mut self) {
        self.wired.shutdown();
    }
}

impl Ctx {
    /// The real command through IPC, on a blocking section (the mock IPC
    /// waits for the async command on Tauri's runtime).
    pub fn call(&self, cmd: &str, req: Value) -> Result<Value, Value> {
        tokio::task::block_in_place(|| invoke(&self.w, cmd, req))
    }

    pub fn world(&self) -> Arc<gilde_code_lib::world::World> {
        self.wired.world.clone()
    }

    /// [`Ctx::call`] with the raw invoke args (e.g. `{ req, channel }` for a
    /// streaming command).
    pub fn call_raw(&self, cmd: &str, args: Value) -> Result<Value, Value> {
        tokio::task::block_in_place(|| invoke_raw(&self.w, cmd, args))
    }

    /// Another mock app (its own window and its own IPC rate limiter, A45.3)
    /// over the same composition root: every state managed as in
    /// [`with_wired`].
    pub fn another_app(&self) -> (tauri::App<MockRuntime>, WebviewWindow<MockRuntime>) {
        let app = mock_builder().invoke_handler(registry::handler()).build(tauri::generate_context!(test = true)).unwrap();
        wiring::manage_core(app.handle(), &self.core, LocalStore::new());
        wiring::manage_all(app.handle(), &self.wired);
        let w = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
        (app, w)
    }
}

/// Serializes the wired probes of one test binary: `Hermetic::enter` sets
/// process-wide env (`HOME`, `TMPDIR`, …).
pub static WIRED: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Run `f` against a fresh composition root. `prepare` may write files under
/// the hermetic home before discovery starts (vendor fixture trees).
pub async fn with_wired<P, F, Fut>(prepare: P, f: F)
where
    P: FnOnce(&std::path::Path),
    F: FnOnce(Ctx) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _s = WIRED.lock().await;
    let h = crate::fixtures::Hermetic::new();
    let _env = h.enter();
    let home = std::fs::canonicalize(h.home()).unwrap();
    prepare(&home);
    let out = match boot::setup_core(AppPaths::for_home(&home), "0.0.0-test", "build-test").await.expect("setup") {
        SetupOutcome::Ready(out) => out,
        SetupOutcome::SecondInstance { .. } => panic!("no other instance holds the lock"),
    };
    // A38.2: detection is pinned to the fixture dir (no login shell, no real
    // CLI); flushed now, so no debounced write lands inside a probe.
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
    // A runtime that cannot start: no probe may start the sidecar.
    let (platform, recorded) =
        wiring::fakes::platform(SidecarRuntime::explicit("/nonexistent/node", "/nonexistent/sidecar.cjs"));
    let (discovery, procs) = wiring::fakes::hermetic_discovery(&home);
    let wired = Arc::new(
        wiring::wire(
            &core,
            platform,
            Options {
                discovery: Some(discovery),
                procs: Some(procs.clone()),
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
    f(Ctx { wired, w, home, core, recorded, procs, _app: app, _out: out }).await;
}

// ---------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

/// The process-wide TRACE capture of this test binary (all threads).
pub struct Logs {
    buf: Buf,
}

impl Logs {
    /// Install (once) and return the capture.
    pub fn global() -> &'static Logs {
        static LOGS: OnceLock<Logs> = OnceLock::new();
        LOGS.get_or_init(|| {
            let buf = Buf::default();
            let sub = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(buf.clone())
                .finish();
            let _ = tracing::subscriber::set_global_default(sub);
            Logs { buf }
        })
    }

    /// Everything logged so far.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf.0.lock().unwrap()).into_owned()
    }

    /// The length now (to read only what follows with [`Logs::since`]).
    pub fn mark(&self) -> usize {
        self.buf.0.lock().unwrap().len()
    }

    pub fn since(&self, mark: usize) -> String {
        let b = self.buf.0.lock().unwrap();
        String::from_utf8_lossy(&b[mark.min(b.len())..]).into_owned()
    }
}
