//! VS Code and Cursor, found by bundle id (contract §4.2.5 `ide_editors`,
//! §9.4) [C9].
//!
//! Owner: T11.
//!
//! Only two editors exist for Gilde 2 — VS Code (`com.microsoft.VSCode`) and
//! Cursor (`com.todesktop.230313mzl4w4u92`) — located by **bundle id** in
//! `/Applications` and `~/Applications` with a directory listing only (one
//! level of `*.app`; each bundle's id is read from its `Info.plist`, nothing
//! is launched or registered). A renamed app is still found; an app with the
//! right name but another id is not. There is no custom executable in v1.

use std::path::{Path, PathBuf};

use crate::contract::ipc::changes::{EditorKind, EditorOption};

/// VS Code's bundle id.
pub const VSCODE_BUNDLE_ID: &str = "com.microsoft.VSCode";
/// Cursor's bundle id.
pub const CURSOR_BUNDLE_ID: &str = "com.todesktop.230313mzl4w4u92";
/// The two editors, in the order they are listed.
pub const EDITORS: [EditorKind; 2] = [EditorKind::VsCode, EditorKind::Cursor];

pub fn bundle_id(kind: EditorKind) -> &'static str {
    match kind {
        EditorKind::VsCode => VSCODE_BUNDLE_ID,
        EditorKind::Cursor => CURSOR_BUNDLE_ID,
    }
}

/// The editor's CLI inside its bundle (§9.4).
pub fn cli_relative(kind: EditorKind) -> &'static str {
    match kind {
        EditorKind::VsCode => "Contents/Resources/app/bin/code",
        EditorKind::Cursor => "Contents/Resources/app/bin/cursor",
    }
}

/// The `ide.editor` setting value (§9.6) as an editor.
pub fn from_setting(value: &str) -> Option<EditorKind> {
    match value {
        "vscode" => Some(EditorKind::VsCode),
        "cursor" => Some(EditorKind::Cursor),
        _ => None,
    }
}

/// An installed editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundEditor {
    pub editor: EditorKind,
    /// The `.app` bundle (canonical).
    pub app: PathBuf,
    /// The bundle CLI when it exists as a regular file.
    pub cli: Option<PathBuf>,
}

/// Reads a bundle's id (its `Info.plist` `CFBundleIdentifier`).
pub trait BundleReader: Send + Sync {
    fn bundle_id(&self, app: &Path) -> Option<String>;
}

/// The production reader: `NSBundle bundleWithPath:` + `bundleIdentifier`
/// (Foundation parses `Contents/Info.plist`; nothing is launched).
pub struct InfoPlistReader;

impl BundleReader for InfoPlistReader {
    fn bundle_id(&self, app: &Path) -> Option<String> {
        use objc2_foundation::{NSBundle, NSString};
        let path = NSString::from_str(app.to_str()?);
        let bundle = NSBundle::bundleWithPath(&path)?;
        bundle.bundleIdentifier().map(|id| id.to_string())
    }
}

/// `/Applications` and `~/Applications`, in that order.
pub fn default_dirs(home: &Path) -> Vec<PathBuf> {
    vec![PathBuf::from("/Applications"), home.join("Applications")]
}

/// The editors in `dirs`: one level of `*.app` entries each (directory listing
/// only), matched by bundle id; the first bundle per editor wins.
pub fn scan(dirs: &[PathBuf], reader: &dyn BundleReader) -> Vec<FoundEditor> {
    let mut found: Vec<FoundEditor> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut apps: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "app"))
            .collect();
        apps.sort();
        for app in apps {
            let Some(id) = reader.bundle_id(&app) else { continue };
            let Some(editor) = EDITORS.into_iter().find(|k| bundle_id(*k) == id) else { continue };
            if found.iter().any(|f| f.editor == editor) {
                continue;
            }
            let app = std::fs::canonicalize(&app).unwrap_or(app);
            let cli = app.join(cli_relative(editor));
            let cli = std::fs::metadata(&cli).is_ok_and(|m| m.is_file()).then_some(cli);
            found.push(FoundEditor { editor, app, cli });
        }
    }
    found.sort_by_key(|f| EDITORS.iter().position(|k| *k == f.editor));
    found
}

/// The wire form: the app path under the home dir as `~/…` (§4.8).
pub fn option(found: &FoundEditor, home: &Path) -> EditorOption {
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let shown = match found.app.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => found.app.display().to_string(),
    };
    EditorOption {
        editor: found.editor,
        bundle_id: bundle_id(found.editor).to_string(),
        app_path: shown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ByName;
    impl BundleReader for ByName {
        fn bundle_id(&self, app: &Path) -> Option<String> {
            std::fs::read_to_string(app.join("Contents/id")).ok().map(|s| s.trim().to_string())
        }
    }

    fn app(dir: &Path, name: &str, id: &str, cli: Option<&str>) {
        let a = dir.join(name);
        std::fs::create_dir_all(a.join("Contents")).unwrap();
        std::fs::write(a.join("Contents/id"), id).unwrap();
        if let Some(rel) = cli {
            let c = a.join(rel);
            std::fs::create_dir_all(c.parent().unwrap()).unwrap();
            std::fs::write(c, "#!/bin/sh\n").unwrap();
        }
    }

    #[test]
    fn editors_are_found_by_bundle_id_not_by_name() {
        let sys = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        // Renamed VS Code with its CLI; a fake "Cursor.app" with another id;
        // the real Cursor in the user's Applications without a CLI.
        app(sys.path(), "My Editor.app", VSCODE_BUNDLE_ID, Some(cli_relative(EditorKind::VsCode)));
        app(sys.path(), "Cursor.app", "com.example.notcursor", None);
        app(user.path(), "Cursor.app", CURSOR_BUNDLE_ID, None);
        std::fs::write(sys.path().join("README.txt"), "x").unwrap();
        let found = scan(&[sys.path().to_path_buf(), user.path().to_path_buf()], &ByName);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].editor, EditorKind::VsCode);
        assert!(found[0].app.ends_with("My Editor.app"));
        assert!(found[0].cli.is_some());
        assert_eq!(found[1].editor, EditorKind::Cursor);
        assert!(found[1].cli.is_none(), "no CLI → the `open -b` fallback");
        assert!(scan(&[PathBuf::from("/definitely/not/here")], &ByName).is_empty());
    }

    #[test]
    fn settings_values_and_display_paths() {
        assert_eq!(from_setting("vscode"), Some(EditorKind::VsCode));
        assert_eq!(from_setting("cursor"), Some(EditorKind::Cursor));
        assert_eq!(from_setting("vim"), None);
        let home = tempfile::tempdir().unwrap();
        let f = FoundEditor {
            editor: EditorKind::Cursor,
            app: std::fs::canonicalize(home.path()).unwrap().join("Applications/Cursor.app"),
            cli: None,
        };
        let o = option(&f, home.path());
        assert_eq!(o.app_path, "~/Applications/Cursor.app");
        assert_eq!(o.bundle_id, CURSOR_BUNDLE_ID);
    }

    /// The production reader parses a real `Info.plist` (no app is launched).
    #[test]
    fn the_info_plist_reader_reads_the_bundle_identifier() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("Fake.app");
        std::fs::create_dir_all(a.join("Contents/MacOS")).unwrap();
        std::fs::write(
            a.join("Contents/Info.plist"),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{VSCODE_BUNDLE_ID}</string><key>CFBundlePackageType</key><string>APPL</string></dict></plist>\n"
            ),
        )
        .unwrap();
        assert_eq!(InfoPlistReader.bundle_id(&a).as_deref(), Some(VSCODE_BUNDLE_ID));
        assert_eq!(InfoPlistReader.bundle_id(&dir.path().join("Missing.app")), None);
    }
}
