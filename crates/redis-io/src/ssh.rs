//! SSH tunneling: config, TOFU host-key check, and the russh tunnel
//! (feature `ssh`).
//!
//! # Shape (PLAN.md §4, PRD stories 6-8)
//!
//! - **Single hop, structurally**: [`SshTunnelConfig`] holds ONE
//!   `(dest_host, dest_port)` target. There is no list, no chaining —
//!   the type cannot express multi-hop.
//! - **Auth**: password (transient), key file (path + optional
//!   passphrase), or system SSH agent. Agent is **best-effort**: PLAN
//!   names Windows-agent as a risk; where the agent is unreachable the
//!   tunnel fails with a clear error naming the mode, and the user
//!   falls back to password/key-file.
//! - **TOFU** ("trust on first use"): first connect pauses with
//!   [`HostKeyVerdict::Unseen`]; the caller must confirm; a changed key
//!   is a HARD BLOCK with no override (anti-MITM, acceptance 5). The
//!   *store* of known hosts lives in `connections-store`
//!   ([`KnownHosts`]); the *check* lives here at the handshake, where
//!   the presented key is known.
//!
//! # Transport choice (documented, per task)
//!
//! The russh direct-tcpip channel is exposed as a **local TCP
//! listener** that the redis client connects to, rather than wiring the
//! channel stream into redis-rs' transport (redis-rs 1.8.1's connection
//! path accepts addresses, not arbitrary `AsyncRead+AsyncWrite`
//! streams, so channel-as-transport would require forking its connect
//! internals). The listener pump is honest, testable, and the standard
//! fallback: [`crate::tunnel::connect`] binds `127.0.0.1:0` and forwards
//! bytes both ways.
//!
//! **Untested live (boundary tier, honesty):** the russh handshake, the
//! channel pump, and agent auth against a real sshd are NOT exercised
//! headless here. What IS tested: TOFU verdicts (byte-exact), config
//! construction/defaults, and that the feature compiles
//! (`cargo check -p redis-io --features ssh`).

use std::path::PathBuf;

use crate::log_hygiene::Secret;

/// How to authenticate to the SSH server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshAuth {
    /// Password auth (transient; OS keyring or per-session prompt —
    /// never persisted by this crate).
    Password(Secret),
    /// Private key file path (+ optional passphrase, transient).
    KeyFile {
        /// Path to the private key file.
        path: PathBuf,
        /// Passphrase for the key, if encrypted (transient).
        passphrase: Option<Secret>,
    },
    /// System SSH agent (best-effort; clear error when unavailable).
    Agent,
}

/// A single-hop SSH tunnel in front of a Redis server.
///
/// All fields optional-ish helpers exist on [`SshTunnelConfig::new`];
/// the struct is plain data so persistence (non-secret fields only) and
/// UI can build against a stable shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTunnelConfig {
    /// SSH server hostname.
    pub host: String,
    /// SSH server port.
    pub port: u16,
    /// SSH username.
    pub user: String,
    /// Authentication method (secrets inside are transient).
    pub auth: SshAuth,
    /// The ONE forwarding target — the Redis server as reachable from
    /// the SSH host. Structurally a single hop; there is no list.
    pub dest_host: String,
    /// The ONE forwarding target's port.
    pub dest_port: u16,
}

impl SshTunnelConfig {
    /// New tunnel config; SSH port defaults to 22.
    pub fn new(
        host: impl Into<String>,
        user: impl Into<String>,
        auth: SshAuth,
        dest_host: impl Into<String>,
        dest_port: u16,
    ) -> Self {
        Self {
            host: host.into(),
            port: 22,
            user: user.into(),
            auth,
            dest_host: dest_host.into(),
            dest_port,
        }
    }

    /// Override the SSH port (rarely needed).
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

/// SHA-256 host-key fingerprint, hex-encoded, as OpenSSH prints it
/// (without the leading `SHA256:` prefix; callers may add it for
/// display). Plain data — fingerprints are NOT secrets.
///
/// (Same shape as connections-store's `Fingerprint`; kept as a plain
/// `String` alias so the non-`ssh` build of this crate stays
/// dependency-free.)
pub type Fingerprint = String;

/// The TOFU verdict type lives in `connections-store`
/// (`known_hosts::HostKeyVerdict` — the store owns it; the check at
/// the handshake consumes it). Re-exported here for the `ssh`-feature
/// API surface so callers need one import.
#[cfg(feature = "ssh")]
pub use connections_store::known_hosts::HostKeyVerdict;

/// What a tunnel connect attempt resolved to (drives the UI flow).
///
/// The `Connected` variant carries the live tunnel (its local
/// address is what the Redis client connects to). Only available
/// with the `ssh` feature.
#[cfg(feature = "ssh")]
#[derive(Debug)]
pub enum HostKeyStatus {
    /// The tunnel is up; `local_addr()` on the tunnel is the address
    /// for the Redis client.
    Connected(crate::tunnel::SshTunnel),
    /// First-seen host key: awaiting user confirmation. Re-attempt the
    /// connect with [`HostKeyDecision::Accept`] (records the key) or
    /// [`HostKeyDecision::Reject`] once the user answers.
    NeedsConfirmation {
        /// Fingerprint awaiting confirmation.
        fingerprint: Fingerprint,
    },
    /// Host key changed (or the user rejected the key): tunnel refused.
    /// Hard block — there is no override, by design.
    Blocked(HostKeyBlocked),
}

/// The user's answer to a [`HostKeyStatus::NeedsConfirmation`] prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// Accept and record the fingerprint (TOFU trust step).
    Accept,
    /// Refuse: do not record, do not connect.
    Reject,
}

/// Plain-data prompt payload for a first-seen host key (serializable
/// for the UI; emitted as [`crate::connection::ConnEvent::HostKeyPrompt`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyPrompt {
    /// `host:port` of the SSH server (non-sensitive).
    pub host: String,
    /// Port of the SSH server.
    pub port: u16,
    /// SHA-256 fingerprint of the presented key, hex.
    pub fingerprint: Fingerprint,
}

/// Plain-data block payload for a changed host key (serializable for
/// the UI; emitted as [`crate::connection::ConnEvent::HostKeyBlocked`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyBlocked {
    /// `host:port` of the SSH server (non-sensitive).
    pub host: String,
    /// Port of the SSH server.
    pub port: u16,
    /// Fingerprint of the presented (untrusted) key.
    pub new_fingerprint: Fingerprint,
    /// The previously recorded fingerprint.
    pub old_fingerprint: Fingerprint,
}

/// Compute the SHA-256 fingerprint of a host key's raw public-key
/// bytes, hex-encoded (lowercase). Byte-exact: same key bytes → same
/// fingerprint; any one-bit difference → different fingerprint.
///
/// Used by the tunnel at handshake time over the key russh presents,
/// and by tests over known vectors.
///
/// Note: this hashes the **wire-encoded key bytes** the caller passes
/// in (for russh, `ssh_key::PublicKey::public_key_bytes()`). Fingerprint
/// text never contains key material beyond the one-way digest.
pub fn fingerprint_of(public_key: &[u8]) -> Fingerprint {
    hex_encode(&sha256_bytes(public_key))
}

/// SHA-256 of `data`. The `ssh` feature (and tests) use the `sha2`
/// crate — the same SHA-2 provider the russh stack links — so the
/// fingerprint stays byte-identical everywhere it is computed.
///
/// Without `ssh`/tests the helper is unreachable (kept for API
/// stability of `fingerprint_of`).
fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    #[cfg(any(test, feature = "ssh"))]
    {
        use sha2::Digest;
        sha2::Sha256::digest(data).into()
    }
    #[cfg(not(any(test, feature = "ssh")))]
    {
        let _ = data;
        unreachable!("fingerprint_of is only called from the ssh feature or tests")
    }
}

/// Lowercase hex encoding without pulling a dependency.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_config_is_data_only_and_constructible() {
        let t = SshTunnelConfig::new(
            "bastion.example",
            "adam",
            SshAuth::KeyFile {
                path: PathBuf::from("/Users/adam/.ssh/id_ed25519"),
                passphrase: None,
            },
            "redis.internal",
            6379,
        );
        assert_eq!(t.host, "bastion.example");
        assert_eq!(t.port, 22, "default port");
        assert_eq!(t.dest_host, "redis.internal");
        assert_eq!(t.dest_port, 6379);
        // exactly one destination — structurally single hop
        let _ = (t.dest_host.clone(), t.dest_port);
    }

    #[test]
    fn ssh_password_never_renders() {
        let t = SshTunnelConfig::new(
            "h",
            "u",
            SshAuth::Password(Secret::new("swordfish".into())),
            "d",
            6379,
        );
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("swordfish"));
    }

    #[test]
    fn fingerprint_is_byte_exact_sha256_hex() {
        // Known vector: sha256("") hex.
        assert_eq!(
            fingerprint_of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // sha256("abc")
        assert_eq!(
            fingerprint_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // one-bit difference changes the fingerprint
        assert_ne!(fingerprint_of(b"key-1"), fingerprint_of(b"key-2"));
    }

    #[test]
    fn fingerprint_deterministic_and_64_hex_chars() {
        let f = fingerprint_of(b"some host key bytes");
        assert_eq!(f.len(), 64);
        assert!(f.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(f, fingerprint_of(b"some host key bytes"));
    }
}
