//! The history index, the memory-only title cache and the two commands'
//! semantics (contract §4.2.8, §5.9) [C10], plus the History cold-start gate
//! on a generated 2.5 k-session tree (first page with titles ≤ 2 s).

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gilde_code_lib::contract::common::Vendor;
use gilde_code_lib::contract::ids::{HistoryCursor, QueryId, SessionUid};
use gilde_code_lib::contract::ipc::{HistoryFilters, HistoryLabel, HistoryQueryReq};
use gilde_code_lib::contract::session::{ContinueOption, Origin};
use gilde_code_lib::contract::ErrorCode;
use gilde_code_lib::discovery::cursor;
use gilde_code_lib::discovery::history::{HistoryService, HistorySources, IndexStore, MemoryUids, SqliteIndexStore, INDEX_BATCH_ROWS};
use gilde_code_lib::store::Db;
use support::*;

fn req(qid: &str, text: &str, limit: u16, cursor: Option<HistoryCursor>) -> HistoryQueryReq {
    HistoryQueryReq { query_id: QueryId::parse(qid).unwrap(), text: text.into(), filters: HistoryFilters::default(), cursor, limit }
}

fn populate(h: &Home, n: u32) -> Vec<String> {
    let cwd = h.workdir("proj");
    let mut sids = Vec::new();
    for i in 0..n {
        let sid = uuid(10_000 + i);
        let p = h.write_transcript(&h.claude, &format!("-proj-{}", i % 40), &sid, "claude/transcripts/normal.jsonl", &[("CWD", cwd.to_str().unwrap())]);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, r#"{{"type":"custom-title","customTitle":"세션 제목 {i} parser work","sessionId":"{sid}"}}"#).unwrap();
        drop(f);
        // Distinct mtimes so the order is deterministic.
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000 + u64::from(i));
        let ft = std::fs::File::options().write(true).open(&p).unwrap();
        ft.set_modified(t).unwrap();
        sids.push(sid);
    }
    sids
}

#[test]
fn query_pages_with_titles_and_cursor() {
    let h = Home::new();
    let sids = populate(&h, 30);
    let svc = h.history();
    let stats = svc.refresh_index();
    assert_eq!(stats.total, 30);
    let first = svc.query(&req("q-1", "", 10, None)).unwrap();
    assert_eq!(first.rows.len(), 10);
    assert!(!first.superseded);
    assert!(first.rows.iter().all(|r| r.title.as_deref().is_some_and(|t| t.starts_with("세션 제목"))), "the page's titles were read on demand");
    assert_eq!(first.rows[0].title.as_deref(), Some("세션 제목 29 parser work"), "newest first");
    assert_eq!(first.coverage.total, 30);
    assert!(first.coverage.titles_loaded >= 10);
    let r0 = &first.rows[0];
    assert_eq!(r0.vendor, Vendor::Claude);
    assert_eq!(r0.origin, Origin::Terminal);
    assert_eq!(r0.label, HistoryLabel::Title);
    assert!(!r0.running && !r0.title_only);
    assert_eq!(r0.continue_options, vec![ContinueOption::Resume, ContinueOption::Fork]);
    assert!(r0.project_id.is_some());
    // Pages never overlap and reach the end.
    let mut seen: Vec<SessionUid> = first.rows.iter().map(|r| r.uid.clone()).collect();
    let mut cursor = first.next_cursor.clone();
    while let Some(c) = cursor {
        let page = svc.query(&req("q-1", "", 10, Some(c))).unwrap();
        seen.extend(page.rows.iter().map(|r| r.uid.clone()));
        cursor = page.next_cursor;
    }
    assert_eq!(seen.len(), 30);
    let uniq: std::collections::HashSet<_> = seen.iter().collect();
    assert_eq!(uniq.len(), 30);
    let _ = sids;
}

#[test]
fn text_search_is_nfc_korean_safe_and_reports_coverage() {
    use unicode_normalization::UnicodeNormalization;
    let h = Home::new();
    populate(&h, 12);
    let svc = h.history();
    svc.refresh_index();
    svc.fill();
    let nfd: String = "세션 제목 7".nfd().collect();
    let res = svc.query(&req("q-2", &nfd, 20, None)).unwrap();
    assert!(res.rows.iter().any(|r| r.title.as_deref() == Some("세션 제목 7 parser work")));
    assert_eq!(res.coverage.titles_loaded, res.coverage.total);
    assert!(svc.query(&req("q-3", "zzzz-nothing", 20, None)).unwrap().rows.is_empty());
}

#[test]
fn newer_query_id_supersedes_and_cursor_of_another_query_is_invalid() {
    let h = Home::new();
    populate(&h, 5);
    let svc = h.history();
    svc.refresh_index();
    let a = svc.query(&req("q-a", "", 2, None)).unwrap();
    let c = a.next_cursor.clone().unwrap();
    let err = svc.query(&req("q-b", "parser", 2, Some(c))).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert_eq!(err.params.get("field").and_then(|v| v.as_str()), Some("cursor"));
}

#[test]
fn preview_reads_on_demand_and_title_only_vendors_are_unsupported() {
    let h = Home::new();
    populate(&h, 2);
    // A cursor chat: no title ("{vendor} 대화 · {date}"), no preview [C6].
    let cwd = h.workdir("cu");
    let chats = h.home.join(".cursor/chats");
    let ws = chats.join(cursor::md5_hex(cwd.to_str().unwrap()));
    std::fs::create_dir_all(ws.join("1f2e3d4c-aaaa")).unwrap();
    std::fs::write(ws.join("1f2e3d4c-aaaa/meta.json"), template("cursor/meta.json", &[("CWD", cwd.to_str().unwrap())])).unwrap();
    let svc: Arc<HistoryService> = h.history();
    svc.refresh_index();
    let res = svc.query(&req("q-p", "", 10, None)).unwrap();
    let claude = res.rows.iter().find(|r| r.vendor == Vendor::Claude).unwrap();
    let pv = svc.preview(&claude.uid).unwrap();
    assert!(pv.first_prompt.as_deref().unwrap().contains("first prompt"));
    assert!(pv.first_prompt.as_deref().unwrap().chars().count() <= 400);
    assert!(pv.last_reply.as_deref().unwrap().contains("final reply"));
    assert_eq!(pv.turns, Some(1));
    let cu = res.rows.iter().find(|r| r.vendor == Vendor::Cursor).unwrap();
    assert_eq!(cu.label, HistoryLabel::VendorDate);
    assert!(cu.title.is_none() && cu.title_only);
    assert_eq!(cu.continue_options, vec![ContinueOption::Resume], "the chat id is on disk");
    assert_eq!(svc.preview(&cu.uid).unwrap_err().code, ErrorCode::Unsupported);
    assert_eq!(svc.preview(&SessionUid::mint()).unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn filters_and_running_rows() {
    let h = Home::new();
    let sids = populate(&h, 6);
    let svc = h.history();
    svc.refresh_index();
    svc.set_live([(Vendor::Claude, sids[5].clone())].into_iter().collect());
    let mut r = req("q-f", "", 20, None);
    r.filters.vendor = Some(Vendor::Codex);
    assert!(svc.query(&r).unwrap().rows.is_empty());
    r.filters.vendor = Some(Vendor::Claude);
    r.filters.origin = Some(Origin::VsCode);
    assert!(svc.query(&r).unwrap().rows.is_empty());
    let all = svc.query(&req("q-g", "", 20, None)).unwrap();
    let running = all.rows.iter().filter(|r| r.running).collect::<Vec<_>>();
    assert_eq!(running.len(), 1);
    assert!(running[0].ended_at.is_none());
    assert_eq!(running[0].continue_options, vec![ContinueOption::Fork], "a live session is only ever copied");
}

#[test]
fn rate_class_query_returns_busy() {
    let h = Home::new();
    populate(&h, 1);
    let svc = h.history();
    svc.refresh_index();
    let mut busy = 0;
    for i in 0..60 {
        if let Err(e) = svc.query(&req(&format!("q-{i}"), "", 1, None)) {
            assert_eq!(e.code, ErrorCode::Busy);
            busy += 1;
        }
    }
    assert!(busy > 0, "20/s burst 20");
}

/// A51.1: the fill rests by thread CPU time, not by blocked time. Every row
/// blocks 40 ms without CPU (the test-only row hook, inside the measured
/// work). The rest hook records each rest the fill asks for, so the rule is
/// checked without timing it (A10.7; a CI runner's sleeps overshoot by tens
/// of milliseconds). `fill` runs on this thread, so its CPU time `c` is
/// measured here. The rests must add up to at most 3c. The old wall-time
/// rule asked for at least 3 × N × 40 ms.
#[test]
fn fill_rests_by_cpu_time_not_blocked_time() {
    const N: u32 = 20;
    const BLOCK: Duration = Duration::from_millis(40);
    let h = Home::new();
    let sids = populate(&h, N);
    let svc = h.history();
    assert_eq!(svc.refresh_index().total, N);
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let rests: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let s = seen.clone();
    svc.set_fill_row_hook(Some(Arc::new(move |p: &std::path::Path| {
        s.lock().unwrap().push(p.file_stem().unwrap().to_string_lossy().into_owned());
        std::thread::sleep(BLOCK); // blocked, no CPU
    })));
    let r = rests.clone();
    svc.set_fill_rest_hook(Some(Arc::new(move |rest: Duration| r.lock().unwrap().push(rest))));
    let (t0, c0) = (Instant::now(), thread_cpu());
    let loaded = svc.fill();
    let (elapsed, cpu) = (t0.elapsed(), thread_cpu().saturating_sub(c0));
    svc.set_fill_row_hook(None);
    svc.set_fill_rest_hook(None);
    let rests = rests.lock().unwrap().clone();
    let rested: Duration = rests.iter().sum();
    eprintln!("fill: {loaded} rows, each blocked {BLOCK:?}, in {elapsed:?}; CPU {cpu:?}; rests asked {rested:?} (≤ 3×CPU = {:?}; the old rule ≥ {:?})", cpu * 3, BLOCK * N * 3);
    assert_eq!(loaded, N, "every row loaded");
    let seen = seen.lock().unwrap().clone();
    let newest_first: Vec<String> = sids.iter().rev().cloned().collect();
    assert_eq!(seen, newest_first, "the hook ran once per row, newest first");
    let cov = svc.query(&req("q-cpu", "", 1, None)).unwrap().coverage;
    assert_eq!((cov.titles_loaded, cov.total), (N, N), "every title loaded");
    assert!(elapsed >= BLOCK * N, "the blocks themselves ran: {elapsed:?}");
    assert_eq!(rests.len(), N as usize, "one rest per row");
    assert!(rested <= cpu * 3, "the rests follow CPU time: asked {rested:?} > 3 × CPU {cpu:?}");
    assert!(rested < BLOCK * N, "the rests do not follow blocked time: asked {rested:?}");
}

/// This thread's CPU time (`CLOCK_THREAD_CPUTIME_ID`).
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out-parameter.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    assert_eq!(rc, 0, "the thread CPU clock reads");
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// A real index store on a temp `state.db`, laid out as the app has it: the
/// main store connection (`Db::open`: migrated, WAL) and the index
/// connection (`SqliteIndexStore::with_store_pragmas`, A51.3), written by the service's
/// index writer thread (A51.4).
struct TempIndex {
    _dir: tempfile::TempDir,
    _db: Db,
    path: std::path::PathBuf,
    store: Arc<SqliteIndexStore>,
}

impl TempIndex {
    fn new() -> TempIndex {
        let dir = tempfile::Builder::new().prefix("gilde-index-").tempdir().expect("temp dir");
        let path = dir.path().join("state.db");
        let db = Db::open(&path).expect("state.db");
        let store = Arc::new(SqliteIndexStore::with_store_pragmas(rusqlite::Connection::open(&path).expect("the index connection")).expect("pragmas"));
        TempIndex { _dir: dir, _db: db, path, store }
    }

    /// `Home::history()` writing through this store.
    fn history(&self, h: &Home) -> Arc<HistoryService> {
        HistoryService::new(
            HistorySources {
                claude: vec![(h.claude_account(), h.claude.clone())],
                codex: vec![],
                cursor_chats: Some(h.home.join(".cursor/chats")),
                agy_dir: Some(h.home.join(".gemini/antigravity-cli")),
                home: h.home.clone(),
                app_data: h.app_data.clone(),
            },
            Arc::new(MemoryUids::default()),
            Some(self.store.clone() as Arc<dyn IndexStore>),
        )
    }

    /// The stored rows equal the service's loaded rows of `sids`, field by
    /// field (the store keeps no device number).
    fn assert_holds_exactly(&self, svc: &HistoryService, sids: &[String]) {
        let stored: HashMap<_, _> = self.store.load().into_iter().map(|r| (r.path.clone(), r)).collect();
        assert_eq!(stored.len(), sids.len(), "history_index holds exactly the loaded rows");
        for sid in sids {
            let mut mem = svc.row_of(Vendor::Claude, sid).expect("an index row");
            assert!(mem.loaded, "{sid} was loaded");
            mem.file.dev = 0;
            assert_eq!(stored.get(&mem.path), Some(&mem), "the stored row equals the loaded row");
        }
    }
}

/// Poll `done` every 10 ms until it holds or `limit` passes (a generous
/// ceiling, A10.7); returns whether it held.
fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

const QOS_CLASS_UTILITY: u32 = 0x11;

/// The calling thread's QoS class (`pthread_get_qos_class_np`).
fn current_qos() -> u32 {
    extern "C" {
        fn pthread_get_qos_class_np(thread: libc::pthread_t, qos_class: *mut u32, relative_priority: *mut i32) -> i32;
    }
    let (mut class, mut relative) = (0u32, 0i32);
    // SAFETY: the calling thread's own handle and two valid out-parameters.
    let rc = unsafe { pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative) };
    assert_eq!(rc, 0, "pthread_get_qos_class_np");
    class
}

/// A51.4 (1): the query path never waits for the index store. The writer's
/// first transaction blocks inside (the store's test hook: it holds the
/// index connection and the SQLite write lock) for up to 10 s; meanwhile a
/// page of 50 rows never loaded answers with titles well within 5 s
/// (A10.7). After the hook releases, the rows reach the store.
#[test]
fn a_query_answers_while_the_index_writer_holds_the_write_lock() {
    const N: u32 = 120;
    const HOOK_BLOCK: Duration = Duration::from_secs(10);
    const CEILING: Duration = Duration::from_secs(5);
    let h = Home::new();
    let sids = populate(&h, N);
    let idx = TempIndex::new();
    let svc = idx.history(&h);
    svc.set_index_batch_interval(Duration::from_millis(20));
    assert_eq!(svc.refresh_index().total, N);

    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<Instant>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let first = AtomicBool::new(true);
    idx.store.set_write_hook(Some(Arc::new(move || {
        if first.swap(false, Ordering::SeqCst) {
            entered_tx.send(Instant::now()).expect("the test waits");
            // Released by the test, or after HOOK_BLOCK at the latest.
            let _ = release_rx.lock().unwrap().recv_timeout(HOOK_BLOCK);
        }
    })));

    // One row loaded on demand starts the writer's first transaction.
    let warm = svc.query(&req("q-warm", "", 1, None)).unwrap();
    assert_eq!(warm.rows.len(), 1);
    let entered = entered_rx.recv_timeout(Duration::from_secs(20)).expect("the writer began its transaction");

    // While it blocks there: the next page, 50 rows never loaded.
    let t0 = Instant::now();
    let page = svc.query(&req("q-page", "", 50, warm.next_cursor.clone())).unwrap();
    let took = t0.elapsed();
    let commits_meanwhile = idx.store.committed_writes();
    let held = entered.elapsed();
    let _ = release_tx.send(());
    eprintln!("page of {} rows in {took:?} while the writer held the write lock for {held:?} (ceiling {CEILING:?}, hook block {HOOK_BLOCK:?})", page.rows.len());
    assert_eq!(page.rows.len(), 50);
    assert!(page.rows.iter().all(|r| r.title.as_deref().is_some_and(|t| t.starts_with("세션 제목"))), "the page's titles, from memory");
    assert!(took < CEILING, "the query waited for the index store: {took:?}");
    assert_eq!(commits_meanwhile, 0, "the writer's transaction was still open while the page answered");

    // Released: the warm row and the page reach the store.
    svc.stop_and_drain();
    idx.store.set_write_hook(None);
    assert!(idx.store.committed_writes() >= 2, "the blocked transaction, then the page");
    idx.assert_holds_exactly(&svc, &sids[(N - 51) as usize..]);
}

/// A51.4 (2), A51.5: the index writer thread and the fill thread both run
/// at QoS utility (each read inside a test hook on that thread). Joining the background thread after `stop` — the engine's
/// shutdown — also waits for the writer's drain: every loaded row is stored.
#[test]
fn the_index_writer_and_the_fill_run_at_utility() {
    const N: u32 = 8;
    let h = Home::new();
    let sids = populate(&h, N);
    let idx = TempIndex::new();
    let svc = idx.history(&h);
    svc.set_index_batch_interval(Duration::from_millis(10));
    let fill_qos: Arc<Mutex<Vec<u32>>> = Arc::default();
    let writer_qos: Arc<Mutex<Vec<u32>>> = Arc::default();
    {
        let q = fill_qos.clone();
        svc.set_fill_row_hook(Some(Arc::new(move |_: &std::path::Path| q.lock().unwrap().push(current_qos()))));
        let q = writer_qos.clone();
        idx.store.set_write_hook(Some(Arc::new(move || q.lock().unwrap().push(current_qos()))));
    }
    let background = svc.spawn_fill();
    assert!(wait_until(Duration::from_secs(30), || svc.coverage().1 == N), "the background pass loaded every row");
    svc.stop();
    background.join().expect("the background thread");
    svc.set_fill_row_hook(None);
    idx.store.set_write_hook(None);
    idx.assert_holds_exactly(&svc, &sids);
    let (fill, writer) = (fill_qos.lock().unwrap().clone(), writer_qos.lock().unwrap().clone());
    eprintln!("QoS (hex): fill thread {fill:x?}, writer thread {writer:x?}");
    assert_eq!(fill.len(), N as usize, "the fill hook ran once per row");
    assert!(fill.iter().all(|&q| q == QOS_CLASS_UTILITY), "the fill thread runs at QoS utility (A51.5): {fill:x?}");
    assert!(!writer.is_empty(), "the writer wrote");
    assert!(writer.iter().all(|&q| q == QOS_CLASS_UTILITY), "the writer thread runs at QoS utility: {writer:x?}");
}

/// A51.4 (3): a fill of 600 rows reaches the store in ⌈600/64⌉ = 10 write
/// transactions (batches by size: the interval is set out of reach) and the
/// store holds exactly the rows it loaded. The fill never waits for SQLite:
/// the last row's work takes the write lock on a second connection and
/// holds it 1.5 s; the fill returns before the lock is released, and the
/// writer's last batch waits it out (`busy_timeout`) and loses nothing.
#[test]
fn fill_hands_rows_to_the_writer_in_batches_of_64_and_never_waits_for_the_lock() {
    const N: u32 = 600;
    const HOLD: Duration = Duration::from_millis(1_500);
    let h = Home::new();
    let sids = populate(&h, N);
    let idx = TempIndex::new();
    let svc = idx.history(&h);
    svc.set_index_batch_interval(Duration::from_secs(3_600));
    assert_eq!(svc.refresh_index().total, N);
    assert_eq!(idx.store.committed_writes(), 0, "a stat pass that removes nothing writes nothing");

    let locker = Arc::new(Mutex::new(Some(rusqlite::Connection::open(&idx.path).expect("second connection"))));
    let release: Arc<Mutex<Option<std::thread::JoinHandle<Instant>>>> = Arc::default();
    let calls = Arc::new(AtomicU32::new(0));
    {
        let (locker, release, calls) = (locker.clone(), release.clone(), calls.clone());
        svc.set_fill_row_hook(Some(Arc::new(move |_: &std::path::Path| {
            if calls.fetch_add(1, Ordering::SeqCst) + 1 != N {
                return;
            }
            let conn = locker.lock().unwrap().take().expect("one lock");
            conn.execute_batch("BEGIN IMMEDIATE").expect("the write lock");
            *release.lock().unwrap() = Some(std::thread::spawn(move || {
                std::thread::sleep(HOLD);
                let releasing = Instant::now();
                conn.execute_batch("COMMIT").expect("release");
                releasing
            }));
        })));
    }
    let t0 = Instant::now();
    let loaded = svc.fill();
    let done = Instant::now();
    svc.set_fill_row_hook(None);
    svc.stop_and_drain();
    let drained = Instant::now();
    let releasing = release.lock().unwrap().take().expect("the last row took the lock").join().expect("releaser");
    let commits = idx.store.committed_writes();
    eprintln!(
        "fill: {loaded} rows in {:?}, returned {:?} before the lock's release; drained {:?} after it; {commits} write transactions",
        done - t0,
        releasing.saturating_duration_since(done),
        drained.saturating_duration_since(releasing)
    );
    assert_eq!(loaded, N);
    assert_eq!(commits, u64::from(N.div_ceil(INDEX_BATCH_ROWS as u32)), "⌈{N}/{INDEX_BATCH_ROWS}⌉ write transactions");
    assert!(done < releasing, "the fill returned while the other connection held the lock: it never waits for SQLite");
    assert!(drained >= releasing, "the writer's last batch waited for the other connection's lock");
    idx.assert_holds_exactly(&svc, &sids);
}

/// A51.4: a queued row waits at most the interval for its batch (plus the
/// kernel's timer leeway: macOS coalesces a utility-QoS thread's timed wait
/// by up to 75 ms, `kern.timer_coalesce_tier3_ns_max`; measured here at load
/// ≈ 14: a 30 ms wait fired 110–170 ms after the row was queued). Each row
/// blocks 250 ms (the fill's test hook) and the interval is 20 ms, so a
/// batch holds one row, rarely two: 10 rows take ≥ 4 transactions (1 by size
/// alone); a generous floor (A10.7).
#[test]
fn the_writer_writes_a_row_at_most_an_interval_after_it_is_queued() {
    const N: u32 = 10;
    const GAP: Duration = Duration::from_millis(250);
    let h = Home::new();
    let sids = populate(&h, N);
    let idx = TempIndex::new();
    let svc = idx.history(&h);
    svc.set_index_batch_interval(Duration::from_millis(20));
    assert_eq!(svc.refresh_index().total, N);
    svc.set_fill_row_hook(Some(Arc::new(|_: &std::path::Path| std::thread::sleep(GAP))));
    assert_eq!(svc.fill(), N);
    svc.set_fill_row_hook(None);
    svc.stop_and_drain();
    let commits = idx.store.committed_writes();
    eprintln!("{N} rows, {GAP:?} apart, interval 20 ms: {commits} write transactions");
    assert!((4..=u64::from(N)).contains(&commits), "{commits} write transactions");
    idx.assert_holds_exactly(&svc, &sids);
}

/// A51.4: a fill that is stopped mid-pass has every row it loaded written —
/// the row in hand too — in one final batch.
#[test]
fn a_stopped_fill_has_the_rows_it_loaded_written() {
    const N: u32 = 12;
    const STOP_AT: u32 = 5;
    let h = Home::new();
    let sids = populate(&h, N);
    let idx = TempIndex::new();
    let svc = idx.history(&h);
    svc.set_index_batch_interval(Duration::from_secs(3_600));
    assert_eq!(svc.refresh_index().total, N);
    let me = Arc::downgrade(&svc);
    let calls = AtomicU32::new(0);
    svc.set_fill_row_hook(Some(Arc::new(move |_: &std::path::Path| {
        if calls.fetch_add(1, Ordering::SeqCst) + 1 == STOP_AT {
            if let Some(s) = me.upgrade() {
                s.stop();
            }
        }
    })));
    assert_eq!(svc.fill(), STOP_AT, "the row in hand finishes, then the pass stops");
    svc.set_fill_row_hook(None);
    svc.stop_and_drain();
    assert_eq!(idx.store.committed_writes(), 1, "one final batch, after the pass queued the row in hand");
    // Newest first: the last STOP_AT sessions.
    idx.assert_holds_exactly(&svc, &sids[(N - STOP_AT) as usize..]);
}

/// A51.4 (4): `stop` drains the queue. Rows a query queued just before it
/// (fewer than a batch, the interval out of reach: nothing written yet)
/// are stored once `stop` is called, with no join needed; rows loaded on
/// demand after the drain stay in memory only.
#[test]
fn stop_drains_the_rows_queued_just_before_it() {
    const N: u32 = 30;
    const PAGE: u16 = 20;
    let h = Home::new();
    let sids = populate(&h, N);
    let idx = TempIndex::new();
    let svc = idx.history(&h);
    svc.set_index_batch_interval(Duration::from_secs(3_600));
    assert_eq!(svc.refresh_index().total, N);
    let page = svc.query(&req("q-stop", "", PAGE, None)).unwrap();
    assert_eq!(page.rows.len(), usize::from(PAGE));
    assert_eq!(idx.store.committed_writes(), 0, "queued, not written yet");
    svc.stop();
    assert!(wait_until(Duration::from_secs(20), || idx.store.committed_writes() == 1), "stop alone drained the queue");
    svc.stop_and_drain();
    assert_eq!(idx.store.committed_writes(), 1, "one batch");
    idx.assert_holds_exactly(&svc, &sids[(N - u32::from(PAGE)) as usize..]);

    let all = svc.query(&req("q-after", "", 50, None)).unwrap();
    assert_eq!(all.rows.len(), N as usize);
    assert!(all.rows.iter().all(|r| r.title.is_some()), "rows loaded after the drain still answer from memory");
    assert_eq!((idx.store.committed_writes(), idx.store.load().len()), (1, usize::from(PAGE)), "and are not written");
}

/// The History cold start on a 2.5 k-session tree; returns the elapsed time.
fn cold_start() -> std::time::Duration {
    let h = Home::new();
    populate(&h, 2_500);
    let t0 = Instant::now();
    let svc = h.history();
    let stats = svc.refresh_index();
    let page = svc.query(&req("q-cold", "", 50, None)).unwrap();
    let elapsed = t0.elapsed();
    eprintln!("history cold start: {} rows indexed, first page of {} in {:?}", stats.total, page.rows.len(), elapsed);
    assert_eq!(stats.total, 2_500);
    assert_eq!(page.rows.len(), 50);
    assert!(page.rows.iter().all(|r| r.title.is_some()));
    elapsed
}

/// Functional (A10.7): the first page arrives with titles, within 10× the gate.
#[test]
fn history_cold_start_functional() {
    let elapsed = cold_start();
    assert!(elapsed.as_millis() <= 20_000, "{elapsed:?}");
}

/// The History cold-start gate (§11.1, C10, G5): first page with titles ≤ 2 s.
#[test]
#[ignore = "G5 benchmark (A10.7): cargo test --test discovery_history -- --ignored"]
fn bench_history_cold_start_first_page_with_titles_under_2s() {
    let elapsed = cold_start();
    assert!(elapsed.as_millis() <= 2_000, "{elapsed:?}");
}
