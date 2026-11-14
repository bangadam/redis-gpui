//! Saved connection metadata files: non-secret fields ONLY.
//!
//! # The guarantee (PRD acceptance 6, at unit level)
//!
//! The JSON document holds name/host/port/db, TLS mode + CA/cert
//! **paths**, SSH config + key **paths**, and username. The password
//! / passphrase never enters this file — it lives in the credential
//! store ([`crate::credentials`]) or a per-session prompt. The
//! self-audit test below byte-scans the written file for the secret
//! in raw, hex, and base64 encodings.
//!
//! # Config dir
//!
//! [`config_dir`] resolves in this order:
//! 1. `REDIS_GPUI_CONFIG_DIR` env override (tests + portability),
//! 2. `$HOME/.config/redis-gpui` (XDG-style on every OS; the app is
//!    not yet packaged per-OS, and one rule is auditable).
//!
//! Files live at `<config_dir>/connections/<id>.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::credentials::{CredentialOutcome, CredentialStore};

/// A saved connection's non-secret metadata (the file content).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    /// Stable id (also the credential-store key and file name).
    pub id: String,
    /// User-facing display name.
    pub name: String,
    /// Redis server host.
    pub host: String,
    /// Redis server port.
    pub port: u16,
    /// Redis logical database number.
    pub db: i64,
    /// ACL username, if any.
    pub username: Option<String>,
    /// TLS mode + material PATHS (never bytes).
    pub tls: TlsPolicy,
    /// SSH tunnel config (non-secret fields; auth mode + key path).
    pub ssh: Option<SshPolicy>,
}

/// TLS policy as persisted (mirrors redis-io's `TlsConfig` shape, but
/// this crate stays dependency-free: plain data only).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum TlsPolicy {
    /// Plaintext (no TLS).
    #[default]
    Disabled,
    /// Full verification; optional custom CA / client cert PATHS.
    Verified {
        /// Optional custom CA bundle path.
        #[serde(skip_serializing_if = "Option::is_none")]
        ca_bundle: Option<String>,
        /// Optional client cert path (mTLS).
        #[serde(skip_serializing_if = "Option::is_none")]
        client_cert: Option<String>,
        /// Optional client key path (mTLS).
        #[serde(skip_serializing_if = "Option::is_none")]
        client_key: Option<String>,
    },
    /// Insecure skip-verification override (explicit opt-in only; the
    /// UI must warn persistently while active).
    InsecureOverride,
}

/// SSH tunnel policy as persisted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshPolicy {
    /// SSH server host.
    pub host: String,
    /// SSH server port.
    pub port: u16,
    /// SSH username.
    pub user: String,
    /// Auth mode + key path (secret material never here).
    pub auth: SshAuthPolicy,
    /// The single forwarding target host.
    pub dest_host: String,
    /// The single forwarding target port.
    pub dest_port: u16,
}

/// SSH auth mode as persisted (mode + non-secret paths only).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum SshAuthPolicy {
    /// Password auth (the password itself: keyring or per-session).
    Password,
    /// Key-file auth (path only; passphrase: keyring or per-session).
    KeyFile {
        /// Path to the private key file.
        path: String,
    },
    /// System SSH agent (best-effort).
    Agent,
}

/// How a save-with-credential resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Metadata file written; secret stored in the OS keyring.
    Saved,
    /// Keyring unavailable: metadata file NOT written, the caller
    /// prompts per session (PromptPerSession, PLAN §4) — nothing hit
    /// the disk (acceptance 7).
    PromptPerSession,
}

/// Resolve the config dir (env override first).
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("REDIS_GPUI_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("redis-gpui")
}

/// Where a connection's metadata file lives.
pub fn connection_file(conn_id: &str) -> PathBuf {
    config_dir()
        .join("connections")
        .join(format!("{conn_id}.json"))
}

/// Save the metadata file. The secret goes to the credential store,
/// NEVER into the file.
///
/// `StoreUnavailable` → [`SaveOutcome::PromptPerSession`] AND no
/// metadata file written: with no keyring, a "saved" connection would
/// either store the secret on disk (forbidden) or save a connection
/// that can never authenticate silently — so nothing is written and
/// the session prompts (acceptance 7).
pub fn save_connection(
    conn: &SavedConnection,
    secret: Option<&str>,
    store: &dyn CredentialStore,
) -> Result<SaveOutcome, String> {
    // Secret to keyring FIRST: if the store is unavailable, we learn
    // before touching disk.
    if let Some(secret) = secret {
        match store.save(&conn.id, secret) {
            CredentialOutcome::Ok(()) => {}
            CredentialOutcome::StoreUnavailable => {
                return Ok(SaveOutcome::PromptPerSession);
            }
            CredentialOutcome::Failure(e) => {
                return Err(format!("credential store failure: {e}"));
            }
            CredentialOutcome::NotSaved => unreachable!("save cannot report NotSaved"),
        }
    }
    let path = connection_file(&conn.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create config dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(conn).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("write connection file: {e}"))?;
    Ok(SaveOutcome::Saved)
}

/// Load a saved connection's metadata (secret comes separately via
/// [`load_secret`]).
pub fn load_connection(conn_id: &str) -> Result<Option<SavedConnection>, String> {
    let path = connection_file(conn_id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Ok(None), // missing = not saved (not an error)
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("parse connection file: {e}"))
}

/// Load the secret for a connection from the credential store.
pub fn load_secret(
    conn_id: &str,
    store: &dyn CredentialStore,
) -> Result<CredentialOutcome<String>, String> {
    Ok(store.load(conn_id))
}

/// Delete a connection: metadata file + credential entry.
pub fn delete_connection(conn_id: &str, store: &dyn CredentialStore) -> Result<(), String> {
    let _ = std::fs::remove_file(connection_file(conn_id));
    match store.delete(conn_id) {
        CredentialOutcome::Ok(()) | CredentialOutcome::NotSaved => Ok(()),
        CredentialOutcome::StoreUnavailable => Ok(()), // nothing was stored anyway
        CredentialOutcome::Failure(e) => Err(format!("credential delete: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MockCredentialStore;

    /// Multi-encoding secret scan: raw, hex, base64.
    fn secret_leaks(file_bytes: &[u8], secret: &str) -> bool {
        let raw = secret.as_bytes();
        if file_bytes.windows(raw.len().max(1)).any(|w| w == raw) {
            return true;
        }
        // hex (lowercase + uppercase)
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let hex_upper: String = raw.iter().map(|b| format!("{b:02X}")).collect();
        for enc in [hex, hex_upper] {
            if file_bytes.windows(enc.len()).any(|w| w == enc.as_bytes()) {
                return true;
            }
        }
        // base64 (standard alphabet, padded)
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut b64 = String::new();
        for chunk in raw.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            b64.push(TABLE[(n >> 18) as usize & 63] as char);
            b64.push(TABLE[(n >> 12) as usize & 63] as char);
            b64.push(if chunk.len() > 1 {
                TABLE[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            b64.push(if chunk.len() > 2 {
                TABLE[n as usize & 63] as char
            } else {
                '='
            });
        }
        file_bytes.windows(b64.len()).any(|w| w == b64.as_bytes())
    }

    fn test_conn(id: &str) -> SavedConnection {
        SavedConnection {
            id: id.into(),
            name: "prod".into(),
            host: "redis.internal".into(),
            port: 6379,
            db: 0,
            username: Some("adam".into()),
            tls: TlsPolicy::Verified {
                ca_bundle: Some("/etc/ca.pem".into()),
                client_cert: None,
                client_key: None,
            },
            ssh: Some(SshPolicy {
                host: "bastion".into(),
                port: 22,
                user: "adam".into(),
                auth: SshAuthPolicy::KeyFile {
                    path: "/Users/adam/.ssh/id_ed25519".into(),
                },
                dest_host: "redis.internal".into(),
                dest_port: 6379,
            }),
        }
    }

    /// Tests run in parallel threads; the env var is process-global.
    /// A mutex + serial guard keeps `with_config_dir` scopes from
    /// racing each other (the classic env-var test trap).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_config_dir(f: impl FnOnce(&std::path::Path)) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        // Scoped env override, serialized across this module's tests.
        std::env::set_var("REDIS_GPUI_CONFIG_DIR", dir.path());
        f(dir.path());
        std::env::remove_var("REDIS_GPUI_CONFIG_DIR");
    }

    #[test]
    fn save_then_load_round_trips_metadata() {
        with_config_dir(|_| {
            let store = MockCredentialStore::working();
            let conn = test_conn("c1");
            let outcome = save_connection(&conn, Some("hunter2"), &store).expect("save");
            assert_eq!(outcome, SaveOutcome::Saved);
            let loaded = load_connection("c1").expect("load").expect("present");
            assert_eq!(loaded, conn);
            // secret retrievable via the store seam
            assert_eq!(
                load_secret("c1", &store).expect("secret"),
                CredentialOutcome::Ok("hunter2".into())
            );
        });
    }

    /// Acceptance 6 at unit level: the file bytes contain no trace of
    /// the secret in any common encoding.
    #[test]
    fn self_audit_file_contains_no_secret_in_any_encoding() {
        with_config_dir(|_| {
            let store = MockCredentialStore::working();
            let conn = test_conn("c2");
            // a distinctive secret
            let secret = "Zk9$ecretPASSphrase42x!";
            save_connection(&conn, Some(secret), &store).expect("save");
            let file = connection_file("c2");
            let bytes = std::fs::read(&file).expect("file written");
            assert!(
                !secret_leaks(&bytes, secret),
                "secret leaked into the saved connection file (raw/hex/base64)"
            );
            // sanity: the scan actually works (the secret DOES leak if
            // we put it in the file).
            let mut with_secret = bytes.clone();
            with_secret.extend_from_slice(secret.as_bytes());
            assert!(secret_leaks(&with_secret, secret));
        });
    }

    /// Acceptance 7 (seam b): keyring absent → prompt per session and
    /// NOTHING written to disk.
    #[test]
    fn keyring_absent_prompts_per_session_and_writes_nothing() {
        with_config_dir(|dir| {
            let store = MockCredentialStore::unavailable();
            let conn = test_conn("c3");
            let outcome = save_connection(&conn, Some("topsecret"), &store).expect("save");
            assert_eq!(outcome, SaveOutcome::PromptPerSession);
            // nothing on disk: no file, no dir content
            assert!(load_connection("c3").expect("load").is_none());
            let conns_dir = dir.join("connections");
            let entries = std::fs::read_dir(&conns_dir)
                .map(|rd| rd.filter_map(|e| e.ok()).count())
                .unwrap_or(0);
            assert_eq!(entries, 0, "no connection files may be written");
            // and the mock never stored anything
            assert!(store.entries.lock().unwrap().is_empty());
            // load path also degrades to the prompt signal:
            assert_eq!(
                load_secret("c3", &store).expect("secret"),
                CredentialOutcome::StoreUnavailable
            );
        });
    }

    #[test]
    fn save_without_secret_writes_metadata_only() {
        with_config_dir(|_| {
            let store = MockCredentialStore::working();
            let conn = test_conn("c4");
            let outcome = save_connection(&conn, None, &store).expect("save");
            assert_eq!(outcome, SaveOutcome::Saved);
            assert_eq!(
                load_secret("c4", &store).expect("secret"),
                CredentialOutcome::NotSaved
            );
        });
    }

    #[test]
    fn delete_removes_file_and_credential() {
        with_config_dir(|_| {
            let store = MockCredentialStore::working();
            let conn = test_conn("c5");
            save_connection(&conn, Some("pw"), &store).expect("save");
            delete_connection("c5", &store).expect("delete");
            assert!(load_connection("c5").expect("load").is_none());
            assert_eq!(
                load_secret("c5", &store).expect("secret"),
                CredentialOutcome::NotSaved
            );
        });
    }

    #[test]
    fn tls_policy_serializes_paths_only() {
        let t = TlsPolicy::Verified {
            ca_bundle: Some("/ca.pem".into()),
            client_cert: Some("/c.pem".into()),
            client_key: Some("/k.pem".into()),
        };
        let json = serde_json::to_string(&t).expect("ser");
        assert!(json.contains("/ca.pem"));
        assert!(json.contains("\"mode\":\"Verified\""));
        let back: TlsPolicy = serde_json::from_str(&json).expect("de");
        assert_eq!(back, t);
    }

    #[test]
    fn insecure_override_round_trips_as_explicit_mode() {
        let t = TlsPolicy::InsecureOverride;
        let json = serde_json::to_string(&t).expect("ser");
        assert!(json.contains("InsecureOverride"));
        let back: TlsPolicy = serde_json::from_str(&json).expect("de");
        assert_eq!(back, t);
    }
}
