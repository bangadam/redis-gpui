//! Fault-injection seam (feature `fault-injection`, OFF by default).
//!
//! This is one of the ONLY two adapter seams the PRD allows (the other is
//! the keyring seam in `connections-store`). It exists because
//! "the server's reply was lost around a disconnect" cannot be produced
//! deterministically at the high-level boundary — you'd have to race a
//! real server. Tests arm a fault on this wrapper instead.
//!
//! Because it is feature-gated and the feature is not default, it cannot
//! leak into release builds: `cargo build --release` (and any build
//! without `--features fault-injection`) compiles this file to nothing.

use async_trait::async_trait;

use crate::connection::{Command, ConnError, Connection, Reply, ScanResult};
use crate::outcome::DefinitiveError;
use crate::outcome::DefinitiveErrorKind;

/// Which nasty network behavior to inject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// Swallow the server's reply for matching commands, then SEVER the
    /// link: the wrapped connection sees a disconnect boundary right after
    /// the write may have landed. This deterministically tests
    /// acceptance 3 (dropped write ack → OutcomeUnknown, never resent).
    DropReplyThenDisconnect,
    /// Immediately sever the link (before sending).
    DisconnectNow,
    /// Fail with a definitive error (server-error style).
    DefinitiveError,
}

/// One armed fault: what to inject and on which commands.
#[derive(Clone, Debug)]
pub struct FaultSeam {
    /// What to do when triggered.
    pub kind: FaultKind,
    /// Uppercase command names the fault triggers on (empty = all).
    pub commands: Vec<String>,
}

impl FaultSeam {
    /// Arm a fault on specific command names.
    pub fn on(kind: FaultKind, commands: Vec<String>) -> Self {
        Self { kind, commands }
    }

    fn matches(&self, cmd: &Command) -> bool {
        self.commands.is_empty() || self.commands.iter().any(|c| c == &cmd.name)
    }
}

/// A `Connection` wrapper that injects armed faults.
///
/// Behavior for [`FaultKind::DropReplyThenDisconnect`]: the command is
/// forwarded to the inner connection (so it really is issued — the test
/// then checks it is NOT issued twice), the reply is discarded, and the
/// wrapper returns [`ConnError::Disconnected`] with `in_flight` set.
/// Subsequent calls also return `Disconnected` (nothing sent), mirroring
/// the real backend's sticky-broken state until an explicit reconnect.
#[derive(Debug)]
pub struct FaultInjectingConnection<C> {
    inner: C,
    seams: Vec<FaultSeam>,
    severed: bool,
}

impl<C: Connection> FaultInjectingConnection<C> {
    /// Wrap a connection with armed faults.
    pub fn new(inner: C, seams: Vec<FaultSeam>) -> Self {
        Self {
            inner,
            seams,
            severed: false,
        }
    }

    /// Access the wrapped connection (assertions on issue counts etc.).
    pub fn inner(&mut self) -> &mut C {
        &mut self.inner
    }

    fn trigger(&self, cmd: &Command) -> Option<&FaultSeam> {
        self.seams.iter().find(|s| s.matches(cmd))
    }

    fn disconnected(in_flight: Command) -> ConnError {
        ConnError::Disconnected {
            reason: Some("fault-injected disconnect".into()),
            in_flight: Some(in_flight),
        }
    }
}

#[async_trait]
impl<C: Connection> Connection for FaultInjectingConnection<C> {
    async fn issue(&mut self, cmd: Command) -> Result<Reply, ConnError> {
        if self.severed {
            // Sticky: nothing is sent after the boundary.
            return Err(ConnError::Disconnected {
                reason: Some("fault-injected disconnect (sticky)".into()),
                in_flight: None,
            });
        }
        if let Some(seam) = self.trigger(&cmd) {
            match seam.kind {
                FaultKind::DropReplyThenDisconnect => {
                    // Forward once (the write is really issued), swallow
                    // the reply, then sever.
                    let _ = self.inner.issue(cmd.clone()).await;
                    self.severed = true;
                    return Err(Self::disconnected(cmd));
                }
                FaultKind::DisconnectNow => {
                    self.severed = true;
                    return Err(Self::disconnected(cmd));
                }
                FaultKind::DefinitiveError => {
                    return Err(ConnError::Definitive(DefinitiveError::new(
                        DefinitiveErrorKind::ServerError,
                        "fault-injected server error",
                    )));
                }
            }
        }
        self.inner.issue(cmd).await
    }

    async fn scan_batch(
        &mut self,
        cursor: u64,
        pattern: Option<Vec<u8>>,
        count: u64,
    ) -> Result<ScanResult, ConnError> {
        if self.severed {
            return Err(ConnError::Disconnected {
                reason: Some("fault-injected disconnect (sticky)".into()),
                in_flight: None,
            });
        }
        self.inner.scan_batch(cursor, pattern, count).await
    }

    async fn ping(&mut self) -> Result<(), ConnError> {
        if self.severed {
            return Err(ConnError::Disconnected {
                reason: Some("fault-injected disconnect (sticky)".into()),
                in_flight: None,
            });
        }
        self.inner.ping().await
    }
}
