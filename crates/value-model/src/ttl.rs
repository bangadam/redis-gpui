//! TTL model (PRD story 32, acceptance 4).
//!
//! Data-only; the app layer pairs a value with its [`TtlState`] and the UI
//! renders from it. Redis `TTL` semantics per the official docs:
//!
//! - `-2` — key does not exist (surfaced by callers as a missing key, not
//!   as a TTL).
//! - `-1` — key exists but has **no** expiry.
//! - `≥ 0` — seconds remaining (Redis reports truncated seconds; `-1` and
//!   the no-expiry case are what this model distinguishes).

/// The expiry state of one key.
///
/// `None` means no expiry (`TTL` = `-1`): the key lives until deleted.
/// `Some(n)` with `n >= 0` is the seconds remaining. The `-2` (missing
/// key) reply is not representable here — a missing key has no value to
/// model; callers handle it in the refresh path (acceptance 4: expiry
/// removes the key from view on refresh).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtlState {
    /// `None`: no expiry (Redis `TTL` = `-1`). `Some(seconds)`: seconds
    /// remaining.
    pub seconds: Option<i64>,
}

impl TtlState {
    /// A key with no expiry (`TTL` = `-1`).
    pub fn no_expiry() -> Self {
        Self { seconds: None }
    }

    /// A key expiring in `seconds` (must be ≥ 0 — the `-1`/`-2` sentinels
    /// are not TTL *values*, they are replies; use [`TtlState::no_expiry`]
    /// for the no-expiry case).
    ///
    /// # Panics
    /// If `seconds < 0` — the sentinel replies must not be smuggled in as
    /// a duration.
    pub fn expiring_in(seconds: i64) -> Self {
        assert!(
            seconds >= 0,
            "TTL seconds must be >= 0; use no_expiry() for -1"
        );
        Self {
            seconds: Some(seconds),
        }
    }

    /// Maps a raw Redis `TTL` reply to a state, handling the documented
    /// sentinels honestly: `-1` → no expiry; `-2` → the key is gone
    /// (returned as [`TtlState::missing_key`] — distinct from any stored
    /// state so the refresh path can remove the key from view).
    pub fn from_redis_reply(raw: i64) -> TtlReply {
        match raw {
            -2 => TtlReply::MissingKey,
            -1 => TtlReply::State(Self::no_expiry()),
            n => TtlReply::State(Self::expiring_in(n)),
        }
    }

    /// True when the key has an expiry.
    pub fn has_expiry(&self) -> bool {
        self.seconds.is_some()
    }
}

/// The result of mapping a raw `TTL` reply: either a usable state or the
/// "key no longer exists" answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlReply {
    /// The key does not exist (`TTL` = `-2`): the refresh path should drop
    /// the key from view (acceptance 4).
    MissingKey,
    /// The key's TTL state.
    State(TtlState),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_expiry_is_none() {
        let ttl = TtlState::no_expiry();
        assert_eq!(ttl.seconds, None);
        assert!(!ttl.has_expiry());
    }

    #[test]
    fn positive_seconds_roundtrip() {
        let ttl = TtlState::expiring_in(42);
        assert_eq!(ttl.seconds, Some(42));
        assert!(ttl.has_expiry());
    }

    #[test]
    fn redis_reply_sentinels_map_honestly() {
        assert_eq!(
            TtlState::from_redis_reply(-1),
            TtlReply::State(TtlState::no_expiry())
        );
        assert_eq!(TtlState::from_redis_reply(-2), TtlReply::MissingKey);
        assert_eq!(
            TtlState::from_redis_reply(0),
            TtlReply::State(TtlState::expiring_in(0))
        );
        assert_eq!(
            TtlState::from_redis_reply(300),
            TtlReply::State(TtlState::expiring_in(300))
        );
    }

    #[test]
    #[should_panic(expected = "TTL seconds must be >= 0")]
    fn negative_duration_is_rejected() {
        TtlState::expiring_in(-1);
    }
}
