//! The normalised domain model shared by every surface of the app.
//!
//! - [`ids`] — UUID v7 newtypes.
//! - [`graph`] — ★ single owner (T04): node / edge / patch shapes.
//! - [`status`] — session status, its evidence, node badges and metrics.

pub mod graph;
pub mod ids;
pub mod status;
