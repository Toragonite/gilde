//! Render (contract §3.7): normalizers, the shared Claude message → block helper, tool summaries, `sanitize_display_text`. `claude_stream` is T05's.
//!
//! Owner: T04. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.

pub mod blocks;
pub mod claude_stream;
pub mod images;
pub mod mesh;
pub mod rollout;
pub mod sanitize;
pub mod tools;
pub mod transcript;
