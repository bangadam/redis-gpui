//! The `Connection` trait: the adapter seam between the app and the Redis
//! client library (PLAN.md §3-4, and the only fault-injection seam for
//! dropped acknowledgments per PLAN.md §7).
//!
//! # Design choices (documented, as required)
//!
//! - **`async_trait`** is used instead of hand-rolled `BoxFuture` returns:
//!   one small dependency, and it keeps the trait object-safe and pleasant
//!   to implement for both the real backend and `MockConnection`.
//! - **Events via a channel, not polling.** A connection holds an
//!   `mpsc::Sender<ConnEvent>` supplied at construction; the app layer
//!   (Phase 2) drains it to drive gate resets and UI state. This matches
//!   PLAN.md §5 ("results return to the entity via an event channel").
//!   Rationale: a poll method would need the UI to own a timer; a channel
//!   surfaces `Disconnected` even while no command is in flight.
//! - **The reconnect trap (critical, see module docs of [`crate`])**:
//!   [`RedisConnection`][`crate::connection::redis_backend::RedisConnection`]
//!   does NOT use redis-rs' auto-reconnecting `ConnectionManager`, because
//!   it can replay in-flight commands after a reconnect boundary. It uses
//!   `MultiplexedConnection` (no built-in reconnect) and translates any
//!   connection-dropped error into [`ConnError::Disconnected`] with the
//!   in-flight command attached. Reconnection is explicit
//!   ([`RedisConnection::reconnect`]) and never re-sends anything.
//!
//! # Byte safety
//!
//! [`Command`] args are `Vec<u8>`, not `String`: keys and values may be
//! non-UTF-8. [`Reply`] mirrors redis-rs' `Value` but is owned by us, so a
//! redis-rs upgrade cannot silently change the app-facing shape.

use std::fmt;

use async_trait::async_trait;

use crate::outcome::DefinitiveError;

/// A pre-encoded Redis command: name + raw byte arguments.
///
/// The name is ASCII by Redis protocol rules, so it is a `String`; every
/// argument (keys, values, patterns, counts when textual) is raw bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Command {
    /// Uppercase command name, e.g. `"SET"`.
    pub name: String,
    /// Raw arguments; may contain non-UTF-8 bytes.
    pub args: Vec<Vec<u8>>,
}

impl Command {
    /// Build a command from a name and raw byte arguments.
    pub fn new(name: impl Into<String>, args: Vec<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            args,
        }
    }
}

impl fmt::Debug for Command {
    /// Byte-safe Debug: shows the command name and argument *lengths* only.
    /// Key/value bytes never reach logs or `dbg!` output (log hygiene).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("name", &self.name)
            .field(
                "arg_bytes",
                &self.args.iter().map(|a| a.len()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Reply from the server, mirroring the RESP model with bytes preserved.
///
/// Only the variants the app consumes are modeled; unknown/extension
/// replies decode to [`Reply::Other`] with a non-sensitive label.
#[derive(Clone, PartialEq)]
pub enum Reply {
    /// Server nil (`NIL` in RESP3, null bulk in RESP2).
    Nil,
    /// Integer reply.
    Int(i64),
    /// Binary-safe bulk string (or simple string decoded to bytes).
    Bulk(Vec<u8>),
    /// `OK` status.
    Okay,
    /// Array of replies.
    Array(Vec<Reply>),
    /// Double (RESP3).
    Double(f64),
    /// Boolean (RESP3).
    Boolean(bool),
    /// Map (RESP3), e.g. from `INFO`-style commands.
    Map(Vec<(Reply, Reply)>),
    /// Anything else, labeled without payload bytes.
    Other(String),
}

impl fmt::Debug for Reply {
    /// Byte-safe Debug: bulk payloads print only their length.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reply::Nil => write!(f, "Reply::Nil"),
            Reply::Int(i) => write!(f, "Reply::Int({i})"),
            Reply::Bulk(b) => write!(f, "Reply::Bulk({} bytes)", b.len()),
            Reply::Okay => write!(f, "Reply::Okay"),
            Reply::Array(items) => {
                write!(f, "Reply::Array({})", items.len())
            }
            Reply::Double(d) => write!(f, "Reply::Double({d})"),
            Reply::Boolean(b) => write!(f, "Reply::Boolean({b})"),
            Reply::Map(entries) => write!(f, "Reply::Map({} entries)", entries.len()),
            Reply::Other(label) => write!(f, "Reply::Other({label})"),
        }
    }
}

impl Reply {
    /// Borrow a bulk payload as bytes, if this reply is bulk.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Reply::Bulk(b) => Some(b),
            _ => None,
        }
    }
}

/// One SCAN batch: the next cursor (0 = iteration complete) and the keys
/// found in this batch, as raw bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanResult {
    /// Cursor for the next `SCAN` call; `0` means "iteration complete".
    pub next_cursor: u64,
    /// Keys in this batch (raw bytes, possibly non-UTF-8).
    pub keys: Vec<Vec<u8>>,
}

/// Error from issuing a command.
///
/// The distinction that matters (acceptance 3): [`ConnError::Disconnected`]
/// means the command was (or may have been) sent and the acknowledgment
/// never arrived → the call site MUST map this to
/// [`crate::outcome::WriteOutcome::OutcomeUnknown`] and MUST NOT re-send.
/// [`ConnError::Definitive`] means nothing is uncertain.
#[derive(Clone)]
pub enum ConnError {
    /// Definitive failure (server error, invalid command, auth, ...).
    /// Safe to display; contains no key/value bytes.
    Definitive(DefinitiveError),
    /// The link dropped before a reply arrived. Carries the in-flight
    /// command when the layer knows it (the wrapper always does).
    Disconnected {
        /// Non-sensitive reason hint, if known.
        reason: Option<String>,
        /// The command whose ack is lost, if recorded by the layer.
        in_flight: Option<Command>,
    },
}

impl fmt::Debug for ConnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnError::Definitive(e) => f.debug_tuple("ConnError::Definitive").field(e).finish(),
            ConnError::Disconnected { reason, .. } => f
                .debug_struct("ConnError::Disconnected")
                .field("reason", reason)
                .field("in_flight", &"[Command]")
                .finish(),
        }
    }
}

impl fmt::Display for ConnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnError::Definitive(e) => write!(f, "{e}"),
            ConnError::Disconnected { reason, .. } => {
                write!(f, "connection lost before reply arrived")?;
                if let Some(r) = reason {
                    write!(f, ": {r}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConnError {}

impl From<DefinitiveError> for ConnError {
    fn from(e: DefinitiveError) -> Self {
        ConnError::Definitive(e)
    }
}

/// Events surfaced by a live connection through its event channel.
///
/// Phase 2 wires these to the app's connection sessions (gate resets on
/// `Disconnected`, etc.). `CommandDone` is informational (observability);
/// the command's actual result travels through the future returned by
/// [`Connection::issue`].
///
/// Phase 6 adds the security surfacing events (plain data, serializable
/// for the UI):
/// - [`ConnEvent::TlsWarning`] — emitted when a connection with the
///   insecure TLS override is established, and *re-emitted on every
///   successful (re)connect* while that mode is active, so the warning
///   is persistent state, not a one-shot log line (PRD acceptance 5).
/// - [`ConnEvent::HostKeyPrompt`] — first-seen SSH host key: the tunnel
///   is PAUSED awaiting explicit user confirmation of the fingerprint.
/// - [`ConnEvent::HostKeyBlocked`] — SSH host key changed: the connection
///   is hard-blocked (no override exists, by design).
#[derive(Clone, Debug)]
pub enum ConnEvent {
    /// The connection became (re)established and authenticated.
    Connected,
    /// The link went down. `reason` is non-sensitive.
    Disconnected(Option<String>),
    /// A command completed with a byte-safe outcome summary.
    CommandDone(u64, Result<ReplySummary, ConnError>),
    /// TLS verification is disabled on this connection (insecure
    /// override). Emitted at establish and re-emitted on each reconnect
    /// while the mode is active; the UI renders it permanently.
    TlsWarning(crate::tls::TlsWarning),
    /// SSH first-use host key awaiting confirmation. The tunnel does not
    /// proceed until the user confirms (the connect future resolves to
    /// [`HostKeyStatus::NeedsConfirmation`] and the caller retries with
    /// an explicit decision).
    HostKeyPrompt(crate::ssh::HostKeyPrompt),
    /// SSH host key changed since the recorded one — connection blocked.
    HostKeyBlocked(crate::ssh::HostKeyBlocked),
}

/// Byte-safe one-line summary of a reply for event consumers.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplySummary {
    /// Nil reply.
    Nil,
    /// Integer reply.
    Int(i64),
    /// Bulk payload of N bytes (content omitted).
    Bulk(usize),
    /// OK status.
    Okay,
    /// Array of N items.
    Array(usize),
    /// Other typed reply, labeled.
    Other(String),
}

impl From<&Reply> for ReplySummary {
    fn from(r: &Reply) -> Self {
        match r {
            Reply::Nil => ReplySummary::Nil,
            Reply::Int(i) => ReplySummary::Int(*i),
            Reply::Bulk(b) => ReplySummary::Bulk(b.len()),
            Reply::Okay => ReplySummary::Okay,
            Reply::Array(items) => ReplySummary::Array(items.len()),
            Reply::Double(_) => ReplySummary::Other("double".into()),
            Reply::Boolean(_) => ReplySummary::Other("boolean".into()),
            Reply::Map(m) => ReplySummary::Other(format!("map:{}", m.len())),
            Reply::Other(l) => ReplySummary::Other(l.clone()),
        }
    }
}

/// The adapter seam: everything the app does to a Redis server goes through
/// this trait, which is also where fault injection hooks in (feature-gated).
///
/// Implementations:
/// - [`crate::connection::redis_backend::RedisConnection`] (real, redis-rs)
/// - `MockConnection` in tests (and Phase 2's app-workflows harness)
/// - `FaultInjectingConnection` (feature `fault-injection` only)
#[async_trait]
pub trait Connection: Send {
    /// Issue a single command and await its reply.
    ///
    /// If the link drops before the reply arrives, returns
    /// [`ConnError::Disconnected`] with `in_flight` set. Implementations
    /// MUST NOT re-send the command on their own.
    async fn issue(&mut self, cmd: Command) -> Result<Reply, ConnError>;

    /// Issue one `SCAN` batch. `pattern` is the raw `MATCH` glob (no
    /// escaping performed); `count` is the `COUNT` hint (server may ignore).
    /// A zero cursor starts a new iteration; a returned zero cursor ends it.
    async fn scan_batch(
        &mut self,
        cursor: u64,
        pattern: Option<Vec<u8>>,
        count: u64,
    ) -> Result<ScanResult, ConnError>;

    /// Round-trip health check (`PING`).
    async fn ping(&mut self) -> Result<(), ConnError>;
}
