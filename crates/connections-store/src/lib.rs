//! Saved connections with a no-plaintext guarantee (Phase 6).
//!
//! This crate keeps connection metadata auditable in one place (PLAN.md
//! section 3): saved connection files hold **only non-secret fields**;
//! secrets live in the OS keyring (macOS Keychain / Windows Credential
//! Manager / Linux Secret Service). Keyring absent → the store layer
//! signals [`CredentialOutcome::PromptPerSession`], a first-class state,
//! not an error path (PLAN.md section 4, PRD acceptance 7). TOFU
//! known-hosts for SSH also live here.
//!
//! # Module map
//!
//! - [`known_hosts`] — the TOFU known-hosts store (pure; fingerprints
//!   only, never secret material)
//! - [`credentials`] — the `CredentialStore` trait, the OS-keyring
//!   implementation, and the mock seam for the keyring-absent state
//! - [`saved_connections`] — the on-disk metadata files (non-secret
//!   fields only) under a configurable config dir

pub mod credentials;
pub mod known_hosts;
pub mod saved_connections;
