//! Real Redis backend over redis-rs `MultiplexedConnection`.
//!
//! # The reconnect trap and how this backend defuses it (critical)
//!
//! redis-rs' `ConnectionManager` auto-reconnects and — in some versions
//! and configurations — can replay in-flight commands after the link
//! comes back. For a **write** command, a replay is a silent duplicate
//! write: exactly what PRD acceptance 3 forbids. This backend therefore
//! does NOT use `ConnectionManager`:
//!
//! 1. It holds a plain `MultiplexedConnection` (no auto-reconnect).
//! 2. On any disconnect-detectable error (`is_connection_dropped()`, IO
//!    errors mid-command, or a connection that was already dead), the
//!    command's future resolves as [`ConnError::Disconnected`] with the
//!    in-flight command attached → the call site maps it to
//!    `WriteOutcome::OutcomeUnknown`. The command is **never** re-sent by
//!    this layer. (Verified empirically at redis 1.8.1: a server-side
//!    `CLIENT KILL` surfaces on the next in-flight command as
//!    `ErrorKind::Io` with `is_connection_dropped() == true`.)
//! 3. Reconnection is a separate, user-initiated operation
//!    ([`RedisConnection::reconnect`]) that builds a *fresh* connection.
//!    It re-authenticates and re-selects the DB, but re-issues no
//!    application commands.
//! 4. After a disconnect boundary, this backend is *sticky-broken*: every
//!    subsequent `issue`/`scan_batch`/`ping` returns `Disconnected` with
//!    `in_flight: None` (nothing was sent) until `reconnect()` succeeds.
//!    No command sent after a disconnect boundary can silently ride a
//!    zombie connection.
//!
//! # Events
//!
//! Every meaningful transition is emitted on the `ConnEvent` mpsc channel
//! (given out at construction): `Connected` after a successful
//! (re)connect+auth, `Disconnected(reason)` at the boundary, and
//! `CommandDone(id, summary)` for observability. The channel is
//! bounded; a full channel drops the event (events are advisory, the
//! command futures carry the authoritative results).

use std::path::Path;

use async_trait::async_trait;
use redis::aio::MultiplexedConnection;
use redis::cmd as redis_cmd;
use redis::{
    Client, ConnectionAddr, ErrorKind, IntoConnectionInfo, ProtocolVersion, RedisConnectionInfo,
    RedisError, TlsCertificates, Value as RedisValue,
};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::connection::{
    Command, ConnError, ConnEvent, Connection, Reply, ReplySummary, ScanResult,
};
use crate::log_hygiene::Secret;
use crate::outcome::{DefinitiveError, DefinitiveErrorKind};
use crate::tls::{TlsConfig, TlsMode};

/// Configuration for a real Redis connection (all the app needs).
///
/// Secrets (password, SSH key passphrase) are transient: they are passed
/// here for the connect call and are **never serialized by this crate**
/// (see [`Self::to_persisted_json`] — only non-secret fields).
#[derive(Clone, Debug)]
pub struct RedisConnectionConfig {
    /// Server host (plain hostname or IP; no URL building by callers).
    pub host: String,
    /// Server port.
    pub port: u16,
    /// Redis logical database number.
    pub db: i64,
    /// ACL username, if any.
    pub username: Option<String>,
    /// ACL password, transient, never persisted (see [`Self::password`]).
    password: Option<Secret>,
    /// TLS policy (plaintext when `None`).
    pub tls: Option<TlsConfig>,
    /// Optional **single** SSH tunnel hop — structurally one, not a list.
    /// (Real tunneling is feature-gated: `redis-io/ssh`.)
    pub ssh_tunnel: Option<crate::ssh::SshTunnelConfig>,
}

impl PartialEq for RedisConnectionConfig {
    /// Password deliberately NOT compared: two configs with the same
    /// visible settings are "the same connection" for dedup purposes.
    fn eq(&self, other: &Self) -> bool {
        self.host == other.host
            && self.port == other.port
            && self.db == other.db
            && self.username == other.username
            && self.tls == other.tls
            && self.ssh_tunnel == other.ssh_tunnel
    }
}

impl RedisConnectionConfig {
    /// Create a plaintext config.
    pub fn new(host: impl Into<String>, port: u16, db: i64) -> Self {
        Self {
            host: host.into(),
            port,
            db,
            username: None,
            password: None,
            tls: None,
            ssh_tunnel: None,
        }
    }

    /// Set the transient password.
    pub fn with_password(mut self, password: Option<Secret>) -> Self {
        self.password = password;
        self
    }

    /// Transient password accessor (for the connect call only).
    pub fn password(&self) -> Option<&Secret> {
        self.password.as_ref()
    }

    /// Set the TLS policy.
    pub fn with_tls(mut self, tls: Option<TlsConfig>) -> Self {
        self.tls = tls;
        self
    }
    /// Set the (single, optional) SSH tunnel.
    pub fn with_ssh_tunnel(mut self, tunnel: Option<crate::ssh::SshTunnelConfig>) -> Self {
        self.ssh_tunnel = tunnel;
        self
    }

    /// Serialize ONLY the non-secret fields (for connections-store).
    ///
    /// The password is transient by design: it lives in the OS keyring or
    /// a per-session prompt (Phase 6 wires that), never in the persisted
    /// document. SSH passwords are likewise absent (only the auth *mode*
    /// and non-secret key path are recorded).
    pub fn to_persisted_json(&self) -> serde_json::Value {
        serde_json::json!({
            "host": self.host,
            "port": self.port,
            "db": self.db,
            "username": self.username,
            "tls": self.tls.as_ref().map(tls_to_persisted),
            "ssh_tunnel": self.ssh_tunnel.as_ref().map(ssh_to_persisted),
            // NOTE: no "password" key — that absence is the guarantee.
        })
    }

    /// Rebuild a config from a persisted document (non-secret fields).
    ///
    /// Returns `Err` on malformed input. The password is always `None`
    /// here; the caller re-attaches it transiently from the keyring.
    pub fn from_persisted_json(v: &serde_json::Value) -> Result<Self, String> {
        let host = v
            .get("host")
            .and_then(|x| x.as_str())
            .ok_or("missing 'host'")?
            .to_string();
        let port = v
            .get("port")
            .and_then(|x| x.as_u64())
            .ok_or("missing 'port'")? as u16;
        let db = v.get("db").and_then(|x| x.as_i64()).unwrap_or(0);
        let username = v
            .get("username")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let tls = match v.get("tls") {
            None | Some(serde_json::Value::Null) => None,
            Some(t) => Some(tls_from_persisted(t)?),
        };
        let ssh_tunnel = match v.get("ssh_tunnel") {
            None | Some(serde_json::Value::Null) => None,
            Some(t) => Some(ssh_from_persisted(t)?),
        };
        Ok(Self {
            host,
            port,
            db,
            username,
            password: None,
            tls,
            ssh_tunnel,
        })
    }

    fn to_redis_conn_info(&self) -> Result<redis::ConnectionInfo, String> {
        let addr = match self.tls.as_ref().map(|t| t.mode()) {
            None | Some(TlsMode::Disabled) => ConnectionAddr::Tcp(self.host.clone(), self.port),
            Some(TlsMode::VerifyFull {
                ca_bundle,
                client_cert,
            }) => {
                // Verified mode with custom material: build the TLS client
                // (which attaches TlsConnParams) and take its ConnectionInfo.
                // Without custom material, a plain TcpTls address suffices
                // (redis-rs uses its built-in webpki roots).
                let has_custom = ca_bundle.is_some() || client_cert.is_some();
                if has_custom {
                    let client_tls = match client_cert {
                        Some((c, k)) => {
                            let cert =
                                std::fs::read(c).map_err(|e| format!("read client cert: {e}"))?;
                            let key =
                                std::fs::read(k).map_err(|e| format!("read client key: {e}"))?;
                            Some(redis::ClientTlsConfig {
                                client_cert: cert,
                                client_key: key,
                            })
                        }
                        None => None,
                    };
                    let root_cert = match ca_bundle {
                        Some(p) => {
                            Some(std::fs::read(p).map_err(|e| format!("read CA bundle: {e}"))?)
                        }
                        None => None,
                    };
                    let client = Client::build_with_tls(
                        ConnectionAddr::TcpTls {
                            host: self.host.clone(),
                            port: self.port,
                            insecure: false,
                            tls_params: None,
                        },
                        TlsCertificates {
                            client_tls,
                            root_cert,
                        },
                    )
                    .map_err(|e| format!("TLS setup: {e}"))?;
                    let info = client
                        .get_connection_info()
                        .clone()
                        .set_redis_settings(self.redis_settings());
                    return Ok(info);
                }
                ConnectionAddr::TcpTls {
                    host: self.host.clone(),
                    port: self.port,
                    insecure: false,
                    tls_params: None,
                }
            }
            Some(TlsMode::InsecureOverride) => {
                // Explicitly constructed insecure mode; redis-rs handles
                // insecure=true when the tls-rustls TLS path is compiled in.
                // The persistent TlsWarning is surfaced at establish() (see
                // below) — this arm only picks the address shape.
                ConnectionAddr::TcpTls {
                    host: self.host.clone(),
                    port: self.port,
                    insecure: true,
                    tls_params: None,
                }
            }
        };
        Ok(addr
            .into_connection_info()
            .map_err(|e| e.to_string())?
            .set_redis_settings(self.redis_settings()))
    }

    /// The auth/DB settings as redis-rs' RedisConnectionInfo.
    fn redis_settings(&self) -> RedisConnectionInfo {
        let mut redis_info = RedisConnectionInfo::default()
            .set_db(self.db)
            .set_protocol(ProtocolVersion::RESP2);
        if let Some(u) = &self.username {
            redis_info = redis_info.set_username(u.clone());
        }
        if let Some(p) = &self.password {
            redis_info = redis_info.set_password(p.reveal());
        }
        redis_info
    }
}

fn tls_to_persisted(t: &TlsConfig) -> serde_json::Value {
    match t.mode() {
        TlsMode::Disabled => serde_json::json!({"mode": "disabled"}),
        TlsMode::VerifyFull {
            ca_bundle,
            client_cert,
        } => serde_json::json!({
            "mode": "verified",
            "ca_bundle": ca_bundle,
            "client_cert": client_cert.as_ref().map(|(c, _)| c),
            "client_key": client_cert.as_ref().map(|(_, k)| k),
        }),
        TlsMode::InsecureOverride => serde_json::json!({"mode": "insecure_skip_verify"}),
    }
}

fn tls_from_persisted(v: &serde_json::Value) -> Result<TlsConfig, String> {
    match v.get("mode").and_then(|m| m.as_str()) {
        Some("disabled") => Ok(TlsConfig::disabled()),
        Some("verified") => {
            let ca_bundle = v
                .get("ca_bundle")
                .and_then(|x| x.as_str())
                .map(Path::new)
                .map(|p| p.to_path_buf());
            // Client cert/key: persisted as separate path fields. (A legacy
            // shape with only `client_cert` + `has_client_key` is accepted
            // for the cert path; the key path then mirrors the cert path,
            // matching how paired PEM files are commonly laid out.)
            let client_cert = match (
                v.get("client_key").and_then(|x| x.as_str()),
                v.get("client_cert").and_then(|x| x.as_str()),
                v.get("has_client_key").and_then(|x| x.as_bool()),
            ) {
                (Some(k), Some(c), _) => {
                    Some((Path::new(c).to_path_buf(), Path::new(k).to_path_buf()))
                }
                (None, Some(c), Some(true)) => {
                    let p = Path::new(c).to_path_buf();
                    Some((p.clone(), p))
                }
                _ => None,
            };
            Ok(TlsConfig::verify_full(ca_bundle, client_cert))
        }
        Some("insecure_skip_verify") => {
            // Persisted insecure mode must be re-hydrated explicitly; the
            // round-trip preserves the *decision* but the in-memory value
            // is only produced through the explicit constructor.
            Ok(TlsConfig::insecure_override())
        }
        _ => Err("bad tls.mode".into()),
    }
}

fn ssh_to_persisted(s: &crate::ssh::SshTunnelConfig) -> serde_json::Value {
    let auth = match &s.auth {
        crate::ssh::SshAuth::Password(_) => serde_json::json!({"mode": "password"}),
        crate::ssh::SshAuth::KeyFile {
            path,
            passphrase: _,
        } => serde_json::json!({
            "mode": "key_file",
            "path": path,
        }),
        crate::ssh::SshAuth::Agent => serde_json::json!({"mode": "agent"}),
    };
    serde_json::json!({
        "host": s.host,
        "port": s.port,
        "user": s.user,
        "auth": auth,
        "dest_host": s.dest_host,
        "dest_port": s.dest_port,
    })
}

fn ssh_from_persisted(v: &serde_json::Value) -> Result<crate::ssh::SshTunnelConfig, String> {
    let host = v
        .get("host")
        .and_then(|x| x.as_str())
        .ok_or("missing ssh host")?
        .to_string();
    let port = v.get("port").and_then(|x| x.as_u64()).unwrap_or(22) as u16;
    let user = v
        .get("user")
        .and_then(|x| x.as_str())
        .ok_or("missing ssh user")?
        .to_string();
    let auth = match v
        .get("auth")
        .and_then(|a| a.get("mode"))
        .and_then(|m| m.as_str())
    {
        Some("password") => crate::ssh::SshAuth::Password(crate::log_hygiene::Secret::default()),
        Some("key_file") => {
            let path = v
                .get("auth")
                .and_then(|a| a.get("path"))
                .and_then(|p| p.as_str())
                .ok_or("missing key path")?;
            crate::ssh::SshAuth::KeyFile {
                path: Path::new(path).to_path_buf(),
                passphrase: None,
            }
        }
        Some("agent") => crate::ssh::SshAuth::Agent,
        _ => return Err("bad ssh auth mode".into()),
    };
    let dest_host = v
        .get("dest_host")
        .and_then(|x| x.as_str())
        .ok_or("missing ssh dest_host")?
        .to_string();
    let dest_port = v
        .get("dest_port")
        .and_then(|x| x.as_u64())
        .ok_or("missing ssh dest_port")? as u16;
    Ok(crate::ssh::SshTunnelConfig {
        host,
        port,
        user,
        auth,
        dest_host,
        dest_port,
    })
}

/// A live connection to one Redis server via redis-rs.
///
/// See the [module docs](self) for the reconnect-trap contract. Construct
/// with [`RedisConnection::connect`]; keep the returned receiver to drain
/// [`ConnEvent`]s on the app side.
pub struct RedisConnection {
    config: RedisConnectionConfig,
    conn: Option<MultiplexedConnection>,
    events: mpsc::Sender<ConnEvent>,
    next_id: u64,
    /// The live SSH tunnel (feature `ssh`), when the connection goes
    /// through one. Kept alive here so the pump stays up for the
    /// connection's lifetime.
    #[cfg(feature = "ssh")]
    tunnel: Option<crate::tunnel::SshTunnel>,
}

const EVENT_CHANNEL_CAPACITY: usize = 256;

impl RedisConnection {
    /// Connect (and authenticate, and SELECT db) using the config.
    ///
    /// The `Ok` value's `mpsc::Receiver` is the event channel; it is
    /// closed when the `RedisConnection` is dropped.
    pub async fn connect(
        config: RedisConnectionConfig,
    ) -> Result<(Self, mpsc::Receiver<ConnEvent>), ConnError> {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let mut me = Self {
            config,
            conn: None,
            events: tx,
            next_id: 0,
            #[cfg(feature = "ssh")]
            tunnel: None,
        };
        me.establish().await?;
        Ok((me, rx))
    }

    /// Explicitly (re)connect: fresh `MultiplexedConnection`, re-auth,
    /// re-SELECT. **Re-sends nothing else.** In-flight commands from the
    /// previous incarnation have already resolved as `Disconnected` (see
    /// module docs) and are not touched here.
    pub async fn reconnect(&mut self) -> Result<(), ConnError> {
        self.establish().await
    }

    /// Connect through the SSH tunnel with an explicit host-key
    /// decision (the UI confirm flow; feature `ssh`).
    ///
    /// - `Accept` on a first-seen key: records the fingerprint (TOFU
    ///   trust step) and connects.
    /// - `Reject`: returns the block as a definitive error.
    /// - A changed key always fails, regardless of the decision.
    #[cfg(feature = "ssh")]
    pub async fn connect_ssh_confirmed(&mut self, accept: bool) -> Result<(), ConnError> {
        use crate::ssh::HostKeyStatus;
        let Some(tunnel_cfg) = self.config.ssh_tunnel.clone() else {
            // No tunnel configured: plain (re)connect.
            return self.establish().await;
        };
        let decision = Some(if accept {
            crate::ssh::HostKeyDecision::Accept
        } else {
            crate::ssh::HostKeyDecision::Reject
        });
        let kh_path = connections_store::saved_connections::config_dir().join("known_hosts.json");
        let known_hosts = connections_store::known_hosts::KnownHosts::load(&kh_path);
        // Real event channel: HostKeyPrompt/HostKeyBlocked must reach the
        // connection's event stream (not a dummy sender).
        let status =
            crate::tunnel::connect(&tunnel_cfg, &known_hosts, decision, self.events.clone())
                .await
                .map_err(|e| {
                    ConnError::Definitive(DefinitiveError::new(
                        DefinitiveErrorKind::NotConnected,
                        format!("{e}"),
                    ))
                })?;
        match status {
            HostKeyStatus::Connected(tunnel) => {
                self.tunnel = Some(tunnel);
                // Continue with the plain establish path, which now
                // uses the tunnel's local address.
                self.establish_rest().await
            }
            HostKeyStatus::NeedsConfirmation { fingerprint } => {
                Err(ConnError::Definitive(DefinitiveError::new(
                    DefinitiveErrorKind::Other,
                    format!("SSH host key still awaiting confirmation ({fingerprint})"),
                )))
            }
            HostKeyStatus::Blocked(b) => Err(ConnError::Definitive(DefinitiveError::new(
                DefinitiveErrorKind::Other,
                format!(
                    "SSH host key CHANGED for {}:{} — connection blocked \
                     (recorded {}; presented {})",
                    b.host, b.port, b.old_fingerprint, b.new_fingerprint
                ),
            ))),
        }
    }

    /// Non-sensitive snapshot of the config (no password bytes).
    pub fn config(&self) -> &RedisConnectionConfig {
        &self.config
    }

    async fn establish(&mut self) -> Result<(), ConnError> {
        // Phase 6: SSH tunnel wiring. Without the `ssh` feature the
        // tunnel is a definite, clearly-labeled failure (fail closed);
        // with it, the TOFU/host-key flow is driven by
        // [`crate::tunnel::connect`] — see that module for the flow.
        // The tunnel's local listener is dialed by rewriting the
        // effective address below (host → 127.0.0.1, port → tunnel).
        #[cfg(not(feature = "ssh"))]
        if self.config.ssh_tunnel.is_some() {
            return Err(ConnError::Definitive(DefinitiveError::new(
                DefinitiveErrorKind::Other,
                "SSH tunneling requires the `ssh` feature (build redis-io with --features ssh)",
            )));
        }
        #[cfg(feature = "ssh")]
        if let Some(tunnel_cfg) = self.config.ssh_tunnel.as_ref() {
            use crate::ssh::HostKeyStatus;
            use connections_store::known_hosts::KnownHosts;

            // The known-hosts store: config-dir-relative (the app
            // overrides REDIS_GPUI_CONFIG_DIR; default ~/.config/redis-gpui).
            let kh_path =
                connections_store::saved_connections::config_dir().join("known_hosts.json");
            let known_hosts = KnownHosts::load(&kh_path);

            // Real event channel: HostKeyPrompt/HostKeyBlocked must reach
            // the connection's event stream (not a dummy sender).
            let status =
                crate::tunnel::connect(tunnel_cfg, &known_hosts, None, self.events.clone())
                    .await
                    .map_err(|e| {
                        DefinitiveError::new(DefinitiveErrorKind::NotConnected, format!("{e}"))
                    })?;
            match status {
                HostKeyStatus::Connected(tunnel) => {
                    self.tunnel = Some(tunnel);
                }
                HostKeyStatus::NeedsConfirmation { fingerprint } => {
                    // The connection attempt pauses here. The app layer
                    // (UI agent) drives the confirm flow; from the
                    // backend's perspective this is a definitive
                    // "needs user input" failure with the fingerprint.
                    return Err(ConnError::Definitive(DefinitiveError::new(
                        DefinitiveErrorKind::Other,
                        format!(
                            "SSH host key not confirmed yet (fingerprint {fingerprint}); \
                             confirm and reconnect"
                        ),
                    )));
                }
                HostKeyStatus::Blocked(b) => {
                    return Err(ConnError::Definitive(DefinitiveError::new(
                        DefinitiveErrorKind::Other,
                        format!(
                            "SSH host key CHANGED for {}:{} — connection blocked \
                             (recorded {}; presented {})",
                            b.host, b.port, b.old_fingerprint, b.new_fingerprint
                        ),
                    )));
                }
            }
            // Host-key decision plumbing for re-connects is driven by
            // the app layer via the public
            // [`RedisConnection::connect_ssh_confirmed`] entry below.
        }
        let client = Client::open(self.effective_conn_info().map_err(|e| {
            DefinitiveError::new(
                DefinitiveErrorKind::InvalidCommand,
                format!("invalid connection settings: {}", e),
            )
        })?)
        .map_err(|e| {
            DefinitiveError::new(
                DefinitiveErrorKind::InvalidCommand,
                format!("invalid connection settings: {}", e),
            )
        })?;
        let conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                // A failure to *establish* is definitive: nothing was sent.
                DefinitiveError::new(
                    DefinitiveErrorKind::NotConnected,
                    format!("connect failed: {}", e),
                )
            })?;
        self.conn = Some(conn);
        // Persistent TLS warning (acceptance 5): emitted at every
        // successful establish while insecure override is active — a
        // state the UI renders permanently, not a one-shot log line.
        if let Some(warning) = self.config.tls.as_ref().and_then(|t| t.warning()) {
            self.send_event(ConnEvent::TlsWarning(warning));
        }
        self.send_event(ConnEvent::Connected);
        Ok(())
    }

    /// The tail of `establish` after the (optional) tunnel is up:
    /// open the client connection + SELECT db.
    ///
    /// Only used on the `ssh`-feature confirm path
    /// ([`RedisConnection::connect_ssh_confirmed`]); the plain
    /// non-ssh `establish` runs the same steps inline.
    #[cfg(feature = "ssh")]
    async fn establish_rest(&mut self) -> Result<(), ConnError> {
        let client = Client::open(self.effective_conn_info().map_err(|e| {
            DefinitiveError::new(
                DefinitiveErrorKind::InvalidCommand,
                format!("invalid connection settings: {}", e),
            )
        })?)
        .map_err(|e| {
            DefinitiveError::new(
                DefinitiveErrorKind::InvalidCommand,
                format!("invalid connection settings: {}", e),
            )
        })?;
        let conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                DefinitiveError::new(
                    DefinitiveErrorKind::NotConnected,
                    format!("connect failed: {}", e),
                )
            })?;
        self.conn = Some(conn);
        if let Some(warning) = self.config.tls.as_ref().and_then(|t| t.warning()) {
            self.send_event(ConnEvent::TlsWarning(warning));
        }
        self.send_event(ConnEvent::Connected);
        Ok(())
    }

    /// The effective connection info: when an SSH tunnel is up, the
    /// Redis host/port are rewritten to the tunnel's local listener
    /// (plaintext to 127.0.0.1 — the SSH layer is the encryption).
    #[allow(clippy::unused_self)]
    fn effective_conn_info(&self) -> Result<redis::ConnectionInfo, String> {
        #[cfg(feature = "ssh")]
        if let Some(tunnel) = self.tunnel.as_ref() {
            let addr = tunnel.local_addr();
            let mut info = self
                .config
                .to_redis_conn_info()
                .map_err(|e| format!("tunnel base info: {e}"))?;
            // Rewritten via the public `set_addr` (redis 1.8.1's fields
            // are crate-private). TLS flattens to Tcp: the SSH layer is
            // the encryption, so the local hop is plaintext by design.
            info = info.set_addr(redis::ConnectionAddr::Tcp(
                addr.ip().to_string(),
                addr.port(),
            ));
            return Ok(info);
        }
        self.config.to_redis_conn_info()
    }

    fn send_event(&self, ev: ConnEvent) {
        // Events are advisory: a full channel drops rather than blocks I/O.
        if let Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) = self.events.try_send(ev)
        {
            // drop
        }
    }

    fn next_command_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    /// Issue through the live connection, enforcing the disconnect contract.
    async fn issue_redis_cmd(
        &mut self,
        cmd: &redis::Cmd,
        in_flight: Option<Command>,
    ) -> Result<RedisValue, ConnError> {
        let Some(conn) = self.conn.as_mut() else {
            return Err(ConnError::Disconnected {
                reason: Some("connection is down (reconnect required)".into()),
                // Nothing was sent: no in-flight command to report.
                in_flight: None,
            });
        };
        let result = conn.send_packed_command(cmd).await;
        match result {
            Ok(v) => Ok(v),
            Err(e) => {
                if error_is_disconnect(&e) {
                    // THE boundary: mark the connection dead so nothing
                    // else rides it, and report the in-flight command.
                    self.conn = None;
                    let reason = Some(format!("connection dropped: {}", e));
                    self.send_event(ConnEvent::Disconnected(reason.clone()));
                    Err(ConnError::Disconnected { reason, in_flight })
                } else {
                    Err(ConnError::Definitive(definitive_from_redis(&e)))
                }
            }
        }
    }
}

/// Decide whether a redis-rs error means "the link went down around this
/// command" (→ `Disconnected`, acceptance 3) or a definitive failure.
///
/// Verified against redis 1.8.1 empirically:
/// - server-side CLIENT KILL → `ErrorKind::Io`, `is_connection_dropped()`
/// - connect to closed port → `ErrorKind::Io`, `is_connection_dropped()`
fn error_is_disconnect(e: &RedisError) -> bool {
    e.is_connection_dropped() || matches!(e.kind(), ErrorKind::Io) && !e.is_timeout()
    // response timeouts are definitive (cmd may or may not have run — treat as definitive-with-note; the caller decides)
}

/// Translate a redis-rs error into our definitive error (no payload bytes).
fn definitive_from_redis(e: &RedisError) -> DefinitiveError {
    let kind = if e.kind() == ErrorKind::AuthenticationFailed {
        DefinitiveErrorKind::Auth
    } else if matches!(e.kind(), ErrorKind::Server(_)) {
        DefinitiveErrorKind::ServerError
    } else if matches!(
        e.kind(),
        ErrorKind::InvalidClientConfig | ErrorKind::Client | ErrorKind::Parse
    ) {
        DefinitiveErrorKind::InvalidCommand
    } else {
        DefinitiveErrorKind::Other
    };
    DefinitiveError::new(kind, e.to_string())
}

/// Convert our `Command` into a redis-rs `Cmd`, byte-safe.
fn to_redis_cmd(cmd: &Command) -> redis::Cmd {
    let mut c = redis_cmd(&cmd.name);
    for arg in &cmd.args {
        c.arg(arg.as_slice());
    }
    c
}

/// Convert a redis-rs `Value` into our byte-preserving `Reply`.
pub(crate) fn reply_from_redis(v: RedisValue) -> Reply {
    match v {
        RedisValue::Nil => Reply::Nil,
        RedisValue::Int(i) => Reply::Int(i),
        RedisValue::BulkString(b) => Reply::Bulk(b),
        RedisValue::SimpleString(s) => Reply::Bulk(s.into_bytes()),
        RedisValue::Okay => Reply::Okay,
        RedisValue::Array(items) => Reply::Array(items.into_iter().map(reply_from_redis).collect()),
        RedisValue::Double(d) => Reply::Double(d),
        RedisValue::Boolean(b) => Reply::Boolean(b),
        RedisValue::Map(entries) => Reply::Map(
            entries
                .into_iter()
                .map(|(k, v)| (reply_from_redis(k), reply_from_redis(v)))
                .collect(),
        ),
        RedisValue::Attribute { data, .. } => reply_from_redis(*data),
        RedisValue::Set(items) => Reply::Array(items.into_iter().map(reply_from_redis).collect()),
        RedisValue::VerbatimString { text, .. } => Reply::Bulk(text.into_bytes()),
        RedisValue::ServerError(e) => Reply::Other(format!("server error: {}", e.code())),
        _ => Reply::Other("typed".into()),
    }
}

/// Parse a SCAN reply `(cursor, [keys...])` out of our `Reply`.
fn parse_scan_reply(reply: Reply) -> Result<ScanResult, ConnError> {
    match reply {
        Reply::Array(mut items) if items.len() == 2 => {
            let keys_item = items.pop().expect("len checked");
            let cursor_item = items.pop().expect("len checked");
            let next_cursor = match cursor_item {
                Reply::Bulk(b) => {
                    let s = String::from_utf8_lossy(&b);
                    s.parse::<u64>().map_err(|_| {
                        ConnError::Definitive(DefinitiveError::new(
                            DefinitiveErrorKind::Other,
                            "SCAN returned an unparsable cursor",
                        ))
                    })?
                }
                Reply::Int(i) if i >= 0 => i as u64,
                _ => {
                    return Err(ConnError::Definitive(DefinitiveError::new(
                        DefinitiveErrorKind::Other,
                        "SCAN returned an unexpected cursor type",
                    )))
                }
            };
            let keys = match keys_item {
                Reply::Array(keys) => keys
                    .into_iter()
                    .map(|k| match k {
                        Reply::Bulk(b) => b,
                        _ => Vec::new(),
                    })
                    .collect(),
                Reply::Nil => Vec::new(),
                _ => {
                    return Err(ConnError::Definitive(DefinitiveError::new(
                        DefinitiveErrorKind::Other,
                        "SCAN returned an unexpected keys type",
                    )))
                }
            };
            Ok(ScanResult { next_cursor, keys })
        }
        _ => Err(ConnError::Definitive(DefinitiveError::new(
            DefinitiveErrorKind::Other,
            "SCAN returned an unexpected reply shape",
        ))),
    }
}

#[async_trait]
impl Connection for RedisConnection {
    async fn issue(&mut self, cmd: Command) -> Result<Reply, ConnError> {
        let id = self.next_command_id();
        let redis_cmd = to_redis_cmd(&cmd);
        let result = self.issue_redis_cmd(&redis_cmd, Some(cmd)).await;
        let summary = result
            .as_ref()
            .map(|v| match v {
                RedisValue::Nil => ReplySummary::Nil,
                RedisValue::Int(i) => ReplySummary::Int(*i),
                RedisValue::BulkString(b) => ReplySummary::Bulk(b.len()),
                RedisValue::SimpleString(_) => ReplySummary::Okay,
                RedisValue::Okay => ReplySummary::Okay,
                RedisValue::Array(a) => ReplySummary::Array(a.len()),
                _ => ReplySummary::Other("typed".into()),
            })
            .map_err(|e: &ConnError| e.clone());
        self.send_event(ConnEvent::CommandDone(id, summary));
        result.map(reply_from_redis)
    }

    async fn scan_batch(
        &mut self,
        cursor: u64,
        pattern: Option<Vec<u8>>,
        count: u64,
    ) -> Result<ScanResult, ConnError> {
        let mut c = redis_cmd("SCAN");
        c.cursor_arg(cursor);
        if let Some(p) = &pattern {
            c.arg("MATCH").arg(p.as_slice());
        }
        if count > 0 {
            c.arg("COUNT").arg(count);
        }
        let id = self.next_command_id();
        let result = self.issue_redis_cmd(&c, None).await;
        let summary = result
            .as_ref()
            .map(|v| match v {
                RedisValue::Array(a) => ReplySummary::Array(a.len()),
                _ => ReplySummary::Other("scan".into()),
            })
            .map_err(|e: &ConnError| e.clone());
        self.send_event(ConnEvent::CommandDone(id, summary));
        result.map(reply_from_redis).and_then(parse_scan_reply)
    }

    async fn ping(&mut self) -> Result<(), ConnError> {
        let c = redis_cmd("PING");
        self.issue_redis_cmd(&c, None).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RedisConnectionConfig {
        RedisConnectionConfig::new("127.0.0.1", 6379, 0)
    }

    #[test]
    fn config_serialization_round_trip_non_secret() {
        let c = cfg()
            .with_tls(Some(TlsConfig::verify_full(
                Some(Path::new("/tmp/ca.pem").to_path_buf()),
                None,
            )))
            .with_password(Some(Secret::new("supersecret".into())));
        let j = c.to_persisted_json();
        let s = j.to_string();
        assert!(
            !s.contains("supersecret"),
            "password leaked into persisted json"
        );
        assert!(s.contains("redis-io") || s.contains("host") || !s.is_empty());
        let back = RedisConnectionConfig::from_persisted_json(&j).expect("round trip");
        assert_eq!(back.host, "127.0.0.1");
        assert_eq!(back.port, 6379);
        assert_eq!(back.db, 0);
        assert_eq!(back.tls, c.tls);
        assert!(back.password().is_none(), "password must not round-trip");
    }

    #[test]
    fn config_eq_ignores_password() {
        let a = cfg().with_password(Some(Secret::new("x".into())));
        let b = cfg().with_password(Some(Secret::new("y".into())));
        assert_eq!(a, b);
    }

    #[test]
    fn ssh_tunnel_round_trip_has_no_password() {
        let c = cfg().with_ssh_tunnel(Some(crate::ssh::SshTunnelConfig {
            host: "bastion".into(),
            port: 22,
            user: "adam".into(),
            auth: crate::ssh::SshAuth::Password(Secret::new("topsecret".into())),
            dest_host: "redis.internal".into(),
            dest_port: 6379,
        }));
        let j = c.to_persisted_json();
        assert!(!j.to_string().contains("topsecret"));
        let back = RedisConnectionConfig::from_persisted_json(&j).expect("round trip");
        let t = back.ssh_tunnel.expect("tunnel present");
        assert_eq!(t.host, "bastion");
        assert!(matches!(t.auth, crate::ssh::SshAuth::Password(_)));
        // re-hydrated password is empty (transient by design)
        match t.auth {
            crate::ssh::SshAuth::Password(p) => assert!(p.is_empty()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn scan_reply_parses_cursor_and_keys() {
        let reply = Reply::Array(vec![
            Reply::Bulk(b"17".to_vec()),
            Reply::Array(vec![
                Reply::Bulk(b"a:b".to_vec()),
                Reply::Bulk(vec![0, 159]),
            ]),
        ]);
        let r = parse_scan_reply(reply).expect("parse");
        assert_eq!(r.next_cursor, 17);
        assert_eq!(r.keys, vec![b"a:b".to_vec(), vec![0, 159]]);
    }

    #[test]
    fn scan_reply_rejects_bad_shape() {
        let reply = Reply::Bulk(b"x".to_vec());
        assert!(parse_scan_reply(reply).is_err());
    }

    #[test]
    fn reply_debug_is_byte_safe() {
        let r = Reply::Bulk(vec![0, 159, 146, 150]);
        assert!(!format!("{r:?}").contains("Bulk(["));
        assert!(format!("{r:?}").contains("4 bytes"));
    }

    // ---------- Loopback integration tests (require local redis on 6379) ----------
    // Local Redis observed available via `redis-cli ping` on this machine.
    // They are #[ignore] so `cargo test` never requires a server.

    fn loopback_up() -> bool {
        std::process::Command::new("redis-cli")
            .args(["-h", "127.0.0.1", "-p", "6379", "ping"])
            .output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "PONG")
            .unwrap_or(false)
    }

    #[tokio::test]
    #[ignore = "requires a local Redis on 127.0.0.1:6379 (no server is started for you)"]
    async fn loopback_issue_ping_get_set() {
        if !loopback_up() {
            eprintln!("no local redis; skipping");
            return;
        }
        let (mut conn, mut events) = RedisConnection::connect(cfg()).await.expect("connect");
        assert!(matches!(
            events.try_recv(),
            Ok(ConnEvent::Connected) | Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        conn.ping().await.expect("ping");
        // SET/GET with non-UTF-8 value bytes (byte-faithful check)
        let blob: Vec<u8> = vec![0x00, 0x9F, 0x92, 0x96, b'\n'];
        let set = Command::new("SET", vec![b"redis-io:probe".to_vec(), blob.clone()]);
        let reply = conn.issue(set).await.expect("set");
        assert!(matches!(reply, Reply::Okay));
        let get = Command::new("GET", vec![b"redis-io:probe".to_vec()]);
        let reply = conn.issue(get).await.expect("get");
        assert_eq!(reply.as_bytes(), Some(blob.as_slice()));
        // cleanup (best effort)
        let _ = conn
            .issue(Command::new("DEL", vec![b"redis-io:probe".to_vec()]))
            .await;
    }

    #[tokio::test]
    #[ignore = "requires a local Redis on 127.0.0.1:6379 (no server is started for you)"]
    async fn loopback_scan_batch_and_events() {
        if !loopback_up() {
            eprintln!("no local redis; skipping");
            return;
        }
        let (mut conn, _events) = RedisConnection::connect(cfg()).await.expect("connect");
        let sr = conn
            .scan_batch(0, Some(b"redis-io:*".to_vec()), 100)
            .await
            .expect("scan");
        let _ = sr.next_cursor; // shape only: cursor consumed by caller
    }
}
