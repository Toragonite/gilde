//! The per-session scrollback ring (DESIGN §6-7) and the snapshot wire format
//! (DESIGN §6-3).
//!
//! The ring is a fixed-size `VecDeque<u8>` plus two counters. It holds **bytes**,
//! never text: nothing in this module (or anywhere under `src/pty/`) decodes
//! what an agent printed. A byte that entered the ring leaves it identical, and
//! that is what makes a remount pixel-identical to what the user was looking at
//! before the webview reloaded.
//!
//! Two counters do all the bookkeeping:
//!
//! * `next_seq` — the sequence number the **next** byte written will get. It is
//!   monotonic for the life of the session and never resets when bytes are
//!   evicted, so it doubles as "total bytes this session has produced".
//! * `first_retained_seq` — `next_seq - len()`. Everything below it has been
//!   evicted; a UI asking for it is asking for history that no longer exists.
//!
//! The snapshot format is deliberately trivial to parse in TypeScript:
//!
//! ```text
//! offset 0..4    magic  b"GLDP"
//! offset 4       version = 1
//! offset 5..8    reserved (0)
//! offset 8..16   next_seq : u64 little-endian
//! offset 16..    raw pty bytes (ring contents, oldest first)
//! ```

use std::collections::VecDeque;

/// `GLDP` — "gilde-code pty".
pub const SNAPSHOT_MAGIC: [u8; 4] = *b"GLDP";

/// Bumping this is a breaking change for `XTermHost`'s snapshot reader.
pub const SNAPSHOT_VERSION: u8 = 1;

/// Bytes before the raw pty payload starts.
pub const SNAPSHOT_HEADER_LEN: usize = 16;

/// DESIGN §6-7: a fixed 2 MiB per session.
pub const DEFAULT_RING_BYTES: usize = 2 * 1024 * 1024;

/// `config.terminal.scrollbackBytes` floor.
pub const MIN_RING_BYTES: usize = 512 * 1024;

/// `config.terminal.scrollbackBytes` ceiling — eight live sessions at the
/// ceiling is 128 MiB, which is the most this design is willing to spend.
pub const MAX_RING_BYTES: usize = 16 * 1024 * 1024;

/// Clamp a configured scrollback size into the supported range.
///
/// A value outside the range is a configuration mistake, not a reason to refuse
/// to open a terminal, so it is clamped rather than rejected.
pub fn clamp_ring_bytes(requested: u64) -> usize {
    let requested = requested.min(MAX_RING_BYTES as u64) as usize;
    requested.max(MIN_RING_BYTES)
}

/// Fixed-size byte ring with sequence accounting.
#[derive(Debug)]
pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    next_seq: u64,
}

impl Ring {
    /// A ring that retains the last `cap` bytes. `cap` is clamped to at least
    /// one byte so the invariants below cannot divide by zero.
    pub fn new(cap: usize) -> Ring {
        let cap = cap.max(1);
        Ring {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
            next_seq: 0,
        }
    }

    /// Retention size in bytes.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Bytes currently retained.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Sequence number the next byte to arrive will carry.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Sequence number of the oldest byte still in the ring.
    pub fn first_retained_seq(&self) -> u64 {
        self.next_seq - self.buf.len() as u64
    }

    /// Append `bytes`, evicting from the front as needed.
    ///
    /// Returns the sequence number the first appended byte received, which is
    /// what the coalescer stamps on the chunk it will send.
    pub fn push(&mut self, bytes: &[u8]) -> u64 {
        let seq = self.next_seq;
        self.next_seq += bytes.len() as u64;

        if bytes.len() >= self.cap {
            // A single write larger than the whole ring: keep only its tail.
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return seq;
        }

        let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.buf.drain(..overflow);
        }
        self.buf.extend(bytes);
        seq
    }

    /// The retained bytes, oldest first, as one contiguous buffer.
    pub fn contents(&self) -> Vec<u8> {
        let (front, back) = self.buf.as_slices();
        let mut out = Vec::with_capacity(front.len() + back.len());
        out.extend_from_slice(front);
        out.extend_from_slice(back);
        out
    }

    /// The 16-byte header followed by the retained bytes — exactly what
    /// `session_snapshot` hands to `tauri::ipc::Response::new`.
    pub fn snapshot(&self) -> Vec<u8> {
        let (front, back) = self.buf.as_slices();
        let mut out = Vec::with_capacity(SNAPSHOT_HEADER_LEN + front.len() + back.len());
        out.extend_from_slice(&snapshot_header(self.next_seq));
        out.extend_from_slice(front);
        out.extend_from_slice(back);
        out
    }
}

/// The 16 header bytes for a snapshot whose payload ends at `next_seq`.
pub fn snapshot_header(next_seq: u64) -> [u8; SNAPSHOT_HEADER_LEN] {
    let mut header = [0u8; SNAPSHOT_HEADER_LEN];
    header[0..4].copy_from_slice(&SNAPSHOT_MAGIC);
    header[4] = SNAPSHOT_VERSION;
    // 5..8 stay zero: reserved.
    header[8..16].copy_from_slice(&next_seq.to_le_bytes());
    header
}

/// Parse a snapshot header. `None` when the magic or the version is not ours —
/// the frontend does the same check before writing anything into xterm.
pub fn parse_snapshot_header(bytes: &[u8]) -> Option<(u8, u64)> {
    if bytes.len() < SNAPSHOT_HEADER_LEN || bytes[0..4] != SNAPSHOT_MAGIC {
        return None;
    }
    let version = bytes[4];
    if version != SNAPSHOT_VERSION {
        return None;
    }
    let mut seq = [0u8; 8];
    seq.copy_from_slice(&bytes[8..16]);
    Some((version, u64::from_le_bytes(seq)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_ring_starts_at_zero() {
        let ring = Ring::new(16);
        assert_eq!(ring.next_seq(), 0);
        assert_eq!(ring.first_retained_seq(), 0);
        assert!(ring.is_empty());
        assert_eq!(ring.contents(), Vec::<u8>::new());
    }

    #[test]
    fn seqs_count_every_byte_that_ever_arrived() {
        let mut ring = Ring::new(8);
        assert_eq!(ring.push(b"abcd"), 0);
        assert_eq!(ring.push(b"efgh"), 4);
        assert_eq!(ring.next_seq(), 8);
        assert_eq!(ring.first_retained_seq(), 0);

        // Eviction moves the floor, never the ceiling.
        assert_eq!(ring.push(b"ij"), 8);
        assert_eq!(ring.next_seq(), 10);
        assert_eq!(ring.first_retained_seq(), 2);
        assert_eq!(ring.contents(), b"cdefghij".to_vec());
        assert_eq!(ring.len(), 8);
    }

    #[test]
    fn a_write_bigger_than_the_ring_keeps_its_tail() {
        let mut ring = Ring::new(4);
        assert_eq!(ring.push(b"0123456789"), 0);
        assert_eq!(ring.contents(), b"6789".to_vec());
        assert_eq!(ring.next_seq(), 10);
        assert_eq!(ring.first_retained_seq(), 6);
    }

    #[test]
    fn arbitrary_byte_values_survive_a_round_trip() {
        // Every byte value, including 0x00, 0x1b and invalid UTF-8 leads.
        let all: Vec<u8> = (0..=255u8).collect();
        let mut ring = Ring::new(all.len());
        ring.push(&all);
        assert_eq!(ring.contents(), all);
    }

    #[test]
    fn the_snapshot_header_is_the_documented_16_bytes() {
        let mut ring = Ring::new(1024);
        ring.push(&[0xFF, 0x00, 0x1B]);
        let snap = ring.snapshot();

        assert_eq!(&snap[0..4], b"GLDP");
        assert_eq!(snap[4], 1);
        assert_eq!(&snap[5..8], &[0, 0, 0], "reserved bytes are zero");
        assert_eq!(u64::from_le_bytes(snap[8..16].try_into().unwrap()), 3);
        assert_eq!(&snap[16..], &[0xFF, 0x00, 0x1B]);
        assert_eq!(parse_snapshot_header(&snap), Some((1, 3)));
    }

    #[test]
    fn a_foreign_header_is_rejected_rather_than_guessed_at() {
        assert_eq!(parse_snapshot_header(b"NOPE"), None);
        let mut wrong_version = snapshot_header(7);
        wrong_version[4] = 2;
        assert_eq!(parse_snapshot_header(&wrong_version), None);
    }

    #[test]
    fn configured_scrollback_is_clamped_not_rejected() {
        assert_eq!(clamp_ring_bytes(0), MIN_RING_BYTES);
        assert_eq!(clamp_ring_bytes(u64::MAX), MAX_RING_BYTES);
        assert_eq!(clamp_ring_bytes(DEFAULT_RING_BYTES as u64), DEFAULT_RING_BYTES);
    }
}
