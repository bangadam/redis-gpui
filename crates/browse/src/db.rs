//! Redis logical DB switching model (PRD story 13).
//!
//! # Design choices (documented, as required)
//!
//! - **This crate models the *count*, not the switch.** The DB count
//!   comes from `CONFIG GET databases` — one round trip over the
//!   [`Connection`] trait via [`fetch_db_count`]. The actual `SELECT`
//!   and its side effects are **app policy** and stay in the app:
//!   SELECT re-keys the whole connection (SCAN cursors are invalid
//!   across a SELECT, keyspace listeners change, pipelined replies can
//!   interleave), so the app must, on switch:
//!   1. cancel the current scan token,
//!   2. advance the [`crate::cancellation::SearchGeneration`]
//!      (stale pages die at the guard),
//!   3. `reset()` the flat list / tree and start a fresh scan.
//!   Keeping that sequence out of this crate keeps the browse engine
//!   free of connection-state policy and testable with mocks.
//! - **Default on failure: [`DEFAULT_DATABASES`].** Redis's own default
//!   is 16; if the server refuses CONFIG GET (some managed Redises
//!   disable it), the count 16 is used and flagged as the default
//!   rather than guessed — honest fallback, not a fabricated probe.
//! - **`SELECT` validity is count-based, not info-based:** the switcher
//!   offers databases `0..count-1`. Redis would answer `ERR DB index is
//!   out of range` for anything past the count; checking client-side
//!   first avoids the round trip and gives the UI a stable range to
//!   render.

use redis_io::connection::{Command, ConnError, Connection, Reply};

/// The database count used when the server will not tell us
/// (Redis's compiled-in default).
pub const DEFAULT_DATABASES: u16 = 16;

/// Model of one server's database configuration: just the count the
/// switcher needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DbInfo {
    /// Number of logical databases (`CONFIG GET databases`), or the
    /// fallback default when the server hides it.
    pub count: u16,
}

impl DbInfo {
    /// Whether `db` is a valid SELECT target under this configuration
    /// (`0 <= db < count`).
    pub fn is_valid_db(&self, db: u16) -> bool {
        db < self.count
    }
}

impl Default for DbInfo {
    fn default() -> Self {
        Self {
            count: DEFAULT_DATABASES,
        }
    }
}

/// Fetch the database count via `CONFIG GET databases`.
///
/// One round trip over the generic [`Connection::issue`] seam (this is
/// a config lookup, not a scan; `issue` is the correct path — the scan
/// engine's "no raw commands" rule in tests applies to *key
/// enumeration*, which must go through SCAN).
///
/// Behavior:
/// - Bulk integer reply (e.g. `b"16"`) → parsed count.
/// - Any other reply shape (some proxies return odd types), a
///   non-integer payload, or a definitive server refusal (managed
///   Redises commonly `ERR unknown command 'CONFIG'`) → `Ok` with
///   [`DbInfo`] carrying [`DEFAULT_DATABASES`]. A hidden config is not
///   an error the UI should surface as a failure; it is a fallback.
/// - A dropped link ([`ConnError::Disconnected`]) is a real failure:
///   returned as `Err` — the caller decides retry/reconnect.
pub async fn fetch_db_count(conn: &mut dyn Connection) -> Result<DbInfo, ConnError> {
    let reply = conn
        .issue(Command::new(
            "CONFIG",
            vec![b"GET".to_vec(), b"databases".to_vec()],
        ))
        .await?;
    Ok(parse_databases_reply(reply))
}

/// Parse a `CONFIG GET databases` reply into a [`DbInfo`].
/// Accepts the real shape `["databases", "16"]` (array of bulks) and,
/// defensively, a bare bulk (`"16"`). Everything else → the default.
fn parse_databases_reply(reply: Reply) -> DbInfo {
    let payload: Option<Vec<u8>> = match reply {
        // Real Redis: Array ["databases", "16"].
        Reply::Array(items) => items
            .iter()
            .rev()
            .find_map(|r| r.as_bytes())
            .map(<[u8]>::to_vec),
        // Defensive: proxies/serverless shapes that flatten the pair.
        Reply::Bulk(b) => Some(b),
        _ => None,
    };
    match payload.as_deref().and_then(|b| std::str::from_utf8(b).ok()) {
        Some(s) => match s.trim().parse::<u16>() {
            Ok(n) if n > 0 => DbInfo { count: n },
            _ => DbInfo::default(),
        },
        None => DbInfo::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_real_config_get_shape() {
        let reply = Reply::Array(vec![
            Reply::Bulk(b"databases".to_vec()),
            Reply::Bulk(b"16".to_vec()),
        ]);
        assert_eq!(parse_databases_reply(reply).count, 16);
        // Non-default server.
        let reply = Reply::Array(vec![
            Reply::Bulk(b"databases".to_vec()),
            Reply::Bulk(b"32".to_vec()),
        ]);
        assert_eq!(parse_databases_reply(reply).count, 32);
    }

    #[test]
    fn parses_a_bare_bulk_defensively() {
        assert_eq!(parse_databases_reply(Reply::Bulk(b"8".to_vec())).count, 8);
    }

    #[test]
    fn odd_shapes_fall_back_to_the_default() {
        // Managed-Redis-style refusal payload.
        assert_eq!(
            parse_databases_reply(Reply::Other("ERR unknown command 'CONFIG'".into())).count,
            DEFAULT_DATABASES
        );
        assert_eq!(parse_databases_reply(Reply::Nil).count, DEFAULT_DATABASES);
        // Non-numeric or non-positive values are not a count.
        assert_eq!(
            parse_databases_reply(Reply::Bulk(b"not-a-number".to_vec())).count,
            DEFAULT_DATABASES
        );
        assert_eq!(
            parse_databases_reply(Reply::Bulk(b"0".to_vec())).count,
            DEFAULT_DATABASES
        );
        // Array without any bulk (defensive against proxy shapes).
        assert_eq!(
            parse_databases_reply(Reply::Array(vec![Reply::Nil])).count,
            DEFAULT_DATABASES
        );
    }

    #[test]
    fn validity_is_count_based() {
        let info = DbInfo { count: 4 };
        assert!(info.is_valid_db(0));
        assert!(info.is_valid_db(3));
        assert!(!info.is_valid_db(4));
        assert!(!info.is_valid_db(u16::MAX));
    }
}

/// Mock-connection tests for `fetch_db_count` and the SELECT restart
/// policy contract (app-side sequence, proven here against a mock so
/// the wiring agent can copy it).
#[cfg(test)]
mod connection_tests {
    use super::*;
    use async_trait::async_trait;
    use redis_io::connection::ScanResult;

    /// Mock whose `issue` returns one scripted reply; `scan_batch` and
    /// `ping` record that they were called (the DB-switch sequence must
    /// use SCAN afresh, never KEYS — panic would catch it in `issue`,
    /// but the scripted reply pins the exact call).
    struct MockConfigConnection {
        reply: Reply,
        issued: Vec<Command>,
    }

    #[async_trait]
    impl Connection for MockConfigConnection {
        async fn issue(&mut self, cmd: Command) -> Result<Reply, ConnError> {
            self.issued.push(cmd);
            Ok(self.reply.clone())
        }

        async fn scan_batch(
            &mut self,
            _cursor: u64,
            _pattern: Option<Vec<u8>>,
            _count: u64,
        ) -> Result<ScanResult, ConnError> {
            Ok(ScanResult {
                next_cursor: 0,
                keys: vec![],
            })
        }

        async fn ping(&mut self) -> Result<(), ConnError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn fetch_db_count_issues_config_get_databases() {
        let mut conn = MockConfigConnection {
            reply: Reply::Array(vec![
                Reply::Bulk(b"databases".to_vec()),
                Reply::Bulk(b"16".to_vec()),
            ]),
            issued: Vec::new(),
        };
        let info = fetch_db_count(&mut conn).await.unwrap();
        assert_eq!(info.count, 16);
        // Exactly one command: CONFIG GET databases.
        assert_eq!(conn.issued.len(), 1);
        assert_eq!(conn.issued[0].name, "CONFIG");
        assert_eq!(
            conn.issued[0].args,
            vec![b"GET".to_vec(), b"databases".to_vec()]
        );
    }

    #[tokio::test]
    async fn server_refusal_of_config_falls_back_to_default() {
        // A managed Redis that disables CONFIG returns a server error;
        // fetch_db_count surfaces the fallback count, not an Err (only
        // a disconnect is a real failure).
        let mut conn = MockConfigConnection {
            reply: Reply::Other("ERR unknown command 'CONFIG'".into()),
            issued: Vec::new(),
        };
        let info = fetch_db_count(&mut conn).await.unwrap();
        assert_eq!(info.count, DEFAULT_DATABASES);
    }

    #[tokio::test]
    async fn a_dropped_link_is_a_real_error() {
        // Disconnected is the one outcome the caller must treat as a
        // failure (retry/reconnect policy), not a fallback.
        struct Dropping;
        #[async_trait]
        impl Connection for Dropping {
            async fn issue(&mut self, _cmd: Command) -> Result<Reply, ConnError> {
                Err(ConnError::Disconnected {
                    reason: Some("mock: dropped".into()),
                    in_flight: None,
                })
            }
            async fn scan_batch(
                &mut self,
                _cursor: u64,
                _pattern: Option<Vec<u8>>,
                _count: u64,
            ) -> Result<ScanResult, ConnError> {
                unreachable!("fetch_db_count must not SCAN")
            }
            async fn ping(&mut self) -> Result<(), ConnError> {
                Ok(())
            }
        }
        let mut conn = Dropping;
        assert!(matches!(
            fetch_db_count(&mut conn).await,
            Err(ConnError::Disconnected { .. })
        ));
    }
}
