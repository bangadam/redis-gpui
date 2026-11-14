//! Scriptable mock of `redis_io::Connection` — the headless stand-in for
//! the app's real `RedisConnection`.
//!
//! It follows the redis-io backend's sticky-broken contract
//! (`redis_backend.rs` module docs, points 2 and 4): after any step
//! returns a `Disconnected` error, every further call returns
//! `Disconnected` with `in_flight: None` — **nothing is sent** — until
//! [`ScriptedConnection::reconnect`] resets the mock. `reconnect` re-sends
//! no application commands (it mirrors `RedisConnection::reconnect`,
//! `redis_backend.rs` lines 421-423).
//!
//! The mock also keeps a **call log** so tests can prove "issued exactly
//! once" and "reconnect issued zero writes" by direct inspection.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use redis_io::connection::{Command, ConnError, Connection, Reply, ScanResult};
use redis_io::outcome::{DefinitiveError, DefinitiveErrorKind};

/// One scripted outcome for an `issue` call.
#[derive(Clone, Debug, PartialEq)]
pub enum MockStep {
    /// The server's reply.
    Reply(Reply),
    /// A definitive failure (e.g. `WRONGTYPE`).
    Definitive(String),
    /// The link drops before the reply: `ConnError::Disconnected`, then
    /// the mock is sticky-broken until `reconnect()`.
    Disconnect,
}

/// Shared call log: every `issue` records the exact command.
///
/// Shared (not borrowed) so a test can keep reading it while the mock is
/// owned by a [`SaveWorkflow`](crate::SaveWorkflow) or a
/// fault-injecting wrapper. Interior-mutable because `issue` takes
/// `&mut self` while the test holds another handle.
#[derive(Debug, Default)]
pub struct CallLog {
    /// Every command passed to `issue`, in order.
    issued: Mutex<Vec<Command>>,
}

impl CallLog {
    /// The number of `issue` calls so far.
    pub fn issue_count(&self) -> usize {
        self.issued.lock().map(|v| v.len()).unwrap_or(0)
    }

    /// The command at `index`, if issued.
    pub fn issued_at(&self, index: usize) -> Option<Command> {
        self.issued.lock().ok().and_then(|v| v.get(index).cloned())
    }

    /// Every issued command's name, in order (byte-safe — `Command`'s
    /// own Debug never shows argument bytes).
    pub fn issued_names(&self) -> Vec<String> {
        self.issued
            .lock()
            .map(|v| v.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }

    /// How many issued commands had this exact name.
    pub fn count_of(&self, name: &str) -> usize {
        self.issued
            .lock()
            .map(|v| v.iter().filter(|c| c.name == name).count())
            .unwrap_or(0)
    }

    fn record(&self, cmd: &Command) {
        if let Ok(mut v) = self.issued.lock() {
            v.push(cmd.clone());
        }
    }
}

/// Scriptable, call-counting mock connection.
pub struct ScriptedConnection {
    steps: Vec<MockStep>,
    log: Arc<CallLog>,
    severed: bool,
}

impl ScriptedConnection {
    /// A mock that pops `steps` in FIFO order (first step = first call).
    /// An empty script replies `Okay` to everything (happy-path server).
    pub fn new(steps: Vec<MockStep>) -> (Self, Arc<CallLog>) {
        let log = Arc::new(CallLog::default());
        (
            Self {
                steps,
                log: Arc::clone(&log),
                severed: false,
            },
            log,
        )
    }

    /// Explicit reconnect: fresh link, **re-sends nothing** (mirrors
    /// `RedisConnection::reconnect`, redis_backend.rs lines 421-423).
    pub fn reconnect(&mut self) {
        self.severed = false;
    }

    fn disconnected(reason: &str) -> ConnError {
        // Post-boundary shape: nothing was sent, so no in-flight command.
        // (The real backend attaches the in-flight command at the
        // boundary itself; the first step models that via MockStep::Disconnect.)
        ConnError::Disconnected {
            reason: Some(reason.into()),
            in_flight: None,
        }
    }
}

#[async_trait]
impl Connection for ScriptedConnection {
    async fn issue(&mut self, cmd: Command) -> Result<Reply, ConnError> {
        self.log.record(&cmd);
        if self.severed {
            return Err(Self::disconnected("mock: severed (sticky)"));
        }
        // Pop the next step first (avoids holding the borrow).
        let step = if self.steps.is_empty() {
            None
        } else {
            Some(self.steps.remove(0))
        };
        match step {
            Some(MockStep::Reply(r)) => Ok(r),
            Some(MockStep::Definitive(msg)) => Err(ConnError::Definitive(DefinitiveError::new(
                DefinitiveErrorKind::ServerError,
                msg,
            ))),
            Some(MockStep::Disconnect) => {
                self.severed = true;
                // The boundary itself: the command WAS sent (it is logged
                // above); the ack is lost. Model the real backend, which
                // attaches the in-flight command at the boundary.
                Err(ConnError::Disconnected {
                    reason: Some("mock: server closed link".into()),
                    in_flight: Some(cmd),
                })
            }
            // No script left: default to a plain OK (always-ok mock).
            None => Ok(Reply::Okay),
        }
    }

    async fn scan_batch(
        &mut self,
        _cursor: u64,
        _pattern: Option<Vec<u8>>,
        _count: u64,
    ) -> Result<ScanResult, ConnError> {
        if self.severed {
            return Err(Self::disconnected("mock: severed (sticky)"));
        }
        Ok(ScanResult {
            next_cursor: 0,
            keys: Vec::new(),
        })
    }

    async fn ping(&mut self) -> Result<(), ConnError> {
        if self.severed {
            return Err(Self::disconnected("mock: severed (sticky)"));
        }
        Ok(())
    }
}
