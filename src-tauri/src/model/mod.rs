//! The normalised domain model shared by every surface of the app.
//!
//! - [`status`] — PTY-engine evidence types (source S5 of contract §3.5.1; T06).
//!
//! The id newtypes moved to `contract::ids` (contract §3.2, T01).

pub mod status;
