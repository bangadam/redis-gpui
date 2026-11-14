//! Phase 6 security form state: the pure mapping from UI form values to
//! redis-io's TLS/SSH policy structs, SSH refusal parsing, and the
//! saved-connection prefill plan.
//!
//! **This module is logic-only** (no GPUI, no I/O): every function is
//! unit-testable headless. The workspace owns the input entities and
//! calls these builders at the boundary. Secrets enter redis-io's
//! [`Secret`] newtype here and never render (log hygiene: PRD privacy —
//! display strings may carry host:port and fingerprints, never
//! passwords or keys).
//!
//! # The mapping contract (tested)
//!
//! - TLS `[Off | TLS | Insecure]` → `None` /
//!   [`TlsConfig::verify_full`] (paths optional, pair-or-nothing) /
//!   [`TlsConfig::insecure_override`] (the ONLY route to insecure).
//! - SSH `via tunnel` → [`SshTunnelConfig::new`] + [`SshAuth`]
//!   (password wrapped in [`Secret::new`] at this boundary).
//! - A refused SSH connect is classified from the backend's definitive
//!   error text (the event receiver is dropped with the failed connect,
//!   so the error string is the only carrier; it contains host:port and
//!   fingerprints only — non-sensitive by design).

use std::path::PathBuf;

use connections_store::credentials::CredentialOutcome;
use connections_store::saved_connections::{SshAuthPolicy, SshPolicy, TlsPolicy};
use redis_io::log_hygiene::Secret;
use redis_io::ssh::{SshAuth, SshTunnelConfig};
use redis_io::tls::TlsConfig;

/// The TLS segmented-control state: `[Off | TLS | Insecure]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TlsFormMode {
    /// Plaintext (no TLS).
    #[default]
    Off,
    /// Full verification (custom CA / client cert paths optional).
    Tls,
    /// Skip verification — the explicit, warned opt-in.
    Insecure,
}

impl TlsFormMode {
    /// Every segmented option, in display order.
    pub const OPTIONS: [TlsFormMode; 3] =
        [TlsFormMode::Off, TlsFormMode::Tls, TlsFormMode::Insecure];

    /// The segmented label.
    pub fn label(self) -> &'static str {
        match self {
            TlsFormMode::Off => "Off",
            TlsFormMode::Tls => "TLS",
            TlsFormMode::Insecure => "Insecure",
        }
    }
}

/// The SSH auth segmented-control state: `[Password | Key file | Agent]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SshAuthFormMode {
    /// Password auth (secret input; never logged).
    #[default]
    Password,
    /// Private key file (path input).
    KeyFile,
    /// System SSH agent (best-effort).
    Agent,
}

impl SshAuthFormMode {
    /// Every segmented option, in display order.
    pub const OPTIONS: [SshAuthFormMode; 3] = [
        SshAuthFormMode::Password,
        SshAuthFormMode::KeyFile,
        SshAuthFormMode::Agent,
    ];

    /// The segmented label.
    pub fn label(self) -> &'static str {
        match self {
            SshAuthFormMode::Password => "Password",
            SshAuthFormMode::KeyFile => "Key file",
            SshAuthFormMode::Agent => "Agent",
        }
    }
}

/// A trimmed non-empty string, if any.
fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Build the backend TLS policy from form values.
///
/// `Insecure` is the only route to [`TlsConfig::insecure_override`]
/// (greppable, explicit). A client cert requires a key path and vice
/// versa — a half pair maps to "no client cert" (the form validation
/// rejects it earlier with a clear message).
pub fn build_tls_config(mode: TlsFormMode, ca: &str, cert: &str, key: &str) -> Option<TlsConfig> {
    match mode {
        TlsFormMode::Off => None,
        TlsFormMode::Tls => Some(TlsConfig::verify_full(
            non_empty(ca).map(PathBuf::from),
            match (non_empty(cert), non_empty(key)) {
                (Some(c), Some(k)) => Some((PathBuf::from(c), PathBuf::from(k))),
                _ => None,
            },
        )),
        TlsFormMode::Insecure => Some(TlsConfig::insecure_override()),
    }
}

/// Build the backend SSH tunnel config; `None` when the tunnel is off.
///
/// The form state travels in `snapshot` (it already mirrors
/// `SecurityFormSnapshot`); `password` is the live secret — wrapped in
/// [`Secret::new`] here, the boundary.
pub fn build_ssh_tunnel(
    snapshot: &SecurityFormSnapshot,
    password: &str,
) -> Option<SshTunnelConfig> {
    if !snapshot.ssh_enabled {
        return None;
    }
    let auth = match snapshot.ssh_auth {
        SshAuthFormMode::Password => SshAuth::Password(Secret::new(password.to_string())),
        SshAuthFormMode::KeyFile => SshAuth::KeyFile {
            path: PathBuf::from(snapshot.ssh_key_path.trim()),
            passphrase: None,
        },
        SshAuthFormMode::Agent => SshAuth::Agent,
    };
    Some(
        SshTunnelConfig::new(
            snapshot.ssh_host.trim(),
            snapshot.ssh_user.trim(),
            auth,
            snapshot.dest_host.trim(),
            snapshot.dest_port,
        )
        .with_port(snapshot.ssh_port),
    )
}

/// Why an SSH connect was refused (parsed from the backend's definitive
/// error text — the only carrier out of a failed initial connect).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshRefusal {
    /// First-seen host key: awaiting the user's TOFU decision. The
    /// fingerprint travels in the message (non-sensitive digest).
    Prompt {
        /// SHA-256 fingerprint awaiting confirmation.
        fingerprint: String,
    },
    /// Host key CHANGED (hard block; no override exists).
    Blocked {
        /// Fingerprint of the presented (untrusted) key.
        new_fingerprint: String,
        /// The previously recorded fingerprint.
        old_fingerprint: String,
    },
}

/// Extract the text from after `anchor` up to (not including) `close`.
fn until<'a>(msg: &'a str, anchor: &str, close: char) -> Option<&'a str> {
    let start = msg.find(anchor)? + anchor.len();
    let rest = &msg[start..];
    let close_ix = rest.find(close)?;
    Some(&rest[..close_ix])
}

/// Classify a connect error as an SSH refusal, if it is one.
///
/// The backend messages (redis-io, stable):
/// - prompt: `"SSH host key not confirmed yet (fingerprint {fpr}); …"`
///   (and `"SSH host key still awaiting confirmation ({fpr})"`),
/// - blocked: `"SSH host key CHANGED for {host}:{port} — connection
///   blocked (recorded {old}; presented {new})"`.
///
/// Host/port for the prompt payload comes from the form's tunnel
/// config (the caller has it); the blocked message's host:port is not
/// re-parsed — the caller also knows it.
pub fn parse_ssh_refusal(msg: &str) -> Option<SshRefusal> {
    if msg.contains("SSH host key CHANGED") {
        let old = until(msg, "recorded ", ';')?.trim().to_string();
        let new = until(msg, "presented ", ')')?.trim().to_string();
        return Some(SshRefusal::Blocked {
            new_fingerprint: new,
            old_fingerprint: old,
        });
    }
    if let Some(fpr) = until(msg, "(fingerprint ", ')') {
        return Some(SshRefusal::Prompt {
            fingerprint: fpr.to_string(),
        });
    }
    if let Some(fpr) = until(msg, "awaiting confirmation (", ')') {
        return Some(SshRefusal::Prompt {
            fingerprint: fpr.to_string(),
        });
    }
    None
}

/// The prefill note shown under the form (PromptPerSession UX).
pub const NOTE_KEYCHAIN_UNAVAILABLE: &str = "keychain unavailable — password needed this session";
/// The prefill note when nothing was ever saved for the connection.
pub const NOTE_NO_SAVED_SECRET: &str = "no saved password — enter it this session";
/// The prefill note when the store itself errored (non-sensitive text).
pub const NOTE_STORE_ERROR: &str = "credential store error — password needed this session";

/// What a saved-connection prefill should do with the password field
/// (the PromptPerSession state machine, acceptance 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrefillPlan {
    /// Fill the password field from the store's secret.
    pub fill_password: bool,
    /// The muted note to render under the form, if any.
    pub note: Option<&'static str>,
}

/// Map a credential-load outcome to the prefill plan.
pub fn prefill_plan(outcome: &CredentialOutcome<String>) -> PrefillPlan {
    match outcome {
        CredentialOutcome::Ok(_) => PrefillPlan {
            fill_password: true,
            note: None,
        },
        CredentialOutcome::StoreUnavailable => PrefillPlan {
            fill_password: false,
            note: Some(NOTE_KEYCHAIN_UNAVAILABLE),
        },
        CredentialOutcome::NotSaved => PrefillPlan {
            fill_password: false,
            note: Some(NOTE_NO_SAVED_SECRET),
        },
        CredentialOutcome::Failure(_) => PrefillPlan {
            fill_password: false,
            note: Some(NOTE_STORE_ERROR),
        },
    }
}

/// The saved-connection metadata snapshot for save/prefill (plain
/// strings; the workspace copies these out of its input entities).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecurityFormSnapshot {
    /// TLS segmented mode.
    pub tls_mode: TlsFormMode,
    /// CA bundle path (optional).
    pub ca_bundle: String,
    /// Client cert path (optional; pairs with the key).
    pub client_cert: String,
    /// Client key path (optional; pairs with the cert).
    pub client_key: String,
    /// Whether the SSH tunnel row is enabled.
    pub ssh_enabled: bool,
    /// SSH server host.
    pub ssh_host: String,
    /// SSH server port.
    pub ssh_port: u16,
    /// SSH username.
    pub ssh_user: String,
    /// SSH auth segmented mode.
    pub ssh_auth: SshAuthFormMode,
    /// SSH key-file path (KeyFile mode only).
    pub ssh_key_path: String,
    /// The forwarding target (the Redis server as reachable from the
    /// SSH host) — filled by the caller from the main form fields.
    pub dest_host: String,
    /// The forwarding target's port.
    pub dest_port: u16,
}

impl SecurityFormSnapshot {
    /// The persisted TLS policy from form values (metadata only — paths).
    pub fn tls_policy(&self) -> TlsPolicy {
        match self.tls_mode {
            TlsFormMode::Off => TlsPolicy::Disabled,
            TlsFormMode::Tls => TlsPolicy::Verified {
                ca_bundle: non_empty(&self.ca_bundle),
                client_cert: non_empty(&self.client_cert),
                client_key: non_empty(&self.client_key),
            },
            TlsFormMode::Insecure => TlsPolicy::InsecureOverride,
        }
    }

    /// The persisted SSH policy from form values; `None` when disabled.
    pub fn ssh_policy(&self) -> Option<SshPolicy> {
        if !self.ssh_enabled {
            return None;
        }
        Some(SshPolicy {
            host: self.ssh_host.trim().to_string(),
            port: self.ssh_port,
            user: self.ssh_user.trim().to_string(),
            auth: match self.ssh_auth {
                SshAuthFormMode::Password => SshAuthPolicy::Password,
                SshAuthFormMode::KeyFile => SshAuthPolicy::KeyFile {
                    path: self.ssh_key_path.trim().to_string(),
                },
                SshAuthFormMode::Agent => SshAuthPolicy::Agent,
            },
            dest_host: self.dest_host.clone(),
            dest_port: self.dest_port,
        })
    }
}

/// The form snapshot a saved connection restores (prefill direction).
pub fn snapshot_from_saved(
    saved: &connections_store::saved_connections::SavedConnection,
) -> SecurityFormSnapshot {
    let (tls_mode, ca_bundle, client_cert, client_key) = match &saved.tls {
        TlsPolicy::Disabled => (
            TlsFormMode::Off,
            String::new(),
            String::new(),
            String::new(),
        ),
        TlsPolicy::Verified {
            ca_bundle,
            client_cert,
            client_key,
        } => (
            TlsFormMode::Tls,
            ca_bundle.clone().unwrap_or_default(),
            client_cert.clone().unwrap_or_default(),
            client_key.clone().unwrap_or_default(),
        ),
        TlsPolicy::InsecureOverride => (
            TlsFormMode::Insecure,
            String::new(),
            String::new(),
            String::new(),
        ),
    };
    let (ssh_enabled, ssh_host, ssh_port, ssh_user, ssh_auth, ssh_key_path) = match &saved.ssh {
        None => (
            false,
            String::new(),
            22,
            String::new(),
            SshAuthFormMode::Password,
            String::new(),
        ),
        Some(p) => {
            let auth = match &p.auth {
                SshAuthPolicy::Password => SshAuthFormMode::Password,
                SshAuthPolicy::KeyFile { .. } => SshAuthFormMode::KeyFile,
                SshAuthPolicy::Agent => SshAuthFormMode::Agent,
            };
            let key_path = match &p.auth {
                SshAuthPolicy::KeyFile { path } => path.clone(),
                _ => String::new(),
            };
            (true, p.host.clone(), p.port, p.user.clone(), auth, key_path)
        }
    };
    SecurityFormSnapshot {
        tls_mode,
        ca_bundle,
        client_cert,
        client_key,
        ssh_enabled,
        ssh_host,
        ssh_port,
        ssh_user,
        ssh_auth,
        ssh_key_path,
        dest_host: saved.host.clone(),
        dest_port: saved.port,
    }
}

/// A stable connection id from the display name (file-name safe).
pub fn slug_for(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "conn".into()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- TLS mapping ---------------------------------------------------

    #[test]
    fn tls_off_maps_to_none() {
        assert!(build_tls_config(TlsFormMode::Off, "/ca.pem", "/c.pem", "/k.pem").is_none());
    }

    #[test]
    fn tls_mode_maps_to_verify_full_with_optional_paths() {
        let bare = build_tls_config(TlsFormMode::Tls, "", "", "").unwrap();
        assert!(!bare.is_insecure());
        let with_ca = build_tls_config(TlsFormMode::Tls, "/etc/ca.pem ", "", "").unwrap();
        assert_eq!(
            with_ca.mode(),
            &redis_io::tls::TlsMode::VerifyFull {
                ca_bundle: Some(PathBuf::from("/etc/ca.pem")),
                client_cert: None,
            }
        );
    }

    #[test]
    fn tls_client_cert_requires_pair() {
        // Half a pair degrades to no client cert (validation catches it
        // earlier with a message; the mapping stays total).
        let half = build_tls_config(TlsFormMode::Tls, "", "/c.pem", "").unwrap();
        assert_eq!(
            half.mode(),
            &redis_io::tls::TlsMode::VerifyFull {
                ca_bundle: None,
                client_cert: None,
            }
        );
        let full = build_tls_config(TlsFormMode::Tls, "", "/c.pem", "/k.pem").unwrap();
        assert_eq!(
            full.mode(),
            &redis_io::tls::TlsMode::VerifyFull {
                ca_bundle: None,
                client_cert: Some((PathBuf::from("/c.pem"), PathBuf::from("/k.pem"))),
            }
        );
    }

    #[test]
    fn tls_insecure_maps_to_the_explicit_override_only() {
        let t = build_tls_config(TlsFormMode::Insecure, "", "", "").unwrap();
        assert!(t.is_insecure());
        assert!(t.warning().is_some());
        // every other mode stays verified/absent
        assert!(build_tls_config(TlsFormMode::Off, "", "", "").is_none());
        assert!(!build_tls_config(TlsFormMode::Tls, "", "", "")
            .unwrap()
            .is_insecure());
    }

    // ---- SSH mapping ----------------------------------------------------

    /// Minimal default snapshot for ssh mapping tests.
    fn snap() -> SecurityFormSnapshot {
        SecurityFormSnapshot::default()
    }

    #[test]
    fn ssh_disabled_maps_to_none() {
        assert!(build_ssh_tunnel(&snap(), "pw").is_none());
    }

    #[test]
    fn ssh_password_auth_wraps_secret_at_boundary() {
        let mut s = snap();
        s.ssh_enabled = true;
        s.ssh_host = "bastion".into();
        s.ssh_port = 2222;
        s.ssh_user = "adam".into();
        s.dest_host = "redis.internal".into();
        s.dest_port = 6379;
        let t = build_ssh_tunnel(&s, "swordfish").unwrap();
        assert_eq!(t.host, "bastion");
        assert_eq!(t.port, 2222);
        assert_eq!(t.dest_host, "redis.internal");
        assert_eq!(t.dest_port, 6379);
        // the secret never renders
        assert!(!format!("{t:?}").contains("swordfish"));
        match &t.auth {
            SshAuth::Password(s) => assert_eq!(s.reveal(), "swordfish"),
            _ => panic!("password auth expected"),
        }
    }

    #[test]
    fn ssh_key_file_and_agent_modes_map() {
        let mut s = snap();
        s.ssh_enabled = true;
        s.ssh_auth = SshAuthFormMode::KeyFile;
        s.ssh_key_path = "/id_ed25519".into();
        let k = build_ssh_tunnel(&s, "").unwrap();
        match &k.auth {
            SshAuth::KeyFile { path, passphrase } => {
                assert_eq!(path, &PathBuf::from("/id_ed25519"));
                assert!(passphrase.is_none());
            }
            _ => panic!("key file auth expected"),
        }
        s.ssh_auth = SshAuthFormMode::Agent;
        let a = build_ssh_tunnel(&s, "").unwrap();
        assert!(matches!(a.auth, SshAuth::Agent));
    }

    // ---- refusal parsing -------------------------------------------------

    /// The exact backend strings (redis-io establish / confirmed paths).
    const PROMPT_MSG: &str = "Other: SSH host key not confirmed yet (fingerprint \
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855); confirm and reconnect";
    const PROMPT_MSG_2: &str =
        "Other: SSH host key still awaiting confirmation (ba7816bf8f01cfea414140de5dae2223)";
    const BLOCKED_MSG: &str = "Other: SSH host key CHANGED for bastion:22 — connection \
blocked (recorded aaaa1111; presented bbbb2222)";

    #[test]
    fn prompt_refusal_parses_fingerprint() {
        assert_eq!(
            parse_ssh_refusal(PROMPT_MSG),
            Some(SshRefusal::Prompt {
                fingerprint: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .into()
            })
        );
        assert_eq!(
            parse_ssh_refusal(PROMPT_MSG_2),
            Some(SshRefusal::Prompt {
                fingerprint: "ba7816bf8f01cfea414140de5dae2223".into()
            })
        );
    }

    #[test]
    fn blocked_refusal_parses_both_fingerprints() {
        assert_eq!(
            parse_ssh_refusal(BLOCKED_MSG),
            Some(SshRefusal::Blocked {
                new_fingerprint: "bbbb2222".into(),
                old_fingerprint: "aaaa1111".into()
            })
        );
    }

    #[test]
    fn unrelated_errors_do_not_parse() {
        assert!(parse_ssh_refusal("connect failed: refused").is_none());
        assert!(parse_ssh_refusal("SSH authentication failed: password").is_none());
    }

    // ---- prefill plan (acceptance 7) --------------------------------------

    #[test]
    fn prefill_plan_covers_all_outcomes() {
        assert_eq!(
            prefill_plan(&CredentialOutcome::Ok("pw".into())),
            PrefillPlan {
                fill_password: true,
                note: None
            }
        );
        let p = prefill_plan(&CredentialOutcome::StoreUnavailable);
        assert!(!p.fill_password);
        assert_eq!(p.note, Some(NOTE_KEYCHAIN_UNAVAILABLE));
        let p = prefill_plan(&CredentialOutcome::NotSaved);
        assert!(!p.fill_password);
        assert_eq!(p.note, Some(NOTE_NO_SAVED_SECRET));
        let p = prefill_plan(&CredentialOutcome::Failure("boom".into()));
        assert!(!p.fill_password);
        assert_eq!(p.note, Some(NOTE_STORE_ERROR));
    }

    // ---- saved-connection mapping round trip -------------------------------

    #[test]
    fn snapshot_round_trips_through_the_persisted_policies() {
        let snap = SecurityFormSnapshot {
            tls_mode: TlsFormMode::Tls,
            ca_bundle: "/ca.pem".into(),
            client_cert: "/c.pem".into(),
            client_key: "/k.pem".into(),
            ssh_enabled: true,
            ssh_host: "bastion".into(),
            ssh_port: 2222,
            ssh_user: "adam".into(),
            ssh_auth: SshAuthFormMode::KeyFile,
            ssh_key_path: "/id_ed25519".into(),
            dest_host: "redis.internal".into(),
            dest_port: 6379,
        };
        let saved = connections_store::saved_connections::SavedConnection {
            id: "c".into(),
            name: "prod".into(),
            host: "redis.internal".into(),
            port: 6379,
            db: 0,
            username: None,
            tls: snap.tls_policy(),
            ssh: snap.ssh_policy(),
        };
        // save direction: paths only, no password anywhere
        let json = serde_json_json(&saved);
        assert!(!json.contains("password"));
        assert!(json.contains("/ca.pem"));
        // prefill direction: the snapshot restores
        assert_eq!(snapshot_from_saved(&saved), snap);
    }

    fn serde_json_json(s: &connections_store::saved_connections::SavedConnection) -> String {
        serde_json::to_string(s).expect("serialize")
    }

    #[test]
    fn insecure_and_disabled_snapshots_round_trip() {
        let snap = SecurityFormSnapshot {
            tls_mode: TlsFormMode::Insecure,
            ..SecurityFormSnapshot::default()
        };
        let saved = connections_store::saved_connections::SavedConnection {
            id: "c".into(),
            name: "n".into(),
            host: "h".into(),
            port: 6379,
            db: 0,
            username: None,
            tls: snap.tls_policy(),
            ssh: snap.ssh_policy(),
        };
        assert_eq!(saved.tls, TlsPolicy::InsecureOverride);
        assert!(saved.ssh.is_none());
        let back = snapshot_from_saved(&saved);
        assert_eq!(back.tls_mode, TlsFormMode::Insecure);
        assert!(!back.ssh_enabled);
    }

    #[test]
    fn slug_is_filename_safe() {
        assert_eq!(slug_for("Prod Redis!"), "prod-redis");
        assert_eq!(slug_for("  "), "conn");
        assert_eq!(slug_for("a//b"), "a-b");
    }
}
