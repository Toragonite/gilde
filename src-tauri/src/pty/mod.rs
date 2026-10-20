//! The PTY lane (DESIGN §6).
//!
//! One agent CLI, one pseudo-terminal, one ring buffer, one delivery channel.
//! Everything the user actually watches happens here; the headless lane (§5) is
//! the other half of the app and shares nothing with this one but the process
//! registry.
//!
//! ## The rule this module exists to keep
//!
//! **Bytes are never decoded in Rust.** Not to check for a prompt, not to log a
//! line, not to split on newlines. A 64 KiB read can end in the middle of a
//! `U+1F600`; a CLI with a broken locale can emit bytes that are not UTF-8 at
//! all; an escape sequence is meaningful only in full. All three survive because
//! the pipeline is `read → VecDeque<u8> → base64 → xterm.js`, and xterm decodes
//! exactly once. `scripts/lint-forbidden.sh` fails the build if either of the
//! two lossy conversions appears anywhere under this directory.
//!
//! ## Layout
//!
//! | file | what it owns |
//! |---|---|
//! | [`ring`] | the fixed-size scrollback ring and the snapshot wire format |
//! | [`session`] | one live session: the reader, the coalescer, the exit watcher |
//! | [`manager`] | the session table, spawning, and the closing ceremony |
//!
//! T16 added `ansi.rs` here — the `vte` tail parser that feeds the status
//! engine. It consumes the same bytes this module already holds and, like
//! everything else here, never turns them into a `String` it hands to anyone
//! else: `vte` decodes `char` by `char`, and what it builds is a separate,
//! never-delivered view.

pub mod ansi; // T16
pub mod dump; // PINNED §36
pub mod manager;
pub mod ring;
pub mod session;

pub use ansi::AnsiTail; // T16
pub use manager::{OpenSessionReq, OpenSpec, PtyManager};
pub use ring::{
    clamp_ring_bytes, parse_snapshot_header, snapshot_header, Ring, SNAPSHOT_HEADER_LEN,
    SNAPSHOT_MAGIC, SNAPSHOT_VERSION,
};
pub use session::{
    CloseMode, ExitInfo, PtyChunk, PtySession, PtySessionInfo, PtySignal, SendPromptOutcome,
};
