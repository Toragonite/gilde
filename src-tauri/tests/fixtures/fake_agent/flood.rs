//! Deterministic byte generator for the `binary-flood` mode of the `fake_agent`
//! fixture binary.
//!
//! This module is compiled into two places:
//!
//!   * the `fake_agent` binary (`mod flood;` in `main.rs`) — it *emits* the bytes;
//!   * the integration-test helper crate (`#[path = "fake_agent/flood.rs"]` in
//!     `tests/fixtures/mod.rs`) — it *predicts* the bytes.
//!
//! Sharing one source file is what makes the byte-fidelity assertion meaningful:
//! the test compares what came out of the pipe against an independently computed
//! buffer, so any corruption in the PTY / ring-buffer / IPC path shows up as a
//! byte diff instead of being silently absorbed.
//!
//! The stream is intentionally hostile to naive UTF-8 handling:
//!
//!   * the bulk is uniform pseudo-random bytes, which contain plenty of invalid
//!     UTF-8 sequences and stray continuation bytes;
//!   * every 64 KiB boundary carries a fixed 16-byte marker in which a 4-byte
//!     `U+1F600` **straddles the boundary** (1 byte before, 3 bytes after), so a
//!     reader that decodes 64 KiB chunks independently will mangle it.
//!
//! Everything is positional: byte `i` of the stream depends only on `i`, so the
//! producer can stream in arbitrary chunk sizes and the consumer can predict any
//! window without materialising the whole thing.

#![allow(dead_code)]

use std::io::{self, Write};

/// Chunk size the marker is aligned to. Matches the 64 KiB PTY read chunk that
/// DESIGN §7 specifies, which is the boundary we want to stress.
pub const CHUNK_BOUNDARY: u64 = 64 * 1024;

/// Fixed seed — the stream must be reproducible across processes and runs.
pub const FLOOD_SEED: u64 = 0x4147_454E_545F_464C; // "AGENT_FL"

/// Bytes written straddling each 64 KiB boundary `b`, starting at `b - 8`.
///
/// ```text
///   offset      bytes            meaning
///   b-8 .. b-5  FF FE C0 80      invalid UTF-8 (never valid lead/overlong)
///   b-4 .. b-2  EC 95 9C         U+D55C '한'   — entirely before the boundary
///   b-1 .. b+2  F0 9F 98 80      U+1F600 '😀'  — STRADDLES the boundary
///   b+3 .. b+4  C3 A9            U+00E9 'é'    — entirely after the boundary
///   b+5 .. b+7  80 F8 FF         invalid UTF-8 (bare continuation, bad lead)
/// ```
pub const MARKER: [u8; 16] = [
    0xFF, 0xFE, 0xC0, 0x80, // invalid
    0xEC, 0x95, 0x9C, // '한'
    0xF0, 0x9F, 0x98, 0x80, // '😀' — crosses the boundary
    0xC3, 0xA9, // 'é'
    0x80, 0xF8, 0xFF, // invalid
];

/// How many marker bytes sit before the boundary.
pub const MARKER_LEAD: u64 = 8;

/// The 4-byte character inside [`MARKER`] that crosses the boundary.
pub const STRADDLING_CHAR: [u8; 4] = [0xF0, 0x9F, 0x98, 0x80];

/// splitmix64 — a small, fast, well-distributed finalizer.
#[inline]
fn mix(seed: u64) -> u64 {
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The pseudo-random byte at absolute offset `i`, before markers are applied.
#[inline]
pub fn base_byte(i: u64) -> u8 {
    let block = mix(FLOOD_SEED ^ (i / 8));
    ((block >> (8 * (i % 8))) & 0xFF) as u8
}

/// Fill `buf` with the stream bytes starting at absolute offset `start`.
///
/// Chunking is irrelevant: `fill(0, &mut all)` and a sequence of smaller
/// `fill(off, &mut part)` calls produce the same bytes, markers included.
pub fn fill(start: u64, buf: &mut [u8]) {
    for (i, slot) in buf.iter_mut().enumerate() {
        *slot = base_byte(start + i as u64);
    }
    if buf.is_empty() {
        return;
    }
    let end = start + buf.len() as u64;

    // Boundaries whose marker window [b - 8, b + 8) overlaps [start, end).
    let first = (start.saturating_sub(MARKER_LEAD) / CHUNK_BOUNDARY).max(1);
    let last = (end + MARKER_LEAD) / CHUNK_BOUNDARY;
    for k in first..=last {
        let boundary = k * CHUNK_BOUNDARY;
        // boundary >= CHUNK_BOUNDARY > MARKER_LEAD, so this cannot underflow.
        let marker_start = boundary - MARKER_LEAD;
        for (j, byte) in MARKER.iter().enumerate() {
            let pos = marker_start + j as u64;
            if pos >= start && pos < end {
                buf[(pos - start) as usize] = *byte;
            }
        }
    }
}

/// Materialise the first `total` bytes of the stream.
pub fn flood_bytes(total: usize) -> Vec<u8> {
    let mut out = vec![0u8; total];
    fill(0, &mut out);
    out
}

/// Chunk size used when emitting. Deliberately *not* a power of two so that
/// writes straddle the 64 KiB markers and exercise [`fill`]'s overlap logic.
const EMIT_CHUNK: usize = 40_000;

/// Stream `total` bytes of the flood to `w` without buffering the whole thing.
pub fn write_flood<W: Write>(w: &mut W, total: u64) -> io::Result<()> {
    let mut buf = vec![0u8; EMIT_CHUNK];
    let mut written: u64 = 0;
    while written < total {
        let n = EMIT_CHUNK.min((total - written) as usize);
        fill(written, &mut buf[..n]);
        w.write_all(&buf[..n])?;
        written += n as u64;
    }
    w.flush()
}
