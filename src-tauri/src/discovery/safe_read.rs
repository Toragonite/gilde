//! The one gate every discovery file read goes through (contract §5.1, §5.8,
//! §9.7) [C6].
//!
//! Order of checks for every open, each final:
//!
//! 1. the path must be UTF-8 (non-UTF-8 paths are skipped and counted, §5.8);
//! 2. the **never-open filter** ([`never_open`]) refuses credential-bearing
//!    files *by path, before `open`* — `*.key`, `ide/*.lock`, `.claude.json*`,
//!    `.credentials.json`, `auth.json`, `*token*`, `*credential*`, `*oauth*`,
//!    `*.pem`, cursor `store.db*`, keychains and `history.jsonl` (content);
//! 3. `open(O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)` with `EINTR`
//!    retry (`O_NONBLOCK` so a FIFO planted under a vendor root can never block
//!    discovery; it has no effect on regular files);
//! 4. `fstat` must say "regular file";
//! 5. `fcntl(F_GETPATH)` — the path the kernel actually opened — must lie under
//!    the canonical root (so a symlinked *directory* inside a root cannot lead a
//!    read outside it, and nothing races a check-then-open).
//!
//! Reads are bounded (a cap per caller, §5.8), `EINTR`-safe, and return
//! [`Zeroizing`] buffers so files that mix metadata with secrets are wiped when
//! the parse is done (§9.7 "the read buffer is zeroed after parsing").
//!
//! Nothing here ever opens for writing: discovery never writes under a vendor
//! root (§9.10).

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// the never-open filter (§9.7)
// ---------------------------------------------------------------------------

/// Why a path is never opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NeverOpen {
    /// `*.key` — `<cfg>/sessions/<pid>.<hex>.key`, `<cfg>/daemon/control.key`.
    KeyFile,
    /// `<cfg>/ide/*.lock` (holds an `authToken`).
    IdeLock,
    /// `.claude.json` (and its temp/backup copies) in any directory.
    ClaudeJson,
    /// `.credentials.json` and anything named `*credential*`.
    Credential,
    /// `~/.codex/auth.json` (any `auth.json`).
    AuthJson,
    /// `*token*` (incl. `antigravity-oauth-token*`).
    Token,
    /// `*oauth*` (incl. `~/.gemini/oauth_creds.json`).
    OAuth,
    /// `*.pem`.
    Pem,
    /// cursor `store.db` (the chat-name record holds `blobEncryptionKey`).
    CursorStore,
    /// Keychain files.
    Keychain,
    /// `history.jsonl` (typed prompts: content).
    HistoryJsonl,
    /// A regular file with more than one hard link (A34.3): an innocent name
    /// may be another name of a never-open file, and the kernel reports the
    /// name used to open it, so the filter cannot vouch for its other names.
    /// Refused after `open`, before any byte is read.
    HardLink,
}

impl NeverOpen {
    pub const fn as_str(self) -> &'static str {
        match self {
            NeverOpen::KeyFile => "keyFile",
            NeverOpen::IdeLock => "ideLock",
            NeverOpen::ClaudeJson => "claudeJson",
            NeverOpen::Credential => "credential",
            NeverOpen::AuthJson => "authJson",
            NeverOpen::Token => "token",
            NeverOpen::OAuth => "oauth",
            NeverOpen::Pem => "pem",
            NeverOpen::CursorStore => "cursorStore",
            NeverOpen::Keychain => "keychain",
            NeverOpen::HistoryJsonl => "historyJsonl",
            NeverOpen::HardLink => "hardLink",
        }
    }
}

/// The never-open filter, applied to the path **before** any `open` (§9.7).
///
/// Deliberately broader than the list of known files: a false refusal costs a
/// fact we never needed, a false acceptance costs a secret.
pub fn never_open(path: &Path) -> Option<NeverOpen> {
    let name = path.file_name()?;
    let lower = String::from_utf8_lossy(name.as_bytes()).to_lowercase();
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .map(|p| String::from_utf8_lossy(p.as_bytes()).to_lowercase());

    if lower.ends_with(".key") {
        return Some(NeverOpen::KeyFile);
    }
    if lower.starts_with(".claude.json") {
        return Some(NeverOpen::ClaudeJson);
    }
    if lower.contains("credential") {
        return Some(NeverOpen::Credential);
    }
    if lower.contains("oauth") {
        return Some(NeverOpen::OAuth);
    }
    if lower.contains("token") {
        return Some(NeverOpen::Token);
    }
    if lower == "auth.json" {
        return Some(NeverOpen::AuthJson);
    }
    if lower.ends_with(".pem") {
        return Some(NeverOpen::Pem);
    }
    if lower.ends_with(".keychain") || lower.ends_with(".keychain-db") {
        return Some(NeverOpen::Keychain);
    }
    if path
        .components()
        .any(|c| c.as_os_str().as_bytes().eq_ignore_ascii_case(b"Keychains"))
    {
        return Some(NeverOpen::Keychain);
    }
    if lower == "store.db" || lower.starts_with("store.db-") {
        return Some(NeverOpen::CursorStore);
    }
    if lower == "history.jsonl" {
        return Some(NeverOpen::HistoryJsonl);
    }
    if lower.ends_with(".lock") && parent.as_deref() == Some("ide") {
        return Some(NeverOpen::IdeLock);
    }
    None
}

// ---------------------------------------------------------------------------
// errors and counters
// ---------------------------------------------------------------------------

/// How a read failed. Every variant is an answer the caller maps to a §5.8 row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// The never-open filter refused it (no `open` happened).
    Refused(NeverOpen),
    NotFound,
    /// `EACCES`/`EPERM` — macOS privacy (TCC) or file modes. Never "empty".
    PermissionDenied,
    /// Not a regular file (dir, FIFO, socket, device).
    NotRegular,
    /// The final component is a symlink (`O_NOFOLLOW` → `ELOOP`).
    Symlink,
    /// The opened file does not lie under the root after realpath.
    OutsideRoot,
    /// Over the caller's cap (§5.8).
    TooLarge { size: u64 },
    /// `EBUSY`/`EAGAIN`/`EINTR` storms: retry per the reader's policy.
    Transient(i32),
    /// The path is not UTF-8 (skipped and counted).
    NonUtf8Path,
    /// Any other errno.
    Io(i32),
}

impl ReadError {
    pub fn from_errno(errno: i32) -> ReadError {
        match errno {
            libc::ENOENT | libc::ENOTDIR => ReadError::NotFound,
            libc::EACCES | libc::EPERM => ReadError::PermissionDenied,
            libc::ELOOP => ReadError::Symlink,
            libc::EBUSY | libc::EAGAIN | libc::EINTR | libc::ETIMEDOUT => ReadError::Transient(errno),
            other => ReadError::Io(other),
        }
    }

    pub fn from_io(e: &std::io::Error) -> ReadError {
        ReadError::from_errno(e.raw_os_error().unwrap_or(libc::EIO))
    }

    pub fn is_transient(&self) -> bool {
        matches!(self, ReadError::Transient(_))
    }

    /// Stable identifier for diagnostics (never a path).
    pub fn as_str(&self) -> &'static str {
        match self {
            ReadError::Refused(_) => "refused",
            ReadError::NotFound => "notFound",
            ReadError::PermissionDenied => "permissionDenied",
            ReadError::NotRegular => "notRegular",
            ReadError::Symlink => "symlink",
            ReadError::OutsideRoot => "outsideRoot",
            ReadError::TooLarge { .. } => "tooLarge",
            ReadError::Transient(_) => "transient",
            ReadError::NonUtf8Path => "nonUtf8Path",
            ReadError::Io(_) => "io",
        }
    }
}

/// Counters of this thread's reads (tests assert "never opened" with them; the
/// engine folds them into diagnostics). Thread-local so parallel tests do not
/// see each other's counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    /// Successful opens.
    pub opened: u64,
    /// Refusals by the never-open filter (no `open` call).
    pub refused: u64,
    /// `open`/`opendir` calls that hit `EACCES`/`EPERM`.
    pub eacces: u64,
    /// Non-UTF-8 paths skipped.
    pub non_utf8: u64,
    /// Over-cap files.
    pub too_large: u64,
}

thread_local! {
    static STATS: Cell<ReadStats> = const {
        Cell::new(ReadStats { opened: 0, refused: 0, eacces: 0, non_utf8: 0, too_large: 0 })
    };
}

fn bump(f: impl FnOnce(&mut ReadStats)) {
    STATS.with(|s| {
        let mut v = s.get();
        f(&mut v);
        s.set(v);
    });
}

/// This thread's counters since the last [`reset_stats`].
pub fn stats() -> ReadStats {
    STATS.with(Cell::get)
}

pub fn reset_stats() {
    STATS.with(|s| s.set(ReadStats::default()));
}

// ---------------------------------------------------------------------------
// roots
// ---------------------------------------------------------------------------

/// A canonical (realpath) root. Every resolved path must start with it (§5.8).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Root {
    canon: PathBuf,
}

impl Root {
    /// `realpath(path)`; the root itself may be reached through a symlink
    /// (e.g. a config dir symlinked elsewhere), files under it may not.
    pub fn new(path: &Path) -> Result<Root, ReadError> {
        if path.to_str().is_none() {
            return Err(ReadError::NonUtf8Path);
        }
        match std::fs::canonicalize(path) {
            Ok(canon) => Ok(Root { canon }),
            Err(e) => Err(ReadError::from_io(&e)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.canon
    }

    /// `real` (a realpath) is the root or lies under it.
    pub fn contains(&self, real: &Path) -> bool {
        real == self.canon || real.starts_with(&self.canon)
    }

    /// `rel` joined onto the canonical root.
    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.canon.join(rel)
    }
}

// ---------------------------------------------------------------------------
// opened files
// ---------------------------------------------------------------------------

/// `(size, mtime, inode)` identity of a file (cache keys, §5.2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FileId {
    pub size: u64,
    pub mtime_ms: i64,
    pub inode: u64,
    pub dev: u64,
}

impl FileId {
    pub fn from_metadata(m: &std::fs::Metadata) -> FileId {
        FileId {
            size: m.size(),
            mtime_ms: m.mtime().saturating_mul(1000) + m.mtime_nsec() / 1_000_000,
            inode: m.ino(),
            dev: m.dev(),
        }
    }

    fn from_stat(st: &libc::stat) -> FileId {
        FileId {
            size: st.st_size.max(0) as u64,
            mtime_ms: st.st_mtime.saturating_mul(1000) + st.st_mtime_nsec / 1_000_000,
            inode: st.st_ino,
            dev: st.st_dev as u64,
        }
    }
}

/// A file opened read-only through every check above.
#[derive(Debug)]
pub struct Opened {
    fd: OwnedFd,
    pub id: FileId,
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

/// Open `path` (which must lie under `root`) with every check of this module.
pub fn open_in(root: &Root, path: &Path) -> Result<Opened, ReadError> {
    if path.to_str().is_none() {
        bump(|s| s.non_utf8 += 1);
        return Err(ReadError::NonUtf8Path);
    }
    if let Some(why) = never_open(path) {
        bump(|s| s.refused += 1);
        tracing::debug!(target: "discovery", reason = why.as_str(), "never-open filter refused a path");
        return Err(ReadError::Refused(why));
    }
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| ReadError::NotFound)?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let fd = loop {
        // SAFETY: `c` is a valid NUL-terminated C string for the call's duration.
        let fd = unsafe { libc::open(c.as_ptr(), flags) };
        if fd >= 0 {
            break fd;
        }
        let e = errno();
        if e == libc::EINTR {
            continue;
        }
        if e == libc::EACCES || e == libc::EPERM {
            bump(|s| s.eacces += 1);
        }
        return Err(ReadError::from_errno(e));
    };
    // SAFETY: `fd` was just returned by a successful `open` and is owned here.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };

    // SAFETY: an all-zero `stat` is a valid out-parameter for `fstat`.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open; `st` is a valid pointer.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(ReadError::from_errno(errno()));
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(ReadError::NotRegular);
    }
    // A34.3: `O_NOFOLLOW` stops a symlink, not a hard link. A file with a
    // second name is refused (and closed by the drop of `fd`), its bytes
    // never read.
    if st.st_nlink > 1 {
        bump(|s| s.refused += 1);
        tracing::debug!(target: "discovery", reason = NeverOpen::HardLink.as_str(), "never-open filter refused an opened file");
        return Err(ReadError::Refused(NeverOpen::HardLink));
    }

    let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: F_GETPATH writes at most MAXPATHLEN bytes incl. NUL into `buf`.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
        return Err(ReadError::from_errno(errno()));
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let real = PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len]));
    if !root.contains(&real) || real == root.path() {
        return Err(ReadError::OutsideRoot);
    }
    // A34.3: the filter also applies to what was actually opened.
    if let Some(why) = never_open(&real) {
        bump(|s| s.refused += 1);
        tracing::debug!(target: "discovery", reason = why.as_str(), "never-open filter refused an opened file");
        return Err(ReadError::Refused(why));
    }

    bump(|s| s.opened += 1);
    Ok(Opened { fd, id: FileId::from_stat(&st) })
}

impl Opened {
    /// `pread` at `offset`, `EINTR`-safe; returns the bytes read (0 = EOF).
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        loop {
            // SAFETY: `buf` is valid for `buf.len()` bytes; the fd is open.
            let n = unsafe {
                libc::pread(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    offset as libc::off_t,
                )
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = errno();
            if e == libc::EINTR {
                continue;
            }
            return Err(ReadError::from_errno(e));
        }
    }

    /// Read `[offset, offset + len)` (short at EOF).
    pub fn read_range(&self, offset: u64, len: usize) -> Result<Zeroizing<Vec<u8>>, ReadError> {
        let mut out = Zeroizing::new(vec![0u8; len]);
        let mut got = 0usize;
        while got < len {
            let n = self.read_at(offset + got as u64, &mut out[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        out.truncate(got);
        Ok(out)
    }

    /// The whole file, refusing anything over `cap` bytes (also when it grows
    /// past the cap while being read).
    pub fn read_all_capped(&self, cap: u64) -> Result<Zeroizing<Vec<u8>>, ReadError> {
        if self.id.size > cap {
            bump(|s| s.too_large += 1);
            return Err(ReadError::TooLarge { size: self.id.size });
        }
        let want = (self.id.size as usize).saturating_add(1).min(cap as usize + 1);
        let mut out = Zeroizing::new(Vec::with_capacity(want));
        let mut chunk = Zeroizing::new(vec![0u8; 64 * 1024]);
        let mut offset = 0u64;
        loop {
            let n = self.read_at(offset, &mut chunk)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
            offset += n as u64;
            if out.len() as u64 > cap {
                bump(|s| s.too_large += 1);
                return Err(ReadError::TooLarge { size: out.len() as u64 });
            }
        }
        Ok(out)
    }
}

/// Open and read a whole small file under `root` (cap per §5.8).
pub fn read_file(root: &Root, path: &Path, cap: u64) -> Result<(Zeroizing<Vec<u8>>, FileId), ReadError> {
    let f = open_in(root, path)?;
    let bytes = f.read_all_capped(cap)?;
    Ok((bytes, f.id))
}

// ---------------------------------------------------------------------------
// stat and listing (no file content)
// ---------------------------------------------------------------------------

/// What `lstat` found. Never-open files may be stat'ed: existence by `stat`
/// only (§9.7: `.claude.json` is checked with `stat` and never opened).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatInfo {
    pub kind: EntryKind,
    pub id: FileId,
    pub uid: u32,
}

/// `lstat(path)` (never follows the final component).
pub fn lstat(path: &Path) -> Result<StatInfo, ReadError> {
    if path.to_str().is_none() {
        bump(|s| s.non_utf8 += 1);
        return Err(ReadError::NonUtf8Path);
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            let ft = m.file_type();
            let kind = if ft.is_symlink() {
                EntryKind::Symlink
            } else if ft.is_dir() {
                EntryKind::Dir
            } else if ft.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            Ok(StatInfo { kind, id: FileId::from_metadata(&m), uid: m.uid() })
        }
        Err(e) => Err(ReadError::from_io(&e)),
    }
}

/// One directory entry with a UTF-8 name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    pub name: String,
    pub kind: EntryKind,
}

/// A bounded directory listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirListing {
    pub entries: Vec<DirEntryInfo>,
    /// The 50,000-entry cap (§5.8) was hit: stop, diagnostic.
    pub truncated: bool,
    /// Entries skipped because their name is not UTF-8.
    pub non_utf8: u32,
}

/// The per-readdir cap of §5.8.
pub const DIR_ENTRIES_CAP: usize = 50_000;

/// List `dir` (which must be the root or lie under it after realpath). Symlinked
/// entries are reported as `Symlink` and never followed by callers.
pub fn list_dir(root: &Root, dir: &Path, cap: usize) -> Result<DirListing, ReadError> {
    if dir.to_str().is_none() {
        bump(|s| s.non_utf8 += 1);
        return Err(ReadError::NonUtf8Path);
    }
    let meta = std::fs::symlink_metadata(dir).map_err(|e| ReadError::from_io(&e))?;
    if meta.file_type().is_symlink() && dir != root.path() {
        return Err(ReadError::Symlink);
    }
    let real = std::fs::canonicalize(dir).map_err(|e| ReadError::from_io(&e))?;
    if !root.contains(&real) {
        return Err(ReadError::OutsideRoot);
    }
    let rd = std::fs::read_dir(&real).map_err(|e| {
        let code = e.raw_os_error().unwrap_or(libc::EIO);
        if code == libc::EACCES || code == libc::EPERM {
            bump(|s| s.eacces += 1);
        }
        ReadError::from_errno(code)
    })?;
    let mut out = DirListing::default();
    for ent in rd {
        let Ok(ent) = ent else { continue };
        if out.entries.len() >= cap {
            out.truncated = true;
            break;
        }
        let name = ent.file_name();
        let Some(name) = name.to_str() else {
            out.non_utf8 += 1;
            bump(|s| s.non_utf8 += 1);
            continue;
        };
        let kind = match ent.file_type() {
            Ok(ft) if ft.is_symlink() => EntryKind::Symlink,
            Ok(ft) if ft.is_dir() => EntryKind::Dir,
            Ok(ft) if ft.is_file() => EntryKind::File,
            _ => EntryKind::Other,
        };
        out.entries.push(DirEntryInfo { name: name.to_string(), kind });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// transient-failure backoff (§5.8 last rows, A §10.7)
// ---------------------------------------------------------------------------

/// "After 3 failures the file is skipped for 60 s and counted."
#[derive(Debug, Default)]
pub struct Backoff {
    failures: HashMap<PathBuf, (u8, Option<u64>)>,
    /// Files put on the 60 s skip list since creation (diagnostics).
    pub skipped_total: u64,
}

impl Backoff {
    pub const LIMIT: u8 = 3;
    pub const SKIP_MS: u64 = 60_000;

    /// Whether `path` is on the skip list at uptime `now_up_ms`.
    pub fn skipping(&mut self, path: &Path, now_up_ms: u64) -> bool {
        match self.failures.get(path) {
            Some((_, Some(until))) if *until > now_up_ms => true,
            Some((_, Some(_))) => {
                self.failures.remove(path);
                false
            }
            _ => false,
        }
    }

    /// Record one transient failure; returns true when the file was just put
    /// on the skip list.
    pub fn fail(&mut self, path: &Path, now_up_ms: u64) -> bool {
        let e = self.failures.entry(path.to_path_buf()).or_insert((0, None));
        e.0 = e.0.saturating_add(1);
        if e.0 >= Self::LIMIT && e.1.is_none() {
            e.1 = Some(now_up_ms + Self::SKIP_MS);
            self.skipped_total += 1;
            return true;
        }
        false
    }

    pub fn ok(&mut self, path: &Path) {
        self.failures.remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn never_open_list_of_section_9_7() {
        let refused = [
            "/h/.claude/sessions/123.0a1b2c.key",
            "/h/.claude/daemon/control.key",
            "/h/.claude/ide/51234.lock",
            "/h/.claude/.credentials.json",
            "/h/.claude/.claude.json",
            "/h/.claude.json",
            "/h/.claude/backups/.claude.json.backup.1700000000",
            "/h/.codex/auth.json",
            "/h/.gemini/oauth_creds.json",
            "/h/.gemini/antigravity-cli/antigravity-oauth-token",
            "/h/.gemini/antigravity-cli/antigravity-oauth-token.json",
            "/h/.cursor/chats/abc/def/store.db",
            "/h/.cursor/chats/abc/def/store.db-wal",
            "/h/x/server.pem",
            "/h/x/my_token.txt",
            "/h/x/aws-credentials",
            "/h/Library/Keychains/login.keychain-db",
            "/h/.claude/history.jsonl",
            "/h/.gemini/antigravity-cli/history.jsonl",
        ];
        for p in refused {
            assert!(never_open(Path::new(p)).is_some(), "{p} must be refused");
        }
        let allowed = [
            "/h/.claude/sessions/123.json",
            "/h/.claude/teams/session-ab/config.json",
            "/h/.claude/teams/session-ab/inboxes/team-lead.json",
            "/h/.claude/projects/-a-b/0e2c.jsonl",
            "/h/.claude/jobs/04b2b152/state.json",
            "/h/.claude/daemon/roster.json",
            "/h/.cursor/chats/abc/def/meta.json",
            "/h/.codex/state_5.sqlite",
            "/h/x/.lock",
        ];
        for p in allowed {
            assert_eq!(never_open(Path::new(p)), None, "{p} must be readable");
        }
    }

    #[test]
    fn refusal_happens_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("77.abcdef.key");
        std::fs::write(&key, b"secret").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o000)).unwrap();
        let root = Root::new(dir.path()).unwrap();
        reset_stats();
        let r = open_in(&root, &key);
        assert_eq!(r.unwrap_err(), ReadError::Refused(NeverOpen::KeyFile));
        let s = stats();
        assert_eq!(s.eacces, 0, "no open() was attempted on the key file");
        assert_eq!(s.refused, 1);
    }

    #[test]
    fn symlinks_fifos_and_escapes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("t.json");
        std::fs::write(&target, b"{}").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("link.json")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("sub/escape")).unwrap();
        let root = Root::new(dir.path()).unwrap();
        assert_eq!(open_in(&root, &dir.path().join("link.json")).unwrap_err(), ReadError::Symlink);
        assert_eq!(
            open_in(&root, &dir.path().join("sub/escape/t.json")).unwrap_err(),
            ReadError::OutsideRoot
        );
        let fifo = dir.path().join("f.json");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(open_in(&root, &fifo).unwrap_err(), ReadError::NotRegular);
        assert_eq!(open_in(&root, &dir.path().join("sub")).unwrap_err(), ReadError::NotRegular);
    }

    #[test]
    fn caps_and_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.json");
        std::fs::write(&f, vec![b'x'; 1000]).unwrap();
        let root = Root::new(dir.path()).unwrap();
        assert!(matches!(read_file(&root, &f, 999), Err(ReadError::TooLarge { size: 1000 })));
        let (bytes, id) = read_file(&root, &f, 1000).unwrap();
        assert_eq!(bytes.len(), 1000);
        assert_eq!(id.size, 1000);
        let o = open_in(&root, &f).unwrap();
        assert_eq!(o.read_range(990, 64).unwrap().len(), 10);
    }

    #[test]
    fn permission_denied_is_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("r.json");
        std::fs::write(&f, b"{}").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
        let root = Root::new(dir.path()).unwrap();
        assert_eq!(open_in(&root, &f).unwrap_err(), ReadError::PermissionDenied);
    }

    #[test]
    fn backoff_skips_after_three_failures() {
        let mut b = Backoff::default();
        let p = Path::new("/x");
        assert!(!b.fail(p, 0));
        assert!(!b.fail(p, 0));
        assert!(b.fail(p, 0));
        assert!(b.skipping(p, 1000));
        assert!(!b.skipping(p, 61_000));
        assert_eq!(b.skipped_total, 1);
    }

    /// A34.3: a hard link with an innocent name to a never-open file is
    /// refused after `open` (the kernel reports the innocent name, so the
    /// link count decides), its bytes never read; a single-link file opens.
    #[test]
    fn hard_links_are_refused_after_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::new(dir.path()).unwrap();
        let cred = dir.path().join(".credentials.json");
        std::fs::write(&cred, b"{\"token\":\"canary\"}").unwrap();
        std::fs::create_dir_all(dir.path().join("teams/x")).unwrap();
        let innocent = dir.path().join("teams/x/config.json");
        std::fs::hard_link(&cred, &innocent).unwrap();
        let before = stats().opened;
        assert_eq!(open_in(&root, &innocent).unwrap_err(), ReadError::Refused(NeverOpen::HardLink));
        assert!(matches!(read_file(&root, &innocent, 1 << 20), Err(ReadError::Refused(NeverOpen::HardLink))));
        assert_eq!(stats().opened, before, "never counted as opened");
        // `*.key` through a `.json` name.
        let key = dir.path().join("4242.key");
        std::fs::write(&key, b"0a1b").unwrap();
        let json = dir.path().join("4242.json");
        std::fs::hard_link(&key, &json).unwrap();
        assert_eq!(open_in(&root, &json).unwrap_err(), ReadError::Refused(NeverOpen::HardLink));
        // Control: an ordinary file opens.
        let ok = dir.path().join("teams/x/ok.json");
        std::fs::write(&ok, b"{}").unwrap();
        assert!(open_in(&root, &ok).is_ok());
    }
}
