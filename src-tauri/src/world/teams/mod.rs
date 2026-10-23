//! Team assembly behind the vendor-neutral protocol (§7.1–§7.3).
//!
//! Owner: T02. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.

pub mod assembly;
pub mod bottlenecks;
pub mod claude;
pub mod orchestrator;
#[cfg(any(test, feature = "showcase"))]
pub mod fake;
