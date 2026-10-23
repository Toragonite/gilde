//! The Gilde 2 interface contract in code (contract §3, §4, §7.1; task T01).
//!
//! Every type that crosses a process boundary is defined once here and
//! exported to TypeScript with `ts-rs` (`scripts/gen-ipc.mjs` runs
//! `cargo test --features ts-export export_bindings` and writes
//! `src/lib/gen/**` and `src-tauri/sidecar/src/gen/**`; CI fails on drift)
//! [C1]. Hand-written mirrors are forbidden.
//!
//! Conventions (§3.1): structs `rename_all = "camelCase"`; unit enums are
//! camelCase string unions; data enums `tag = "kind"` (+ `rename_all_fields`)
//! → TS discriminated unions on `kind`; request structs `deny_unknown_fields`;
//! every `i64`/`u64` carries `#[ts(type = "number")]` (values clamped to
//! 2^53 − 1); `Option<T>` → `T | null`. Types are **[meta]** (may be
//! persisted) or **[content]** (memory only; never persisted or logged).
//!
//! After wave 0 these files are frozen for workers: a new type, command or
//! field is a contract change made by the lead (§15.1).
//!
//! The shim ↔ PaneHost frames (§4.6) live in `contract/shim.rs`, which is not a
//! module of this tree: `panehost/proto.rs` includes it by path so the
//! `gilde-tmux` binary can share it without linking this crate.

pub mod account;
pub mod activity;
pub mod attention;
pub mod common;
pub mod compat;
pub mod content;
pub mod display;
pub mod error;
pub mod ids;
pub mod interaction;
pub mod ipc;
pub mod ipc_manifest;
pub mod process;
pub mod project;
pub mod services;
pub mod session;
pub mod sidecar;
pub mod store;
pub mod team;
pub mod validate;

pub use error::{ErrorCode, IpcError};
pub use validate::{Req, Validate};

/// The shim ↔ app frames (§4.6), as compiled into this crate.
pub use crate::panehost::proto::{ShimRequest, ShimResponse};

#[cfg(test)]
mod export;
#[cfg(test)]
mod deps_smoke;
