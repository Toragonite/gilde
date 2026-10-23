//! The spawn gate opened by the boot sweep (§2.4 B6).
//!
//! Owner: T09.
//!
//! No driver, probe or helper spawns before the boot sweep has fenced and
//! adopted what the previous instance left behind (contract §2.4 B6, §6.0
//! *Spawn gate*): a fresh child planned while the sweep still runs could be
//! mistaken for — or collide with — a crash leftover. Every spawn therefore
//! awaits the gate for up to [`GATE_WAIT`]; after that it fails with
//! `NOT_READY{reason: "bootSweep"}`, which is retryable.
//!
//! The gate only ever opens; it never closes again.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::contract::IpcError;

/// How long a spawn waits for the gate (§2.4 B6).
pub const GATE_WAIT: Duration = Duration::from_secs(5);

/// The `NOT_READY` reason for a closed gate.
pub const REASON_BOOT_SWEEP: &str = "bootSweep";

/// Opens once, after the boot sweep.
#[derive(Debug)]
pub struct SpawnGate {
    tx: watch::Sender<bool>,
}

impl SpawnGate {
    /// A gate that is closed until [`SpawnGate::open`] (the app's boot path).
    pub fn closed() -> Arc<SpawnGate> {
        let (tx, _rx) = watch::channel(false);
        Arc::new(SpawnGate { tx })
    }

    /// A gate that is already open (tests and tools that run no boot sweep).
    pub fn opened() -> Arc<SpawnGate> {
        let (tx, _rx) = watch::channel(true);
        Arc::new(SpawnGate { tx })
    }

    /// Open the gate. Idempotent.
    pub fn open(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_open(&self) -> bool {
        *self.tx.borrow()
    }

    /// Wait up to `max` for the gate; `NOT_READY{bootSweep}` if it stays shut.
    pub async fn wait_for(&self, max: Duration) -> Result<(), IpcError> {
        if self.is_open() {
            return Ok(());
        }
        let mut rx = self.tx.subscribe();
        let opened = async move { rx.wait_for(|open| *open).await.map(|_| ()) };
        match tokio::time::timeout(max, opened).await {
            Ok(Ok(())) => Ok(()),
            _ => Err(IpcError::not_ready(REASON_BOOT_SWEEP)),
        }
    }

    /// [`SpawnGate::wait_for`] with the contract's 5 s.
    pub async fn wait(&self) -> Result<(), IpcError> {
        self.wait_for(GATE_WAIT).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ErrorCode;

    #[tokio::test]
    async fn a_closed_gate_times_out_with_not_ready_boot_sweep() {
        let gate = SpawnGate::closed();
        let err = gate.wait_for(Duration::from_millis(30)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotReady);
        assert_eq!(err.params.get("reason").and_then(|v| v.as_str()), Some("bootSweep"));
        assert!(err.retryable);
    }

    #[tokio::test]
    async fn waiters_are_released_when_the_gate_opens() {
        let gate = SpawnGate::closed();
        let g = gate.clone();
        let waiter = tokio::spawn(async move { g.wait_for(Duration::from_secs(5)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.open();
        assert!(waiter.await.unwrap().is_ok());
        assert!(gate.is_open());
        gate.open();
        assert!(SpawnGate::opened().wait().await.is_ok());
    }
}
