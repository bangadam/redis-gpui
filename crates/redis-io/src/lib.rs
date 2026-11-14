//! All Redis network I/O in one place.
//!
//! This crate owns connection management, TLS, the SSH tunnel, command
//! futures, and **tri-state write results** (`Acknowledged` /
//! `Failed(definitive)` / `OutcomeUnknown`) — hiding the underlying Redis
//! client crate behind our own [`connection::Connection`] trait, which is the
//! fault-injection seam for testing dropped acknowledgments (PLAN.md
//! sections 3-4).
//!
//! # The auto-reconnect trap (critical; how this crate defuses it)
//!
//! redis-rs' `ConnectionManager` auto-reconnects and can replay in-flight
//! writes across the boundary — a silent duplicate write, exactly what
//! PRD story 40 / acceptance 3 forbids. Therefore the real backend
//! ([`redis_backend::RedisConnection`]) does NOT use `ConnectionManager`:
//! it uses a plain `MultiplexedConnection`, translates any
//! connection-dropped error into [`connection::ConnError::Disconnected`]
//! with the in-flight command attached (which the call site maps to
//! [`outcome::WriteOutcome::OutcomeUnknown`]), and stays broken until an
//! explicit user-initiated reconnect. Nothing is ever auto re-sent. This
//! behavior is proven by the fault-injection test
//! (`faults_drop_reply_then_disconnect...`).
//!
//! # Module map
//!
//! - [`connection`] — the `Connection` trait + event channel model
//! - [`redis_backend`] — the real redis-rs implementation + config
//! - [`outcome`] — tri-state write results (acceptance-3 backbone)
//! - [`faults`] — fault-injection seam (feature `fault-injection`, off by
//!   default, cannot leak into release builds)
//! - [`tls`] — TLS policy types (Phase 1 scaffolding)
//! - [`ssh`] — SSH tunnel data types + TOFU policy docs (Phase 6 wiring)
//! - [`log_hygiene`] — `Secret` newtype + redaction helpers

/// Connection management behind the `Connection` trait (the fault-injection
/// seam) and the auto-reconnect wrapper that yields `OutcomeUnknown`.
pub mod connection;

/// The real redis-rs backend: `RedisConnection`, `RedisConnectionConfig`.
pub mod redis_backend;

/// Tri-state write outcome model: `Acknowledged` / `Failed` / `OutcomeUnknown`.
pub mod outcome;

/// TLS support: custom CAs, client certificates, insecure override.
pub mod tls;

/// SSH tunneling (single hop, enforced structurally) and TOFU host keys.
pub mod ssh;

/// The russh-backed tunnel implementation (feature `ssh` only).
#[cfg(feature = "ssh")]
pub mod tunnel;

/// Logging hygiene: nothing sensitive (hostnames, keys, values, secrets)
/// reaches logs.
pub mod log_hygiene;

/// Fault injection (feature-gated; OFF by default).
#[cfg(feature = "fault-injection")]
pub mod faults;
