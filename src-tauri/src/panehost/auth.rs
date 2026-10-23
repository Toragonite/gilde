//! Peer uid, token, lead pid and kernel peer-pid parent checks (§4.6) [F-07].
//!
//! Owner: T07.
//!
//! Order of the checks for one request (the server applies them in this
//! order, see `server::PaneHost::handle`):
//!
//! 1. **peer uid** from `getpeereid` == the app's uid;
//! 2. the frame parses (`v`, token form, argv caps, absolute cwd);
//! 3. `v` + `build` equal the app's → else the update message (the version
//!    is **not** marked failed);
//! 4. the **token**, compared in constant time against every live lead's;
//! 5. the `claude-swarm-<pid>` of argv names that lead's pid exactly, and the
//!    lead process is still the one that was started (kernel start time);
//! 6. the per-lead rate (≤ 200 requests per rolling minute);
//! 7. for every **mutating** subcommand, the kernel-reported peer pid
//!    (`LOCAL_PEERPID`, never the self-reported `shimPid`) has the lead as its
//!    parent: the lead execs `tmux` directly, while its Bash-tool children —
//!    which inherit the token — have a shell in between.
//!
//! Every refusal answers `gilde-tmux: not authorized` (or the version /
//! busy line), increments a counter and writes one security log line without
//! the token or argv values. A refusal is **never** a compat deviation and is
//! never followed by a fallback to the real tmux [N-02].

use std::collections::VecDeque;
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

/// The kernel's view of the connecting process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub uid: u32,
    /// `LOCAL_PEERPID`: the process that connected.
    pub pid: i32,
}

/// `getpeereid` + `getsockopt(SOL_LOCAL, LOCAL_PEERPID)`.
pub fn peer_cred(stream: &UnixStream) -> io::Result<PeerCred> {
    let (uid, _gid) = nix::unistd::getpeereid(stream).map_err(io::Error::from)?;
    let pid = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerPid)
        .map_err(io::Error::from)?;
    Ok(PeerCred { uid: uid.as_raw(), pid })
}

/// The app's effective uid.
pub fn app_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// A per-lead token: 32 bytes from the system CSPRNG (`arc4random_buf`) as
/// 64 lowercase hex characters. Zeroed on drop; never logged.
pub fn mint_token() -> Zeroizing<String> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    // SAFETY: `bytes` is a writable 32-byte buffer; arc4random_buf fills it
    // and cannot fail.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    Zeroizing::new(hex::encode(*bytes))
}

/// Constant-time equality (no early exit on the first differing byte).
pub fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// The subcommands that change state (§4.6 auth (4)); everything else (`-V`,
/// `has-session`, `list-*`, the ignored layout calls) keeps token-only auth.
pub fn is_mutating(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "new-session" | "new-window" | "split-window" | "respawn-pane" | "kill-pane" | "select-pane" | "set-option"
    )
}

/// The kernel parent of `pid`; `None` when it is gone (or a zombie).
pub fn parent_of(pid: i32) -> Option<i32> {
    crate::supervise::procscan::kinfo_of(pid)
        .filter(|k| !k.is_zombie())
        .map(|k| k.ppid)
}

/// The kernel start time (seconds) of `pid`; `None` when it is gone.
pub fn start_of(pid: i32) -> Option<i64> {
    crate::supervise::procscan::start_time_of(pid)
}

/// Why a call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// `getpeereid` names another user (or could not be read).
    ForeignUid,
    /// The frame did not parse or failed the §4.6 form checks.
    BadFrame,
    /// `v`/`build` differ: the update message, never "failed".
    VersionMismatch,
    /// No live lead holds this token.
    Token,
    /// `claude-swarm-<pid>` is not the token's lead (e.g. a backgrounded lead).
    LeadPid,
    /// The token's lead is not started yet, or its process is gone/replaced.
    LeadGone,
    /// A mutating call whose kernel peer is not a direct child of the lead.
    NotParent,
    /// More than 200 requests in the last minute for this lead.
    RateLimited,
}

impl Refusal {
    pub const ALL: [Refusal; 8] = [
        Refusal::ForeignUid,
        Refusal::BadFrame,
        Refusal::VersionMismatch,
        Refusal::Token,
        Refusal::LeadPid,
        Refusal::LeadGone,
        Refusal::NotParent,
        Refusal::RateLimited,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::ForeignUid => "foreignUid",
            Refusal::BadFrame => "badFrame",
            Refusal::VersionMismatch => "versionMismatch",
            Refusal::Token => "token",
            Refusal::LeadPid => "leadPid",
            Refusal::LeadGone => "leadGone",
            Refusal::NotParent => "notParent",
            Refusal::RateLimited => "rateLimited",
        }
    }

    fn index(self) -> usize {
        Refusal::ALL.iter().position(|r| *r == self).unwrap_or(0)
    }
}

/// The diagnostic counters of §4.6 (refusals by reason, plus empty
/// connections such as an abandoned connect).
#[derive(Debug, Default)]
pub struct Counters {
    refused: [AtomicU64; 8],
    empty_connections: AtomicU64,
    dropped_busy: AtomicU64,
}

impl Counters {
    pub fn refused(&self, why: Refusal) {
        self.refused[why.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub fn refusals(&self, why: Refusal) -> u64 {
        self.refused[why.index()].load(Ordering::Relaxed)
    }

    pub fn total_refusals(&self) -> u64 {
        self.refused.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    pub fn empty_connection(&self) {
        self.empty_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn empty_connections(&self) -> u64 {
        self.empty_connections.load(Ordering::Relaxed)
    }

    /// A connection closed unanswered because too many were in flight.
    pub fn dropped_busy(&self) {
        self.dropped_busy.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dropped(&self) -> u64 {
        self.dropped_busy.load(Ordering::Relaxed)
    }
}

/// Requests admitted per lead per rolling window (§4.6: ≤ 200/min).
pub const RATE_LIMIT: usize = 200;
pub const RATE_WINDOW: Duration = Duration::from_secs(60);

/// A rolling-window counter on the monotonic clock.
#[derive(Debug)]
pub struct RateWindow {
    hits: VecDeque<Instant>,
    limit: usize,
    window: Duration,
}

impl Default for RateWindow {
    fn default() -> Self {
        RateWindow::new(RATE_LIMIT, RATE_WINDOW)
    }
}

impl RateWindow {
    pub fn new(limit: usize, window: Duration) -> RateWindow {
        RateWindow { hits: VecDeque::with_capacity(limit.min(256)), limit, window }
    }

    /// Admit one request at `now`, or refuse it (a refused request does not
    /// count against the window).
    pub fn admit(&mut self, now: Instant) -> bool {
        while let Some(&first) = self.hits.front() {
            if now.saturating_duration_since(first) >= self.window {
                self.hits.pop_front();
            } else {
                break;
            }
        }
        if self.hits.len() >= self.limit {
            return false;
        }
        self.hits.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_64_hex_and_distinct() {
        let a = mint_token();
        let b = mint_token();
        assert!(crate::panehost::proto::is_token_form(&a));
        assert!(crate::panehost::proto::is_token_form(&b));
        assert_ne!(*a, *b);
    }

    #[test]
    fn token_eq_is_exact() {
        let t = "0123456789abcdef".repeat(4);
        assert!(token_eq(&t, &t.clone()));
        let mut other = t.clone();
        other.replace_range(63..64, "e");
        assert!(!token_eq(&t, &other));
        assert!(!token_eq(&t, &t[..63]));
        assert!(!token_eq("", &t));
    }

    #[test]
    fn mutating_set_is_the_contract_list() {
        for m in ["new-session", "new-window", "split-window", "respawn-pane", "kill-pane", "select-pane", "set-option"] {
            assert!(is_mutating(m), "{m}");
        }
        for r in ["has-session", "list-panes", "list-windows", "select-layout", "resize-pane", "display-message", "-V"] {
            assert!(!is_mutating(r), "{r}");
        }
    }

    #[test]
    fn rate_window_admits_200_per_minute_then_refuses_until_it_rolls() {
        let mut w = RateWindow::default();
        let t0 = Instant::now();
        for i in 0..RATE_LIMIT {
            assert!(w.admit(t0), "request {i}");
        }
        assert!(!w.admit(t0), "the 201st within the minute is refused");
        assert!(!w.admit(t0 + Duration::from_secs(59)));
        assert!(w.admit(t0 + Duration::from_secs(60)), "the window rolled");
    }

    #[test]
    fn the_peer_of_a_socketpair_is_this_process() {
        let (a, _b) = UnixStream::pair().unwrap();
        let cred = peer_cred(&a).unwrap();
        assert_eq!(cred.uid, app_uid());
        assert_eq!(cred.pid, std::process::id() as i32);
    }

    #[test]
    fn parent_of_this_process_is_known() {
        let me = std::process::id() as i32;
        assert_eq!(parent_of(me), Some(std::os::unix::process::parent_id() as i32));
        assert!(start_of(me).is_some());
    }

    #[test]
    fn counters_count_by_reason() {
        let c = Counters::default();
        c.refused(Refusal::Token);
        c.refused(Refusal::Token);
        c.refused(Refusal::NotParent);
        assert_eq!(c.refusals(Refusal::Token), 2);
        assert_eq!(c.refusals(Refusal::NotParent), 1);
        assert_eq!(c.total_refusals(), 3);
        for r in Refusal::ALL {
            assert!(!r.as_str().is_empty());
        }
    }
}
