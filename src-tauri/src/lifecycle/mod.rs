//! Lifecycle (contract §2.4, §10): boot order, spawn gate, memory-only mode, quit and crash records.
//!
//! Owner: T09. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.

pub mod boot;
pub mod crash;
pub mod memory_only;
pub mod quit;
pub mod spawn_gate;
pub mod wakeups; // T09b: the core wakeup counter file (A9.1)
pub mod wiring; // T09b: the composition root's service graph (A22)
