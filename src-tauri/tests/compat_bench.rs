//! T21 — the static canary budget: **≤ 1 s per binary at QoS utility**
//! (contract §2.4 B10, §11.1). Ignored by default (it writes a 274 MB file);
//! run it with an optimized build:
//!
//! ```text
//! cargo test --release --test compat_bench -- --ignored --nocapture
//! ```
//!
//! The synthetic binary has the size of the installed global 2.1.307 binary
//! (274,428,784 bytes, measured with `stat` on 2026-10-22): a Mach-O header,
//! ~70 % machine-code-like noise, then a JS-like region dense with `function`
//! and `async function` (the shape regexes' prefixes). Both measured cases
//! force a full pass — the worst case: every entry present but one of them at
//! the very start of the file, and one entry missing.
//!
//! Each case runs three times on a thread at QoS utility (as the canary
//! worker does). The budget is asserted on the best wall time and on the
//! thread's CPU time: a utility-QoS thread is starved on a busy machine, which
//! stretches wall time without saying anything about the scan.
//!
//! `GILDE_CANARY_BENCH_BINARY=<realpath>` additionally scans a real binary,
//! **read-only** (it is never executed): once with the shipped list and once
//! with one never-present literal added (a forced full pass).

#[path = "fixtures/compat/mod.rs"]
mod compat_fixtures;

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use compat_fixtures::*;
use gilde_code_lib::compat::canary::{
    enter_utility_qos, CanaryLiterals, ScanOutcome, Scanner, BUNDLED_CANARY_LITERALS,
};

const REAL_SIZE: usize = 274_428_784;
const BUDGET: Duration = Duration::from_secs(1);
const RUNS: usize = 3;

struct XorShift(u64);

impl XorShift {
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            let w = self.0.to_le_bytes();
            chunk.copy_from_slice(&w[..chunk.len()]);
        }
    }
}

/// JS-like text with frequent `function` / `async function` / `if(!` that
/// never forms the team-store shapes or any pinned literal.
fn js_block(i: usize) -> String {
    format!(
        "function a{i}(t,n){{if(!t)return n;return t+{i}}}var b{i}=async function(e){{if(e)return[];return null}};\
         async function c{i}(e,r){{for(let o of e)await r(o);return {i}}}let d{i}=Object.create(null);"
    )
}

fn write_synthetic(path: &Path, script: &str, early_entry: Option<&str>) {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(path).expect("create"));
    let mut written = 0usize;
    w.write_all(&MACHO_ARM64_HEADER).unwrap();
    written += MACHO_ARM64_HEADER.len();
    if let Some(e) = early_entry {
        w.write_all(e.as_bytes()).unwrap();
        written += e.len();
    }
    // ~70 %: machine-code-like noise
    let noise_end = REAL_SIZE * 7 / 10;
    let mut rng = XorShift(0x2545_f491_4f6c_dd1d);
    let mut buf = vec![0u8; 1 << 20];
    while written < noise_end {
        let n = buf.len().min(noise_end - written);
        rng.fill(&mut buf[..n]);
        w.write_all(&buf[..n]).unwrap();
        written += n;
    }
    // the JS region, with the pinned entries in the middle of it
    let js_end = REAL_SIZE - script.len();
    let middle = written + (js_end - written) / 2;
    let mut i = 0;
    let mut placed = false;
    while written < js_end {
        if !placed && written >= middle {
            w.write_all(script.as_bytes()).unwrap();
            written += script.len();
            placed = true;
            continue;
        }
        let block = js_block(i);
        let n = block.len().min(js_end - written);
        w.write_all(&block.as_bytes()[..n]).unwrap();
        written += n;
        i += 1;
    }
    if !placed {
        w.write_all(script.as_bytes()).unwrap();
        written += script.len();
    }
    w.write_all(&vec![b' '; REAL_SIZE.saturating_sub(written)])
        .unwrap();
    w.flush().unwrap();
}

fn warm(path: &Path) {
    let mut f = File::open(path).unwrap();
    let mut buf = vec![0u8; 8 << 20];
    while f.read(&mut buf).unwrap() > 0 {}
}

/// CPU time of the calling thread.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid out-pointer to a stack timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

struct Timing {
    outcome: ScanOutcome,
    best_wall: Duration,
    worst_cpu: Duration,
}

/// Scan `RUNS` times on a thread at QoS utility, as the canary worker does.
fn timed_scans(scanner: &Scanner, path: &Path) -> Timing {
    std::thread::scope(|s| {
        s.spawn(|| {
            enter_utility_qos();
            let mut t = Timing {
                outcome: ScanOutcome::Scanned {
                    missing: Default::default(),
                },
                best_wall: Duration::MAX,
                worst_cpu: Duration::ZERO,
            };
            for _ in 0..RUNS {
                let (wall, cpu) = (Instant::now(), thread_cpu());
                t.outcome = scanner.scan_path(path, None, Duration::from_secs(600));
                t.best_wall = t.best_wall.min(wall.elapsed());
                t.worst_cpu = t.worst_cpu.max(thread_cpu().saturating_sub(cpu));
            }
            t
        })
        .join()
        .unwrap()
    })
}

fn report(label: &str, bytes: u64, t: &Timing, assert_budget: bool) {
    let optimized = !cfg!(debug_assertions);
    println!(
        "{label}: best wall {:.3} s, worst thread CPU {:.3} s over {RUNS} runs, {bytes} bytes{}",
        t.best_wall.as_secs_f64(),
        t.worst_cpu.as_secs_f64(),
        if optimized {
            ""
        } else {
            " [debug build: budget not asserted]"
        }
    );
    if optimized && assert_budget {
        assert!(
            t.best_wall <= BUDGET,
            "{label}: wall {:?} > {BUDGET:?}",
            t.best_wall
        );
        assert!(
            t.worst_cpu <= BUDGET,
            "{label}: CPU {:?} > {BUDGET:?}",
            t.worst_cpu
        );
    }
}

/// The bundled list plus one literal no binary contains: a forced full pass.
fn forced_full_pass_scanner() -> Scanner {
    let needle = r#"{ "literal": "can_use_tool" },"#;
    let json = BUNDLED_CANARY_LITERALS.replacen(
        needle,
        r#"{ "literal": "can_use_tool" }, { "literal": "NEVER_PRESENT_LITERAL_X" },"#,
        1,
    );
    assert_ne!(
        json, BUNDLED_CANARY_LITERALS,
        "the bundled file layout changed"
    );
    Scanner::new(&CanaryLiterals::parse(&json).unwrap()).unwrap()
}

#[test]
#[ignore = "writes a 274 MB file; run with --release -- --ignored"]
fn canary_scan_of_a_realistic_binary_stays_within_one_second() {
    let scanner = Scanner::new(&CanaryLiterals::bundled()).unwrap();
    let (_d, dir) = temp_dir();
    let path = dir.join("claude.exe");
    let script = format!("{CONVERSATION_JS}{PANE_HOSTING_JS}{TEAM_STORE_307_JS}");

    // 1. every entry present, one of them at the very start: a full pass.
    let rest = script.replace("can_use_tool", "############");
    write_synthetic(&path, &rest, Some("\"can_use_tool\""));
    assert_eq!(std::fs::metadata(&path).unwrap().len() as usize, REAL_SIZE);
    warm(&path);
    let t = timed_scans(&scanner, &path);
    assert!(t.outcome.passed(), "{:?}", t.outcome);
    report(
        "pass, entry at offset 32 (full pass)",
        REAL_SIZE as u64,
        &t,
        true,
    );

    // 2. one entry missing: a full pass that ends in `Missing`.
    let missing = script.replace("--replay-user-messages", "######################");
    write_synthetic(&path, &missing, None);
    warm(&path);
    let t = timed_scans(&scanner, &path);
    assert!(
        matches!(&t.outcome, ScanOutcome::Scanned { missing } if missing.len() == 1),
        "{:?}",
        t.outcome
    );
    report("one entry missing (full pass)", REAL_SIZE as u64, &t, true);
}

#[test]
#[ignore = "reads the binary named by GILDE_CANARY_BENCH_BINARY (read-only, never executed)"]
fn canary_scan_of_an_installed_binary_read_only() {
    let Some(path) = std::env::var_os("GILDE_CANARY_BENCH_BINARY") else {
        println!("GILDE_CANARY_BENCH_BINARY not set; nothing to measure");
        return;
    };
    let path = std::path::PathBuf::from(path);
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    warm(&path);
    let t = timed_scans(&Scanner::new(&CanaryLiterals::bundled()).unwrap(), &path);
    println!("verdict with the shipped list: {:?}", t.outcome.to_result());
    report("installed binary, shipped list", bytes, &t, false);
    let t = timed_scans(&forced_full_pass_scanner(), &path);
    report("installed binary, forced full pass", bytes, &t, true);
}
