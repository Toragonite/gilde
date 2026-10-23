//! T21 — `CompatApi` + `CompatSink` (contract §3.10, A1; §14.2 T-compat-*:
//! shipped table lookup, canary pass/missing/unknown, T-compat-canaryTimeout,
//! T-compat-handshake, runtime deviations, T-compat-inherit, minimum version,
//! T-compat-binarySwap; premortems PM-09/10/11/42/43/45, F-22, N-07).
//!
//! Only synthesized fixture files are scanned; nothing is ever executed.

#[path = "fixtures/compat/mod.rs"]
mod compat_fixtures;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use compat_fixtures::*;
use gilde_code_lib::compat::canary::{fingerprint_of, CanaryLiterals, BUNDLED_CANARY_LITERALS};
use gilde_code_lib::compat::features::CLAUDE_FLAGS;
use gilde_code_lib::compat::notices::{
    details_lines, CONVERSATION_MIN_VERSION, CONVERSATION_UNAVAILABLE,
    DETAILS_TEAM_FORMAT_UNVERIFIED, DETAILS_UNVERIFIED_VERSION, TEAM_IN_PROCESS,
};
use gilde_code_lib::compat::sink::{load_rows, ObservedStore, VERSION_ROW};
use gilde_code_lib::compat::table::ShippedTable;
use gilde_code_lib::compat::{
    CompatConfig, CompatService, NOTE_VERSION_UNREADABLE, SPAWN_WAIT, UNKNOWN_VERSION,
};
use gilde_code_lib::contract::common::CompatState::{Failed, Unverified, Verified};
use gilde_code_lib::contract::common::Notice;
use gilde_code_lib::contract::compat::{
    CanaryResult, ClaudeCompat, CompatCapability, CompatSource,
};
use gilde_code_lib::contract::services::{CompatApi, CompatSink};
use gilde_code_lib::contract::session::BinaryRef;
use gilde_code_lib::store::Db;

const IDLE: Duration = Duration::from_secs(120);

/// The scans run at QoS utility, which a loaded machine starves: with the
/// production 1 s wait a spawn may (correctly) proceed with `Unknown`. Tests
/// that need the scan's verdict wait longer; the timeout path itself is
/// tested with `Duration::ZERO`.
const TEST_SPAWN_WAIT: Duration = Duration::from_secs(120);

fn config(store: Option<Arc<dyn ObservedStore>>) -> CompatConfig {
    let mut cfg = CompatConfig::bundled(store);
    cfg.spawn_wait = TEST_SPAWN_WAIT;
    cfg
}

fn service() -> CompatService {
    CompatService::new(config(None)).expect("bundled service")
}

fn service_with(edit: impl FnOnce(&mut CompatConfig)) -> CompatService {
    let mut cfg = config(None);
    edit(&mut cfg);
    CompatService::new(cfg).expect("service")
}

#[test]
fn the_bundled_config_has_the_contract_timings() {
    let cfg = CompatConfig::bundled(None);
    assert_eq!(
        cfg.spawn_wait,
        Duration::from_secs(1),
        "§6.0: wait up to 1 s"
    );
    assert_eq!(cfg.spawn_wait, SPAWN_WAIT);
    assert!(cfg.scan_deadline >= cfg.spawn_wait);
}

fn keys(notices: &[Notice]) -> Vec<&str> {
    notices.iter().map(|n| n.key.as_str()).collect()
}

fn flag(c: &ClaudeCompat, name: &str) -> Option<bool> {
    c.features.get(name).copied()
}

// ---------------------------------------------------------------------------
// input 1: the shipped table
// ---------------------------------------------------------------------------

#[test]
fn shipped_versions_are_listed_and_looked_up() {
    let svc = service();
    let c = svc.lookup("2.1.306").expect("2.1.306");
    assert_eq!((c.conversation, c.pane_hosting), (Verified, Unverified));
    assert_eq!(c.source, CompatSource::Shipped);
    assert_eq!(c.static_canary, CanaryResult::NotRun);
    assert_eq!(c.handshake_ok, None);
    assert!(!c.below_minimum);
    assert_eq!(c.note, None);
    assert_eq!(flag(&c, "allowSession"), Some(true));
    assert_eq!(flag(&c, "resumeRearm"), Some(false));
    assert_eq!(flag(&c, "teamStoreVerified"), None, "no scan yet");

    let c = svc.lookup("2.1.307").expect("2.1.307");
    assert_eq!((c.conversation, c.pane_hosting), (Unverified, Verified));
    assert_eq!(flag(&c, "paneHostingVerified"), Some(true));
    assert_eq!(
        flag(&c, "allowSession"),
        None,
        "inherited only after the binary's canary passed"
    );

    assert!(svc.lookup("2.1.400").is_none(), "never seen, not shipped");
    assert!(svc.lookup("latest").is_none());
    let listed: Vec<String> = svc.list().into_iter().map(|c| c.version).collect();
    assert_eq!(listed, ["2.1.306", "2.1.307"]);
    assert!(svc.notices("2.1.306").is_empty());
    assert!(svc.notices("2.1.307").is_empty());
}

#[test]
fn the_service_is_both_trait_objects() {
    let svc = Arc::new(service());
    let api: Arc<dyn CompatApi> = svc.clone();
    let sink: Arc<dyn CompatSink> = svc;
    sink.note("2.1.306", "unreadable permission request".into());
    assert_eq!(
        api.lookup("2.1.306").unwrap().note.as_deref(),
        Some("unreadable permission request")
    );
    for app_flag in [
        "vscodeWindowFocus",
        "notificationCenter",
        "dockMenu",
        "ideGotoWindow",
    ] {
        assert!(!api.app_feature(app_flag), "{app_flag} ships its fallback");
    }
}

// ---------------------------------------------------------------------------
// input 2: the static canary through the service (T-compat-canary)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_new_version_whose_canary_passes_runs_unverified_with_inherited_flags() {
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("claude"), &Spec::full("2.1.310"));
    let svc = service();
    let c = svc.for_binary(&resolve(&p)).await;
    assert_eq!(c.version, "2.1.310");
    assert_eq!((c.conversation, c.pane_hosting), (Unverified, Unverified));
    assert_eq!(c.source, CompatSource::StaticCanary);
    assert_eq!(c.static_canary, CanaryResult::Pass);
    // forward inheritance (PM-42): the newest table version below that names the flag
    assert_eq!(flag(&c, "allowSession"), Some(true)); // 2.1.306
    assert_eq!(flag(&c, "denyAndStop"), Some(true)); // 2.1.306
    assert_eq!(flag(&c, "resumeRearm"), Some(false)); // 2.1.306 fallback stays a fallback
    assert_eq!(flag(&c, "paneHostingVerified"), Some(true)); // 2.1.307
    assert_eq!(flag(&c, "inFlightSignals"), Some(false)); // 2.1.307 fallback
    assert_eq!(flag(&c, "teamStoreVerified"), Some(true)); // its own teamStore set
    assert_eq!(flag(&c, "ruleAllowMarker"), None); // open: fallback
    assert!(svc.feature("2.1.310", "allowSession"));
    assert!(!svc.feature("2.1.310", "resumeRearm"));
    assert!(!svc.feature("2.1.310", "ruleAllowMarker"));
    assert!(
        svc.notices("2.1.310").is_empty(),
        "Unverified shows no notice"
    );
    assert_eq!(keys(&details_lines(&c)), [DETAILS_UNVERIFIED_VERSION]);
    assert_eq!(svc.scans_started(), 1);

    // The same binary again: no second scan, the same record.
    assert_eq!(svc.for_binary(&resolve(&p)).await, c);
    assert_eq!(svc.scans_started(), 1);
    assert!(svc.list().iter().any(|r| r.version == "2.1.310"));
}

#[tokio::test]
async fn a_missing_conversation_entry_fails_conversation_only() {
    // PM-09/PM-10: the stdio prompt protocol changed → Terminal mode, loudly.
    let (_d, dir) = temp_dir();
    let p = write_fake(
        &dir.join("claude"),
        &Spec::full("2.1.310").without("can_use_tool"),
    );
    let svc = service();
    let c = svc.for_binary(&resolve(&p)).await;
    assert_eq!((c.conversation, c.pane_hosting), (Failed, Unverified));
    assert_eq!(c.source, CompatSource::StaticCanary);
    assert_eq!(
        c.static_canary,
        CanaryResult::Missing {
            literals: vec!["conversation: can_use_tool".into()]
        }
    );
    assert_eq!(
        flag(&c, "allowSession"),
        None,
        "no inheritance without a passing canary"
    );
    assert_eq!(flag(&c, "teamStoreVerified"), Some(true));
    assert_eq!(keys(&svc.notices("2.1.310")), [CONVERSATION_UNAVAILABLE]);
}

#[tokio::test]
async fn a_missing_pane_entry_fails_pane_hosting_only() {
    // PM-11: a changed tmux surface → new teams in-process, with ux-ia #103.
    let (_d, dir) = temp_dir();
    let p = write_fake(
        &dir.join("claude"),
        &Spec::full("2.1.310").without("split-window"),
    );
    let svc = service();
    let c = svc.for_binary(&resolve(&p)).await;
    assert_eq!((c.conversation, c.pane_hosting), (Unverified, Failed));
    assert_eq!(keys(&svc.notices("2.1.310")), [TEAM_IN_PROCESS]);
}

#[tokio::test]
async fn missing_team_store_shapes_turn_team_store_verified_off() {
    // §5.2.4: team files read as "unverified"; the capabilities are untouched.
    let (_d, dir) = temp_dir();
    let p = write_fake(
        &dir.join("claude"),
        &Spec::full("2.1.310").team(Some("var g=5000;")),
    );
    let svc = service();
    let c = svc.for_binary(&resolve(&p)).await;
    assert_eq!((c.conversation, c.pane_hosting), (Unverified, Unverified));
    assert_eq!(flag(&c, "teamStoreVerified"), Some(false));
    assert!(svc.notices("2.1.310").is_empty());
    assert_eq!(
        keys(&details_lines(&c)),
        [DETAILS_UNVERIFIED_VERSION, DETAILS_TEAM_FORMAT_UNVERIFIED]
    );
}

#[tokio::test]
async fn the_canary_can_lower_but_never_raise_a_shipped_version() {
    let (_d, dir) = temp_dir();
    let svc = service();
    let bad = write_fake(
        &dir.join("bad307"),
        &Spec::full("2.1.307").without("respawn-pane"),
    );
    let c = svc.for_binary(&resolve(&bad)).await;
    assert_eq!(
        (c.conversation, c.pane_hosting),
        (Unverified, Failed),
        "Verified lowered to Failed"
    );
    assert_eq!(c.source, CompatSource::StaticCanary);

    let good = write_fake(
        &dir.join("good306"),
        &Spec::full("2.1.306").team(Some(TEAM_STORE_306_JS)),
    );
    let c = svc.for_binary(&resolve(&good)).await;
    assert_eq!(
        (c.conversation, c.pane_hosting),
        (Verified, Unverified),
        "a pass never raises pane hosting"
    );
    assert_eq!(c.source, CompatSource::Shipped);
    assert_eq!(c.static_canary, CanaryResult::Pass);
    assert_eq!(flag(&c, "teamStoreVerified"), Some(true));
}

#[tokio::test]
async fn a_shipped_version_inherits_older_flags_once_its_canary_passed() {
    let (_d, dir) = temp_dir();
    let svc = service();
    let p = write_fake(&dir.join("claude"), &Spec::full("2.1.307"));
    let c = svc.for_binary(&resolve(&p)).await;
    assert_eq!((c.conversation, c.pane_hosting), (Unverified, Verified));
    assert_eq!(flag(&c, "paneHostingVerified"), Some(true)); // own
    assert_eq!(flag(&c, "inFlightSignals"), Some(false)); // own fallback
    assert_eq!(flag(&c, "handshakeShape"), Some(true)); // P1: 2.1.307 by static parity
    assert_eq!(flag(&c, "allowSession"), Some(true)); // from 2.1.306
    assert_eq!(flag(&c, "stdinEofCancel"), Some(false)); // from 2.1.306
}

#[tokio::test]
async fn a_launcher_script_or_unreadable_binary_stays_unverified_never_failed() {
    // N-07: only Mach-O is scanned; PM-43: a scan that cannot run is Unknown.
    let (_d, dir) = temp_dir();
    let svc = service();
    let script = write_fake(
        &dir.join("cli-wrapper.cjs"),
        &Spec::empty("2.1.311").not_macho(),
    );
    let c = svc.for_binary(&resolve(&script)).await;
    assert_eq!(c.static_canary, CanaryResult::Unknown);
    assert_eq!((c.conversation, c.pane_hosting), (Unverified, Unverified));
    assert!(svc.notices("2.1.311").is_empty());
    assert_eq!(flag(&c, "allowSession"), None);
    assert_eq!(flag(&c, "teamStoreVerified"), None);
    assert!(keys(&details_lines(&c)).contains(&DETAILS_TEAM_FORMAT_UNVERIFIED));
    // A wrong file type is final: the next spawn does not rescan it.
    let scans = svc.scans_started();
    svc.for_binary(&resolve(&script)).await;
    assert_eq!(svc.scans_started(), scans);

    let locked = write_fake(&dir.join("locked"), &Spec::empty("2.1.312"));
    let b = resolve(&locked);
    let _guard = Unreadable::make(&locked).unwrap();
    let c = svc.for_binary(&b).await;
    assert_eq!(c.static_canary, CanaryResult::Unknown);
    assert_eq!((c.conversation, c.pane_hosting), (Unverified, Unverified));
}

#[tokio::test]
async fn a_spawn_that_outwaits_the_canary_proceeds_unknown_and_the_scan_lands_later() {
    // T-compat-canaryTimeout: the 1 s wait of §6.0 ran out (zero here).
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("claude"), &Spec::full("2.1.310").filler(24 << 20));
    let svc = service_with(|c| c.spawn_wait = Duration::ZERO);
    let c = svc.for_binary(&resolve(&p)).await;
    assert_eq!(
        c.static_canary,
        CanaryResult::Unknown,
        "a timeout = Unknown"
    );
    assert_eq!((c.conversation, c.pane_hosting), (Unverified, Unverified));
    assert_eq!(flag(&c, "allowSession"), None);

    assert!(svc.wait_idle(IDLE));
    let later = svc.lookup("2.1.310").unwrap();
    assert_eq!(
        later.static_canary,
        CanaryResult::Pass,
        "the scan finished in the background"
    );
    assert_eq!(later.conversation, Unverified, "never Failed");
    assert_eq!(c.version, later.version);
}

#[tokio::test]
async fn a_timed_out_scan_is_not_kept_and_runs_again_next_time() {
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("claude"), &Spec::full("2.1.310"));
    let svc = service_with(|c| c.scan_deadline = Duration::ZERO);
    let c = svc.for_binary(&resolve(&p)).await;
    assert_eq!(c.static_canary, CanaryResult::Unknown);
    assert_eq!((c.conversation, c.pane_hosting), (Unverified, Unverified));
    assert_eq!(svc.scans_started(), 1);
    svc.for_binary(&resolve(&p)).await;
    assert_eq!(
        svc.scans_started(),
        2,
        "a timeout is retried, never cached as a verdict"
    );
}

#[test]
fn schedule_canary_scans_in_the_background() {
    // B10: boot and FSEvents schedule scans; nobody waits.
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("claude"), &Spec::full("2.1.310"));
    let svc = service();
    svc.schedule_canary(&resolve(&p));
    svc.schedule_canary(&resolve(&p)); // deduplicated
    assert!(svc.wait_idle(IDLE));
    assert_eq!(svc.scans_started(), 1);
    assert_eq!(
        svc.lookup("2.1.310").unwrap().static_canary,
        CanaryResult::Pass
    );
}

#[tokio::test]
async fn one_version_from_two_binaries_takes_the_worse_result() {
    // F-22: a global install and an editor-bundled copy of the same version.
    let (_d, dir) = temp_dir();
    let svc = service();
    let global = write_fake(&dir.join("global/claude.exe"), &Spec::full("2.1.310"));
    let bundled = write_fake(
        &dir.join("editor/claude"),
        &Spec::full("2.1.310").without("list-panes"),
    );
    assert_eq!(
        svc.for_binary(&resolve(&global)).await.pane_hosting,
        Unverified
    );
    // The version was scanned already, so this spawn does not wait (§6.0):
    // the second binary's canary lands in the background, for new spawns.
    let spawned = svc.for_binary(&resolve(&bundled)).await;
    assert_eq!(spawned.pane_hosting, Unverified);
    assert!(svc.wait_idle(IDLE));
    assert_eq!(svc.scans_started(), 2);
    let c = svc.lookup("2.1.310").unwrap();
    assert_eq!(c.pane_hosting, Failed);
    assert_eq!(
        c.static_canary,
        CanaryResult::Missing {
            literals: vec!["paneHosting: list-panes".into()]
        }
    );
}

#[tokio::test]
async fn a_binary_without_a_readable_version_is_treated_as_below_the_minimum() {
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("claude"), &Spec::full("2.1.310"));
    let mut b = resolve(&p);
    b.version = None;
    let svc = service();
    let c = svc.for_binary(&b).await;
    assert_eq!(c.version, UNKNOWN_VERSION);
    assert!(c.below_minimum);
    assert_eq!(c.note.as_deref(), Some(NOTE_VERSION_UNREADABLE));
    assert_eq!(
        c.static_canary,
        CanaryResult::Pass,
        "its own scan still ran"
    );
    assert!(svc.list().iter().all(|r| r.version != UNKNOWN_VERSION));
}

// ---------------------------------------------------------------------------
// input 3: the handshake (T-compat-handshake, the sink side)
// ---------------------------------------------------------------------------

#[test]
fn handshake_results_only_ever_lower() {
    let svc = service();
    // A timeout or an exit without a usage error is not compat evidence: the
    // transport reports nothing, and nothing changes.
    svc.schedule_canary(&BinaryRef {
        version: Some("2.1.310".into()),
        ..fake_ref_nowhere()
    });
    assert_eq!(svc.lookup("2.1.310").unwrap().handshake_ok, None);

    svc.handshake("2.1.310", true, None);
    let c = svc.lookup("2.1.310").unwrap();
    assert_eq!(c.handshake_ok, Some(true));
    assert_eq!(
        c.conversation, Unverified,
        "a handshake never verifies; only the table does"
    );

    svc.handshake("2.1.306", true, None);
    let c = svc.lookup("2.1.306").unwrap();
    assert_eq!(
        (c.conversation, c.source, c.handshake_ok),
        (Verified, CompatSource::Shipped, Some(true))
    );

    svc.handshake("2.1.320", false, Some("initialize: error subtype".into()));
    let c = svc.lookup("2.1.320").unwrap();
    assert_eq!((c.conversation, c.pane_hosting), (Failed, Unverified));
    assert_eq!(c.source, CompatSource::Handshake);
    assert_eq!(c.note.as_deref(), Some("initialize: error subtype"));
    assert_eq!(keys(&svc.notices("2.1.320")), [CONVERSATION_UNAVAILABLE]);

    svc.handshake("2.1.320", true, None);
    let c = svc.lookup("2.1.320").unwrap();
    assert_eq!(
        (c.conversation, c.handshake_ok),
        (Failed, Some(false)),
        "a later success does not undo it"
    );

    svc.handshake("2.1.306", false, None);
    assert_eq!(
        svc.lookup("2.1.306").unwrap().conversation,
        Failed,
        "a shipped Verified is lowered too"
    );
}

/// A `BinaryRef` at a path that does not exist (the scan reports Unknown).
fn fake_ref_nowhere() -> BinaryRef {
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("gone"), &Spec::empty("0.0.1"));
    let r = binary_ref(&p, None);
    drop(_d);
    r
}

// ---------------------------------------------------------------------------
// input 4: runtime deviations
// ---------------------------------------------------------------------------

#[test]
fn a_deviation_fails_its_capability_at_once_for_that_version_only() {
    let svc = service();
    svc.deviation(
        "2.1.307",
        CompatCapability::PaneHosting,
        "unsupported command: display-message".into(),
    );
    let c = svc.lookup("2.1.307").unwrap();
    assert_eq!(c.pane_hosting, Failed, "Verified lowered by observation");
    assert_eq!(c.conversation, Unverified);
    assert_eq!(c.source, CompatSource::Observed);
    assert_eq!(
        c.note.as_deref(),
        Some("unsupported command: display-message")
    );
    assert_eq!(keys(&svc.notices("2.1.307")), [TEAM_IN_PROCESS]);

    svc.deviation(
        "2.1.307",
        CompatCapability::PaneHosting,
        "teammate without respawn-pane".into(),
    );
    assert_eq!(
        svc.lookup("2.1.307").unwrap().note.as_deref(),
        Some("unsupported command: display-message"),
        "note = the first deviation"
    );

    let other = svc.lookup("2.1.306").unwrap();
    assert_eq!(
        (other.pane_hosting, other.source),
        (Unverified, CompatSource::Shipped)
    );

    svc.deviation(
        "2.1.310",
        CompatCapability::Conversation,
        "control_request without request_id".into(),
    );
    let c = svc.lookup("2.1.310").unwrap();
    assert_eq!((c.conversation, c.source), (Failed, CompatSource::Observed));
}

#[tokio::test]
async fn the_allow_session_guard_turns_the_flag_off_and_stops_inheritance() {
    // PM-42: a newer version wrote "session" rules to a settings file.
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("claude"), &Spec::full("2.1.310"));
    let svc = service();
    assert_eq!(
        flag(&svc.for_binary(&resolve(&p)).await, "allowSession"),
        Some(true)
    );

    svc.deviation(
        "2.1.310",
        CompatCapability::Feature("allowSession".into()),
        "settings file changed after AllowSession".into(),
    );
    let c = svc.lookup("2.1.310").unwrap();
    assert_eq!(flag(&c, "allowSession"), Some(false));
    assert_eq!(
        flag(&c, "denyAndStop"),
        None,
        "a deviated version inherits nothing"
    );
    assert_eq!(
        flag(&c, "teamStoreVerified"),
        Some(true),
        "its own canary facts stay"
    );
    assert_eq!(
        (c.conversation, c.source),
        (Unverified, CompatSource::Observed)
    );
    assert!(!svc.feature("2.1.310", "allowSession"));

    // A shipped version keeps its own flags; only the deviated one goes off.
    svc.deviation(
        "2.1.306",
        CompatCapability::Feature("allowSession".into()),
        "settings file changed".into(),
    );
    let c = svc.lookup("2.1.306").unwrap();
    assert_eq!(flag(&c, "allowSession"), Some(false));
    assert_eq!(flag(&c, "denyAndStop"), Some(true));
    assert_eq!(c.conversation, Verified);
}

#[test]
fn a_note_changes_nothing_but_the_note() {
    let svc = service();
    svc.note("2.1.306", "unreadable permission request".into());
    let c = svc.lookup("2.1.306").unwrap();
    assert_eq!(
        (c.conversation, c.source),
        (Verified, CompatSource::Shipped)
    );
    assert_eq!(c.note.as_deref(), Some("unreadable permission request"));
    svc.note("2.1.306", "another".into());
    assert_eq!(
        svc.lookup("2.1.306").unwrap().note.as_deref(),
        Some("unreadable permission request")
    );
    svc.deviation(
        "2.1.306",
        CompatCapability::Conversation,
        "initialize failed on a later start".into(),
    );
    assert_eq!(
        svc.lookup("2.1.306").unwrap().note.as_deref(),
        Some("initialize failed on a later start"),
        "a deviation's note wins over a plain note"
    );

    svc.note("2.1.330", format!("line\nbreak\u{7}{}", "x".repeat(500)));
    let n = svc.lookup("2.1.330").unwrap().note.unwrap();
    assert!(!n.chars().any(char::is_control));
    assert!(n.len() <= 200);

    svc.note("latest", "ignored".into());
    assert!(svc.list().iter().all(|c| c.version != "latest"));
}

// ---------------------------------------------------------------------------
// minimum version [C3]
// ---------------------------------------------------------------------------

#[test]
fn the_minimum_version_gate() {
    let svc = service();
    svc.handshake("2.1.267", false, None);
    let c = svc.lookup("2.1.267").unwrap();
    assert!(c.below_minimum);
    assert_eq!(
        keys(&svc.notices("2.1.267")),
        [CONVERSATION_MIN_VERSION],
        "one line, the min-version one"
    );
    assert!(svc.notices("2.1.268").is_empty());
    assert_eq!(keys(&svc.notices("2.0.999")), [CONVERSATION_MIN_VERSION]);
    assert_eq!(
        keys(&svc.notices("not-a-version")),
        [CONVERSATION_MIN_VERSION]
    );
    assert!(!svc.feature("not-a-version", "allowSession"));
}

// ---------------------------------------------------------------------------
// forward inheritance on a synthetic table (T-compat-inherit)
// ---------------------------------------------------------------------------

fn synthetic_table() -> ShippedTable {
    let versions = serde_json::json!({
        "2.1.300": { "features": { "allowSession": true, "denyAndStop": true },
                     "evidence": { "allowSession": "t", "denyAndStop": "t" } },
        "2.1.305": { "features": { "allowSession": false, "planAnswer": true },
                     "evidence": { "allowSession": "t", "planAnswer": "t" } },
        "2.1.310": { "features": { "bgAttach": true }, "evidence": { "bgAttach": "t" } }
    });
    let set = [
        "allowSession",
        "denyAndStop",
        "planAnswer",
        "bgAttach",
        "teamStoreVerified",
    ];
    let pending: serde_json::Map<String, serde_json::Value> = CLAUDE_FLAGS
        .iter()
        .filter(|f| !set.contains(f))
        .map(|f| (f.to_string(), serde_json::json!("open")))
        .collect();
    ShippedTable::parse(
        &serde_json::json!({ "schema": 1, "provenance": "t", "pending": pending, "versions": versions }).to_string(),
    )
    .expect("synthetic table")
}

#[tokio::test]
async fn forward_inheritance_takes_the_newest_version_below_that_names_the_flag() {
    let (_d, dir) = temp_dir();
    let svc = service_with(|c| c.table = synthetic_table());
    let at = |v: &str| {
        let p = write_fake(&dir.join(v).join("claude"), &Spec::full(v));
        resolve(&p)
    };
    let c = svc.for_binary(&at("2.1.320")).await;
    assert_eq!(
        flag(&c, "allowSession"),
        Some(false),
        "2.1.305's false beats 2.1.300's true"
    );
    assert_eq!(flag(&c, "denyAndStop"), Some(true), "only 2.1.300 names it");
    assert_eq!(flag(&c, "planAnswer"), Some(true));
    assert_eq!(flag(&c, "bgAttach"), Some(true));

    let c = svc.for_binary(&at("2.1.303")).await;
    assert_eq!(flag(&c, "allowSession"), Some(true));
    assert_eq!(flag(&c, "planAnswer"), None, "2.1.305 is not below 2.1.303");

    let c = svc.for_binary(&at("2.1.299")).await;
    assert_eq!(
        c.features.len(),
        1,
        "nothing below: only its canary fact: {:?}",
        c.features
    );

    let c = svc.for_binary(&at("2.1.310")).await;
    assert_eq!(flag(&c, "bgAttach"), Some(true), "own");
    assert_eq!(flag(&c, "planAnswer"), Some(true), "inherited");

    // Without a passing canary: the version's own flags only.
    let launcher = write_fake(&dir.join("x/cli.cjs"), &Spec::full("2.1.310").not_macho());
    let svc2 = service_with(|c| c.table = synthetic_table());
    let c = svc2.for_binary(&resolve(&launcher)).await;
    assert_eq!(c.static_canary, CanaryResult::Unknown);
    assert_eq!(c.features, BTreeMap::from([("bgAttach".to_string(), true)]));
}

// ---------------------------------------------------------------------------
// T-compat-binarySwap [A1, PM-45]
// ---------------------------------------------------------------------------

/// What T09's resolver does: cache the `BinaryRef` per path; a fingerprint
/// mismatch at the next spawn (or an FSEvents change) re-resolves it.
#[derive(Default)]
struct ResolverCache {
    cached: HashMap<PathBuf, BinaryRef>,
    resolves: usize,
}

impl ResolverCache {
    fn at_spawn(&mut self, path: &Path) -> BinaryRef {
        if let Some(b) = self.cached.get(path) {
            let still = fingerprint_of(Path::new(b.realpath.as_str())).ok() == Some(b.fingerprint)
                && std::fs::canonicalize(path).ok().as_deref()
                    == Some(Path::new(b.realpath.as_str()));
            if still {
                return b.clone();
            }
        }
        self.resolves += 1;
        let b = resolve(path);
        self.cached.insert(path.to_path_buf(), b.clone());
        b
    }
}

#[derive(Debug, Clone, Copy)]
enum Swap {
    SymlinkRetarget,
    InPlaceOverwrite,
    RenameReplace,
}

#[tokio::test]
async fn a_binary_swapped_under_a_running_session_changes_only_new_spawns() {
    for swap in [
        Swap::SymlinkRetarget,
        Swap::InPlaceOverwrite,
        Swap::RenameReplace,
    ] {
        let (_d, dir) = temp_dir();
        let svc = service();
        let mut resolver = ResolverCache::default();
        // X: intact; Y: the next patch, whose tmux surface changed.
        let spec_x = Spec::full("2.1.310");
        let spec_y = Spec::full("2.1.311")
            .without("respawn-pane")
            .filler(65 << 10);
        let bin = dir.join("bin/claude");
        match swap {
            Swap::SymlinkRetarget => {
                let x = write_fake(&dir.join("lib/2.1.310/claude.exe"), &spec_x);
                std::fs::create_dir_all(dir.join("bin")).unwrap();
                std::os::unix::fs::symlink(&x, &bin).unwrap();
            }
            Swap::InPlaceOverwrite | Swap::RenameReplace => {
                write_fake(&bin, &spec_x);
                set_mtime(&bin, 1_700_000_000);
            }
        }

        // A session spawns on X and keeps the record it resolved.
        let spawn_x = resolver.at_spawn(&bin);
        assert_eq!(spawn_x.version.as_deref(), Some("2.1.310"));
        let session_x = svc.for_binary(&spawn_x).await;
        assert_eq!(
            (session_x.conversation, session_x.pane_hosting),
            (Unverified, Unverified),
            "{swap:?}"
        );
        assert_eq!(session_x.static_canary, CanaryResult::Pass);

        // The auto-updater installs Y at the same path.
        match swap {
            Swap::SymlinkRetarget => {
                let y = write_fake(&dir.join("lib/2.1.311/claude.exe"), &spec_y);
                retarget_symlink(&bin, &y);
            }
            Swap::InPlaceOverwrite => {
                overwrite_in_place(&bin, &spec_y);
                set_mtime(&bin, 1_700_000_100);
            }
            Swap::RenameReplace => replace_by_rename(&bin, &spec_y),
        }

        // The next spawn notices the mismatch, re-runs --version, and Y's
        // canary runs before its mode is chosen.
        let scans_before = svc.scans_started();
        let spawn_y = resolver.at_spawn(&bin);
        assert_eq!(
            resolver.resolves, 2,
            "{swap:?}: the cached BinaryRef was invalidated"
        );
        assert_ne!(spawn_y.fingerprint, spawn_x.fingerprint, "{swap:?}");
        assert_eq!(spawn_y.version.as_deref(), Some("2.1.311"));
        let rec_y = svc.for_binary(&spawn_y).await;
        assert_eq!(svc.scans_started(), scans_before + 1, "{swap:?}: Y scanned");
        assert_eq!(rec_y.version, "2.1.311");
        assert_eq!(rec_y.pane_hosting, Failed, "{swap:?}: Y's own facts");
        assert_eq!(keys(&svc.notices("2.1.311")), [TEAM_IN_PROCESS]);

        // The running session's record — and X's — are untouched.
        assert_eq!(svc.lookup("2.1.310").unwrap(), session_x, "{swap:?}");
        assert_eq!(
            spawn_x.version.as_deref(),
            Some("2.1.310"),
            "the session's BinaryRef stays X's"
        );

        // system/init of X's session reports a third version (the update raced
        // the spawn): a note on X's compat line; mode and flags unchanged.
        svc.note("2.1.310", "system/init reported 2.1.312".into());
        let x_now = svc.lookup("2.1.310").unwrap();
        assert_eq!(
            (
                x_now.conversation,
                x_now.pane_hosting,
                x_now.source,
                &x_now.features
            ),
            (
                session_x.conversation,
                session_x.pane_hosting,
                session_x.source,
                &session_x.features
            ),
            "{swap:?}"
        );
        assert_eq!(x_now.note.as_deref(), Some("system/init reported 2.1.312"));
        assert!(
            svc.lookup("2.1.312").is_none(),
            "the third version gets no record from a note on X"
        );
    }
}

// ---------------------------------------------------------------------------
// persistence (`claude_compat_observed`, §3.9)
// ---------------------------------------------------------------------------

fn observed_db() -> Db {
    let db = Db::open_in_memory().expect("db");
    db.blocking(|c| {
        c.execute_batch(OBSERVED_DDL)?;
        Ok(())
    })
    .expect("ddl");
    db
}

#[tokio::test]
async fn observed_rows_survive_a_restart_and_known_binaries_are_not_rescanned() {
    let db = observed_db();
    let store: Arc<dyn ObservedStore> = Arc::new(db.clone());
    let (_d, dir) = temp_dir();
    let full = write_fake(&dir.join("a/claude"), &Spec::full("2.1.310"));
    let nopane = write_fake(
        &dir.join("b/claude"),
        &Spec::full("2.1.311").without("kill-pane"),
    );
    let script = write_fake(&dir.join("c/cli.cjs"), &Spec::full("2.1.313").not_macho());

    let before = {
        let svc = CompatService::new(config(Some(store.clone()))).unwrap();
        svc.for_binary(&resolve(&full)).await;
        svc.for_binary(&resolve(&nopane)).await;
        svc.for_binary(&resolve(&script)).await;
        svc.handshake("2.1.310", true, None);
        svc.deviation(
            "2.1.310",
            CompatCapability::Feature("allowSession".into()),
            "settings file changed".into(),
        );
        svc.note("2.1.306", "unreadable permission request".into());
        assert!(svc.wait_idle(IDLE));
        svc.list()
    };

    let rows = db.blocking(|c| Ok(load_rows(c)?)).unwrap();
    let canary_rows = rows
        .iter()
        .filter(|r| r.binary_fingerprint != VERSION_ROW)
        .count();
    let version_rows = rows
        .iter()
        .filter(|r| r.binary_fingerprint == VERSION_ROW)
        .count();
    assert_eq!(canary_rows, 2, "Unknown scans are never stored");
    assert_eq!(
        version_rows, 2,
        "2.1.310 (handshake + deviation) and 2.1.306 (note)"
    );

    let svc = CompatService::new(config(Some(store.clone()))).unwrap();
    let after: Vec<ClaudeCompat> = svc
        .list()
        .into_iter()
        .filter(|c| c.version != "2.1.313")
        .collect();
    let expected: Vec<ClaudeCompat> = before
        .into_iter()
        .filter(|c| c.version != "2.1.313")
        .collect();
    assert_eq!(after, expected);
    svc.for_binary(&resolve(&full)).await;
    svc.for_binary(&resolve(&nopane)).await;
    assert_eq!(svc.scans_started(), 0, "cached by fingerprint");

    // A different entry list makes every cached scan stale.
    let mut cfg = config(Some(store));
    cfg.literals = CanaryLiterals::parse(&format!("{BUNDLED_CANARY_LITERALS} ")).unwrap();
    let svc = CompatService::new(cfg).unwrap();
    svc.for_binary(&resolve(&full)).await;
    assert_eq!(svc.scans_started(), 1);
}

#[test]
fn without_the_table_the_store_is_memory_only() {
    // Before T09's migration v2 exists: no error, nothing stored.
    let db = Db::open_in_memory().expect("db");
    let store: Arc<dyn ObservedStore> = Arc::new(db);
    assert!(store.load().unwrap().is_empty());
    let svc = CompatService::bundled(Some(store)).unwrap();
    svc.handshake("2.1.310", false, None);
    assert!(svc.wait_idle(IDLE));
    assert_eq!(svc.lookup("2.1.310").unwrap().conversation, Failed);
}

// ---------------------------------------------------------------------------
// change notifications (for the `Compat{list}` store op)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_change_listener_hears_scans_and_evidence() {
    let (_d, dir) = temp_dir();
    let p = write_fake(&dir.join("claude"), &Spec::full("2.1.310"));
    let svc = service();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    svc.set_on_change(Arc::new(move || {
        h.fetch_add(1, Ordering::SeqCst);
    }));
    svc.for_binary(&resolve(&p)).await;
    assert!(svc.wait_idle(IDLE));
    assert!(
        hits.load(Ordering::SeqCst) >= 2,
        "a new version, then its scan"
    );

    let n = hits.load(Ordering::SeqCst);
    svc.deviation("2.1.310", CompatCapability::PaneHosting, "x".into());
    assert_eq!(hits.load(Ordering::SeqCst), n + 1);
    svc.deviation("2.1.310", CompatCapability::PaneHosting, "y".into());
    assert_eq!(
        hits.load(Ordering::SeqCst),
        n + 1,
        "no change, no notification"
    );
}
