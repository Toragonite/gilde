/**
 * Legacy test files that the runner skips.
 *
 * Gilde 2 wave 1 replaced the store, the IPC boundary and the message tables
 * under the old screens. The old screens and their tests belong to the wave-2
 * owners (contract §15.2 T14–T18), who rewrite or delete them. Until then their
 * test files no longer build, so the runner skips them here.
 *
 * Each entry is pinned to the SHA-256 of the file as it was at `b425b5e`. A file
 * whose content changed is no longer skipped: rewriting a test in place puts it
 * back in the run without touching this list, and nothing new can hide here.
 * Entries whose file is gone are ignored, so deleting a legacy test needs no
 * change here either.
 */

/** @type {readonly { path: string, sha256: string, owner: string }[]} */
export const QUARANTINE = [
];
