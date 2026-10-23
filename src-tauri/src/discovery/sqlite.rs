//! The SQLite rule (contract §5.7, A11.5) [R2 Q7.1]: vendor databases are
//! never opened in place in a mode that can write.
//!
//! * No `-wal` sibling: open the original as
//!   `file:<path>?mode=ro&immutable=1` (no lock, no `-shm`, no journal).
//! * A `-wal` sibling exists: clone the db and its `-wal` (not `-shm`) with
//!   `clonefile` (APFS clone; a bounded copy on other volumes) into
//!   `<app_data>/tmp/vendorcopy/<uuid>/` (dir 0700), query the **copy**, delete
//!   the copy dir (db, `-wal`, `-shm`) in the same call. The copy is opened
//!   `mode=ro` *without* `immutable`: with `immutable=1` SQLite ignores the
//!   `-wal`, so every row still in the WAL would be invisible (measured: 0 of 5
//!   WAL-only rows visible with `immutable=1`, 5 of 5 with `mode=ro`; the test
//!   `immutable_hides_wal_rows_mode_ro_does_not` keeps that measurement). The
//!   only file SQLite may create then is the copy's own `-shm`, inside Gilde's
//!   private temp dir.
//! * Size cap 256 MiB (skip with a diagnostic); `SQLITE_CORRUPT`/`SQLITE_NOTADB`
//!   → one retry after 500 ms, then skip this cycle.
//! * Leftover copies are deleted at boot ([`cleanup_leftovers`]).
//! * **Never-open holds on this path too (A47.2).** The db and every file
//!   cloned pass all of [`safe_read::open_in`]'s checks before SQLite or
//!   `clonefile` sees them. Those checks are the never-open filter on the name
//!   and on the real path of the opened file, no symlink, a regular file under
//!   the root, and **one hard link** (A34.3: a second name may be a never-open
//!   file's). A clone is made from that checked inode by descriptor
//!   (`fclonefileat`), never from a name that may have been swapped since. A
//!   db opened in place counts only if its name still names the checked inode,
//!   with one link, after the query.
//!
//! **The one write site of discovery** (§9.10, A11.5) is [`vendorcopy`]: it
//! creates, clones into and deletes `<app_data>/tmp/vendorcopy/<uuid>/` and
//! nothing else, and every path it is handed is checked at runtime to lie
//! under that root (anything else is refused, nothing is touched).
//! `scripts/check-vendor-writes.mjs` exempts exactly that function; every
//! other write in `discovery/` is rejected (a static test enforces the same
//! here).

use std::ffi::CString;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::discovery::safe_read::{self, EntryKind, FileId, NeverOpen, ReadError, Root};

/// The vendor SQLite size cap (§5.7, §5.8).
pub const SQLITE_CAP: u64 = 256 * 1024 * 1024;
pub const CORRUPT_RETRY: Duration = Duration::from_millis(500);

/// Why a vendor database could not be queried this cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteError {
    TooLarge,
    Corrupt,
    Read(ReadError),
    /// A SQLite error code (never a message: messages can quote content).
    Sqlite(i32),
    CopyFailed,
    /// A path handed to the write site was not under `<app_data>/tmp/vendorcopy/`.
    OutsideCopyRoot,
}

/// `<app_data>/tmp/vendorcopy`.
pub fn vendorcopy_dir(app_data: &Path) -> PathBuf {
    app_data.join("tmp").join("vendorcopy")
}

/// Delete leftover copies (boot, §5.7) — through the one write site.
pub fn cleanup_leftovers(app_data: &Path) {
    let _ = vendorcopy(app_data, CopyOp::Purge);
}

// ---------------------------------------------------------------------------
// never-open on the SQLite path (A34.3, A47.2)
// ---------------------------------------------------------------------------

/// `path` passes every check of [`safe_read::open_in`]. The returned file
/// stays open while it is used, so its inode cannot be reused by another
/// file before [`same_file`] compares against it.
fn vet(root: &Root, path: &Path) -> Result<safe_read::Opened, SqliteError> {
    safe_read::open_in(root, path).map_err(SqliteError::Read)
}

/// Why `found` does not answer for the checked file `want`: another inode
/// now has the name (it changed under us; next cycle), or the file gained
/// a second name (A34.3).
fn same_identity(found: &std::fs::Metadata, want: &FileId) -> Result<(), SqliteError> {
    if !found.is_file() {
        return Err(SqliteError::Read(ReadError::NotRegular));
    }
    if found.dev() != want.dev || found.ino() != want.inode {
        return Err(SqliteError::Read(ReadError::Transient(libc::EAGAIN)));
    }
    if found.nlink() > 1 {
        return Err(SqliteError::Read(ReadError::Refused(NeverOpen::HardLink)));
    }
    Ok(())
}

/// `path` still names the checked file `want`, with one link (an `lstat`,
/// no open).
fn same_file(path: &Path, want: &FileId) -> Result<(), SqliteError> {
    let found = std::fs::symlink_metadata(path).map_err(|e| SqliteError::Read(ReadError::from_io(&e)))?;
    same_identity(&found, want)
}

/// A second read-only descriptor of the checked file `want`, opened by
/// `path` (no symlink, no blocking) and accepted only when it is the same
/// inode with one link. A file swapped in since the check is closed before
/// any byte is read, as `open_in` does.
fn reopen_same(path: &Path, want: &FileId) -> Result<std::fs::File, SqliteError> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| SqliteError::Read(ReadError::NotFound))?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let fd = loop {
        // SAFETY: `c` is a valid NUL-terminated C string for the call's duration.
        let fd = unsafe { libc::open(c.as_ptr(), flags) };
        if fd >= 0 {
            break fd;
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(SqliteError::Read(ReadError::from_io(&e)));
        }
    };
    // SAFETY: `fd` was just returned by a successful `open` and is owned here.
    let f = unsafe { std::fs::File::from_raw_fd(fd) };
    let found = f.metadata().map_err(|e| SqliteError::Read(ReadError::from_io(&e)))?;
    same_identity(&found, want)?;
    Ok(f)
}

// ---------------------------------------------------------------------------
// the one write site of discovery (A11.5)
// ---------------------------------------------------------------------------

/// What [`vendorcopy`] does.
pub enum CopyOp<'a> {
    /// Create `<vendorcopy>/<uuid>/` (0700) and clone `files` (regular files
    /// under `root`) into it by name; returns the new dir. On any failure the
    /// partial dir is deleted before the error returns.
    Clone { root: &'a Root, files: &'a [&'a Path] },
    /// Delete one copy dir (db, `-wal`, `-shm`).
    Delete { dir: &'a Path },
    /// Delete every copy dir (boot).
    Purge,
}

/// `p` is `<base>/<uuid>` or `<base>/<uuid>/<file>`: no `..`, no root or
/// prefix components, the first component a UUID, at most one level below it.
fn under_copy_root(base: &Path, p: &Path) -> bool {
    let Ok(rest) = p.strip_prefix(base) else { return false };
    let comps: Vec<Component<'_>> = rest.components().collect();
    if comps.is_empty() || comps.len() > 2 || !comps.iter().all(|c| matches!(c, Component::Normal(_))) {
        return false;
    }
    comps[0].as_os_str().to_str().is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
}

/// **The one write/delete site of discovery** (§9.10, A11.5). It touches only
/// `<app_data>/tmp/vendorcopy/<uuid>/`; every path is checked at runtime to lie
/// under that root, and a path that does not is refused before any effect.
pub fn vendorcopy(app_data: &Path, op: CopyOp<'_>) -> Result<Option<PathBuf>, SqliteError> {
    use std::os::unix::fs::DirBuilderExt;
    let base = vendorcopy_dir(app_data);
    match op {
        CopyOp::Clone { root, files } => {
            let dir = base.join(uuid::Uuid::new_v4().to_string());
            if !under_copy_root(&base, &dir) {
                return Err(SqliteError::OutsideCopyRoot);
            }
            let mut targets = Vec::with_capacity(files.len());
            for src in files {
                let dst = dir.join(src.file_name().ok_or(SqliteError::CopyFailed)?);
                if !under_copy_root(&base, &dst) {
                    return Err(SqliteError::OutsideCopyRoot);
                }
                targets.push((*src, dst));
            }
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&base)
                .map_err(|_| SqliteError::CopyFailed)?;
            std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|_| SqliteError::CopyFailed)?;
            let cloned = targets.iter().try_for_each(|(src, dst)| clone_file(root, src, dst));
            match cloned {
                Ok(()) => Ok(Some(dir)),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    Err(e)
                }
            }
        }
        CopyOp::Delete { dir } => {
            if !under_copy_root(&base, dir) || dir.components().count() != base.components().count() + 1 {
                return Err(SqliteError::OutsideCopyRoot);
            }
            let _ = std::fs::remove_dir_all(dir);
            Ok(None)
        }
        CopyOp::Purge => {
            let Ok(rd) = std::fs::read_dir(&base) else { return Ok(None) };
            for e in rd.flatten() {
                let p = e.path();
                if under_copy_root(&base, &p) {
                    let _ = std::fs::remove_dir_all(&p);
                }
            }
            Ok(None)
        }
    }
}

/// Clone `src` to `dst` from the checked file itself (A47.2). `src` first
/// passes [`vet`], so a never-open name, a hard link or a symlink is refused
/// before anything is cloned. The APFS clone is then made by descriptor
/// (`fclonefileat`) from that same inode, so a name swapped since the check
/// is never what gets cloned. On other volumes the checked file's bytes are
/// read (bounded) and written. Only called by [`vendorcopy`] with a checked
/// `dst`.
fn clone_file(root: &Root, src: &Path, dst: &Path) -> Result<(), SqliteError> {
    let f = vet(root, src)?;
    let same = reopen_same(src, &f.id)?;
    let d = CString::new(dst.as_os_str().as_bytes()).map_err(|_| SqliteError::CopyFailed)?;
    // SAFETY: `same` is an open descriptor for the call's duration and `d` a
    // valid C string; fclonefileat reads that file and creates `dst`.
    if unsafe { libc::fclonefileat(same.as_raw_fd(), libc::AT_FDCWD, d.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    drop(same);
    let bytes = f.read_all_capped(SQLITE_CAP).map_err(|e| match e {
        ReadError::TooLarge { .. } => SqliteError::TooLarge,
        other => SqliteError::Read(other),
    })?;
    std::fs::write(dst, &*bytes).map_err(|_| SqliteError::CopyFailed)
}
// -- end of the one write site ------------------------------------------------

/// A private copy dir, deleted (through [`vendorcopy`]) on drop.
struct CopyDir<'a> {
    app_data: &'a Path,
    dir: PathBuf,
}

impl Drop for CopyDir<'_> {
    fn drop(&mut self) {
        let _ = vendorcopy(self.app_data, CopyOp::Delete { dir: &self.dir });
    }
}

fn uri_for(path: &Path, query: &str) -> Option<String> {
    let url = url::Url::from_file_path(path).ok()?;
    Some(format!("{}?{query}", url.as_str()))
}

fn sqlite_code(e: &rusqlite::Error) -> i32 {
    match e {
        rusqlite::Error::SqliteFailure(f, _) => f.extended_code,
        _ => -1,
    }
}

fn is_corrupt(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::DatabaseCorrupt || f.code == rusqlite::ErrorCode::NotADatabase
    )
}

const RO_FLAGS: OpenFlags = OpenFlags::SQLITE_OPEN_READ_ONLY
    .union(OpenFlags::SQLITE_OPEN_URI)
    .union(OpenFlags::SQLITE_OPEN_NO_MUTEX);

/// Run `query` against the vendor database `db` (which must lie under `root`)
/// following the SQLite rule.
pub fn with_vendor_db<T>(
    root: &Root,
    db: &Path,
    app_data: &Path,
    query: impl Fn(&Connection) -> rusqlite::Result<T>,
) -> Result<T, SqliteError> {
    // A47.2: every check of `safe_read::open_in` before SQLite sees the file
    // (never-open name and real path, no symlink, regular, under the root, one
    // hard link). `vetted` stays open until the answer is checked against it.
    let vetted = vet(root, db)?;
    let real = std::fs::canonicalize(db).map_err(|e| SqliteError::Read(ReadError::from_io(&e)))?;
    if !root.contains(&real) {
        return Err(SqliteError::Read(ReadError::OutsideRoot));
    }
    if let Some(why) = safe_read::never_open(&real) {
        return Err(SqliteError::Read(ReadError::Refused(why)));
    }
    same_file(&real, &vetted.id)?;
    if vetted.id.size > SQLITE_CAP {
        return Err(SqliteError::TooLarge);
    }
    let mut wal = real.clone().into_os_string();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    let has_wal = safe_read::lstat(&wal).is_ok_and(|w| w.kind == EntryKind::File);

    let run_once = || -> Result<T, SqliteError> {
        let opened = if has_wal {
            let files: [&Path; 2] = [&real, &wal];
            let dir = vendorcopy(app_data, CopyOp::Clone { root, files: &files })?.ok_or(SqliteError::CopyFailed)?;
            let copy = CopyDir { app_data, dir };
            let cdb = copy.dir.join(real.file_name().ok_or(SqliteError::CopyFailed)?);
            let uri = uri_for(&cdb, "mode=ro").ok_or(SqliteError::CopyFailed)?;
            let conn = Connection::open_with_flags(uri, RO_FLAGS);
            let out = conn.and_then(|c| query(&c));
            // The connection is closed before `copy` drops (the dir, -shm
            // included, is deleted in the same call).
            drop(copy);
            out
        } else {
            let uri = uri_for(&real, "mode=ro&immutable=1").ok_or(SqliteError::CopyFailed)?;
            Connection::open_with_flags(uri, RO_FLAGS).and_then(|c| query(&c))
        };
        opened.map_err(|e| if is_corrupt(&e) { SqliteError::Corrupt } else { SqliteError::Sqlite(sqlite_code(&e)) })
    };
    let answer = match run_once() {
        Err(SqliteError::Corrupt) => {
            std::thread::sleep(CORRUPT_RETRY);
            run_once()
        }
        other => other,
    };
    // A47.2: an answer counts only if `real` still names the checked file
    // with one link (SQLite opened the name, not our descriptor).
    same_file(&real, &vetted.id)?;
    drop(vetted);
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal_db(dir: &Path) -> (PathBuf, Connection) {
        let db = dir.join("state_5.sqlite");
        let w = Connection::open(&db).unwrap();
        w.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(x);").unwrap();
        w.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        for i in 0..5 {
            w.execute("INSERT INTO t VALUES (?1)", [i]).unwrap();
        }
        (db, w)
    }

    /// The A11.5 measurement: on a copy of a db + `-wal`, `immutable=1` sees
    /// none of the WAL-only rows, `mode=ro` sees all of them.
    #[test]
    fn immutable_hides_wal_rows_mode_ro_does_not() {
        let vendor = tempfile::tempdir().unwrap();
        let (db, w) = wal_db(vendor.path());
        let copy = tempfile::tempdir().unwrap();
        std::fs::copy(&db, copy.path().join("c.db")).unwrap();
        std::fs::copy(vendor.path().join("state_5.sqlite-wal"), copy.path().join("c.db-wal")).unwrap();
        let count = |q: &str| -> i64 {
            let c = Connection::open_with_flags(uri_for(&copy.path().join("c.db"), q).unwrap(), RO_FLAGS).unwrap();
            c.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(count("immutable=1"), 0, "immutable=1 ignores the -wal");
        assert_eq!(count("mode=ro"), 5);
        drop(w);
    }

    #[test]
    fn wal_rows_are_read_from_a_private_copy_and_the_dir_goes_with_its_shm() {
        let vendor = tempfile::tempdir().unwrap();
        let app = tempfile::tempdir().unwrap();
        let (db, w) = wal_db(vendor.path());
        let before: Vec<_> = std::fs::read_dir(vendor.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        let root = Root::new(vendor.path()).unwrap();
        let base = vendorcopy_dir(app.path());
        let (n, seen): (i64, Vec<String>) = with_vendor_db(&root, &db, app.path(), |c| {
            let n = c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?;
            let dirs: Vec<_> = std::fs::read_dir(&base).unwrap().map(|e| e.unwrap().path()).collect();
            assert_eq!(dirs.len(), 1, "one private copy dir");
            let mut files: Vec<String> = std::fs::read_dir(&dirs[0]).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            files.sort();
            Ok((n, files))
        })
        .unwrap();
        assert_eq!(n, 5, "rows that live only in the WAL are visible");
        assert!(seen.iter().any(|f| f.ends_with("-shm")), "SQLite made the copy's own -shm: {seen:?}");
        // The clone itself holds exactly the db and its -wal (the vendor -shm is never cloned).
        let wal = vendor.path().join("state_5.sqlite-wal");
        let files: [&Path; 2] = [&db, &wal];
        let dir = vendorcopy(app.path(), CopyOp::Clone { root: &root, files: &files }).unwrap().unwrap();
        let mut cloned: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        cloned.sort();
        assert_eq!(cloned, vec!["state_5.sqlite".to_string(), "state_5.sqlite-wal".to_string()]);
        vendorcopy(app.path(), CopyOp::Delete { dir: &dir }).unwrap();
        let after: Vec<_> = std::fs::read_dir(vendor.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(before.len(), after.len(), "nothing created next to the vendor db");
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0, "the copy dir (db, -wal, -shm) is deleted in the same call");
        drop(w);
    }

    #[test]
    fn the_write_site_refuses_paths_outside_its_root() {
        let app = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        std::fs::write(victim.path().join("keep"), b"x").unwrap();
        let base = vendorcopy_dir(app.path());
        for bad in [
            victim.path().to_path_buf(),
            base.clone(),
            base.join("not-a-uuid"),
            base.join(format!("{}/../..", uuid::Uuid::new_v4())),
            base.join(format!("{}/x/y", uuid::Uuid::new_v4())),
        ] {
            assert_eq!(vendorcopy(app.path(), CopyOp::Delete { dir: &bad }), Err(SqliteError::OutsideCopyRoot), "{bad:?}");
        }
        assert!(victim.path().join("keep").exists());
        // Boot purge removes only copy dirs.
        std::fs::create_dir_all(base.join(uuid::Uuid::new_v4().to_string())).unwrap();
        std::fs::create_dir_all(base.join("other")).unwrap();
        cleanup_leftovers(app.path());
        let left: Vec<_> = std::fs::read_dir(&base).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("other")]);
    }

    #[test]
    fn no_wal_opens_immutable_and_caps_and_never_open() {
        let vendor = tempfile::tempdir().unwrap();
        let app = tempfile::tempdir().unwrap();
        let db = vendor.path().join("conversation_summaries.db");
        {
            let w = Connection::open(&db).unwrap();
            w.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);").unwrap();
        }
        let root = Root::new(vendor.path()).unwrap();
        let n: i64 = with_vendor_db(&root, &db, app.path(), |c| c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))).unwrap();
        assert_eq!(n, 1);
        assert!(!vendorcopy_dir(app.path()).exists(), "no copy without a -wal");
        let store = vendor.path().join("store.db");
        std::fs::write(&store, b"x").unwrap();
        assert!(matches!(with_vendor_db(&root, &store, app.path(), |_| Ok(())), Err(SqliteError::Read(ReadError::Refused(_)))));
        let junk = vendor.path().join("junk.db");
        std::fs::write(&junk, vec![7u8; 4096]).unwrap();
        assert!(matches!(
            with_vendor_db(&root, &junk, app.path(), |c| c.query_row("SELECT 1 FROM t", [], |r| r.get::<_, i64>(0))),
            Err(SqliteError::Corrupt) | Err(SqliteError::Sqlite(_))
        ));
    }

    // -- A47.2 (G7c-18): never-open on the SQLite path ------------------------

    const CANARY: &[u8] = b"G7C-SECRET-CANARY";

    /// A never-open file (by name) under a fresh vendor root.
    fn secret_in(vendor: &Path) -> PathBuf {
        let secret = vendor.join("auth-token.json");
        std::fs::write(&secret, b"{\"access_token\":\"G7C-SECRET-CANARY\"}").unwrap();
        secret
    }

    /// Whether any file of any copy dir holds the canary.
    fn canary_copied(app: &Path) -> bool {
        let Ok(dirs) = std::fs::read_dir(vendorcopy_dir(app)) else { return false };
        dirs.flatten().any(|d| {
            std::fs::read_dir(d.path()).into_iter().flatten().flatten().any(|f| {
                std::fs::read(f.path()).is_ok_and(|b| b.windows(CANARY.len()).any(|w| w == CANARY))
            })
        })
    }

    fn copy_dirs(app: &Path) -> usize {
        std::fs::read_dir(vendorcopy_dir(app)).map(|r| r.count()).unwrap_or(0)
    }

    /// The reviewer's G7c-18 test, ported: a hard link named `state_5.sqlite`
    /// to a never-open file is refused before SQLite opens it, in place and
    /// with a `-wal` sibling (where its bytes were cloned into Gilde's tmp dir).
    #[test]
    fn g7c_18_a_hard_linked_never_open_file_is_never_opened_as_a_vendor_database() {
        let vendor = tempfile::tempdir().unwrap();
        let vendor = std::fs::canonicalize(vendor.path()).unwrap();
        let app = tempfile::tempdir().unwrap();
        let secret = secret_in(&vendor);
        let db = vendor.join("state_5.sqlite");
        std::fs::hard_link(&secret, &db).unwrap();
        let root = Root::new(&vendor).unwrap();
        let ran = std::sync::atomic::AtomicBool::new(false);
        let query = |_: &Connection| {
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(0i64)
        };

        // No -wal: SQLite would open the file in place.
        let r = with_vendor_db(&root, &db, app.path(), query);
        assert_eq!(r, Err(SqliteError::Read(ReadError::Refused(NeverOpen::HardLink))));
        // With a -wal sibling: the bytes would be cloned first.
        std::fs::write(vendor.join("state_5.sqlite-wal"), b"").unwrap();
        let r = with_vendor_db(&root, &db, app.path(), query);
        assert_eq!(r, Err(SqliteError::Read(ReadError::Refused(NeverOpen::HardLink))));
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst), "no query ran on it");
        assert!(!canary_copied(app.path()), "the never-open file's bytes were never cloned");
        assert_eq!(copy_dirs(app.path()), 0, "no copy dir was made for it");
    }

    /// Variant: the `-wal` sibling is the hard link (the db itself is a real
    /// one): the clone is refused and nothing of the secret is left behind.
    #[test]
    fn a47_2_a_hard_linked_wal_sibling_is_never_cloned() {
        let vendor = tempfile::tempdir().unwrap();
        let app = tempfile::tempdir().unwrap();
        let (db, w) = wal_db(vendor.path());
        drop(w);
        let wal = vendor.path().join("state_5.sqlite-wal");
        let _ = std::fs::remove_file(&wal);
        std::fs::hard_link(secret_in(vendor.path()), &wal).unwrap();
        let root = Root::new(vendor.path()).unwrap();
        let r = with_vendor_db(&root, &db, app.path(), |c| c.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0)));
        assert_eq!(r, Err(SqliteError::Read(ReadError::Refused(NeverOpen::HardLink))));
        assert!(!canary_copied(app.path()));
        assert_eq!(copy_dirs(app.path()), 0, "the partial copy dir (with the db already cloned) is deleted");
        // The write site refuses it on its own too.
        let files: [&Path; 1] = [&wal];
        let e = vendorcopy(app.path(), CopyOp::Clone { root: &root, files: &files }).unwrap_err();
        assert_eq!(e, SqliteError::Read(ReadError::Refused(NeverOpen::HardLink)));
        assert_eq!(copy_dirs(app.path()), 0);
    }

    /// Variant: a hard-linked `-shm` sibling is never opened or cloned; the
    /// query reads its private copy (db + `-wal`) as before.
    #[test]
    fn a47_2_a_hard_linked_shm_sibling_is_never_cloned() {
        let vendor = tempfile::tempdir().unwrap();
        let app = tempfile::tempdir().unwrap();
        let (db, w) = wal_db(vendor.path());
        let shm = vendor.path().join("state_5.sqlite-shm");
        let secret = secret_in(vendor.path());
        // Kept open by the writer `w`: the db and -wal stay; the -shm name is
        // replaced by a second name of the secret.
        let _ = std::fs::remove_file(&shm);
        std::fs::hard_link(&secret, &shm).unwrap();
        let root = Root::new(vendor.path()).unwrap();
        let app_path = app.path().to_path_buf();
        let n = with_vendor_db(&root, &db, app.path(), |c| {
            assert!(!canary_copied(&app_path), "the copy holds no byte of the secret");
            c.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
        })
        .unwrap();
        assert_eq!(n, 5, "WAL rows are still read from the private copy");
        assert_eq!(copy_dirs(app.path()), 0);
        drop(w);
    }

    /// The identity checks behind the in-place open and the clone: a name
    /// that now names another inode, or a file that gained a second name, is
    /// never taken for the checked file.
    #[test]
    fn a47_2_a_name_swapped_or_linked_after_the_check_does_not_answer() {
        let vendor = tempfile::tempdir().unwrap();
        let vendor = std::fs::canonicalize(vendor.path()).unwrap();
        let root = Root::new(&vendor).unwrap();
        let db = vendor.join("state_5.sqlite");
        std::fs::write(&db, b"db").unwrap();
        let checked = vet(&root, &db).unwrap();
        assert_eq!(same_file(&db, &checked.id), Ok(()));
        assert!(reopen_same(&db, &checked.id).is_ok());

        // Another file renamed over the name (the checked inode stays open).
        let other = vendor.join("other");
        std::fs::write(&other, b"other").unwrap();
        std::fs::rename(&other, &db).unwrap();
        assert_eq!(same_file(&db, &checked.id), Err(SqliteError::Read(ReadError::Transient(libc::EAGAIN))));
        assert_eq!(reopen_same(&db, &checked.id).unwrap_err(), SqliteError::Read(ReadError::Transient(libc::EAGAIN)));

        // The name now has a second name: refused as a hard link.
        let now = vet(&root, &db).unwrap();
        std::fs::hard_link(&db, vendor.join("second")).unwrap();
        assert_eq!(same_file(&db, &now.id), Err(SqliteError::Read(ReadError::Refused(NeverOpen::HardLink))));
        assert_eq!(reopen_same(&db, &now.id).unwrap_err(), SqliteError::Read(ReadError::Refused(NeverOpen::HardLink)));
        assert_eq!(vet(&root, &db).unwrap_err(), SqliteError::Read(ReadError::Refused(NeverOpen::HardLink)));
    }
}
