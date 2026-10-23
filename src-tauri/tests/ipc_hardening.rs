//! A45 — a compromised WebView gets nowhere (G7c-13/14/15). Owner: T42.
//!
//! The reviewer's two evidence files (`g2/g7c` `tests/g7c_ipc_exec.rs`,
//! `tests/g7c_ipc_ui_state.rs`) ported as passing regressions, plus variants:
//!
//! * A45.1 — no IPC path carries a control, bidi or zero-width character
//!   (every executable key), and the native confirmation names — and the
//!   write applies — the canonical path, anything unprintable escaped;
//! * A45.2 — every UI key has its own content-free schema, `teamLayout:*`
//!   rows are capped at 64 (least recently written first), rows that fail
//!   their schema are dropped at boot;
//! * A45.3 — the central limiter answers `BUSY{retryAfterMs}` by class
//!   (`ui_state_set`, `pty_write`, `link_open`, `agents_rescan`), and
//!   detection reruns coalesce.
//!
//! Real IPC on the mock runtime with the shipped ACL; confirmations are
//! recording fakes; no vendor CLI runs (detection is pinned to fake scripts).

mod fixtures;
#[path = "fixtures/verify/mod.rs"]
mod verify;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::test::{mock_builder, MockRuntime};
use tauri::Manager as _;

use gilde_code_lib::agents::AgentRegistry;
use gilde_code_lib::commands::registry;
use gilde_code_lib::commands::settings::{ConfirmState, ExecutableConfirm};
use gilde_code_lib::commands::store::set_ui_state;
use gilde_code_lib::config::ConfigStore;
use gilde_code_lib::contract::ipc::SettingsSetExecutableReq;
use gilde_code_lib::contract::validate::request_from_payload;
use gilde_code_lib::lifecycle::boot::{self, Lifecycle, RescanMode, SetupOutcome};
use gilde_code_lib::paths::AppPaths;
use gilde_code_lib::store::dao;

use verify::{bare_app, invoke, with_wired};

const CANARY: &str = "G7C-CANARY-draft-content-7f3a9c";

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Tests that make temp dirs run one at a time with the wired ones: a wired
/// rig points the process `TMPDIR` into its own hermetic tree (removed when
/// it ends) for as long as it runs.
fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    verify::WIRED.blocking_lock()
}

#[derive(Default)]
struct Recording(Mutex<Vec<(String, String)>>);

#[async_trait]
impl ExecutableConfirm for Recording {
    async fn confirm(&self, key: &str, value_text: &str) -> bool {
        self.0.lock().push((key.to_string(), value_text.to_string()));
        true
    }
}

struct ExecApp {
    _home: tempfile::TempDir,
    home: PathBuf,
    config: Arc<ConfigStore>,
    shown: Arc<Recording>,
    w: tauri::WebviewWindow<MockRuntime>,
    _app: tauri::App<MockRuntime>,
}

/// A mock app with the real registry, settings and a recording confirmation.
fn exec_app() -> ExecApp {
    let dir = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(home.join(".gilde")).unwrap();
    let paths = Arc::new(AppPaths::for_home(&home));
    let config = Arc::new(ConfigStore::open_at(&home.join(".gilde/config.json")).unwrap());
    let shown = Arc::new(Recording::default());
    let app = mock_builder().invoke_handler(registry::handler()).build(tauri::generate_context!(test = true)).unwrap();
    app.manage(config.clone());
    app.manage(paths);
    app.manage(shown.clone() as ConfirmState);
    let w = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
    ExecApp { _home: dir, home, config, shown, w, _app: app }
}

fn script(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn set_exec(a: &ExecApp, key: &str, path: &str) -> Result<Value, Value> {
    invoke(&a.w, "settings_set_executable", json!({"key": key, "value": {"kind": "path", "path": path}}))
}

fn is_spoofing(c: char) -> bool {
    c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

fn assert_invalid(r: &Result<Value, Value>, field: &str, rule: &str, what: &str) {
    let e = r.as_ref().expect_err(what);
    assert_eq!(e["code"], "INVALID_ARGUMENT", "{what}: {e}");
    assert_eq!(e["params"]["field"], field, "{what}: {e}");
    assert_eq!(e["params"]["rule"], rule, "{what}: {e}");
}

fn file_contains(path: &Path, needle: &[u8]) -> bool {
    std::fs::read(path).map(|b| b.windows(needle.len()).any(|w| w == needle)).unwrap_or(false)
}

/// Files of the state directory that hold `needle` (the db, its WAL, …).
fn state_files_with(c: &verify::Ctx, needle: &str) -> Vec<String> {
    let dir = c.core.paths.state_db().parent().unwrap().to_path_buf();
    std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| file_contains(&e.path(), needle.as_bytes()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

async fn ui_rows(c: &verify::Ctx) -> Vec<(String, Value)> {
    c.core.db.read(dao::ui_state::all).await.expect("ui rows")
}

fn team(i: usize) -> String {
    format!("teamLayout:t_{i:016x}")
}

fn member(i: usize) -> String {
    format!("m_{i:016x}")
}

fn uid(i: usize) -> String {
    format!("s_0190a1b2-c3d4-7e5f-8a9b-{i:012x}")
}

fn obj(entries: &[(&str, Value)]) -> Value {
    Value::Object(entries.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect())
}

fn layout(i: usize) -> Value {
    json!({"mode": "panes", "focused": member(i), "zoomed": null, "leadRatio": 0.4, "tileOrder": [member(i)]})
}

// ---------------------------------------------------------------------------
// A45.1 — paths never carry controls; the confirmation shows what is used
// ---------------------------------------------------------------------------

/// The reviewer's G7c-13 probe: a newline-padded and an RLO-reversed name.
#[test]
fn g7c_13_the_executable_confirmation_never_shows_control_or_bidi_characters() {
    let _serial = serial();
    let a = exec_app();
    let proj = a.home.join("work/proj");
    let newline = proj.join("tools\n\n\n\n").join("usr/local/bin/claude");
    let bidi = proj.join(".cache").join("bin\u{202E}edualc");
    script(&newline);
    script(&bidi);

    for (name, path) in [("newline", &newline), ("bidi", &bidi)] {
        let r = set_exec(&a, "agents.claude.binary", &path.to_string_lossy());
        assert_invalid(&r, "value.path", "path", name);
    }
    let shown = a.shown.0.lock().clone();
    assert!(shown.is_empty(), "no dialog for a refused value: {shown:?}");
    assert!(shown.iter().all(|(_, t)| !t.chars().any(is_spoofing)));
    assert_eq!(a.config.get().agent_binary("claude"), None, "nothing was pinned");
}

/// Every executable key, every forbidden character class, at the value rule
/// (the pure decoder, so the matrix is not rate limited) — and two through
/// the IPC, `pathsOverride` and `sidecar.nodePath`.
#[test]
fn a45_1_every_path_key_refuses_every_forbidden_character() {
    let _serial = serial();
    let keys = [
        "agents.claude.binary",
        "agents.codex.binary",
        "agents.cursor.binary",
        "agents.agy.binary",
        "sidecar.nodePath",
        "paths.override",
    ];
    let chars = [
        '\0', '\t', '\n', '\r', '\u{1B}', '\u{7F}', '\u{80}', '\u{85}', '\u{9F}', '\u{200B}', '\u{200C}', '\u{200D}',
        '\u{200E}', '\u{200F}', '\u{202A}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        '\u{FEFF}',
    ];
    for key in keys {
        for c in chars {
            for path in [format!("/Users/u/proj/tools{c}/claude"), format!("/Users/u/proj/claude{c}")] {
                let payload = json!({"req": {"key": key, "value": {"kind": "path", "path": path}}});
                let e = request_from_payload::<SettingsSetExecutableReq>("settings_set_executable", &payload)
                    .expect_err(&format!("{key} {c:?}"));
                assert_eq!(e.params["field"], "value.path", "{key} {c:?}");
                assert_eq!(e.params["rule"], "path", "{key} {c:?}");
            }
        }
    }

    let a = exec_app();
    let dir = a.home.join("work/proj/bin\n\n/usr/local/bin");
    std::fs::create_dir_all(&dir).unwrap();
    let node = a.home.join("work/proj/node\u{2066}");
    script(&node);
    assert_invalid(&set_exec(&a, "paths.override", &dir.to_string_lossy()), "value.path", "path", "pathsOverride \\n");
    assert_invalid(&set_exec(&a, "sidecar.nodePath", &node.to_string_lossy()), "value.path", "path", "nodePath LRI");
    assert!(a.shown.0.lock().is_empty());
    let cfg = a.config.get();
    assert!(cfg.sidecar.node_path.is_none());
    assert!(cfg.to_settings().executable.paths_override.is_none());
}

/// The dialog names the realpath and the write applies it: a clean symlink
/// shows (and pins) its target; a clean name that resolves to a forbidden
/// one is refused; a character the path rule allows but that does not print
/// is shown escaped.
#[test]
fn a45_1_the_confirmation_names_and_applies_the_canonical_path() {
    let _serial = serial();
    let a = exec_app();
    let real = a.home.join("work/tools/real-claude");
    script(&real);
    let link = a.home.join("work/proj/claude");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let r = set_exec(&a, "agents.claude.binary", &link.to_string_lossy());
    assert_eq!(r.as_ref().map(|v| v["applied"].clone()), Ok(json!(true)), "{r:?}");
    let (key, text) = a.shown.0.lock().last().cloned().expect("a dialog");
    assert_eq!(key, "agents.claude.binary");
    assert_eq!(text, real.to_string_lossy(), "the dialog names the target, not the link");
    assert_eq!(a.config.get().agent_binary("claude"), Some(real.to_string_lossy().as_ref()), "the target is pinned");

    // A clean name that resolves through a symlinked dir with an RLO in it.
    let evil_dir = a.home.join("work/evil\u{202E}nib");
    script(&evil_dir.join("codex"));
    let clean = a.home.join("work/clean");
    std::os::unix::fs::symlink(&evil_dir, &clean).unwrap();
    let before = a.shown.0.lock().len();
    let r = set_exec(&a, "agents.codex.binary", &clean.join("codex").to_string_lossy());
    assert_invalid(&r, "value.path", "path", "symlink into an RLO dir");
    assert_eq!(a.shown.0.lock().len(), before, "no dialog");
    assert_eq!(a.config.get().agent_binary("codex"), None);

    // U+2028 and NBSP are path characters, but they never print as themselves.
    let odd = a.home.join("work/line\u{2028}sep\u{A0}x/cursor");
    script(&odd);
    let r = set_exec(&a, "agents.cursor.binary", &odd.to_string_lossy());
    assert_eq!(r.as_ref().map(|v| v["applied"].clone()), Ok(json!(true)), "{r:?}");
    let (_, text) = a.shown.0.lock().last().cloned().unwrap();
    assert!(text.ends_with("/work/line\\u{2028}sep\\u{A0}x/cursor"), "{text:?}");
    assert!(!text.chars().any(|c| c.is_control() || c.is_whitespace() && c != ' '), "{text:?}");

    // pathsOverride: the canonical directory.
    let real_dir = a.home.join("work/realbin");
    std::fs::create_dir_all(&real_dir).unwrap();
    let dir_link = a.home.join("work/binlink");
    std::os::unix::fs::symlink(&real_dir, &dir_link).unwrap();
    let r = set_exec(&a, "paths.override", &dir_link.to_string_lossy());
    assert_eq!(r.as_ref().map(|v| v["applied"].clone()), Ok(json!(true)), "{r:?}");
    let (_, text) = a.shown.0.lock().last().cloned().unwrap();
    assert_eq!(text, real_dir.to_string_lossy());
    let applied = a.config.get().to_settings().executable.paths_override.map(|p| p.into_string());
    assert_eq!(applied.as_deref(), Some(real_dir.to_string_lossy().as_ref()));
}

// ---------------------------------------------------------------------------
// A45.2 — UI state is typed, bounded and content-free
// ---------------------------------------------------------------------------

/// The reviewer's G7c-14 probe: content under an allowlisted key, then 200
/// fresh team keys with 60 KiB each, as fast as the WebView can send them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn g7c_14_ui_state_set_never_persists_content_or_unbounded_rows() {
    with_wired(
        |_| {},
        |c| async move {
            let r = c.call(
                "ui_state_set",
                json!({"key": "lastView", "value": {"screen": "home", "draft": CANARY, "transcript": [CANARY, CANARY]}}),
            );
            assert_invalid(&r, "value", "shape", "content next to a unit view");
            assert!(state_files_with(&c, CANARY).is_empty(), "§3.1 rule 5: content reached state.db");

            // 200 fresh team keys with 60 KiB each: not a layout, nothing stored
            // (and, refused at the value rules, nothing counted, §4.1 rule 3).
            let blob = "x".repeat(60 * 1024);
            for i in 0..200u32 {
                let key = format!("teamLayout:t_{:016x}", 0xdead_0000_u64 + u64::from(i));
                let r = c.call("ui_state_set", json!({"key": key, "value": {"pad": blob}}));
                assert_invalid(&r, "value", "shape", "a free-form layout");
            }
            assert!(ui_rows(&c).await.iter().all(|(k, _)| !k.starts_with("teamLayout:")));

            // 200 fresh team keys with real layouts, as fast as the WebView can
            // send them: the interactive class cuts the burst, the cap the rows.
            let (mut ok, mut busy) = (0usize, 0usize);
            let started = Instant::now();
            for i in 0..200usize {
                match c.call("ui_state_set", json!({"key": team(0xdead_0000 + i), "value": layout(i)})) {
                    Ok(_) => ok += 1,
                    Err(e) if e["code"] == "BUSY" => {
                        assert!(e["params"]["retryAfterMs"].as_u64().is_some_and(|ms| (1..=20).contains(&ms)), "{e}");
                        busy += 1
                    }
                    Err(e) => panic!("unexpected {e}"),
                }
            }
            assert_rate("ui_state_set", ok, busy, started.elapsed(), 100.0, 50.0);
            let layouts = c.core.lifecycle.ui_state().keys().filter(|k| k.starts_with("teamLayout:")).count();
            assert_eq!(layouts, ok.min(64), "rows are capped at 64");
        },
    )
    .await;
}

/// A content string in each UI key, every one refused; then exactly what
/// `persist.ts` sends, every one accepted and persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a45_2_a_content_string_in_each_ui_key_is_refused_and_the_real_shapes_pass() {
    with_wired(
        |_| {},
        |c| async move {
            let refused: Vec<(&str, Value, &str)> = vec![
                ("lastView", json!({"screen": "session", "uid": CANARY, "sub": "conversation"}), "value.uid"),
                ("lastView", json!({"screen": "team", "teamKey": "t_0123456789abcdef", "mode": CANARY}), "value"),
                ("lastView", json!({"screen": CANARY}), "value"),
                ("lastView", json!(CANARY), "value"),
                ("splitSet", json!({"uids": [uid(1), CANARY], "focused": null}), "value.uids[1]"),
                ("splitSet", json!({"uids": [], "focused": null, "note": CANARY}), "value"),
                ("teamLayout:t_0123456789abcdef", json!({"mode": "panes", "focused": CANARY}), "value.focused"),
                ("teamLayout:t_0123456789abcdef", json!({"mode": "panes", "tileOrder": [CANARY]}), "value.tileOrder[0]"),
                ("teamLayout:t_0123456789abcdef", json!({"mode": "panes", "title": CANARY}), "value"),
                ("teamLayout:t_0123456789abcdef", json!({"mode": "panes", "leadRatio": 0.95}), "value.leadRatio"),
                ("sidebarOrder", json!(["p_0123456789abcdef", CANARY]), "value[1]"),
                ("sidebarFolds", obj(&[(CANARY, json!(true))]), "value"),
                ("sidebarFolds", json!({"p:p_0123456789abcdef": CANARY}), "value"),
                ("homeMode", json!(CANARY), "value"),
                ("floorPrefs", json!({"list": CANARY}), "value"),
                ("floorPrefs", json!({"list": true, "note": CANARY}), "value"),
            ];
            for (key, value, field) in &refused {
                let r = c.call("ui_state_set", json!({"key": key, "value": value}));
                let e = r.as_ref().expect_err(&format!("{key}: {value}"));
                assert_eq!(e["code"], "INVALID_ARGUMENT", "{key}: {e}");
                assert_eq!(e["params"]["field"], *field, "{key}: {value} → {e}");
                assert!(!e.to_string().contains(CANARY), "an error never echoes the content: {e}");
            }
            assert!(state_files_with(&c, CANARY).is_empty(), "content reached the state dir");
            assert!(!serde_json::to_string(&c.core.lifecycle.ui_state()).unwrap().contains(CANARY));

            // What persist.ts sends (src/store/persist.ts `persistedValues`).
            let accepted: Vec<(String, Value)> = vec![
                ("lastView".into(), json!({"screen": "home"})),
                ("lastView".into(), json!({"screen": "accounts", "section": "health"})),
                ("lastView".into(), json!({"screen": "team", "teamKey": "t_0123456789abcdef", "mode": "board"})),
                ("lastView".into(), json!({"screen": "session", "uid": uid(7), "sub": "terminal"})),
                ("splitSet".into(), json!({"uids": [uid(1), uid(2)], "focused": uid(2)})),
                ("splitSet".into(), json!({"uids": [], "focused": null})),
                (team(1), json!({"mode": "panes", "focused": null, "zoomed": null, "leadRatio": null, "tileOrder": null})),
                (team(2), layout(2)),
                ("homeMode".into(), json!("classic")),
                ("floorPrefs".into(), json!({"list": true, "traffic": false, "legend": true})),
                ("sidebarOrder".into(), json!(["p_0123456789abcdef", "p_0123456789abcdee"])),
                (
                    "sidebarFolds".into(),
                    obj(&[
                        ("p:p_0123456789abcdef", json!(false)),
                        ("t:t_0123456789abcdef", json!(true)),
                        ("e:p_0123456789abcdef", json!(true)),
                        (&format!("s:{}", uid(3)), json!(true)),
                        ("earlier", json!(true)),
                    ]),
                ),
            ];
            for (key, value) in &accepted {
                let r = c.call("ui_state_set", json!({"key": key, "value": value}));
                assert!(r.is_ok(), "{key}: {value} → {r:?}");
            }
            let rows = ui_rows(&c).await;
            for (key, value) in accepted.iter().rev().fold(Vec::<(String, Value)>::new(), |mut acc, kv| {
                if !acc.iter().any(|(k, _)| *k == kv.0) {
                    acc.push(kv.clone());
                }
                acc
            }) {
                assert!(rows.contains(&(key.clone(), value.clone())), "{key} persisted as sent");
            }
        },
    )
    .await;
}

/// The 65th `teamLayout:*` evicts the least recently written one — in memory,
/// in `app_bootstrap`, and from `settings_kv`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a45_2_the_65th_team_layout_evicts_the_least_recently_written_one() {
    with_wired(
        |_| {},
        |c| async move {
            for i in 0..64 {
                c.call("ui_state_set", json!({"key": team(i), "value": layout(i)})).expect("layout");
            }
            // Team 0 is written again: team 1 becomes the least recently written.
            c.call("ui_state_set", json!({"key": team(0), "value": layout(0)})).expect("rewrite");
            c.call("ui_state_set", json!({"key": team(64), "value": layout(64)})).expect("65th");

            let boot = c.call("app_bootstrap", json!({})).expect("bootstrap");
            let ui = boot["uiState"].as_object().unwrap();
            let layouts: Vec<&String> = ui.keys().filter(|k| k.starts_with("teamLayout:")).collect();
            assert_eq!(layouts.len(), 64);
            assert!(ui.contains_key(&team(0)) && ui.contains_key(&team(64)) && !ui.contains_key(&team(1)));
            assert!(c.core.lifecycle.ui_state_bytes() <= 256 * 1024);

            // The row went in the same transaction as the write.
            let rows = ui_rows(&c).await;
            assert_eq!(rows.iter().filter(|(k, _)| k.starts_with("teamLayout:")).count(), 64);
            assert!(!rows.iter().any(|(k, _)| *k == team(1)), "team 1's row is gone");
            assert!(rows.iter().any(|(k, _)| *k == team(0)) && rows.iter().any(|(k, _)| *k == team(64)));
        },
    )
    .await;
}

/// The write order survives a restart: a layout written again just before
/// the restart is not the one the 65th evicts after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a45_2_least_recently_written_survives_a_restart() {
    let _s = verify::WIRED.lock().await;
    let h = fixtures::Hermetic::new();
    let _env = h.enter();
    let home = std::fs::canonicalize(h.home()).unwrap();
    let setup = || async {
        match boot::setup_core(AppPaths::for_home(&home), "0.0.0-test", "build-test").await.expect("setup") {
            SetupOutcome::Ready(out) => out,
            SetupOutcome::SecondInstance { .. } => panic!("no other instance holds the lock"),
        }
    };
    let first = setup().await;
    for i in 0..64 {
        set_ui_state(&first.lifecycle, &first.db, &team(i), layout(i)).await.expect("layout");
    }
    // Team 0 is written again: team 1 is now the least recently written.
    set_ui_state(&first.lifecycle, &first.db, &team(0), layout(0)).await.expect("rewrite");
    drop(first);

    let second = setup().await;
    assert_eq!(second.lifecycle.ui_state().len(), 64);
    set_ui_state(&second.lifecycle, &second.db, &team(64), layout(64)).await.expect("65th");
    let ui = second.lifecycle.ui_state();
    assert!(ui.contains_key(&team(0)), "the layout written last before the restart stays");
    assert!(!ui.contains_key(&team(1)), "the least recently written one goes");
    let rows = second.db.read(dao::ui_state::all).await.expect("rows");
    assert_eq!(rows.len(), 64);
    assert!(!rows.iter().any(|(k, _)| *k == team(1)));
}

/// Rows that fail their schema are dropped at boot, never served; rows over
/// the 64 cap are evicted oldest first; both are deleted from `settings_kv`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a45_2_persisted_rows_that_fail_their_schema_are_dropped_at_boot() {
    let _s = verify::WIRED.lock().await;
    let h = fixtures::Hermetic::new();
    let _env = h.enter();
    let home = std::fs::canonicalize(h.home()).unwrap();
    let setup = || async {
        match boot::setup_core(AppPaths::for_home(&home), "0.0.0-test", "build-test").await.expect("setup") {
            SetupOutcome::Ready(out) => out,
            SetupOutcome::SecondInstance { .. } => panic!("no other instance holds the lock"),
        }
    };
    let first = setup().await;
    first
        .db
        .write(|conn| {
            let set = |k: &str, v: Value| dao::settings_kv::set(conn, &format!("ui.{k}"), &v);
            set("lastView", json!({"screen": "home", "draft": CANARY}))?;
            set("homeMode", json!(CANARY))?;
            set("evil", json!({"text": CANARY}))?;
            set("floorPrefs", json!({"list": true}))?;
            for i in 0..66 {
                set(&team(i), layout(i))?;
            }
            conn.execute("INSERT INTO settings_kv (key, value_json) VALUES ('ui.splitSet', '{not json')", [])?;
            Ok(())
        })
        .await
        .expect("seed");
    drop(first);

    let second = setup().await;
    let ui = second.lifecycle.ui_state();
    assert!(!serde_json::to_string(&ui).unwrap().contains(CANARY), "an invalid row was served");
    for k in ["lastView", "homeMode", "evil", "splitSet"] {
        assert!(!ui.contains_key(k), "{k}");
    }
    assert_eq!(ui.get("floorPrefs"), Some(&json!({"list": true})));
    assert_eq!(ui.keys().filter(|k| k.starts_with("teamLayout:")).count(), 64);
    assert!(!ui.contains_key(&team(0)) && !ui.contains_key(&team(1)) && ui.contains_key(&team(65)));

    let rows = second.db.read(dao::settings_kv::all).await.expect("rows");
    let ui_keys: Vec<&String> = rows.iter().map(|(k, _)| k).filter(|k| k.starts_with("ui.")).collect();
    assert_eq!(ui_keys.len(), 65, "{ui_keys:?}");
    assert!(rows.iter().all(|(_, v)| !v.contains(CANARY)), "the invalid rows were deleted");
}

// ---------------------------------------------------------------------------
// A45.3 — one rate limiter for every command
// ---------------------------------------------------------------------------

/// The reviewer's G7c-15 probe: 20 `agents_rescan` in a burst.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn g7c_15_agents_rescan_is_rate_limited() {
    with_wired(
        |_| {},
        |c| async move {
            let mut scheduled = 0;
            let mut busy = 0;
            for _ in 0..20 {
                match c.call("agents_rescan", json!({})) {
                    Ok(v) if v["scheduled"] == true => scheduled += 1,
                    Err(e) if e["code"] == "BUSY" => {
                        let ms = e["params"]["retryAfterMs"].as_u64().unwrap_or(0);
                        assert!((59_000..=60_000).contains(&ms), "probe: 1 per 60 s, retry {ms}");
                        busy += 1
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
            // let the scheduled detection finish before the harness tears down
            let deadline = Instant::now() + Duration::from_secs(20);
            while boot::rescan_running(&c.core.lifecycle) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(scheduled == 1 && busy == 19, "probe class: {scheduled} detections scheduled, {busy} BUSY in one burst");
        },
    )
    .await;
}

/// A burst of `through + busy` calls over `elapsed` against a bucket of
/// `burst` refilled at `rate`/s: at most `burst + rate·elapsed` (+1 for the
/// token in flight) get through, and every call beyond that is `BUSY`. Holds
/// at any machine speed (no wall-clock budget, A10.7).
fn assert_rate(what: &str, through: usize, busy: usize, elapsed: Duration, burst: f64, rate: f64) {
    let allowed = (burst + rate * elapsed.as_secs_f64()).floor() as usize + 1;
    let total = through + busy;
    assert!(through <= allowed, "{what}: {through} of {total} got through in {elapsed:?}, at most {allowed} may");
    if total > allowed {
        assert!(busy >= total - allowed, "{what}: only {busy} of {total} BUSY in {elapsed:?}");
    }
}

/// `interactive` per PTY (50/s, burst 100) and `default` per command (10/s):
/// the bursts are cut at the bucket, whatever the command would answer.
#[test]
fn a45_3_bursts_on_pty_write_link_open_and_ui_state_set_answer_busy() {
    let (_app, w) = bare_app();
    let last_busy = Mutex::new(Value::Null);
    let busy_of = |cmd: &str, req: Value, n: usize| -> (usize, Vec<u64>, Duration) {
        let mut other = 0;
        let mut busy = Vec::new();
        let started = Instant::now();
        for _ in 0..n {
            match invoke(&w, cmd, req.clone()) {
                Err(e) if e["code"] == "BUSY" => {
                    busy.push(e["params"]["retryAfterMs"].as_u64().expect("retryAfterMs"));
                    *last_busy.lock() = e;
                }
                _ => other += 1,
            }
        }
        (other, busy, started.elapsed())
    };
    let pty_a = json!({"ptyId": "y_0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b", "b64": "eA=="});
    let pty_b = json!({"ptyId": "y_0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c", "b64": "eA=="});
    let (other, busy, took) = busy_of("pty_write", pty_a.clone(), 300);
    assert_rate("pty_write", other, busy.len(), took, 100.0, 50.0);
    assert!(busy.iter().all(|ms| (1..=20).contains(ms)), "{busy:?}");
    let (other_b, busy_b, _) = busy_of("pty_write", pty_b, 50);
    assert_eq!((other_b, busy_b.len()), (50, 0), "another PTY is not starved");

    // link_open (default class). A valid request is counted; this bare app
    // has no IDE service, so nothing can open (`UNSUPPORTED{notBuilt}`).
    let (other, busy, took) = busy_of("link_open", json!({"url": "https://example.invalid/"}), 60);
    assert_rate("link_open", other, busy.len(), took, 10.0, 10.0);
    assert!(busy.iter().all(|ms| (1..=100).contains(ms)), "{busy:?}");
    // A request refused by its value rules draws no token (§4.1 rule 3).
    let (other, busy, _) = busy_of("session_mark_seen", json!({"uid": "s_not-a-uuid"}), 40);
    assert_eq!((other, busy.len()), (40, 0), "invalid requests are never BUSY");

    let (other, busy, took) = busy_of("ui_state_set", json!({"key": "homeMode", "value": "focus"}), 300);
    assert_rate("ui_state_set", other, busy.len(), took, 100.0, 50.0);

    // The one error shape (§4.4) — of the last BUSY seen (a slow machine
    // may see none; then there is no shape to check).
    let e = last_busy.lock().clone();
    if e.is_null() {
        return;
    }
    assert_eq!(e["code"], "BUSY", "{e}");
    assert!(e["key"].as_str().is_some_and(|k| !k.is_empty()), "{e}");
    assert_eq!(e["retryable"], true, "{e}");
}

// -- coalescing (A45.3): 20 rescans → one detection ---------------------------

/// A fake CLI that counts its runs in `counter` and answers `--version` only
/// once `gate` exists (so a detection stays in flight until the test opens it).
fn counting_cli(dir: &Path, name: &str, version: &str, counter: &Path, gate: &Path) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\necho x >> '{}'\nwhile [ ! -e '{}' ]; do sleep 0.02; done\necho \"{name} {version}\"\n",
            counter.display(),
            gate.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn fake_cli(dir: &Path, name: &str, version: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\necho \"{name} {version}\"\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

struct Rig {
    _env: fixtures::Hermetic,
    _bin: tempfile::TempDir,
    _path: tempfile::TempDir,
    counter: PathBuf,
    gate: PathBuf,
    lifecycle: Arc<Lifecycle>,
    agents: Arc<AgentRegistry>,
}

impl Rig {
    /// Every kind pinned to a fake (no PATH directory is searched); claude
    /// counts its runs.
    fn new() -> Rig {
        let env = fixtures::Hermetic::new();
        let paths = AppPaths::for_home(&env.home());
        paths.ensure_all().expect("dirs");
        let bin = tempfile::Builder::new().prefix("gilde-t42-bin-").tempdir().unwrap();
        let empty_path = tempfile::Builder::new().prefix("gilde-t42-path-").tempdir().unwrap();
        let counter = bin.path().join("claude.runs");
        let gate = bin.path().join("gate");
        let config = Arc::new(ConfigStore::open_at(&paths.config_file()).expect("config"));
        let pins: Vec<(&str, PathBuf)> = vec![
            ("claude", counting_cli(bin.path(), "fake-claude", "2.1.306", &counter, &gate)),
            ("codex", fake_cli(bin.path(), "fake-codex", "0.50.0")),
            ("cursorAgent", fake_cli(bin.path(), "fake-cursor", "2025.10.01")),
            ("agy", fake_cli(bin.path(), "fake-agy", "1.3.5")),
            ("gemini", fake_cli(bin.path(), "fake-gemini", "1.0.0")),
        ];
        let shell = fake_cli(bin.path(), "fake-shell", "5.9");
        let mut binaries = serde_json::Map::new();
        for (k, p) in &pins {
            binaries.insert((*k).to_string(), json!(p.to_string_lossy()));
        }
        config
            .patch(json!({
                "agents": { "binaries": binaries, "pathOverride": empty_path.path().to_string_lossy() },
                "terminal": { "shell": shell.to_string_lossy() }
            }))
            .expect("pins");
        let cfg = config.get();
        for kind in gilde_code_lib::agents::AgentKind::ALL {
            if kind != gilde_code_lib::agents::AgentKind::Shell {
                let pin = cfg.agent_binary(kind.key()).unwrap_or_else(|| panic!("{kind:?} is not pinned"));
                assert!(pin.starts_with(bin.path().to_string_lossy().as_ref()), "{kind:?} pinned outside the fakes");
            }
        }
        let agents = Arc::new(AgentRegistry::without_db(paths, config));
        Rig {
            _env: env,
            _bin: bin,
            _path: empty_path,
            counter,
            gate,
            lifecycle: Lifecycle::new("0.0.0-test", "build"),
            agents,
        }
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(&self.counter).map(|s| s.lines().count()).unwrap_or(0)
    }

    fn reset(&self) {
        let _ = std::fs::remove_file(&self.counter);
        let _ = std::fs::remove_file(&self.gate);
    }

    fn open(&self) {
        std::fs::write(&self.gate, b"").unwrap();
    }

    fn rescan(&self, mode: RescanMode) -> tauri::async_runtime::JoinHandle<()> {
        boot::spawn_rescan_as(self.lifecycle.clone(), self.agents.clone(), None, mode)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a45_3_twenty_rescans_while_one_runs_are_one_detection() {
    let _serial = verify::WIRED.lock().await;
    let rig = Rig::new();
    // One detection alone: how many times it runs the claude fake.
    rig.open();
    rig.rescan(RescanMode::Join).await.expect("baseline");
    let one = rig.runs();
    assert!(one >= 1, "the detection probes claude");

    rig.reset();
    let handles: Vec<_> = (0..20).map(|_| rig.rescan(RescanMode::Join)).collect();
    assert!(boot::rescan_running(&rig.lifecycle));
    rig.open();
    for h in handles {
        h.await.expect("joined");
    }
    assert_eq!(rig.runs(), one, "20 rescans while one ran: one detection");
    assert!(!boot::rescan_running(&rig.lifecycle));
    assert!(rig.lifecycle.feed.boot().agents_detected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a45_3_a_changed_setting_during_a_rescan_runs_exactly_one_more_pass() {
    let _serial = verify::WIRED.lock().await;
    let rig = Rig::new();
    rig.open();
    rig.rescan(RescanMode::Join).await.expect("baseline");
    let one = rig.runs();

    rig.reset();
    let mut handles = vec![rig.rescan(RescanMode::Join)];
    for _ in 0..5 {
        handles.push(rig.rescan(RescanMode::AfterChange));
    }
    handles.push(rig.rescan(RescanMode::Join));
    rig.open();
    for h in handles {
        h.await.expect("joined");
    }
    assert_eq!(rig.runs(), 2 * one, "five changes while one ran: one more pass, not five");
    assert!(!boot::rescan_running(&rig.lifecycle));

    // And a rescan after the run is a new detection again.
    rig.reset();
    rig.open();
    rig.rescan(RescanMode::Join).await.expect("again");
    assert_eq!(rig.runs(), one);
}
