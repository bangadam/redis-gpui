//! The russh-backed SSH tunnel (feature `ssh`).
//!
//! # Transport choice (documented decision)
//!
//! redis-rs 1.8.1's connect path takes **addresses**, not arbitrary
//! `AsyncRead + AsyncWrite` streams, so wiring the russh
//! direct-tcpip channel directly as the Redis transport would mean
//! forking redis-rs' connect internals. Instead — the honest,
//! testable fallback — the tunnel opens a local `127.0.0.1:0` TCP
//! listener and pumps bytes bidirectionally between the accepted
//! connection and the SSH channel:
//!
//! ```text
//! redis-rs ──TCP──> 127.0.0.1:{local_port} ──pump──> russh channel ──SSH──> dest_host:dest_port
//! ```
//!
//! The pump runs on the tokio runtime until the tunnel is dropped.
//!
//! # TOFU flow (acceptance 5; the check lives here, the store in
//! connections-store)
//!
//! [`SshTunnel::connect`] takes the known-hosts store and the user's
//! pending decision:
//!
//! 1. russh's key exchange calls back into our handler's
//!    `check_server_key` with the server's public key BEFORE any
//!    session secret is established. We fingerprint it
//!    (SHA-256 of the wire-encoded key bytes) and ask the store:
//!    [`KnownHosts::verify`][connections_store::known_hosts::KnownHosts::verify].
//! 2. `Trusted` (or `Unseen` + user already said
//!    [`HostKeyDecision::Accept`], which records the key) → the
//!    handshake proceeds, auth runs, the channel opens, the listener
//!    binds. Result: [`HostKeyStatus::Connected`] + the local address.
//! 3. `Unseen`, no decision yet → the handshake is ABORTED (we return
//!    `false` from `check_server_key`, so no secret is ever
//!    established) and the caller gets
//!    [`HostKeyStatus::NeedsConfirmation`] with the fingerprint, plus
//!    a [`ConnEvent::HostKeyPrompt`] on the event stream. The caller
//!    re-connects with the user's decision.
//! 4. `Changed { new, old }` → same abort, but the result is
//!    [`HostKeyStatus::Blocked`] and a
//!    [`ConnEvent::HostKeyBlocked`]. **There is no override** — this
//!    is the anti-MITM hard block.
//!
//! [`ConnEvent::HostKeyPrompt`]: crate::connection::ConnEvent::HostKeyPrompt
//! [`ConnEvent::HostKeyBlocked`]: crate::connection::ConnEvent::HostKeyBlocked
//!
//! **Untested live (boundary tier, honesty):** the russh handshake,
//! auth (password / key-file / agent), the channel open, and the pump
//! against a real sshd are NOT exercised headless in this crate. What
//! IS tested (pure): TOFU verdicts, byte-exact fingerprints, config
//! shapes, event payloads — and `cargo check -p redis-io --features
//! ssh` keeps the whole path compiling.

use std::sync::Arc;

use russh::client::{self, Handle};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::{agent, PrivateKey, PublicKeyBase64, PublicKeyOrCertificate};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::connection::ConnEvent;
use crate::ssh::{
    HostKeyBlocked, HostKeyDecision, HostKeyPrompt, HostKeyStatus, HostKeyVerdict, SshAuth,
    SshTunnelConfig,
};
use connections_store::known_hosts::KnownHosts;

/// The live tunnel: a local listener pumped to the SSH channel.
///
/// Keep it alive for the connection's lifetime; dropping it drops the
/// channel halves (aborting the pump tasks) and the listener.
/// Debug shows only the local address (non-sensitive).
#[derive(Debug)]
pub struct SshTunnel {
    local_addr: std::net::SocketAddr,
    /// Kept so the event sender outlives construction and drops with
    /// the tunnel (advisory only; the connect result is authoritative).
    _events: mpsc::Sender<ConnEvent>,
}

impl SshTunnel {
    /// The local address the Redis client should connect to
    /// (`127.0.0.1:{ephemeral port}`).
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }
}

/// Errors establishing the tunnel. All messages are non-secret: they
/// name the failure mode and the auth mode, never credentials.
#[derive(Debug)]
pub enum TunnelError {
    /// TCP connect or SSH transport failure.
    Connect(String),
    /// Key exchange / host-key protocol failure.
    Handshake(String),
    /// Authentication failed (names the method, not the secret).
    Auth(String),
    /// direct-tcpip channel open failure.
    Channel(String),
    /// Local listener bind failure.
    Bind(String),
}

impl std::fmt::Display for TunnelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TunnelError::Connect(m) => write!(f, "SSH connect failed: {m}"),
            TunnelError::Handshake(m) => write!(f, "SSH handshake failed: {m}"),
            TunnelError::Auth(m) => write!(f, "SSH authentication failed: {m}"),
            TunnelError::Channel(m) => write!(f, "SSH channel open failed: {m}"),
            TunnelError::Bind(m) => write!(f, "local tunnel bind failed: {m}"),
        }
    }
}

impl std::error::Error for TunnelError {}

/// The verdict computed inside `check_server_key`, carried out of the
/// handler via a oneshot so the connect future can act on it.
#[derive(Debug)]
enum KeyCheck {
    /// Proceed with the handshake.
    Proceed,
    /// Abort: first-seen key, awaiting user confirmation.
    PauseUnseen(String),
    /// Abort: key changed — hard block.
    Block(HostKeyBlocked),
}

/// Establish the tunnel per the TOFU flow (see the module docs).
///
/// `decision` is the user's answer from a previous
/// [`HostKeyStatus::NeedsConfirmation`] result: `None` on a first
/// attempt. `Ok(HostKeyStatus)` is the connect outcome; `Err` is
/// reserved for transport failures.
pub async fn connect(
    config: &SshTunnelConfig,
    known_hosts: &KnownHosts,
    decision: Option<HostKeyDecision>,
    events: mpsc::Sender<ConnEvent>,
) -> Result<HostKeyStatus, TunnelError> {
    // The host-key check runs inside the kex callback (check_server_key
    // is called BEFORE any session secret is established). It needs
    // the store, the pending decision, and a way to report its verdict.
    let (verdict_tx, verdict_rx) = tokio::sync::oneshot::channel::<KeyCheck>();
    let handler = TunnelHandler {
        host: config.host.clone(),
        port: config.port,
        known_hosts: known_hosts.clone(),
        decision,
        verdict_tx: Some(verdict_tx),
    };

    let ssh_config = Arc::new(client::Config::default());
    let connect_result =
        client::connect(ssh_config, (config.host.as_str(), config.port), handler).await;

    // A `false` from check_server_key aborts kex → `client::connect`
    // resolves Err(UnknownKey) and the verdict (sent before returning
    // false) explains why. Read it first, then branch.
    let verdict = match verdict_rx.await {
        Ok(v) => v,
        Err(_) => {
            // Handler dropped without a verdict: transport-level
            // failure before host-key check. Fail closed.
            return connect_result
                .map(|_| unreachable!("connect succeeded without a host-key verdict"))
                .map_err(|e| TunnelError::Connect(format!("{e}")));
        }
    };

    let mut handle = match (verdict, connect_result) {
        (KeyCheck::Proceed, Ok(h)) => h,
        (KeyCheck::PauseUnseen(fpr), Err(_)) => {
            let _ = events.try_send(ConnEvent::HostKeyPrompt(HostKeyPrompt {
                host: config.host.clone(),
                port: config.port,
                fingerprint: fpr.clone(),
            }));
            return Ok(HostKeyStatus::NeedsConfirmation { fingerprint: fpr });
        }
        (KeyCheck::Block(b), Err(_)) => {
            let _ = events.try_send(ConnEvent::HostKeyBlocked(b.clone()));
            return Ok(HostKeyStatus::Blocked(b));
        }
        // Mismatched states cannot happen with our handler (verdict
        // Proceed ⇒ check returned true ⇒ connect Ok; anything else ⇒
        // Err). Fail closed rather than assume.
        _ => {
            return Err(TunnelError::Handshake(
                "host-key verdict/handshake state mismatch (fail closed)".into(),
            ))
        }
    };

    authenticate(&mut handle, config).await?;

    // Open the direct-tcpip channel to the ONE destination.
    let channel = handle
        .channel_open_direct_tcpip(
            config.dest_host.clone(),
            config.dest_port as u32,
            "127.0.0.1",
            0,
        )
        .await
        .map_err(|e| TunnelError::Channel(format!("open direct-tcpip: {e}")))?;

    // Bind the local listener the redis client connects to.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| TunnelError::Bind(format!("local listener: {e}")))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| TunnelError::Bind(format!("local addr: {e}")))?;

    let (chan_read, chan_write) = channel.split();
    tokio::spawn(pump(listener, chan_read, chan_write));

    Ok(HostKeyStatus::Connected(SshTunnel {
        local_addr,
        _events: events,
    }))
}

/// russh client handler: runs the TOFU check inside `check_server_key`.
///
/// Carries host/port (for prompt/block payloads and store lookups —
/// russh's callbacks don't provide them), the known-hosts store, the
/// user's pending decision, and a oneshot to report the verdict.
struct TunnelHandler {
    host: String,
    port: u16,
    known_hosts: KnownHosts,
    decision: Option<HostKeyDecision>,
    verdict_tx: Option<tokio::sync::oneshot::Sender<KeyCheck>>,
}

impl client::Handler for TunnelHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key_bytes = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.public_key_bytes(),
            PublicKeyOrCertificate::Certificate(_) => {
                // Host-key certificates are out of scope; fail closed.
                let _ = self.verdict_tx.take().map(|tx| {
                    tx.send(KeyCheck::Block(HostKeyBlocked {
                        host: String::new(),
                        port: 0,
                        new_fingerprint: String::new(),
                        old_fingerprint: String::new(),
                    }))
                });
                return Ok(false);
            }
        };
        let fingerprint = crate::ssh::fingerprint_of(&key_bytes);
        let verdict = match self.known_hosts.verify(&self.host, self.port, &fingerprint) {
            HostKeyVerdict::Trusted => KeyCheck::Proceed,
            HostKeyVerdict::Unseen(fpr) => match self.decision {
                // User already confirmed: record (TOFU) and proceed.
                Some(HostKeyDecision::Accept) => {
                    self.known_hosts.record(&self.host, self.port, &fingerprint);
                    KeyCheck::Proceed
                }
                // First attempt or explicit rejection: pause.
                Some(HostKeyDecision::Reject) => KeyCheck::Block(HostKeyBlocked {
                    host: self.host.clone(),
                    port: self.port,
                    new_fingerprint: fpr,
                    old_fingerprint: String::new(),
                }),
                None => KeyCheck::PauseUnseen(fpr),
            },
            HostKeyVerdict::Changed { new, old } => KeyCheck::Block(HostKeyBlocked {
                host: self.host.clone(),
                port: self.port,
                new_fingerprint: new,
                old_fingerprint: old,
            }),
        };
        let proceed = matches!(verdict, KeyCheck::Proceed);
        if let Some(tx) = self.verdict_tx.take() {
            let _ = tx.send(verdict);
        }
        Ok(proceed)
    }
}

/// TunnelHandler host/port accessors (fields, carried per-connect).
impl TunnelHandler {
    #[allow(dead_code)]
    fn host(&self) -> &str {
        &self.host
    }
}

/// Authenticate with the configured method. Agent auth is best-effort
/// (PLAN's Windows-agent risk): an unreachable agent degrades with a
/// clear error naming the fallback.
async fn authenticate(
    handle: &mut Handle<TunnelHandler>,
    config: &SshTunnelConfig,
) -> Result<(), TunnelError> {
    match &config.auth {
        SshAuth::Password(pw) => {
            let result = handle
                .authenticate_password(config.user.clone(), pw.reveal().to_string())
                .await
                .map_err(|e| TunnelError::Auth(format!("password auth: {e}")))?;
            if !result.success() {
                return Err(TunnelError::Auth(
                    "password auth rejected (check credentials)".into(),
                ));
            }
            Ok(())
        }
        SshAuth::KeyFile { path, passphrase } => {
            let pass = passphrase.as_ref().map(|p| p.reveal());
            let key: PrivateKey = russh::keys::load_secret_key(path, pass)
                .map_err(|e| TunnelError::Auth(format!("load key file: {e}")))?;
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
            let result = handle
                .authenticate_publickey(config.user.clone(), key)
                .await
                .map_err(|e| TunnelError::Auth(format!("publickey auth: {e}")))?;
            if !result.success() {
                return Err(TunnelError::Auth(
                    "publickey auth rejected (check the key is authorized)".into(),
                ));
            }
            Ok(())
        }
        SshAuth::Agent => {
            let mut agent_client =
                agent::client::AgentClient::connect_env()
                    .await
                    .map_err(|e| {
                        TunnelError::Auth(format!(
                            "SSH agent unavailable ({e}); use password or key-file auth instead"
                        ))
                    })?;
            let identities = agent_client
                .request_identities()
                .await
                .map_err(|e| TunnelError::Auth(format!("SSH agent identities: {e}")))?;
            let first = identities
                .into_iter()
                .next()
                .ok_or_else(|| TunnelError::Auth("SSH agent holds no identities".into()))?;
            let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = first else {
                return Err(TunnelError::Auth(
                    "SSH agent identity is a certificate (unsupported)".into(),
                ));
            };
            let result = handle
                .authenticate_publickey_with(config.user.clone(), key, None, &mut agent_client)
                .await
                .map_err(|e| TunnelError::Auth(format!("agent auth: {e}")))?;
            if !result.success() {
                return Err(TunnelError::Auth("SSH agent auth rejected".into()));
            }
            Ok(())
        }
    }
}

/// Relay bytes between the one accepted local connection and the
/// channel. One client at a time suffices: redis-rs'
/// MultiplexedConnection multiplexes over ONE TCP link.
async fn pump(
    listener: TcpListener,
    mut chan_read: russh::ChannelReadHalf,
    chan_write: russh::ChannelWriteHalf<russh::client::Msg>,
) {
    if let Ok((client, _)) = listener.accept().await {
        let (mut cr, mut cw) = client.into_split();
        // client -> channel
        let to_ssh = tokio::spawn(async move {
            let mut writer = chan_write.make_writer();
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match cr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if writer.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                        let _ = writer.flush().await;
                    }
                }
            }
        });
        // channel -> client
        let from_ssh = tokio::spawn(async move {
            let mut reader = chan_read.make_reader();
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                // The reader borrows chan_read; keep it alive for the loop.
                let n = match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if cw.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                let _ = cw.flush().await;
            }
        });
        let _ = to_ssh.await;
        let _ = from_ssh.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_error_display_is_non_sensitive() {
        let e = TunnelError::Auth("password auth rejected (check credentials)".into());
        let s = format!("{e}");
        assert!(s.contains("password auth rejected"));
        assert!(!s.contains("hunter2"));
    }
}
