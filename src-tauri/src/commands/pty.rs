//! `session_*` commands — the PTY lane's IPC surface (DESIGN §3, §6).
//!
//! Thin, like every command module: validate, call [`PtyManager`], hand back a
//! small value. Two of them are unusual and deliberately so:
//!
//! * `session_snapshot` returns [`tauri::ipc::Response`], i.e. **raw bytes**.
//!   It is the one bulk transfer in the app; sending 2 MiB of scrollback as a
//!   JSON number array would cost roughly 8 MiB of string and a full parse.
//! * `session_write` takes base64 rather than a string. The frontend encodes
//!   with `TextEncoder`, so a pasted emoji, a lone surrogate or an IME
//!   composition reaches the child as the exact bytes the user produced.
//!
//! `session_close` returns a `KillReport` even when it did not have to kill
//! anything: DESIGN §8-6 forbids reporting a bare "ok" for anything that ends a
//! process.

use std::sync::Arc;

use base64::Engine as _;
use tauri::ipc::{Channel, Response};
use tauri::State;

use crate::error::{GildeError, GildeErrorPayload, CmdResult};
use crate::model::status::SessionStatusReport;
use crate::pty::{
    CloseMode, OpenSessionReq, PtyChunk, PtyManager, PtySessionInfo, PtySignal, SendPromptOutcome,
};
use crate::supervise::kill::KillReport;

/// Managed state alias so later tasks ask for the same thing.
pub type PtyState = Arc<PtyManager>;

/// Open a pty, start the agent in it, and stream its bytes to `on_chunk`.
#[tauri::command]
pub async fn session_open(
    req: OpenSessionReq,
    on_chunk: Channel<PtyChunk>,
    pty: State<'_, PtyState>,
) -> CmdResult<PtySessionInfo> {
    Ok(pty.open_request(req, on_chunk).await?)
}

/// Point a session at a freshly mounted view's channel.
///
/// Step **one** of the remount sequence (DESIGN §6-3): attach, *then* snapshot.
/// The other order loses whatever arrives in between.
#[tauri::command]
pub async fn session_attach(
    id: String,
    on_chunk: Channel<PtyChunk>,
    pty: State<'_, PtyState>,
) -> CmdResult<()> {
    Ok(pty.attach(&id, on_chunk)?)
}

/// 16-byte header (`GLDP`, version, `next_seq`) followed by the raw ring bytes.
#[tauri::command]
pub async fn session_snapshot(id: String, pty: State<'_, PtyState>) -> CmdResult<Response> {
    Ok(Response::new(pty.snapshot(&id)?))
}

/// Write base64-encoded bytes to the child's tty.
#[tauri::command]
pub async fn session_write(
    id: String,
    b64: String,
    pty: State<'_, PtyState>,
) -> CmdResult<()> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| GildeErrorPayload::from(GildeError::pty(format!("not base64: {e}"))))?;
    Ok(pty.write(&id, &bytes)?)
}

/// `TIOCSWINSZ`; the kernel delivers `SIGWINCH` to the foreground group.
#[tauri::command]
pub async fn session_resize(
    id: String,
    cols: u16,
    rows: u16,
    pty: State<'_, PtyState>,
) -> CmdResult<()> {
    Ok(pty.resize(&id, cols, rows)?)
}

/// Signal the pty's **foreground** process group — what `^C` does in a real
/// terminal, and therefore what reaches the pager rather than the agent when a
/// pager is up.
#[tauri::command]
pub async fn session_signal(
    id: String,
    sig: PtySignal,
    pty: State<'_, PtyState>,
) -> CmdResult<()> {
    Ok(pty.signal(&id, sig)?)
}

/// Type a prompt into a live TUI. Gated on an idle session (DESIGN §6-5).
#[tauri::command]
pub async fn session_send_prompt(
    id: String,
    text: String,
    pty: State<'_, PtyState>,
) -> CmdResult<SendPromptOutcome> {
    Ok(pty.send_prompt(&id, &text)?)
}

/// End a session. Always answers with what it did, what it spared and what
/// survived.
#[tauri::command]
pub async fn session_close(
    id: String,
    mode: CloseMode,
    pty: State<'_, PtyState>,
) -> CmdResult<KillReport> {
    Ok(pty.close(&id, mode).await?)
}

/// Every session this app still owns, oldest first.
///
/// M1.5-6: the first call also adopts the `pty_session` rows a crashed host left
/// behind (their ptys died with it), so a SIGKILL no longer makes interactive
/// sessions disappear without a trace. An adopted row comes back with `pid == 0`
/// and answers `session_status` with `exited`; `session_close` dismisses it.
/// The adoption is idempotent, so every later call is a plain read.
#[tauri::command]
pub async fn session_list(pty: State<'_, PtyState>) -> CmdResult<Vec<PtySessionInfo>> {
    pty.adopt_crashed_sessions().await;
    Ok(pty.list())
}

/// The current status and the evidence behind it.
///
/// T15 answers from output dynamics alone; T16 replaces the tracker behind this
/// command with the three-source engine of DESIGN §6-4 without changing the
/// shape returned here.
#[tauri::command]
pub async fn session_status(
    id: String,
    pty: State<'_, PtyState>,
) -> CmdResult<SessionStatusReport> {
    Ok(pty.status(&id)?)
}
