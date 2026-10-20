//! The bidirectional NDJSON channel to one sidecar generation (DESIGN §5-1).
//!
//! A [`SidecarClient`] owns exactly one generation of the sidecar process. It
//! never spawns, kills or restarts anything — that is [`crate::sidecar::SidecarManager`]'s
//! job — and it never survives its process: when the pipe ends, every in-flight
//! request is rejected with [`SidecarCallError::Gone`] carrying the generation
//! that died, so a caller can tell "the app failed" (`interrupted`) from "the
//! agent failed" (`error`), which is the distinction DESIGN §5-10 insists the
//! user must never have to guess.
//!
//! Three tasks per generation:
//!
//!   * **reader** — frames stdout into lines under a hard [`MAX_LINE_BYTES`] cap,
//!     correlates responses, and routes `host.` / `$` requests to the
//!     [`HostHandler`]. Unparseable lines are warned about and skipped.
//!   * **writer** — serialises outbound lines from an unbounded queue onto stdin.
//!     Requests and responses are never dropped; nothing here is lossy, because
//!     the only lossy direction (`mesh.event`) is the sidecar's to drop.
//!   * **stderr drain** — owned by the manager, not here.
//!
//! A fault the client cannot repair (oversize line, closed pipe, failed write)
//! is reported once on the fault channel and the client marks itself closed. The
//! manager decides what that means.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::error::GildeError;
use crate::sidecar::protocol::{
    self, code, is_host_method, Inbound, LineIssue, Outcome, RpcErrorObject, MAX_LINE_BYTES,
};

/// The future a [`HostHandler`] returns. Boxed by hand rather than through an
/// `async_trait` dependency — the trait has three methods and one lifetime.
pub type HostFuture = Pin<Box<dyn Future<Output = Result<Value, RpcErrorObject>> + Send>>;

/// What answers the sidecar's `host.*` requests (DESIGN §5-5).
pub trait HostHandler: Send + Sync + 'static {
    /// Answer a request. The response is not written until this resolves — which
    /// is the entire point of the bidirectional channel: `host.registerSpawn`
    /// must complete its durable INSERT *before* the sidecar spawns anything.
    fn call(&self, method: String, params: Value) -> HostFuture;

    /// Handle a notification. Must not block; there is nothing to answer.
    fn notify(&self, method: &str, params: Value);
}

/// A fault the client observed but cannot repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFault {
    /// A line exceeded [`MAX_LINE_BYTES`]. The peer is violating the protocol:
    /// the manager kills it and restarts at generation + 1.
    OversizeLine { bytes: usize },
    /// stdout ended — the process is gone or closed the channel.
    StreamEnded,
    /// A read or write failed.
    Io { detail: String },
}

impl std::fmt::Display for ClientFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientFault::OversizeLine { bytes } => {
                write!(f, "inbound line exceeded {MAX_LINE_BYTES} bytes ({bytes} seen)")
            }
            ClientFault::StreamEnded => f.write_str("sidecar stdout ended"),
            ClientFault::Io { detail } => write!(f, "channel io error: {detail}"),
        }
    }
}

/// Why a call did not produce a result.
///
/// [`SidecarCallError::Gone`] is the load-bearing variant: it means the request
/// was in flight when the sidecar died, so the *app* failed rather than the
/// agent, and the affected run settles as `interrupted` (DESIGN §5-10).
#[derive(Debug, Clone, PartialEq)]
pub enum SidecarCallError {
    /// There is no channel right now (never started, or down between restarts).
    Down,
    /// The channel died under this request.
    Gone { generation: i64 },
    /// The sidecar answered with a JSON-RPC error.
    Rpc(RpcErrorObject),
    /// The deadline elapsed with no answer.
    Timeout { method: String, ms: u64 },
    /// The answer did not match the expected shape.
    Decode { method: String, detail: String },
}

impl SidecarCallError {
    /// Whether this failure means "the app broke", not "the agent broke".
    pub fn is_infrastructure(&self) -> bool {
        matches!(
            self,
            SidecarCallError::Down | SidecarCallError::Gone { .. } | SidecarCallError::Timeout { .. }
        )
    }

    /// The AgentMesh error name, for an adapter failure.
    pub fn mesh_error_name(&self) -> Option<&str> {
        match self {
            SidecarCallError::Rpc(err) => err.name(),
            _ => None,
        }
    }
}

impl std::fmt::Display for SidecarCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SidecarCallError::Down => f.write_str("the sidecar is not running"),
            SidecarCallError::Gone { generation } => {
                write!(f, "the sidecar (generation {generation}) died mid-request")
            }
            SidecarCallError::Rpc(err) => write!(f, "{err}"),
            SidecarCallError::Timeout { method, ms } => {
                write!(f, "{method} did not answer within {ms}ms")
            }
            SidecarCallError::Decode { method, detail } => {
                write!(f, "{method} answered with an unreadable shape: {detail}")
            }
        }
    }
}

impl From<SidecarCallError> for GildeError {
    /// Infrastructure failures map to the retryable `SIDECAR_DOWN`; an adapter's
    /// own error keeps its text as untranslated `detail` (§9-9 — Rust returns
    /// `{code, key, params}`, and vendor stderr is never translated).
    fn from(e: SidecarCallError) -> GildeError {
        match e {
            SidecarCallError::Down | SidecarCallError::Gone { .. } => GildeError::SidecarDown,
            other => GildeError::Internal(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// client
// ---------------------------------------------------------------------------

struct Inner {
    generation: i64,
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, oneshot::Sender<Outcome>>>,
    writer: mpsc::UnboundedSender<String>,
    closed: AtomicBool,
}

impl Inner {
    /// Reject every in-flight request. Idempotent: the second caller finds an
    /// empty map, which is what makes "the reader saw EOF" and "the manager is
    /// restarting" safe to race.
    fn reject_all(&self) {
        let drained: Vec<_> = {
            let mut pending = self.pending.lock();
            pending.drain().collect()
        };
        for (id, tx) in drained {
            let _ = tx.send(Outcome::Err(RpcErrorObject::new(
                code::CHANNEL_CLOSED,
                format!("sidecar generation {} went away", self.generation),
            )));
            tracing::debug!(target: "sidecar", id = %id, "rejected an in-flight request");
        }
    }
}

/// One generation's channel. Cheap to clone; every clone shares the same tasks.
#[derive(Clone)]
pub struct SidecarClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SidecarClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarClient")
            .field("generation", &self.inner.generation)
            .field("closed", &self.inner.closed.load(Ordering::Relaxed))
            .field("pending", &self.inner.pending.lock().len())
            .finish()
    }
}

impl SidecarClient {
    /// Take ownership of one generation's pipes and start its two tasks.
    pub fn start<R, W>(
        generation: i64,
        stdout: R,
        stdin: W,
        handler: Arc<dyn HostHandler>,
        faults: mpsc::UnboundedSender<ClientFault>,
    ) -> SidecarClient
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let inner = Arc::new(Inner {
            generation,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            writer: tx,
            closed: AtomicBool::new(false),
        });

        tokio::spawn(writer_task(rx, stdin, inner.clone(), faults.clone()));
        tokio::spawn(reader_task(stdout, inner.clone(), handler, faults));

        SidecarClient { inner }
    }

    pub fn generation(&self) -> i64 {
        self.inner.generation
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Stop writing and reject everything in flight.
    pub fn close(&self, reason: &str) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::info!(
            target: "sidecar",
            generation = self.inner.generation,
            reason,
            "closing the sidecar channel"
        );
        self.inner.reject_all();
    }

    /// Fire-and-forget. Nothing the host sends is lossy, so a full queue is not
    /// a case that exists here; a closed channel simply drops the line.
    pub fn notify<P: Serialize>(&self, method: &str, params: &P) {
        if self.is_closed() {
            return;
        }
        let params = serde_json::to_value(params).unwrap_or(Value::Null);
        let _ = self
            .inner
            .writer
            .send(protocol::notification_line(method, &params));
    }

    /// Issue a request and await the answer.
    ///
    /// `timeout` is optional on purpose: a `mesh.run` can legitimately take the
    /// task's whole wall clock, and imposing a shorter host-side deadline would
    /// abandon a run that is still working. Health checks pass a deadline; work
    /// does not — the wall clock lives in `RunTask.timeoutMs`.
    pub async fn call_value(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, SidecarCallError> {
        if self.is_closed() {
            return Err(SidecarCallError::Down);
        }

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(id.clone(), tx);

        let line = protocol::request_line(&id, method, &params);
        if self.inner.writer.send(line).is_err() {
            self.inner.pending.lock().remove(&id);
            return Err(SidecarCallError::Gone {
                generation: self.inner.generation,
            });
        }

        let outcome = match timeout {
            Some(deadline) => match tokio::time::timeout(deadline, rx).await {
                Ok(received) => received,
                Err(_) => {
                    self.inner.pending.lock().remove(&id);
                    return Err(SidecarCallError::Timeout {
                        method: method.to_string(),
                        ms: deadline.as_millis() as u64,
                    });
                }
            },
            None => rx.await,
        };

        match outcome {
            Ok(Outcome::Ok(value)) => Ok(value),
            Ok(Outcome::Err(err)) if err.code == code::CHANNEL_CLOSED => {
                Err(SidecarCallError::Gone {
                    generation: self.inner.generation,
                })
            }
            Ok(Outcome::Err(err)) => Err(SidecarCallError::Rpc(err)),
            // The sender was dropped without a value: the reader task is gone.
            Err(_) => Err(SidecarCallError::Gone {
                generation: self.inner.generation,
            }),
        }
    }

    /// Typed convenience over [`SidecarClient::call_value`].
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
        timeout: Option<Duration>,
    ) -> Result<R, SidecarCallError> {
        let params = serde_json::to_value(params).map_err(|e| SidecarCallError::Decode {
            method: method.to_string(),
            detail: format!("params are not serializable: {e}"),
        })?;
        let value = self.call_value(method, params, timeout).await?;
        serde_json::from_value(value).map_err(|e| SidecarCallError::Decode {
            method: method.to_string(),
            detail: e.to_string(),
        })
    }

    /// In-flight request count. Used by tests and diagnostics.
    pub fn pending(&self) -> usize {
        self.inner.pending.lock().len()
    }
}

// ---------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------

async fn writer_task<W>(
    mut rx: mpsc::UnboundedReceiver<String>,
    mut stdin: W,
    inner: Arc<Inner>,
    faults: mpsc::UnboundedSender<ClientFault>,
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    while let Some(line) = rx.recv().await {
        if let Err(e) = stdin.write_all(line.as_bytes()).await {
            let _ = faults.send(ClientFault::Io {
                detail: e.to_string(),
            });
            break;
        }
        if let Err(e) = stdin.flush().await {
            let _ = faults.send(ClientFault::Io {
                detail: e.to_string(),
            });
            break;
        }
    }
    // Closing stdin is how a well-behaved sidecar learns to shut down.
    let _ = stdin.shutdown().await;
    tracing::debug!(
        target: "sidecar",
        generation = inner.generation,
        "writer task finished"
    );
}

/// Read size for one syscall. Independent of [`MAX_LINE_BYTES`], which bounds
/// the *accumulated* line rather than one read.
const READ_CHUNK: usize = 64 * 1024;

async fn reader_task<R>(
    mut stdout: R,
    inner: Arc<Inner>,
    handler: Arc<dyn HostHandler>,
    faults: mpsc::UnboundedSender<ClientFault>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut line: Vec<u8> = Vec::with_capacity(4096);
    // True while skipping the remainder of a line that blew the cap.
    let mut discarding = false;

    loop {
        let read = match stdout.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = faults.send(ClientFault::Io {
                    detail: e.to_string(),
                });
                break;
            }
        };

        let mut start = 0usize;
        while start < read {
            match chunk[start..read].iter().position(|b| *b == b'\n') {
                Some(offset) => {
                    let end = start + offset;
                    if discarding {
                        // The line that overflowed ends here; resume framing.
                        discarding = false;
                        line.clear();
                    } else {
                        line.extend_from_slice(&chunk[start..end]);
                        deliver(&line, &inner, &handler);
                        line.clear();
                    }
                    start = end + 1;
                }
                None => {
                    if !discarding {
                        let segment = &chunk[start..read];
                        if line.len() + segment.len() > MAX_LINE_BYTES {
                            // Protocol violation (DESIGN §5-1 rule 3): drop the
                            // buffer, skip to the next newline, and let the
                            // manager restart at generation + 1. The buffer is
                            // bounded, so a runaway peer cannot OOM the host.
                            let seen = line.len() + segment.len();
                            line.clear();
                            line.shrink_to_fit();
                            discarding = true;
                            tracing::error!(
                                target: "sidecar",
                                generation = inner.generation,
                                bytes = seen,
                                cap = MAX_LINE_BYTES,
                                "inbound line exceeds the cap; discarding it and restarting"
                            );
                            let _ = faults.send(ClientFault::OversizeLine { bytes: seen });
                        } else {
                            line.extend_from_slice(segment);
                        }
                    }
                    start = read;
                }
            }
        }
    }

    inner.closed.store(true, Ordering::SeqCst);
    inner.reject_all();
    let _ = faults.send(ClientFault::StreamEnded);
    tracing::debug!(
        target: "sidecar",
        generation = inner.generation,
        "reader task finished"
    );
}

/// Decode one framed line and route it. Never fails the session.
fn deliver(bytes: &[u8], inner: &Arc<Inner>, handler: &Arc<dyn HostHandler>) {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            tracing::warn!(
                target: "sidecar",
                generation = inner.generation,
                bytes = bytes.len(),
                "skipping a non-UTF-8 line"
            );
            return;
        }
    };

    match protocol::parse_line(text) {
        Ok(Inbound::Response { id, outcome }) => {
            let key = id_key(&id);
            let waiting = inner.pending.lock().remove(&key);
            match waiting {
                Some(tx) => {
                    let _ = tx.send(outcome);
                }
                None => tracing::warn!(
                    target: "sidecar",
                    id = %key,
                    "response for an unknown or already-settled request"
                ),
            }
        }
        Ok(Inbound::Request { id, method, params }) => {
            if !is_host_method(&method) {
                let err = RpcErrorObject::new(
                    code::METHOD_NOT_FOUND,
                    format!("unknown host method: {method}"),
                );
                let _ = inner.writer.send(protocol::error_line(&id, &err));
                return;
            }
            let handler = handler.clone();
            let writer = inner.writer.clone();
            // Detached: a handler that awaits a durable write must not stall the
            // reader, or a second `host.registerSpawn` would queue behind it.
            tokio::spawn(async move {
                let outcome = handler.call(method, params).await;
                let line = match outcome {
                    Ok(result) => protocol::result_line(&id, &result),
                    Err(err) => protocol::error_line(&id, &err),
                };
                let _ = writer.send(line);
            });
        }
        Ok(Inbound::Notification { method, params }) => handler.notify(&method, params),
        Err(issue) => {
            // A stray log line, a half-written frame, a dependency writing to
            // stdout despite the hardening: warn and move on (DESIGN §5-1).
            tracing::warn!(
                target: "sidecar",
                generation = inner.generation,
                issue = %issue,
                bytes = text.len(),
                "skipping an unusable inbound line"
            );
            if matches!(issue, LineIssue::NotJson) {
                tracing::debug!(target: "sidecar", line = %truncate(text, 200), "skipped line");
            }
        }
    }
}

/// Correlation key for a response id, preserving the distinction between the
/// string `"1"` and the number `1` so two peers cannot collide.
fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{duplex, DuplexStream};

    /// A handler that answers every `host.*` request with a fixed echo and
    /// records notifications, so a test can assert both directions.
    struct Echo {
        notes: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl HostHandler for Echo {
        fn call(&self, method: String, params: Value) -> HostFuture {
            Box::pin(async move { Ok(json!({"method": method, "echo": params})) })
        }
        fn notify(&self, method: &str, params: Value) {
            self.notes.lock().push((method.to_string(), params));
        }
    }

    /// The host client plus the two ends a stand-in sidecar would hold.
    struct Wired {
        client: SidecarClient,
        /// What the fake sidecar writes into (arrives on the client's reader).
        peer_out: DuplexStream,
        /// What the fake sidecar reads from (the client's writer).
        peer_in: DuplexStream,
        notes: Arc<Mutex<Vec<(String, Value)>>>,
        faults: mpsc::UnboundedReceiver<ClientFault>,
    }

    fn wire(generation: i64) -> Wired {
        let (host_stdout, peer_out) = duplex(256 * 1024);
        let (host_stdin, peer_in) = duplex(256 * 1024);
        let notes = Arc::new(Mutex::new(Vec::new()));
        let (ftx, faults) = mpsc::unbounded_channel();
        let client = SidecarClient::start(
            generation,
            host_stdout,
            host_stdin,
            Arc::new(Echo {
                notes: notes.clone(),
            }),
            ftx,
        );
        Wired {
            client,
            peer_out,
            peer_in,
            notes,
            faults,
        }
    }

    /// Read one NDJSON line from the peer's end of the channel.
    async fn read_line(peer_in: &mut DuplexStream) -> Value {
        let mut acc = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(5), peer_in.read(&mut byte))
                .await
                .expect("a line within 5s")
                .expect("read");
            assert_ne!(n, 0, "the channel closed before a full line arrived");
            if byte[0] == b'\n' {
                break;
            }
            acc.push(byte[0]);
        }
        serde_json::from_slice(&acc).expect("the host writes valid JSON")
    }

    async fn wait_for(mut predicate: impl FnMut() -> bool) -> bool {
        for _ in 0..300 {
            if predicate() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_call_is_correlated_by_its_id() {
        let mut w = wire(7);
        let client = w.client.clone();
        let call =
            tokio::spawn(async move { client.call_value("mesh.ping", json!({"nonce": 1}), None).await });

        let request = read_line(&mut w.peer_in).await;
        assert_eq!(request["method"], json!("mesh.ping"));
        assert_eq!(request["jsonrpc"], json!("2.0"));
        let id = request["id"].clone();
        assert!(id.is_string(), "the host issues string ids");

        w.peer_out
            .write_all(protocol::result_line(&id, &json!({"nonce": 1, "ts": 5})).as_bytes())
            .await
            .expect("write");

        let result = call.await.expect("join").expect("call");
        assert_eq!(result["ts"], json!(5));
        assert_eq!(w.client.pending(), 0);
    }

    #[tokio::test]
    async fn a_host_request_is_answered_with_the_id_type_it_arrived_with() {
        let mut w = wire(1);
        // The sidecar issues NUMERIC ids; the answer must carry a number back.
        let line = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": protocol::host_method::SPAWNED,
            "params": {"spawnId": "s1", "pid": 111, "pgid": 111},
        })
        .to_string()
            + "\n";
        w.peer_out.write_all(line.as_bytes()).await.expect("write");

        let answer = read_line(&mut w.peer_in).await;
        assert_eq!(answer["id"], json!(42), "numeric ids are echoed as numbers");
        assert_eq!(answer["result"]["method"], json!("host.spawned"));
        assert_eq!(answer["result"]["echo"]["pid"], json!(111));
    }

    #[tokio::test]
    async fn a_request_for_a_non_host_method_is_refused_without_reaching_the_handler() {
        let mut w = wire(1);
        let line = json!({"jsonrpc": "2.0", "id": 9, "method": "mesh.run", "params": {}}).to_string()
            + "\n";
        w.peer_out.write_all(line.as_bytes()).await.expect("write");

        let answer = read_line(&mut w.peer_in).await;
        assert_eq!(answer["id"], json!(9));
        assert_eq!(answer["error"]["code"], json!(code::METHOD_NOT_FOUND));
    }

    #[tokio::test]
    async fn garbage_lines_are_skipped_and_the_session_continues() {
        let mut w = wire(3);
        w.peer_out
            .write_all(b"this is not json\n{\"hello\":\"world\"}\n{\"jsonrpc\":\"2.0\"}\n")
            .await
            .expect("write");
        w.peer_out
            .write_all(
                protocol::notification_line(protocol::host_method::EVENT, &json!({"runId": "r"}))
                    .as_bytes(),
            )
            .await
            .expect("write");

        let notes = w.notes.clone();
        assert!(
            wait_for(move || !notes.lock().is_empty()).await,
            "the notification after three unusable lines still arrived"
        );
        assert_eq!(w.notes.lock()[0].0, protocol::host_method::EVENT);
        assert!(!w.client.is_closed(), "garbage is never fatal");
    }

    #[tokio::test]
    async fn a_non_utf8_line_is_skipped_rather_than_lossily_decoded() {
        let mut w = wire(3);
        w.peer_out.write_all(&[0xff, 0xfe, 0x80, b'\n']).await.expect("write");
        w.peer_out
            .write_all(
                protocol::notification_line(protocol::host_method::EVENT, &json!({"runId": "r"}))
                    .as_bytes(),
            )
            .await
            .expect("write");

        let notes = w.notes.clone();
        assert!(wait_for(move || !notes.lock().is_empty()).await);
        assert!(!w.client.is_closed());
    }

    #[tokio::test]
    async fn an_oversize_line_is_reported_once_and_framing_recovers() {
        let mut w = wire(4);
        // One 9 MiB line — past both the host cap and the sidecar's own limit.
        w.peer_out
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"mesh.event\",\"params\":\"")
            .await
            .expect("write");
        let blob = vec![b'x'; 1024 * 1024];
        for _ in 0..9 {
            w.peer_out.write_all(&blob).await.expect("write");
        }
        w.peer_out.write_all(b"\"}\n").await.expect("write");
        w.peer_out
            .write_all(
                protocol::notification_line(protocol::host_method::EVENT, &json!({"runId": "r"}))
                    .as_bytes(),
            )
            .await
            .expect("write");

        let fault = tokio::time::timeout(Duration::from_secs(20), w.faults.recv())
            .await
            .expect("a fault within 20s")
            .expect("fault");
        assert!(matches!(fault, ClientFault::OversizeLine { .. }), "{fault:?}");

        let notes = w.notes.clone();
        assert!(
            wait_for(move || !notes.lock().is_empty()).await,
            "the line after the oversize one is framed correctly"
        );
    }

    #[tokio::test]
    async fn in_flight_calls_are_rejected_as_gone_when_the_pipe_ends() {
        let w = wire(7);
        let client = w.client.clone();
        let call = tokio::spawn(async move { client.call_value("mesh.quota", json!({}), None).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(w.peer_out);
        drop(w.peer_in);

        let err = call.await.expect("join").expect_err("must fail");
        assert_eq!(err, SidecarCallError::Gone { generation: 7 });
        assert!(err.is_infrastructure(), "the app failed, not the agent");
        assert!(wait_for(move || w.client.is_closed()).await);
    }

    #[tokio::test]
    async fn an_rpc_error_keeps_the_agent_mesh_error_name() {
        let mut w = wire(2);
        let client = w.client.clone();
        let call =
            tokio::spawn(async move { client.call_value("mesh.quota", json!({}), None).await });
        let request = read_line(&mut w.peer_in).await;
        let err = RpcErrorObject::new(code::ADAPTER_ERROR, "quota wall")
            .with_data(json!({"name": "QuotaExceededError"}));
        w.peer_out
            .write_all(protocol::error_line(&request["id"], &err).as_bytes())
            .await
            .expect("write");

        let failed = call.await.expect("join").expect_err("must fail");
        assert_eq!(failed.mesh_error_name(), Some("QuotaExceededError"));
        assert!(!failed.is_infrastructure(), "the agent failed, not the app");
    }

    #[tokio::test]
    async fn a_deadline_produces_a_timeout_and_frees_the_slot() {
        let w = wire(5);
        let err = w
            .client
            .call_value("mesh.ping", json!({}), Some(Duration::from_millis(50)))
            .await
            .expect_err("must time out");
        assert!(matches!(err, SidecarCallError::Timeout { .. }));
        assert_eq!(w.client.pending(), 0, "a timed-out slot is not leaked");
    }

    #[tokio::test]
    async fn closing_rejects_pending_work_and_stops_writing() {
        let w = wire(11);
        let client = w.client.clone();
        let call = tokio::spawn(async move { client.call_value("mesh.run", json!({}), None).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        w.client.close("test");
        let err = call.await.expect("join").expect_err("must fail");
        assert_eq!(err, SidecarCallError::Gone { generation: 11 });
        assert!(matches!(
            w.client.call_value("mesh.ping", json!({}), None).await,
            Err(SidecarCallError::Down)
        ));
    }
}
