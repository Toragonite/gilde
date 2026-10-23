//! The §5.9 discovery budget: with 20 live sessions and nothing changing,
//! discovery spends ≤ 0.3 % of one core averaged over 60 s. Measured on the
//! engine's own thread as the work of one simulated minute at the focused
//! cadence (20 scans at 3 s + 1 reconcile), so it does not need to sleep 60 s
//! and other test binaries cannot disturb it.
//!
//! A10.7: the strict budget is a G5 benchmark (`#[ignore]`, run with
//! `--ignored`); the functional test keeps the same scenario with a 10×
//! ceiling and the "nothing changing → no evidence" assertion.

#[path = "fixtures/vendor/support.rs"]
mod support;

use std::sync::Arc;

use gilde_code_lib::discovery::{now_up_ms, now_wall_ms, Engine};
use support::*;

fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Returns the CPU share of one core over the simulated minute.
fn idle_minute_cpu_percent() -> f64 {
    let h = Home::new();
    let p = procs();
    let start = 1_792_558_000;
    for i in 0..20 {
        let cwd = h.workdir(&format!("p{i}"));
        let sid = uuid(i + 1);
        let pid = 6000 + i as i32;
        h.write_registry(&h.claude, pid, &sid, start, &cwd, "interactive", "cli", "idle", "");
        h.write_transcript(&h.claude, &format!("-p{i}"), &sid, "claude/transcripts/open_tool.jsonl", &[("CWD", cwd.to_str().unwrap())]);
        p.insert(pid, claude_proc(start, 1, &["claude"], &cwd));
    }
    // Background noise: 580 other processes of this user.
    for i in 0..580 {
        p.insert(20_000 + i, gilde_code_lib::discovery::procs::FakeProc {
            ppid: 1,
            uid: UID,
            start_sec: 100,
            comm: "proc".into(),
            exec_path: Some(format!("/usr/bin/tool{i}")),
            ..Default::default()
        });
    }
    let rec = Arc::new(Rec::default());
    let mut e = Engine::new(h.config(p, Arc::new(TeamsRec::default()), None), rec.clone());
    e.reconcile(); // pass 1 (not part of the idle budget)
    e.tick();
    let sent = rec.all().len();
    let t = thread_cpu_ns();
    let wall = now_wall_ms();
    let up = now_up_ms();
    for i in 1..=20u64 {
        e.tick_at(wall + (i * 3000) as i64, up + i * 3000);
    }
    e.reconcile();
    let spent = thread_cpu_ns() - t;
    let pct = spent as f64 / 60e9 * 100.0;
    eprintln!("discovery idle minute: {:.1} ms CPU = {:.3} % of one core", spent as f64 / 1e6, pct);
    assert_eq!(rec.all().len(), sent, "nothing changing → no evidence");
    pct
}

#[test]
fn idle_minute_with_20_live_sessions_functional() {
    let pct = idle_minute_cpu_percent();
    assert!(pct <= 3.0, "{pct:.3} % > 10× the 0.3 % budget");
}

/// G5 (§11.1, §5.9): ≤ 0.3 % of one core.
#[test]
#[ignore = "G5 benchmark (A10.7): cargo test --test discovery_budget -- --ignored"]
fn bench_idle_minute_with_20_live_sessions_under_0_3_percent() {
    let pct = idle_minute_cpu_percent();
    assert!(pct <= 0.3, "{pct:.3} % > 0.3 %");
}
