//! T21 compat fixtures (contract §14.1 "compat fixtures", §14.2 T-compat-*).
//!
//! Synthesized fake "claude binaries": Mach-O-shaped files that carry all,
//! some or none of the pinned canary entries, plus the cases the canary must
//! treat as `Unknown` (unreadable, not Mach-O, not a regular file, too slow)
//! and the swappable pair for T-compat-binarySwap [A1].
//!
//! Nothing here is executable and nothing is ever run. `--version` is
//! simulated by [`fake_version`], which reads the `FAKE_VERSION=` marker the
//! builder writes into the file — the stand-in for T09's `<realpath>
//! --version` (no vendor CLI in any test, §15.1).
//!
//! The script snippets are stand-ins written for these tests from the
//! literals R6 §6 A-P5 recorded (`probes/misc/ap5-canary.tsv`). The 2.1.306
//! team-store stand-in uses the minified names that build really had
//! (`F$r`, `_Lr`, `y0o`); the 2.1.307 stand-in uses other, invented names with
//! the same shapes — the names that build used were not recorded, only that
//! the 2.1.306 ones were gone.

#![allow(dead_code)]

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gilde_code_lib::compat::canary::fingerprint_of;
use gilde_code_lib::contract::common::AbsPath;
use gilde_code_lib::contract::session::BinaryRef;

/// A thin arm64 Mach-O header (`MH_MAGIC_64`, `CPU_TYPE_ARM64`, `MH_EXECUTE`),
/// no load commands. Enough for the canary's input check; not loadable.
pub const MACHO_ARM64_HEADER: [u8; 32] = [
    0xcf, 0xfa, 0xed, 0xfe, // magic (LE)
    0x0c, 0x00, 0x00, 0x01, // cputype arm64
    0x00, 0x00, 0x00, 0x00, // cpusubtype
    0x02, 0x00, 0x00, 0x00, // filetype MH_EXECUTE
    0x00, 0x00, 0x00, 0x00, // ncmds
    0x00, 0x00, 0x00, 0x00, // sizeofcmds
    0x00, 0x00, 0x00, 0x00, // flags
    0x00, 0x00, 0x00, 0x00, // reserved
];

/// The conversation protocol and argv literals, as a bundled CLI spells them.
pub const CONVERSATION_JS: &str = concat!(
    r#"if(q.type==="control_request"&&q.request.subtype==="can_use_tool")"#,
    r#"{let r=q.request;r.permission_suggestions??=[];}"#,
    r#"W.push({type:"control_cancel_request",request_id:z});"#,
    r#"Y.pending_permission_requests=[...K.values()];"#,
    r#".option("--permission-prompt-tool <tool>","MCP tool to use for permission prompts")"#,
    r#".option("--include-partial-messages","Include partial message chunks")"#,
    r#".option("--replay-user-messages","Re-emit user messages from stdin back on stdout")"#,
);

/// The tmux argv and teammate literals of the pane-hosting surface (§7.4.4).
pub const PANE_HOSTING_JS: &str = concat!(
    r#"let S="claude-swarm",V="swarm-view";"#,
    r#"await t(["has-session","-t",S]);"#,
    r##"await t(["split-window","-d","-t",P,"-h","-P","-F","#{pane_id}"]);"##,
    r#"await t(["set-option","-p","-t",P,"remain-on-exit","failed"]);"#,
    r#"await t(["respawn-pane","-k","-t",P,"--",C]);"#,
    r##"await t(["kill-pane","-t",P]);await t(["list-panes","-t",S,"-F","#{pane_id}"]);"##,
    r#"a.push("--agent-id",I,"--parent-session-id",L);if(m==="in-process")return;"#,
);

/// Team-store code of 2.1.306: the shapes with the build's real minified
/// names, plus the task reset (R6 §6 A-P5).
pub const TEAM_STORE_306_JS: &str = concat!(
    "function F$r(){if(!z())return;return}",
    "async function _Lr(e){if(!gs())return[];return[]}",
    "var y0o=1000;var g=5000;",
);

/// Team-store code standing in for 2.1.307: same shapes, other names, and
/// none of the 2.1.306 names.
pub const TEAM_STORE_307_JS: &str = concat!(
    "function Kq1(){if(!Hw())return;return}",
    "async function $mA(e){if(!Ub())return[];return[]}",
    "var g=5000;",
);

/// Which parts a fake binary carries.
#[derive(Debug, Clone)]
pub struct Spec {
    pub version: String,
    pub macho: bool,
    pub conversation: bool,
    pub pane_hosting: bool,
    /// `None` = no team-store code at all.
    pub team_store: Option<&'static str>,
    /// Literals scrubbed from the text after assembly (same length, so the
    /// layout does not move).
    pub scrub: Vec<String>,
    /// Filler bytes before the script part (the "runtime" of the image).
    pub filler: usize,
}

impl Spec {
    /// Every set present, 2.1.307-style team store.
    pub fn full(version: &str) -> Spec {
        Spec {
            version: version.to_string(),
            macho: true,
            conversation: true,
            pane_hosting: true,
            team_store: Some(TEAM_STORE_307_JS),
            scrub: Vec::new(),
            filler: 64 << 10,
        }
    }

    /// The Mach-O header and filler only: no pinned entry at all.
    pub fn empty(version: &str) -> Spec {
        Spec {
            conversation: false,
            pane_hosting: false,
            team_store: None,
            ..Spec::full(version)
        }
    }

    pub fn without(mut self, literal: &str) -> Spec {
        self.scrub.push(literal.to_string());
        self
    }

    pub fn team(mut self, js: Option<&'static str>) -> Spec {
        self.team_store = js;
        self
    }

    pub fn not_macho(mut self) -> Spec {
        self.macho = false;
        self
    }

    pub fn filler(mut self, bytes: usize) -> Spec {
        self.filler = bytes;
        self
    }
}

/// Deterministic filler that holds none of the pinned entries: lowercase
/// letters, digits and spaces (no `-`, `_`, `{`, `(`), from a fixed LCG.
pub fn filler_bytes(n: usize, seed: u64) -> Vec<u8> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789    ";
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ALPHABET[((x >> 33) as usize) % ALPHABET.len()]
        })
        .collect()
}

/// The bytes of a fake binary for `spec`.
pub fn fake_bytes(spec: &Spec) -> Vec<u8> {
    let mut out = Vec::new();
    if spec.macho {
        out.extend_from_slice(&MACHO_ARM64_HEADER);
    } else {
        out.extend_from_slice(b"#!/bin/sh\n# launcher\n");
    }
    out.extend_from_slice(format!("\0FAKE_VERSION={}\0", spec.version).as_bytes());
    out.extend_from_slice(&filler_bytes(spec.filler, 0x9e37_79b9));
    let mut script = String::new();
    if spec.conversation {
        script.push_str(CONVERSATION_JS);
    }
    if spec.pane_hosting {
        script.push_str(PANE_HOSTING_JS);
    }
    if let Some(js) = spec.team_store {
        script.push_str(js);
    }
    for lit in &spec.scrub {
        script = script.replace(lit.as_str(), &"#".repeat(lit.len()));
    }
    out.extend_from_slice(script.as_bytes());
    out.extend_from_slice(&filler_bytes(4 << 10, 0x51ed_270b));
    out
}

/// Write a fake binary (0755: shaped like an installed CLI; never executed).
pub fn write_fake(path: &Path, spec: &Spec) -> PathBuf {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).expect("fixture dir");
    }
    fs::write(path, fake_bytes(spec)).expect("write fake binary");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod fake binary");
    path.to_path_buf()
}

/// Overwrite `path` in place (same inode), as `cp new old` does.
pub fn overwrite_in_place(path: &Path, spec: &Spec) {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .expect("open for in-place replace");
    f.write_all(&fake_bytes(spec)).expect("in-place write");
    f.sync_all().ok();
}

/// Replace `path` by rename (new inode), as an installer does.
pub fn replace_by_rename(path: &Path, spec: &Spec) {
    let tmp = path.with_extension("incoming");
    write_fake(&tmp, spec);
    fs::rename(&tmp, path).expect("rename into place");
}

/// Point the symlink `link` at `target` (atomic retarget via rename).
pub fn retarget_symlink(link: &Path, target: &Path) {
    let tmp = link.with_extension("relink");
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).expect("symlink");
    fs::rename(&tmp, link).expect("retarget symlink");
}

/// Give a file a distinct mtime (so two writes within one millisecond still
/// differ in fingerprint).
pub fn set_mtime(path: &Path, secs_after_epoch: u64) {
    let f = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs_after_epoch))
        .expect("set mtime");
}

/// The simulated `<realpath> --version`: the `FAKE_VERSION=` marker.
pub fn fake_version(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let marker = b"FAKE_VERSION=";
    let at = bytes.windows(marker.len()).position(|w| w == marker)? + marker.len();
    let end = bytes[at..].iter().position(|b| *b == 0)? + at;
    String::from_utf8(bytes[at..end].to_vec()).ok()
}

/// What T09's resolver does at spawn (§6.0 *Binary resolution*), without the
/// process: realpath, `stat`, simulated `--version`.
pub fn resolve(path: &Path) -> BinaryRef {
    let real = fs::canonicalize(path).expect("realpath");
    BinaryRef {
        realpath: AbsPath::parse(real.to_string_lossy().to_string()).expect("abs path"),
        fingerprint: fingerprint_of(&real).expect("fingerprint"),
        version: fake_version(&real),
        pidpath: None,
    }
}

/// A `BinaryRef` for a path that may not resolve (unreadable files).
pub fn binary_ref(path: &Path, version: Option<&str>) -> BinaryRef {
    BinaryRef {
        realpath: AbsPath::parse(path.to_string_lossy().to_string()).expect("abs path"),
        fingerprint: fingerprint_of(path).expect("fingerprint"),
        version: version.map(str::to_string),
        pidpath: None,
    }
}

/// Make a file unreadable (mode 000); the returned guard restores 0644 so the
/// temp dir can be removed.
pub struct Unreadable(PathBuf);

impl Unreadable {
    pub fn make(path: &Path) -> io::Result<Unreadable> {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000))?;
        Ok(Unreadable(path.to_path_buf()))
    }
}

impl Drop for Unreadable {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o644));
    }
}

/// A canonical temp dir (macOS `/var` → `/private/var`), so realpaths compare.
pub fn temp_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("gilde-compat-")
        .tempdir()
        .expect("temp dir");
    let real = fs::canonicalize(dir.path()).expect("canonical temp dir");
    (dir, real)
}

/// `claude_compat_observed` as §3.9 declares it (state.db migration v2, T09).
/// Tests create it in an in-memory `Db` until T09's migration is merged.
pub const OBSERVED_DDL: &str = "CREATE TABLE IF NOT EXISTS claude_compat_observed (\
    version TEXT NOT NULL, conversation TEXT, pane_hosting TEXT, \
    source TEXT NOT NULL, binary_fingerprint TEXT, \
    static_result TEXT, handshake_ok INTEGER, features_json TEXT, \
    note TEXT, observed_at INTEGER NOT NULL, \
    PRIMARY KEY (version, binary_fingerprint))";
