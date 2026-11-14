//! Values are bytes; views are views.
//!
//! This crate is the byte-preservation core (PLAN.md section 4):
//! - [`raw`] keeps the original bytes of every value, never converting them.
//! - [`views`] renders Text/JSON/Hex over the bytes; a save from a **lossy**
//!   text view is structurally forbidden (hex only).
//! - [`editor`] holds dirty-editor state; the working buffer survives refresh
//!   (refresh routes new bytes to the `original` side and sets a divergence
//!   flag).
//! - [`conflict`] handles optimistic save via `WATCH/MULTI` where the type
//!   allows, with an honest disclosure where it cannot be atomic.
//! - [`ttl`] covers TTL display and expiry removal on refresh.
//! - [`unsupported`] classifies value types this client cannot edit.
//!
//! Phase 0: placeholder only.

/// `RawValue`: original bytes, opaque and round-trip-guaranteed.
pub mod raw;

/// Read-only view descriptors: Text / JSON / Hex renderers over raw bytes.
pub mod views;

/// Dirty-editor state machine (working buffer vs. original bytes).
pub mod editor;

/// Concurrent-modification conflict handling (`WATCH/MULTI` where possible).
pub mod conflict;

/// Value types this client can display but not edit.
pub mod unsupported;

/// TTL model: display, refresh semantics, expiry removal.
pub mod ttl;
