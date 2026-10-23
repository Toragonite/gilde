//! Reveal in Finder (contract §4.2.5 `ide_reveal`) by `NSWorkspace` — no
//! subprocess.
//!
//! Owner: T11. Tests use a fake [`Revealer`]; the Finder one is exercised
//! only by an `#[ignore]` test (it opens a Finder window).

use std::path::Path;

use crate::contract::error::IpcError;

/// Shows a file or folder selected in a file viewer.
pub trait Revealer: Send + Sync {
    fn reveal(&self, path: &Path) -> Result<(), IpcError>;
}

/// `NSWorkspace activateFileViewerSelectingURLs:` (thread-safe AppKit API).
pub struct FinderRevealer;

impl Revealer for FinderRevealer {
    fn reveal(&self, path: &Path) -> Result<(), IpcError> {
        use objc2_app_kit::NSWorkspace;
        use objc2_foundation::{NSArray, NSString, NSURL};
        let s = path.to_str().ok_or_else(|| IpcError::not_found("file"))?;
        if !path.exists() {
            return Err(IpcError::not_found("file"));
        }
        let url = NSURL::fileURLWithPath(&NSString::from_str(s));
        let urls = NSArray::from_retained_slice(&[url]);
        NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&urls);
        Ok(())
    }
}
