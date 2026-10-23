//! G5 benchmark: shim startup + one round trip ≤ 15 ms p95 (contract §7.4.2,
//! §11.1). `#[ignore]`: run by the perf harness only (A10.7), never as a
//! functional gate. Owner: T07.

#[path = "fixtures/fake_tmux_client/mod.rs"]
mod fake_tmux_client;

use std::time::{Duration, Instant};

use fake_tmux_client::host::{me, Rig};
use fake_tmux_client::{run, shim_env};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "G5 performance gate: run by the perf harness on a quiet release build"]
async fn shim_round_trip_p95_is_within_15_ms() {
    let rig = Rig::new(me()).start();
    let env = shim_env(Some(&rig.socket()), Some(&rig.token), None, &rig.root);
    let sw = format!("claude-swarm-{}", rig.lead_pid);
    let args = ["-L", sw.as_str(), "list-panes", "-t", "claude-swarm:swarm-view", "-F", "#{pane_id}"];
    let mut samples: Vec<Duration> = Vec::new();
    for _ in 0..10 {
        run(&rig.shim(), &env, &rig.project, &args); // warm-up
    }
    for _ in 0..190 {
        let t = Instant::now();
        let out = run(&rig.shim(), &env, &rig.project, &args);
        samples.push(t.elapsed());
        assert_eq!(out.code, 1, "no session: can't find session");
    }
    samples.sort();
    let p95 = samples[samples.len() * 95 / 100];
    eprintln!("gilde-tmux round trip p50 {:?} p95 {:?}", samples[samples.len() / 2], p95);
    assert!(p95 <= Duration::from_millis(15), "p95 {p95:?} > 15 ms");
}
