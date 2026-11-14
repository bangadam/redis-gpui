//! TLS policy and root-store construction (Phase 6).
//!
//! [`TlsConfig`] is the **policy** type — paths only, never secret bytes.
//! Actual PEM material is loaded from those paths at connect time by
//! [`crate::redis_backend`] and stays out of config files and logs.
//!
//! # Modes
//!
//! - [`TlsMode::Disabled`] — plaintext TCP.
//! - [`TlsMode::VerifyFull`] — **default**: webpki roots + custom CAs +
//!   hostname verification. Untrusted chain / hostname mismatch / expired
//!   cert → the connection attempt fails with a descriptive error. Never
//!   a silent downgrade.
//! - [`TlsMode::InsecureOverride`] — explicit opt-in only. The backend
//!   must surface [`TlsWarning`] (persistent, on the event stream) for
//!   as long as the connection is active — PRD acceptance 5 requires an
//!   override that *warns*.
//!
//! # Root-store verification (pure, unit-tested)
//!
//! [`build_root_store`] is the honest core: given CA bytes it builds a
//! rustls `RootCertStore`. A self-signed CA it trusts; the webpki-bundle
//! default is also exercised. See tests for exactly what is and is NOT
//! covered (live handshakes are boundary-tier, not claimed here).

use std::path::PathBuf;

use rustls::pki_types::CertificateDer;
use rustls::RootCertStore;

/// TLS verification mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TlsMode {
    /// No TLS: plaintext TCP.
    Disabled,
    /// Full verification: chain + hostname + validity (the default).
    VerifyFull {
        /// PEM bundle of a custom certificate authority; `None` uses
        /// the built-in webpki + system roots.
        ca_bundle: Option<PathBuf>,
        /// Client certificate + key (PEM) for mTLS: `(cert_path, key_path)`.
        client_cert: Option<(PathBuf, PathBuf)>,
    },
    /// Skip certificate verification — explicit opt-in ONLY, via
    /// [`TlsConfig::insecure_override`]. Surfaces a persistent
    /// [`TlsWarning`] while active.
    InsecureOverride,
}

impl Default for TlsMode {
    fn default() -> Self {
        TlsMode::VerifyFull {
            ca_bundle: None,
            client_cert: None,
        }
    }
}

/// TLS policy for one connection. Paths only — no secret bytes.
///
/// The insecure override is constructible ONLY via
/// [`TlsConfig::insecure_override`] — an explicit, greppable call site.
/// There is no `From<bool>`, no serde default, no other path.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TlsConfig {
    mode: TlsMode,
}

impl TlsConfig {
    /// Default: full verification (custom roots optional).
    pub fn verify_full(
        ca_bundle: Option<PathBuf>,
        client_cert: Option<(PathBuf, PathBuf)>,
    ) -> Self {
        Self {
            mode: TlsMode::VerifyFull {
                ca_bundle,
                client_cert,
            },
        }
    }

    /// Explicitly construct the insecure override.
    ///
    /// Every call site of this function is a deliberate, reviewable
    /// decision to disable TLS verification. While any connection with
    /// this mode is active the UI must render a persistent warning
    /// (PRD story 5, acceptance 5).
    pub fn insecure_override() -> Self {
        Self {
            mode: TlsMode::InsecureOverride,
        }
    }

    /// TLS disabled (plaintext).
    pub fn disabled() -> Self {
        Self {
            mode: TlsMode::Disabled,
        }
    }

    /// `true` when this config disables verification.
    pub fn is_insecure(&self) -> bool {
        matches!(self.mode, TlsMode::InsecureOverride)
    }

    /// The [`TlsWarning`] the backend must surface persistently while a
    /// connection with this config is active, if any.
    ///
    /// Returned by the backend at establish time; the UI keeps it in
    /// connection state (not a one-shot log line).
    pub fn warning(&self) -> Option<TlsWarning> {
        self.is_insecure().then(|| TlsWarning {
            message: TLS_INSECURE_WARNING.to_string(),
        })
    }

    /// Borrow the mode (match target for persistence/backends).
    pub fn mode(&self) -> &TlsMode {
        &self.mode
    }
}

/// The persistent warning attached to insecure-override connections.
///
/// Plain data, serializable by the UI layer; carried on
/// [`crate::connection::ConnEvent::TlsWarning`] and kept in connection
/// state for as long as the connection is live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsWarning {
    /// Human-readable, non-sensitive warning text.
    pub message: String,
}

/// The text every insecure connection carries (acceptance 5).
pub const TLS_INSECURE_WARNING: &str =
    "TLS certificate verification is disabled for this connection — MITM is possible";

/// Build a rustls root store from PEM CA bytes.
///
/// Pure: used by tests directly, and at connect time by the backend for
/// custom CA bundles. Empty/no-cert input yields an empty store (the
/// caller decides whether that is an error context).
///
/// # Errors
/// - malformed PEM
/// - a certificate that rustls cannot parse
pub fn build_root_store(ca_pem: &[u8]) -> Result<RootCertStore, String> {
    let mut store = RootCertStore::empty();
    let mut cursor = std::io::Cursor::new(ca_pem);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cursor)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("CA bundle is not valid PEM: {e}"))?;
    if certs.is_empty() {
        return Err("CA bundle contains no certificates".into());
    }
    for cert in certs {
        store
            .add(cert)
            .map_err(|e| format!("CA certificate rejected by rustls: {e}"))?;
    }
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_verify_full() {
        assert!(!TlsConfig::default().is_insecure());
        assert_eq!(
            TlsConfig::default().mode(),
            &TlsMode::VerifyFull {
                ca_bundle: None,
                client_cert: None,
            }
        );
    }

    #[test]
    fn insecure_requires_explicit_constructor() {
        let t = TlsConfig::insecure_override();
        assert!(t.is_insecure());
    }

    #[test]
    fn insecure_mode_carries_persistent_warning() {
        let t = TlsConfig::insecure_override();
        let w = t.warning().expect("insecure must warn");
        assert!(w.message.contains("MITM"));
        // and verified mode never warns
        assert!(TlsConfig::default().warning().is_none());
    }

    #[test]
    fn debug_prints_paths_but_that_is_non_sensitive() {
        let t = TlsConfig::verify_full(
            Some(PathBuf::from("/etc/ca.pem")),
            Some((PathBuf::from("/c.pem"), PathBuf::from("/k.pem"))),
        );
        assert!(format!("{t:?}").contains("/etc/ca.pem"));
    }

    // ---------- root-store builder (pure, with a real generated cert) ----------

    #[test]
    fn root_store_accepts_a_self_signed_ca() {
        // rcgen generates an in-memory self-signed CA — the exact shape a
        // user's custom CA bundle has.
        let ca = rcgen::generate_simple_self_signed(vec!["redis-ca.test".into()]).expect("gen ca");
        let pem = ca.cert.pem();
        let store = build_root_store(pem.as_bytes()).expect("ca accepted");
        assert_eq!(store.len(), 1, "one trust anchor");
    }

    #[test]
    fn root_store_rejects_empty_bundle() {
        assert!(build_root_store(b"").is_err());
        assert!(build_root_store(b"not a pem at all").is_err());
    }

    #[test]
    fn root_store_rejects_garbage_pem() {
        let pem = b"-----BEGIN CERTIFICATE-----\nnot base64 !!!\n-----END CERTIFICATE-----\n";
        assert!(build_root_store(pem).is_err());
    }

    #[test]
    fn root_store_rejects_non_certificate_pem() {
        // A PEM that parses but is not a certificate (a key).
        let key = rcgen::generate_simple_self_signed(vec!["k".into()])
            .expect("gen")
            .signing_key
            .serialize_pem();
        let err = build_root_store(key.as_bytes())
            .expect_err("non-cert PEM must not become a trust anchor");
        assert!(err.contains("no certificates"));
    }
}
