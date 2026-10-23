//! Cursor chat metadata and processes (contract §5.4) [C6].
//!
//! * `~/.cursor/chats/<md5(cwd)>/<chatId>/meta.json` (≤ 16 KiB):
//!   `{schemaVersion, cwd, createdAtMs, updatedAtMs, hasConversation}`; a dir
//!   walk (depth 2) on reconcile, FSEvents on `chats/` (1 s).
//! * **`store.db` is never opened**: the record that holds the chat name also
//!   holds `blobEncryptionKey`. Cursor rows have no title ("{vendor} 대화 ·
//!   {date}") and the preview is "can't be read here".
//! * Processes: `cursor-agent`/`agent` `--resume <chatId>` → that id;
//!   otherwise cwd only → Status unknown{ProcessOnly}.
//! * A chat id is resumable only if `^[0-9a-f-]{8,64}$` and its `meta.json`
//!   exists under `md5(cwd)` (§3.2).

use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use serde::Deserialize;

use crate::contract::common::EpochMs;
use crate::discovery::safe_read::{self, EntryKind, FileId, ReadError, Root};

pub const META_CAP: u64 = 16 * 1024;

/// `^[0-9a-f-]{8,64}$`.
pub fn is_chat_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

/// `md5(cwd)` as lowercase hex (the workspace dir name, verified [R2 E23]).
pub fn md5_hex(cwd: &str) -> String {
    hex::encode(Md5::digest(cwd.as_bytes()))
}

/// A chat's `meta.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatMeta {
    pub chat_id: String,
    pub cwd: Option<String>,
    pub created_at: Option<EpochMs>,
    pub updated_at: Option<EpochMs>,
    pub has_conversation: Option<bool>,
    pub schema_version: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMeta {
    schema_version: Option<i64>,
    cwd: Option<String>,
    created_at_ms: Option<i64>,
    updated_at_ms: Option<i64>,
    has_conversation: Option<bool>,
}

pub fn parse_meta(bytes: &[u8], chat_id: &str) -> Option<ChatMeta> {
    let r: RawMeta = serde_json::from_slice(bytes).ok()?;
    Some(ChatMeta {
        chat_id: chat_id.to_string(),
        cwd: r.cwd.filter(|c| c.len() <= 4096),
        created_at: r.created_at_ms.filter(|v| *v > 0),
        updated_at: r.updated_at_ms.filter(|v| *v > 0),
        has_conversation: r.has_conversation,
        schema_version: r.schema_version,
    })
}

/// One chat found by the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatFile {
    pub meta_path: PathBuf,
    pub chat_id: String,
    pub file: FileId,
}

/// Walk `chats/<md5>/<chatId>/meta.json` (depth 2).
pub fn list_chats(chats: &Root, cap: usize) -> Result<(Vec<ChatFile>, bool), ReadError> {
    let mut out = Vec::new();
    let top = safe_read::list_dir(chats, chats.path(), safe_read::DIR_ENTRIES_CAP)?;
    let mut truncated = top.truncated;
    for w in top.entries.iter().filter(|e| e.kind == EntryKind::Dir && e.name.len() == 32) {
        let wd = chats.join(&w.name);
        let Ok(ids) = safe_read::list_dir(chats, &wd, safe_read::DIR_ENTRIES_CAP) else { continue };
        truncated |= ids.truncated;
        for c in ids.entries.iter().filter(|e| e.kind == EntryKind::Dir && is_chat_id(&e.name)) {
            let meta_path = wd.join(&c.name).join("meta.json");
            if let Ok(st) = safe_read::lstat(&meta_path) {
                if st.kind == EntryKind::File {
                    out.push(ChatFile { meta_path, chat_id: c.name.clone(), file: st.id });
                }
            }
        }
    }
    if out.len() > cap {
        out.sort_by_key(|f| std::cmp::Reverse(f.file.mtime_ms));
        out.truncate(cap);
        truncated = true;
    }
    Ok((out, truncated))
}

/// Read one chat's `meta.json` (never `store.db`).
pub fn read_meta(chats: &Root, f: &ChatFile) -> Result<Option<ChatMeta>, ReadError> {
    let (bytes, _) = safe_read::read_file(chats, &f.meta_path, META_CAP)?;
    Ok(parse_meta(&bytes, &f.chat_id))
}

/// The resume check of §3.2: a valid id whose `meta.json` exists for `cwd`.
pub fn chat_exists(chats_dir: &Path, cwd: &str, chat_id: &str) -> bool {
    is_chat_id(chat_id)
        && safe_read::lstat(&chats_dir.join(md5_hex(cwd)).join(chat_id).join("meta.json"))
            .is_ok_and(|s| s.kind == EntryKind::File)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_md5_and_meta() {
        assert!(is_chat_id("1f2e3d4c-aaaa"));
        assert!(!is_chat_id("1F2E3D4C"));
        assert!(!is_chat_id("abc"));
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        let m = parse_meta(br#"{"schemaVersion":1,"cwd":"/w","createdAtMs":1,"updatedAtMs":2,"hasConversation":true}"#, "1f2e3d4c").unwrap();
        assert_eq!(m.updated_at, Some(2));
    }
}
