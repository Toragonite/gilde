//! The one vendor-CLI classifier (contract §5.2.2, §9.8; A39.1(a)).
//!
//! "Is this process a vendor CLI, and which one?" is answered here and only
//! here, from the exec path the kernel reports and, when readable, the argv.
//! Every caller asks this module: the supervisor's kill policy and quit
//! survivor recording (`supervise::procscan`, `supervise::kill`), the
//! registry's `vendor` column (`supervise::registry`), discovery's process
//! scan (`discovery::procs`) and the PTY signature key (`pty::manager`).
//!
//! The module is a leaf: it uses nothing but the contract's [`Vendor`], so
//! `supervise` and `discovery` can both depend on it without a cycle.
//!
//! It knows four shapes:
//!
//! 1. **The binary's basename**: `claude`, `claude.exe`, `codex`,
//!    `cursor-agent`, `agy`; `agent` only under a cursor install directory;
//!    codex's native `codex-<arch>-apple-darwin`.
//! 2. **The native installer**: `~/.local/bin/claude` resolves to
//!    `…/share/claude/versions/<semver>`, a binary named by its version. Gilde
//!    spawns the resolved path, so that is the exec path the kernel reports.
//!    Any `…/claude/versions/<semver>` matches (a relocated data dir or a
//!    symlinked `share` resolves elsewhere).
//! 3. **`node`/`bun` launches**: leading `-` flags are skipped (with the one
//!    word after a bare flag, which may be its value), a `bun run`/`x`/`exec`
//!    word too; the first other word is the script. It is a vendor's when its
//!    basename is a vendor CLI name (an npm bin link such as `…/bin/claude`) or
//!    its path names the package (`@anthropic-ai/claude-code`,
//!    `@openai/codex`, a `cursor-agent` directory such as
//!    `cursor-agent/versions/<v>/index.js`, `antigravity`). Inline code
//!    (`-e`, `-p`) has no script.
//! 4. **A vendored runtime with no visible script**: cursor-agent's launcher
//!    runs `exec -a "$0" "$SCRIPT_DIR/node" --use-system-ca
//!    "$SCRIPT_DIR/index.js"`, so the exec path is `…/cursor-agent/versions/
//!    <v>/node`. With its `index.js` visible, shape 3 answers; when the argv
//!    cannot be read (or shows only flags) that `node` is still cursor's, since
//!    nothing else runs it. If a different script is visible, it is not.
//!
//! A plain `node` or `bun` running anything else is not a vendor CLI.

use crate::contract::common::Vendor;

/// A recognised vendor CLI and where its own arguments begin in the argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorLaunch {
    pub vendor: Vendor,
    /// Index of the first argv word that belongs to the CLI itself: 1 for a
    /// binary, the script's index + 1 under `node`/`bun`. Never more than
    /// `argv.len()`, so `&argv[args_from..]` is always in bounds.
    pub args_from: usize,
}

/// Which vendor CLI a process is, from its exec path and its argv (`argv[0]`
/// included, as the kernel reports it; empty when it could not be read).
pub fn vendor_of_process<S: AsRef<str>>(exec_path: &str, argv: &[S]) -> Option<Vendor> {
    vendor_launch(exec_path, argv).map(|l| l.vendor)
}

/// [`vendor_of_process`] when only the exec path is known.
pub fn vendor_of_exec_path(exec_path: &str) -> Option<Vendor> {
    vendor_of_process::<&str>(exec_path, &[])
}

/// [`vendor_of_process`], plus where the CLI's own arguments begin (discovery
/// reads its label flags and subcommand from there).
pub fn vendor_launch<S: AsRef<str>>(exec_path: &str, argv: &[S]) -> Option<VendorLaunch> {
    if let Some(vendor) = vendor_of_binary(exec_path) {
        return Some(VendorLaunch { vendor, args_from: argv.len().min(1) });
    }
    let runtime = match basename(exec_path) {
        "node" => Runtime::Node,
        "bun" => Runtime::Bun,
        _ => return None,
    };
    match script_of(runtime, argv) {
        Script::Vendor(vendor, at) => Some(VendorLaunch { vendor, args_from: at + 1 }),
        Script::Other => None,
        Script::NotVisible => {
            vendored_runtime(exec_path).map(|vendor| VendorLaunch { vendor, args_from: argv.len() })
        }
    }
}

/// The first vendor CLI a shell command string runs (a hosted pane's
/// `/bin/sh -c <command>`): each word is tried as a program with the words
/// after it as its argv. `None` when the string does not parse.
pub fn vendor_of_command(command: &str) -> Option<Vendor> {
    let words = shell_words::split(command).ok()?;
    (0..words.len()).find_map(|i| vendor_of_process(&words[i], &words[i..]))
}

// ---------------------------------------------------------------------------
// shapes 1 and 2: the executable itself
// ---------------------------------------------------------------------------

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The vendor whose CLI is called `name` (a basename).
fn vendor_of_name(name: &str) -> Option<Vendor> {
    match name {
        "claude" | "claude.exe" => Some(Vendor::Claude),
        "codex" => Some(Vendor::Codex),
        "cursor-agent" => Some(Vendor::Cursor),
        "agy" => Some(Vendor::Agy),
        _ => None,
    }
}

fn vendor_of_binary(exec_path: &str) -> Option<Vendor> {
    let base = basename(exec_path);
    if let Some(vendor) = vendor_of_name(base) {
        return Some(vendor);
    }
    if base == "agent" && is_under_cursor_dir(exec_path) {
        return Some(Vendor::Cursor);
    }
    // codex's npm wrapper runs its native binary, named by target triple in
    // older packages (`bin/codex-aarch64-apple-darwin`).
    if base.starts_with("codex-") && base.ends_with("-apple-darwin") {
        return Some(Vendor::Codex);
    }
    if is_native_claude(exec_path) {
        return Some(Vendor::Claude);
    }
    None
}

/// The directories of `path` (its basename excluded), last first.
fn dirs_rev(path: &str) -> impl Iterator<Item = &str> {
    path.rsplit('/').skip(1)
}

/// `agent` is cursor's only under a cursor install directory
/// (`~/.local/share/cursor-agent/…`, `Cursor.app`, `~/.cursor/…`).
fn is_under_cursor_dir(path: &str) -> bool {
    dirs_rev(path).any(|d| d.to_ascii_lowercase().contains("cursor"))
}

/// `…/claude/versions/<semver>` (the native installer's layout).
fn is_native_claude(exec_path: &str) -> bool {
    let mut dirs = dirs_rev(exec_path);
    is_semver(basename(exec_path)) && dirs.next() == Some("versions") && dirs.next() == Some("claude")
}

/// `MAJOR.MINOR.PATCH`, digits only, with an optional `-pre`/`+build` tail of
/// `[0-9A-Za-z.-]`.
fn is_semver(s: &str) -> bool {
    let (core, tail) = match s.find(['-', '+']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let mut parts = 0;
    for part in core.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts == 3
        && tail.is_none_or(|t| {
            !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+')
        })
}

// ---------------------------------------------------------------------------
// shapes 3 and 4: a JavaScript runtime
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    Node,
    Bun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    /// The script at this argv index is a vendor CLI's.
    Vendor(Vendor, usize),
    /// A script (or inline code) that is not a vendor CLI's.
    Other,
    /// No script in the argv: unreadable, or flags only.
    NotVisible,
}

/// Flags whose following words are inline code, not a script.
fn is_inline_code_flag(arg: &str) -> bool {
    matches!(arg, "-e" | "--eval" | "-p" | "--print" | "-pe" | "-ep")
        || arg.starts_with("--eval=")
        || arg.starts_with("--print=")
}

/// `bun <subcommand> <script>`: the words that come before a script.
const BUN_SUBCOMMANDS: [&str; 3] = ["run", "x", "exec"];

/// Find the script of a `node`/`bun` argv and classify it.
///
/// Leading `-` flags are skipped. A flag written without `=` may take the
/// next word as its value (`-r ts-node/register`, `--max-old-space-size
/// 4096`), so one non-vendor word right after a bare flag is skipped too; a
/// vendor script is recognised wherever it stands among the leading words.
/// The first other word is the script. A skipped word may have been a
/// non-vendor script, so an argv that ends after one is `Other`, never
/// `NotVisible`.
fn script_of<S: AsRef<str>>(runtime: Runtime, argv: &[S]) -> Script {
    let mut i = 1;
    let mut after_bare_flag = false;
    let mut skipped_word = false;
    let mut subcommand_allowed = runtime == Runtime::Bun;
    while let Some(arg) = argv.get(i).map(AsRef::as_ref) {
        if arg == "--" {
            return match argv.get(i + 1).map(AsRef::as_ref) {
                Some(script) => match vendor_of_script(script) {
                    Some(vendor) => Script::Vendor(vendor, i + 1),
                    None => Script::Other,
                },
                None => Script::NotVisible,
            };
        }
        if is_inline_code_flag(arg) {
            return Script::Other;
        }
        if arg.len() > 1 && arg.starts_with('-') {
            after_bare_flag = !arg.contains('=');
            i += 1;
            continue;
        }
        if let Some(vendor) = vendor_of_script(arg) {
            return Script::Vendor(vendor, i);
        }
        if subcommand_allowed && BUN_SUBCOMMANDS.contains(&arg) {
            subcommand_allowed = false;
            after_bare_flag = false;
            i += 1;
            continue;
        }
        if after_bare_flag {
            after_bare_flag = false;
            skipped_word = true;
            i += 1;
            continue;
        }
        return Script::Other;
    }
    if skipped_word {
        Script::Other
    } else {
        Script::NotVisible
    }
}

/// The vendor of a `node`/`bun` script path: its basename, or the package
/// or install directory it lives in.
fn vendor_of_script(script: &str) -> Option<Vendor> {
    if let Some(vendor) = vendor_of_name(basename(script)) {
        return Some(vendor);
    }
    if names_package(script, "@anthropic-ai/claude-code") {
        Some(Vendor::Claude)
    } else if names_package(script, "@openai/codex") {
        Some(Vendor::Codex)
    } else if script.split('/').any(|seg| seg == "cursor-agent") {
        Some(Vendor::Cursor)
    } else if script.contains("antigravity") {
        Some(Vendor::Agy)
    } else {
        None
    }
}

/// Whether `path` names the npm package `pkg` as whole path segments
/// (`…/node_modules/@openai/codex/bin/codex.js`, or the bare name for
/// `bun x`), never a longer package name that merely starts with it.
fn names_package(path: &str, pkg: &str) -> bool {
    path.match_indices(pkg).any(|(at, _)| {
        let before_ok = at == 0 || path[..at].ends_with('/');
        let after = &path[at + pkg.len()..];
        before_ok && (after.is_empty() || after.starts_with('/'))
    })
}

/// A JavaScript runtime that ships inside a vendor's install directory and
/// runs nothing else: cursor-agent's `…/cursor-agent/versions/<v>/node`.
fn vendored_runtime(exec_path: &str) -> Option<Vendor> {
    let mut dirs = dirs_rev(exec_path);
    let version = dirs.next()?;
    (!version.is_empty() && dirs.next() == Some("versions") && dirs.next() == Some("cursor-agent"))
        .then_some(Vendor::Cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(exec: &str, argv: &[&str]) -> Option<Vendor> {
        vendor_of_process(exec, argv)
    }

    #[test]
    fn basenames_name_their_vendor() {
        assert_eq!(vendor_of_exec_path("/x/claude"), Some(Vendor::Claude));
        assert_eq!(vendor_of_exec_path("/x/claude.exe"), Some(Vendor::Claude));
        assert_eq!(vendor_of_exec_path("/x/native-binary/claude"), Some(Vendor::Claude));
        assert_eq!(vendor_of_exec_path("/x/codex"), Some(Vendor::Codex));
        assert_eq!(vendor_of_exec_path("/x/vendor/codex-aarch64-apple-darwin"), Some(Vendor::Codex));
        assert_eq!(vendor_of_exec_path("/x/cursor-agent"), Some(Vendor::Cursor));
        assert_eq!(vendor_of_exec_path("/u/.local/share/cursor-agent/versions/1/agent"), Some(Vendor::Cursor));
        assert_eq!(vendor_of_exec_path("/usr/local/bin/agent"), None);
        assert_eq!(vendor_of_exec_path("/x/agy"), Some(Vendor::Agy));
        assert_eq!(vendor_of_exec_path("/opt/homebrew/bin/tmux"), None);
        assert_eq!(vendor_of_exec_path("/bin/sh"), None);
        assert_eq!(vendor_of_exec_path(""), None);
    }

    #[test]
    fn the_native_installer_layout_is_claude() {
        for path in [
            "/Users/u/.local/share/claude/versions/2.1.307",
            "/Users/u/.local/share/claude/versions/2.1.400",
            "/Users/u/.local/share/claude/versions/10.0.0-beta.1",
            "/Users/u/.local/share/claude/versions/3.0.1+build.7",
            "/Volumes/data/xdg/claude/versions/2.2.0",
        ] {
            assert_eq!(vendor_of_exec_path(path), Some(Vendor::Claude), "{path}");
        }
        for path in [
            "/Users/u/.local/share/claude/versions/2.1",
            "/Users/u/.local/share/claude/versions/2.1.307.tmp",
            "/Users/u/.local/share/claude/versions/latest",
            "/Users/u/.local/share/claude/versions/2.1.307-",
            "/Users/u/.local/share/other/versions/2.1.307",
            "/Users/u/.local/share/claude/2.1.307",
            "/Users/u/.local/share/claude/versions/v2.1.307",
        ] {
            assert_eq!(vendor_of_exec_path(path), None, "{path}");
        }
    }

    #[test]
    fn cursors_vendored_node_is_cursor() {
        let node = "/Users/u/.local/share/cursor-agent/versions/2026.10.19-6bbf2c4/node";
        let index = "/Users/u/.local/share/cursor-agent/versions/2026.10.19-6bbf2c4/index.js";
        // The launcher's exact argv (`exec -a "$0"` keeps the link as argv[0]).
        let argv = ["/Users/u/.local/bin/cursor-agent", "--use-system-ca", index, "--resume", "abc"];
        assert_eq!(
            vendor_launch(node, &argv),
            Some(VendorLaunch { vendor: Vendor::Cursor, args_from: 3 })
        );
        // argv unreadable, or only the flag: the exec path alone decides.
        assert_eq!(of(node, &[]), Some(Vendor::Cursor));
        assert_eq!(of(node, &[node, "--use-system-ca"]), Some(Vendor::Cursor));
        // A different script under the vendored node is not cursor's.
        assert_eq!(of(node, &[node, "--use-system-ca", "/tmp/other.js"]), None);
        // A system node running cursor's index.js is cursor.
        assert_eq!(of("/opt/homebrew/bin/node", &["node", index]), Some(Vendor::Cursor));
    }

    #[test]
    fn node_and_bun_skip_flags_before_the_script() {
        let cli = "/u/.npm-global/lib/node_modules/@anthropic-ai/claude-code/cli.js";
        assert_eq!(
            vendor_launch("/x/node", &["node", cli, "--resume", "id"]),
            Some(VendorLaunch { vendor: Vendor::Claude, args_from: 2 })
        );
        assert_eq!(
            vendor_launch("/x/node", &["node", "--max-old-space-size=8192", "--no-warnings", cli, "daemon"]),
            Some(VendorLaunch { vendor: Vendor::Claude, args_from: 4 })
        );
        // A bare flag's value is not the script.
        assert_eq!(of("/x/node", &["node", "--max-old-space-size", "8192", cli]), Some(Vendor::Claude));
        assert_eq!(of("/x/node", &["node", "-r", "/x/preload.js", cli]), Some(Vendor::Claude));
        assert_eq!(of("/x/node", &["node", "--", cli]), Some(Vendor::Claude));
        // An npm bin link run by its shebang: the script's basename.
        assert_eq!(of("/x/node", &["node", "/opt/homebrew/bin/claude", "-p"]), Some(Vendor::Claude));
        assert_eq!(of("/x/node", &["node", "/u/.npm/bin/codex"]), Some(Vendor::Codex));
        assert_eq!(
            of("/x/node", &["node", "/u/lib/node_modules/@openai/codex/bin/codex.js"]),
            Some(Vendor::Codex)
        );
        // bun, with and without a subcommand.
        assert_eq!(of("/x/bun", &["bun", cli]), Some(Vendor::Claude));
        assert_eq!(of("/x/bun", &["bun", "--smol", "run", cli]), Some(Vendor::Claude));
        assert_eq!(of("/x/bun", &["bun", "x", "@anthropic-ai/claude-code"]), Some(Vendor::Claude));
        assert_eq!(of("/x/node", &["node", "/x/antigravity/cli.js"]), Some(Vendor::Agy));
    }

    #[test]
    fn a_plain_runtime_running_anything_else_is_not_a_vendor() {
        assert_eq!(of("/x/node", &["node", "/srv/server.js"]), None);
        assert_eq!(of("/x/node", &["node", "--inspect", "/srv/server.js", "--port", "1"]), None);
        // The vendor path appears only as an argument of another script.
        assert_eq!(of("/x/node", &["node", "/srv/a.js", "/srv/b.js", "/y/@anthropic-ai/claude-code/cli.js"]), None);
        // Inline code has no script.
        assert_eq!(of("/x/node", &["node", "-e", "1", "/y/@anthropic-ai/claude-code/cli.js"]), None);
        // A longer package name that merely starts with a vendor's.
        assert_eq!(of("/x/node", &["node", "/y/node_modules/@anthropic-ai/claude-code-router/cli.js"]), None);
        assert_eq!(of("/x/node", &["node", "/y/node_modules/@openai/codex-sdk/index.js"]), None);
        assert_eq!(of("/x/node", &["node"]), None);
        assert_eq!(of("/x/node", &[] as &[&str]), None);
        assert_eq!(of("/x/bun", &["bun", "run", "dev"]), None);
        // A system node is not a vendored runtime.
        assert_eq!(of("/opt/homebrew/bin/node", &[] as &[&str]), None);
        assert_eq!(of("/u/.local/share/node/versions/20.1.0/node", &[] as &[&str]), None);
    }

    #[test]
    fn pane_commands_name_their_vendor() {
        assert_eq!(
            vendor_of_command("cd /tmp/p && env CLAUDECODE=1 /opt/bin/claude --agent-id a@t"),
            Some(Vendor::Claude)
        );
        assert_eq!(
            vendor_of_command("cd /tmp && /Users/u/.local/share/claude/versions/2.1.307 --agent-id a@t"),
            Some(Vendor::Claude)
        );
        assert_eq!(
            vendor_of_command("/usr/local/bin/node --no-warnings /y/@anthropic-ai/claude-code/cli.js"),
            Some(Vendor::Claude)
        );
        assert_eq!(vendor_of_command("cat"), None);
        assert_eq!(vendor_of_command("node /srv/server.js"), None);
        assert_eq!(vendor_of_command("'unterminated"), None);
    }

    #[test]
    fn args_from_never_overruns_the_argv() {
        assert_eq!(vendor_launch::<&str>("/x/claude", &[]).map(|l| l.args_from), Some(0));
        assert_eq!(vendor_launch("/x/claude", &["claude"]).map(|l| l.args_from), Some(1));
        let node = "/u/cursor-agent/versions/1/node";
        assert_eq!(vendor_launch(node, &[node, "--use-system-ca"]).map(|l| l.args_from), Some(2));
    }
}
