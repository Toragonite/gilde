//! G6 probe HP-5 (contract §14.5, A31.3): resource exhaustion through the
//! integrated core — 2,000 registry files, 10,000 transcripts and 25,000
//! `session-env` dirs under the hermetic home, the composition root with
//! fakes only (real World, hermetic discovery, real `history_query` over
//! Tauri's mock IPC). `#[ignore]`d: the lead runs it at a quiet moment with
//! `cargo test --test verify_scale -- --ignored --nocapture`. Prints one
//! `PROBE HP-5: …` line.
//!
//! Expected (§14.5): first rows ≤ 2 s; idle ≤ 1 %; caps reported.
//!
//! * **First rows** — two measures, both against the 2 s bound:
//!   (a) History: from launch (the tree is on disk, `setup()` starts) to the
//!   first `history_query` page with rows and titles (§11.1 History cold
//!   start; the History screen is assumed open from the first moment, which
//!   is stricter than the gate); (b) Home: from the moment the 20 live
//!   processes exist and every one of the 2,000 registry files is rewritten
//!   (a full registry pass, as at boot) to the store showing all 20 live
//!   sessions (§11.1 "first discovered rows"; the harness seeds no process
//!   before discovery's pass 1, so the boot pass itself cannot have live
//!   rows).
//! * **Idle** — after the History background fill finished: the whole test
//!   process's CPU (`getrusage`) over a real 60 s idle window (core threads:
//!   discovery at the focused 3 s cadence + its 60 s reconcile, the history
//!   thread, World's publisher), and the `discovery_budget` bench method
//!   (the engine's thread CPU over one simulated focused minute) on the
//!   same tree.
//! * **Caps** — what the sources report at the HP-5 sizes (no cap is
//!   exceeded by 2,000 / 10,000), then over the caps: 4,200 registry files
//!   (> 4,096) and 20,001 transcripts (> 20,000) → `SourceStatus.truncated`.
//! * `session-env/` — never a watch root (the wired discovery runs with
//!   `watch = false`; the watch specs of an engine over the same tree are
//!   checked).
//!
//! Every timing is recorded with the process CPU spent in its window (to
//! tell work from waiting on a loaded machine), and the 2 s / 1 % bounds are
//! asserted only at the end, so one run always records every measurement; a
//! failed bound prints `PROBE HP-5: FAILED after: …` with all of them. The
//! History fill (QoS background, throttled to work 1 : rest 3) can take far
//! longer on a busy machine: after [`FILL_WAIT_SECS`] the idle window is
//! measured anyway and labelled so.
//!
//! Owner: T33.

mod fixtures;
#[path = "fixtures/vendor/support.rs"]
mod support;
#[path = "fixtures/verify/mod.rs"]
mod verify;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use gilde_code_lib::contract::common::Vendor;
use gilde_code_lib::contract::services::WorldApi;
use gilde_code_lib::contract::store::{SourceKind, SourceStatus, StoreSnapshot};
use gilde_code_lib::discovery::procs::{FakeBundles, FakeProc};
use gilde_code_lib::discovery::{now_up_ms, now_wall_ms, AccountSpec, DiscoveryConfig, DiscoveryDiagnostics, Engine, NoOwnProcesses};
use gilde_code_lib::store::now_ms;

use verify::{with_wired, Probe};

const REGISTRY_FILES: usize = 2_000;
const LIVE: usize = 20;
const TRANSCRIPTS: usize = 10_000;
const SESSION_ENV: usize = 25_000;
/// Over the caps: `SESSIONS_ENTRIES_CAP` = 4,096, `INDEX_CAP` = 20,000.
const REGISTRY_OVER: usize = 4_200;
const TRANSCRIPTS_OVER: usize = 20_001;
const SLUG: &str = "-hp5-big-project";
const WORKERS: usize = 8;

const FIRST_ROWS_MS: u128 = 2_000;
/// The longest the probe waits for a result at all (a liveness bound, not a
/// budget: the measured times are checked against the §14.5 bounds).
const LIVENESS: Duration = Duration::from_secs(900);
/// How long the probe waits for the History background fill before it
/// measures the idle window anyway (a quiet machine needs a fraction).
const FILL_WAIT_SECS: u64 = 900;
const IDLE_PCT: f64 = 1.0;

fn live_pid(i: usize) -> i32 {
    50_000 + i as i32
}

fn live_sid(i: usize) -> String {
    support::uuid(0x5000_0000 + i as u32)
}

fn registry_sid(i: usize) -> String {
    support::uuid(0x5100_0000 + i as u32)
}

fn transcript_sid(i: usize) -> String {
    if i < LIVE {
        live_sid(i)
    } else {
        support::uuid(0x5200_0000 + i as u32)
    }
}

/// What the tree builder fixed (times, the live sessions' starts).
struct Tree {
    now_s: i64,
    built: Duration,
}

fn live_start(now_s: i64, i: usize) -> i64 {
    now_s - 3_600 - i as i64
}

fn live_cwd(home: &Path, i: usize) -> PathBuf {
    home.join("work").join(format!("hp5-{i}"))
}

/// Run `f(i)` for `i in 0..n` on [`WORKERS`] threads.
fn parallel(n: usize, f: impl Fn(usize) + Sync) {
    std::thread::scope(|s| {
        for w in 0..WORKERS {
            let f = &f;
            s.spawn(move || {
                let mut i = w;
                while i < n {
                    f(i);
                    i += WORKERS;
                }
            });
        }
    });
}

fn registry_body(pid: i32, sid: &str, cwd: &Path, start: i64, status_at: i64) -> String {
    json!({
        "pid": pid, "sessionId": sid, "cwd": cwd.to_str().unwrap(),
        "startedAt": start * 1000 + 400, "procStart": support::ctime_utc(start),
        "version": "2.1.306", "peerProtocol": 1, "kind": "interactive", "entrypoint": "cli",
        "pidDomain": "darwin", "name": "hp5", "nameSource": "derived",
        "updatedAt": status_at, "status": "idle", "statusUpdatedAt": status_at,
    })
    .to_string()
}

/// One small transcript: a closed turn three days ago and a custom title.
fn transcript_body(sid: &str, cwd: &str, n: usize, at: i64) -> String {
    let rows = [
        json!({"type": "permission-mode", "permissionMode": "default", "sessionId": sid}),
        json!({"type": "user", "message": {"role": "user", "content": format!("hp5 prompt {n}")}, "uuid": "u1", "timestamp": iso(at), "sessionId": sid, "cwd": cwd, "entrypoint": "cli", "version": "2.1.306"}),
        json!({"type": "assistant", "message": {"id": "msg_1", "role": "assistant", "content": [{"type": "text", "text": "hp5 reply"}], "stop_reason": "end_turn"}, "uuid": "a1", "timestamp": iso(at + 2_000), "sessionId": sid, "cwd": cwd}),
        json!({"type": "system", "subtype": "turn_duration", "durationMs": 2_000, "timestamp": iso(at + 2_000), "sessionId": sid}),
        json!({"type": "custom-title", "customTitle": format!("hp5 session {n}"), "sessionId": sid}),
    ];
    rows.iter().map(|r| format!("{r}\n")).collect()
}

/// The HP-5 tree: 2,000 registry files (20 of live sessions), 10,000
/// transcripts in one project, 25,000 `session-env` dirs.
fn build(home: &Path) -> Tree {
    let t = Instant::now();
    let claude = home.join(".claude");
    for d in ["sessions", "projects", "teams", "tasks", "session-env"] {
        std::fs::create_dir_all(claude.join(d)).unwrap();
    }
    std::fs::create_dir_all(claude.join("projects").join(SLUG)).unwrap();
    let now = now_ms();
    let now_s = now / 1000;
    let big = home.join("work/hp5-big");
    std::fs::create_dir_all(&big).unwrap();
    let big = std::fs::canonicalize(big).unwrap();
    for i in 0..LIVE {
        std::fs::create_dir_all(live_cwd(home, i)).unwrap();
    }
    parallel(REGISTRY_FILES, |i| {
        let (pid, sid, cwd, start) = if i < LIVE {
            (live_pid(i), live_sid(i), std::fs::canonicalize(live_cwd(home, i)).unwrap(), live_start(now_s, i))
        } else {
            (60_000 + i as i32, registry_sid(i), big.clone(), now_s - 7_200)
        };
        std::fs::write(claude.join(format!("sessions/{pid}.json")), registry_body(pid, &sid, &cwd, start, now - 60_000)).unwrap();
    });
    let three_days = now - 3 * 86_400_000;
    let big_s = big.to_str().unwrap().to_string();
    parallel(TRANSCRIPTS, |i| {
        let sid = transcript_sid(i);
        let cwd = if i < LIVE { std::fs::canonicalize(live_cwd(home, i)).unwrap().to_string_lossy().into_owned() } else { big_s.clone() };
        let p = claude.join("projects").join(SLUG).join(format!("{sid}.jsonl"));
        std::fs::write(p, transcript_body(&sid, &cwd, i, three_days - i as i64 * 1_000)).unwrap();
    });
    parallel(SESSION_ENV, |i| {
        std::fs::create_dir(claude.join("session-env").join(format!("{i:08x}-0000-4000-8000-000000000000"))).unwrap();
    });
    Tree { now_s, built: t.elapsed() }
}

/// Over the caps: registry files up to 4,200 and transcripts up to 20,001
/// (dead pids; tiny transcripts).
fn grow_over_caps(home: &Path, now_s: i64) {
    let claude = home.join(".claude");
    let cwd = std::fs::canonicalize(home.join("work/hp5-big")).unwrap();
    parallel(REGISTRY_OVER - REGISTRY_FILES, |k| {
        let i = REGISTRY_FILES + k;
        let pid = 60_000 + i as i32;
        std::fs::write(claude.join(format!("sessions/{pid}.json")), registry_body(pid, &registry_sid(i), &cwd, now_s - 7_200, (now_s - 3_600) * 1000)).unwrap();
    });
    let dir = claude.join("projects").join("-hp5-overflow");
    std::fs::create_dir_all(&dir).unwrap();
    parallel(TRANSCRIPTS_OVER - TRANSCRIPTS, |k| {
        let sid = support::uuid(0x5300_0000 + k as u32);
        let line = json!({"type": "user", "message": {"role": "user", "content": "x"}, "timestamp": iso((now_s - 5 * 86_400) * 1000), "sessionId": sid});
        std::fs::write(dir.join(format!("{sid}.jsonl")), format!("{line}\n")).unwrap();
    });
}

fn proc(start: i64, cwd: &Path) -> FakeProc {
    let mut p = support::claude_proc(start, 1, &["claude"], cwd);
    // SAFETY: `getuid` has no failure mode and touches no memory.
    p.uid = unsafe { libc::getuid() };
    p
}

/// New mtimes on every registry file: the next pass re-reads all of them.
fn rewrite_registry_mtimes(home: &Path) {
    let entries: Vec<PathBuf> = std::fs::read_dir(home.join(".claude/sessions")).unwrap().flatten().map(|e| e.path()).collect();
    parallel(entries.len(), |i| {
        let p = &entries[i];
        let m = std::fs::metadata(p).unwrap().modified().unwrap();
        std::fs::File::options().write(true).open(p).unwrap().set_modified(m + Duration::from_secs(1)).unwrap();
    });
}

// ---------------------------------------------------------------------------
// reading the integrated core
// ---------------------------------------------------------------------------

fn settle(c: &verify::Ctx) -> DiscoveryDiagnostics {
    tokio::task::block_in_place(|| {
        let d = c.wired.discovery.read();
        d.as_ref().expect("discovery is running").sync(LIVENESS).expect("discovery answered sync")
    })
}

fn snap(c: &verify::Ctx) -> StoreSnapshot {
    let w = c.world();
    w.flush_now();
    w.snapshot()
}

fn source(s: &StoreSnapshot, kind: SourceKind) -> Option<SourceStatus> {
    s.sources.iter().find(|x| x.vendor == Vendor::Claude && x.source == kind).cloned()
}

fn live_rows(s: &StoreSnapshot) -> usize {
    (0..LIVE).filter(|i| s.sessions.iter().any(|x| x.vendor_session_id.as_deref() == Some(live_sid(*i).as_str()))).count()
}

/// The process's CPU time (user + system), ms.
fn process_cpu_ms() -> f64 {
    // SAFETY: `rusage` is plain data; `getrusage` only writes the struct we pass.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 * 1e3 + t.tv_usec as f64 / 1e3;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// The `discovery_budget` bench's clock: this thread's CPU time, ns.
fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let milli = ms.rem_euclid(1000);
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Days → (y, m, d) (H. Hinnant's civil_from_days).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Poll `cond` every `every` until it holds or `limit` passes.
async fn wait_for(limit: Duration, every: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(every).await;
    }
}

// ---------------------------------------------------------------------------
// HP-5
// ---------------------------------------------------------------------------

/// HP-5: 2,000 registry files, 10 k transcripts, 25 k `session-env` dirs →
/// first rows ≤ 2 s; idle ≤ 1 %; caps reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "HP-5 scale probe: the lead runs it at a quiet moment"]
async fn probe_hp5_resource_exhaustion() {
    let shared: Arc<Mutex<Option<(Tree, Instant, f64)>>> = Arc::new(Mutex::new(None));
    let slot = shared.clone();
    with_wired(
        move |home| {
            let tree = build(home);
            // t0: the tree is on disk; the launch (setup, wiring, discovery) starts now.
            *slot.lock().unwrap() = Some((tree, Instant::now(), process_cpu_ms()));
        },
        |c| async move {
            let (tree, t0, cpu_t0) = shared.lock().unwrap().take().expect("the tree was built");
            let mut p = Probe::new("HP-5");
            let home = c.home.clone();
            p.saw(
                "tree",
                format!("{REGISTRY_FILES} registry ({LIVE} live), {TRANSCRIPTS} transcripts, {SESSION_ENV} session-env dirs built in {} ms", tree.built.as_millis()),
            );

            // -- first rows (a): History, from launch ------------------------------
            let mut n = 0u32;
            let mut first: Option<(u128, usize, Value, f64)> = None;
            let ok = wait_for(LIVENESS, Duration::from_millis(100), || {
                n += 1;
                let r = c.call("history_query", json!({"queryId": format!("q-hp5-{n}"), "text": "", "filters": {}, "cursor": null, "limit": 50}));
                if let Ok(v) = r {
                    let rows = v["rows"].as_array().cloned().unwrap_or_default();
                    if !rows.is_empty() && rows.iter().all(|r| r["title"].is_string()) {
                        first = Some((t0.elapsed().as_millis(), rows.len(), v["coverage"].clone(), process_cpu_ms() - cpu_t0));
                        return true;
                    }
                }
                false
            })
            .await;
            assert!(ok, "history_query never returned a page with titles");
            let (hist_ms, hist_rows, coverage, hist_cpu) = first.unwrap();

            // -- first rows (b): Home, a full registry pass with 20 live --------------
            for i in 0..LIVE {
                c.procs.insert(live_pid(i), proc(live_start(tree.now_s, i), &std::fs::canonicalize(live_cwd(&home, i)).unwrap()));
            }
            rewrite_registry_mtimes(&home);
            let (t1, cpu_t1) = (Instant::now(), process_cpu_ms());
            c.wired.discovery_handle().wake();
            let mut home_ms = None;
            let ok = wait_for(LIVENESS, Duration::from_millis(20), || {
                if live_rows(&snap(&c)) == LIVE {
                    home_ms = Some((t1.elapsed().as_millis(), process_cpu_ms() - cpu_t1));
                    true
                } else {
                    false
                }
            })
            .await;
            assert!(ok, "the 20 live sessions never showed");
            let (home_ms, home_cpu) = home_ms.unwrap();
            let diag = settle(&c);
            assert_eq!(diag.registry_live as usize, LIVE, "{diag:?}");
            assert_eq!(diag.registry_stale as usize, REGISTRY_FILES - LIVE, "{diag:?}");
            let first_rows = format!(
                "History first page ({hist_rows} rows with titles, coverage {}/{}) {hist_ms} ms from launch (process CPU {hist_cpu:.0} ms); Home {LIVE} live of {REGISTRY_FILES} registry files {home_ms} ms after a full registry pass (process CPU {home_cpu:.0} ms)",
                coverage["titlesLoaded"], coverage["total"]
            );
            // The timing bounds are asserted at the end, so one run records
            // every measurement (a failed bound prints `FAILED after: …`).
            let mut over: Vec<String> = Vec::new();
            if hist_ms > FIRST_ROWS_MS || home_ms > FIRST_ROWS_MS {
                over.push(format!("first rows over 2 s: {first_rows}"));
            }
            p.saw("first rows ≤ 2 s", first_rows);

            // -- session-env is never a watch root -----------------------------------
            let env_root = std::fs::canonicalize(home.join(".claude/session-env")).unwrap();
            let claude_root = std::fs::canonicalize(home.join(".claude")).unwrap();
            // A second engine over the same tree (the bench's setup: the
            // fixture process table, no history, its own sinks and app data).
            let mut engine = {
                let app_data = home.join("hp5-engine-appdata");
                std::fs::create_dir_all(&app_data).unwrap();
                let mut cfg = DiscoveryConfig::for_home(&home, &app_data, Arc::new(NoOwnProcesses));
                cfg.accounts = vec![AccountSpec::new(Vendor::Claude, &home.join(".claude"))];
                cfg.cursor_dir = None;
                cfg.agy_dir = None;
                cfg.source = c.procs.clone();
                cfg.bundles = Arc::new(FakeBundles::default());
                cfg.teams = Arc::new(support::TeamsRec::default());
                cfg.watch = false;
                cfg.registry_retry = Duration::from_millis(1);
                cfg.team_retries = vec![Duration::ZERO; 3];
                Engine::new(cfg, Arc::new(support::Rec::default()))
            };
            let specs = engine.watch_specs();
            assert!(!specs.is_empty());
            for s in &specs {
                assert_ne!(s.path, claude_root, "the config root is never watched");
                assert!(!env_root.starts_with(&s.path) || !s.tag.recursive(), "{s:?} would cover session-env");
            }
            p.saw("25 k session-env dirs", format!("no watch root covers them ({} roots, none recursive over the config dir)", specs.len()));

            // -- idle (1): the discovery_budget bench method on this tree --------------
            let engine_pct = tokio::task::block_in_place(|| {
                engine.reconcile(); // pass 1 (not part of the idle budget)
                engine.tick();
                let t = thread_cpu_ns();
                let (wall, up) = (now_wall_ms(), now_up_ms());
                for i in 1..=20u64 {
                    engine.tick_at(wall + (i * 3000) as i64, up + i * 3000);
                }
                engine.reconcile();
                (thread_cpu_ns() - t) as f64 / 60e9 * 100.0
            });
            drop(engine);

            // -- idle (2): the integrated core, after the History fill -----------------
            let fill_t = Instant::now();
            // The fill runs at QoS background, throttled to work 1 : rest 3
            // (wall time), so a loaded machine slows it a lot; past the bound
            // the idle window is measured anyway and the run fails at the end.
            let filled = wait_for(Duration::from_secs(FILL_WAIT_SECS), Duration::from_millis(500), || {
                let (total, loaded) = c.wired.history.coverage();
                total as usize >= TRANSCRIPTS && loaded >= total
            })
            .await;
            let (total, loaded) = c.wired.history.coverage();
            let fill_s = fill_t.elapsed().as_secs();
            let fill = if filled {
                format!("after the History fill of {total} rows, {fill_s} s")
            } else {
                over.push(format!("the History background fill did not finish in {fill_s} s ({loaded}/{total})"));
                format!("with the History fill still running: {loaded}/{total} after {fill_s} s")
            };
            let _ = settle(&c);
            let before = settle(&c);
            let (cpu0, w0) = (process_cpu_ms(), Instant::now());
            tokio::time::sleep(Duration::from_secs(60)).await;
            let (cpu1, w1) = (process_cpu_ms(), w0.elapsed());
            let after = settle(&c);
            let core_pct = (cpu1 - cpu0) / w1.as_millis() as f64 * 100.0;
            let idle = format!(
                "process {core_pct:.3} % of one core over {} s ({fill}; {} scans, {} reconciles in the window); discovery engine {engine_pct:.3} % (bench method, simulated focused minute)",
                w1.as_secs(),
                after.scans - before.scans,
                after.reconciles - before.reconciles,
            );
            if core_pct > IDLE_PCT || engine_pct > IDLE_PCT {
                over.push(format!("idle over 1 %: {idle}"));
            }
            p.saw("idle ≤ 1 %", idle);

            // -- caps at the HP-5 sizes -----------------------------------------------
            let s = snap(&c);
            let reg = source(&s, SourceKind::Registry).expect("a Registry source status");
            let tr = source(&s, SourceKind::Transcripts).expect("a Transcripts source status");
            assert!(!reg.truncated && !tr.truncated, "no cap is exceeded at the HP-5 sizes: {reg:?} {tr:?}");
            assert_eq!(tr.indexed as usize, TRANSCRIPTS, "{tr:?}");
            p.saw(
                "caps at HP-5 sizes",
                format!(
                    "none exceeded (Registry indexed={} hiddenStale={} truncated=false; Transcripts indexed={} truncated=false)",
                    reg.indexed, reg.hidden_stale, tr.indexed
                ),
            );

            // -- caps exceeded ------------------------------------------------------
            grow_over_caps(&home, tree.now_s);
            c.wired.discovery.read().as_ref().unwrap().reconcile_now();
            let _ = settle(&c);
            // The reconcile kicked the history thread's stat pass; it runs
            // after the thread's current fill, which on a busy machine may
            // still be loading the HP-5 rows — then the probe runs that same
            // stat pass itself (and says so).
            let history = c.wired.history.clone();
            let kicked = wait_for(Duration::from_secs(60), Duration::from_millis(200), || history.truncated()).await;
            let stat_pass = if kicked {
                "the history thread's stat pass".to_string()
            } else {
                let (total, loaded) = history.coverage();
                tokio::task::block_in_place(|| history.refresh_index());
                format!("the stat pass run by the probe (the background fill was at {loaded}/{total})")
            };
            assert!(history.truncated(), "the History index never reported its 20,000 cap");
            c.wired.discovery.read().as_ref().unwrap().reconcile_now();
            let _ = settle(&c);
            let s = snap(&c);
            let reg = source(&s, SourceKind::Registry).expect("a Registry source status");
            let tr = source(&s, SourceKind::Transcripts).expect("a Transcripts source status");
            assert!(reg.truncated, "{REGISTRY_OVER} registry files > 4,096 → truncated: {reg:?}");
            assert!(tr.truncated, "{TRANSCRIPTS_OVER} transcripts > 20,000 → truncated: {tr:?}");
            p.saw(
                "caps exceeded",
                format!(
                    "reported ({REGISTRY_OVER} registry files → Registry.truncated=true; {TRANSCRIPTS_OVER} transcripts → Transcripts.truncated=true, indexed={}, via {stat_pass})",
                    tr.indexed
                ),
            );
            assert!(over.is_empty(), "{}", over.join("; "));
            p.finish();
        },
    )
    .await;
}
