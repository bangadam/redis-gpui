//! App-side mapping from redis-io `Reply` shapes to value-model
//! `RawValue` (PLAN.md section 2.4/2.7, PRD stories 19–22).
//!
//! value-model deliberately has no redis-io dependency (its `RawValue`
//! is pure bytes), so the Reply → RawValue conversion lives here, in the
//! app crate — the only crate that sees both types. This module is the
//! *single* place raw replies become modeled values; everywhere else
//! works from [`RawValue`].
//!
//! # Contract
//!
//! - Byte-identity: every `Bulk` payload becomes [`RawBytes`] untouched.
//!   Nothing here decodes, normalizes, or re-orders bytes (container
//!   order is the server's reply order).
//! - Honesty: shapes that do not match the documented reply layout of
//!   the type produce [`ReplyToRawError`], never a best-effort guess.
//! - `Ok(None)` means "the key is not there" (a `Nil` reply — GET on a
//!   vanished key); the session maps that to the missing-key panel.
//! - Module/unknown types never pass through here; the session routes
//!   them to `ReadonlyUnsupported` before issuing a read command.
//!
//! # Reply layouts (RESP2, verified against the commands' docs)
//!
//! - `HGETALL` → flat `[field, value, field, value, …]`
//! - `LRANGE 0 -1` → `[item, …]`
//! - `SMEMBERS` → `[member, …]` (RESP3 set replies decode to
//!   `Reply::Array` in redis-io)
//! - `ZRANGE 0 -1 WITHSCORES` → `[member, score, member, score, …]`
//!   (scores are ASCII numbers; Redis parses them canonically, so the
//!   model keeps `f64`)
//! - `XRANGE - +` → `[[id, [field, value, …]], …]`

use redis_io::connection::{Command, Reply};
use value_model::raw::{RawBytes, RawValue, StreamEntry, ZSetEntry};

/// The full-read command for one Redis type, or `None` when this client
/// does not model the type (module types, `none`).
///
/// Phase 3 reads whole containers (PRD scale story is about *key
/// listing*, not entry pagination); huge-list pagination is Phase 5
/// polish.
pub fn full_read_command(type_str: &str, key: &[u8]) -> Option<Command> {
    let key = key.to_vec();
    match type_str {
        "string" => Some(Command::new("GET", vec![key])),
        "hash" => Some(Command::new("HGETALL", vec![key])),
        "list" => Some(Command::new(
            "LRANGE",
            vec![key, b"0".to_vec(), b"-1".to_vec()],
        )),
        "set" => Some(Command::new("SMEMBERS", vec![key])),
        "zset" => Some(Command::new(
            "ZRANGE",
            vec![key, b"0".to_vec(), b"-1".to_vec(), b"WITHSCORES".to_vec()],
        )),
        "stream" => Some(Command::new(
            "XRANGE",
            vec![key, b"-".to_vec(), b"+".to_vec()],
        )),
        _ => None,
    }
}

/// Why a reply could not become a [`RawValue`]. Messages are
/// non-sensitive (shapes and type names only — no key/value bytes, per
/// redis-io log hygiene).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyToRawError {
    /// The reply's shape does not match the documented layout for the
    /// type (e.g. an odd-length HGETALL array). Carries the type name.
    UnexpectedShape(String),
    /// A zset score was not parsable as a number.
    BadScore,
    /// The server replied with an error (e.g. `WRONGTYPE` after a race),
    /// already flattened by redis-io into `Reply::Other`. Carries the
    /// non-sensitive label.
    Server(String),
    /// The type has no modeled full-read (caller should have routed it
    /// to `ReadonlyUnsupported` first).
    UnmodeledType(String),
}

impl std::fmt::Display for ReplyToRawError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplyToRawError::UnexpectedShape(t) => {
                write!(f, "unexpected reply shape for type {t}")
            }
            ReplyToRawError::BadScore => {
                write!(f, "zset reply contained an unparsable score")
            }
            ReplyToRawError::Server(label) => write!(f, "{label}"),
            ReplyToRawError::UnmodeledType(t) => {
                write!(f, "type {t} has no read conversion")
            }
        }
    }
}

/// Convert a full-read `Reply` for `type_str` into a [`RawValue`].
///
/// `Ok(None)` = the key is not there (`Nil`). Errors are honest and
/// displayable. The conversion is total for the six modeled types; it
/// never inspects the bytes.
pub fn reply_to_raw(type_str: &str, reply: Reply) -> Result<Option<RawValue>, ReplyToRawError> {
    match reply {
        Reply::Nil => Ok(None),
        Reply::Other(label) => Err(ReplyToRawError::Server(label)),
        _ => match type_str {
            "string" => from_string(reply),
            "hash" => from_hash(reply),
            "list" => from_list(reply),
            "set" => from_set(reply),
            "zset" => from_zset(reply),
            "stream" => from_stream(reply),
            other => Err(ReplyToRawError::UnmodeledType(other.to_string())),
        },
    }
}

fn from_string(reply: Reply) -> Result<Option<RawValue>, ReplyToRawError> {
    match reply {
        Reply::Bulk(bytes) => Ok(Some(RawValue::String(RawBytes::new(bytes)))),
        _ => Err(ReplyToRawError::UnexpectedShape("string".into())),
    }
}

fn from_hash(reply: Reply) -> Result<Option<RawValue>, ReplyToRawError> {
    let Reply::Array(items) = reply else {
        return Err(ReplyToRawError::UnexpectedShape("hash".into()));
    };
    if items.len() % 2 != 0 {
        return Err(ReplyToRawError::UnexpectedShape("hash".into()));
    }
    let mut pairs = Vec::with_capacity(items.len() / 2);
    let mut items = items.into_iter();
    while let (Some(field), Some(value)) = (items.next(), items.next()) {
        match (field, value) {
            (Reply::Bulk(f), Reply::Bulk(v)) => {
                pairs.push((RawBytes::new(f), RawBytes::new(v)));
            }
            _ => return Err(ReplyToRawError::UnexpectedShape("hash".into())),
        }
    }
    Ok(Some(RawValue::Hash(pairs)))
}

fn from_list(reply: Reply) -> Result<Option<RawValue>, ReplyToRawError> {
    let Reply::Array(items) = reply else {
        return Err(ReplyToRawError::UnexpectedShape("list".into()));
    };
    let list = bulk_all(items, "list")?;
    Ok(Some(RawValue::List(list)))
}

fn from_set(reply: Reply) -> Result<Option<RawValue>, ReplyToRawError> {
    let Reply::Array(items) = reply else {
        return Err(ReplyToRawError::UnexpectedShape("set".into()));
    };
    let members = bulk_all(items, "set")?;
    Ok(Some(RawValue::Set(members)))
}

fn from_zset(reply: Reply) -> Result<Option<RawValue>, ReplyToRawError> {
    let Reply::Array(items) = reply else {
        return Err(ReplyToRawError::UnexpectedShape("zset".into()));
    };
    if items.len() % 2 != 0 {
        return Err(ReplyToRawError::UnexpectedShape("zset".into()));
    }
    let mut entries = Vec::with_capacity(items.len() / 2);
    let mut items = items.into_iter();
    while let (Some(member), Some(score)) = (items.next(), items.next()) {
        match (member, score) {
            (Reply::Bulk(m), Reply::Bulk(s)) => {
                let score = std::str::from_utf8(&s)
                    .ok()
                    .and_then(|t| t.parse::<f64>().ok())
                    .ok_or(ReplyToRawError::BadScore)?;
                entries.push(ZSetEntry {
                    member: RawBytes::new(m),
                    score,
                });
            }
            _ => return Err(ReplyToRawError::UnexpectedShape("zset".into())),
        }
    }
    Ok(Some(RawValue::ZSet(entries)))
}

fn from_stream(reply: Reply) -> Result<Option<RawValue>, ReplyToRawError> {
    let Reply::Array(entries) = reply else {
        return Err(ReplyToRawError::UnexpectedShape("stream".into()));
    };
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        // Each entry: [id, [field, value, field, value, …]]
        let Reply::Array(mut parts) = entry else {
            return Err(ReplyToRawError::UnexpectedShape("stream".into()));
        };
        if parts.len() != 2 {
            return Err(ReplyToRawError::UnexpectedShape("stream".into()));
        }
        let fields_reply = parts.pop().expect("len checked");
        let id_reply = parts.pop().expect("len checked");
        let (Reply::Bulk(id), Reply::Array(fields)) = (id_reply, fields_reply) else {
            return Err(ReplyToRawError::UnexpectedShape("stream".into()));
        };
        if fields.len() % 2 != 0 {
            return Err(ReplyToRawError::UnexpectedShape("stream".into()));
        }
        let mut pairs = Vec::with_capacity(fields.len() / 2);
        let mut fields = fields.into_iter();
        while let (Some(f), Some(v)) = (fields.next(), fields.next()) {
            match (f, v) {
                (Reply::Bulk(f), Reply::Bulk(v)) => {
                    pairs.push((RawBytes::new(f), RawBytes::new(v)));
                }
                _ => return Err(ReplyToRawError::UnexpectedShape("stream".into())),
            }
        }
        out.push(StreamEntry {
            id: RawBytes::new(id),
            fields: pairs,
        });
    }
    Ok(Some(RawValue::Stream(out)))
}

/// Every reply in `items` must be a `Bulk` (byte-identity preserved).
fn bulk_all(items: Vec<Reply>, type_name: &str) -> Result<Vec<RawBytes>, ReplyToRawError> {
    items
        .into_iter()
        .map(|item| match item {
            Reply::Bulk(bytes) => Ok(RawBytes::new(bytes)),
            _ => Err(ReplyToRawError::UnexpectedShape(type_name.to_string())),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Reply {
        Reply::Bulk(s.as_bytes().to_vec())
    }

    fn raw_bytes(s: &str) -> RawBytes {
        RawBytes::from(s)
    }

    #[test]
    fn string_maps_with_byte_identity() {
        let bytes = vec![0xC3, 0x28, 0x00, 0xEF];
        let v = reply_to_raw("string", Reply::Bulk(bytes.clone()))
            .unwrap()
            .expect("value");
        match v {
            RawValue::String(b) => assert_eq!(b.as_bytes(), bytes.as_slice()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn nil_maps_to_missing() {
        assert!(reply_to_raw("string", Reply::Nil).unwrap().is_none());
        // Containers answer empty-array for missing keys, but Nil is
        // still honest-mapped if a server sends it.
        assert!(reply_to_raw("hash", Reply::Nil).unwrap().is_none());
    }

    #[test]
    fn hash_pairs_preserve_order_and_bytes() {
        let reply = Reply::Array(vec![bulk("f1"), bulk("v1"), bulk("f2"), bulk("v2")]);
        let v = reply_to_raw("hash", reply).unwrap().expect("value");
        match v {
            RawValue::Hash(pairs) => {
                assert_eq!(pairs.len(), 2);
                assert_eq!(pairs[0], (raw_bytes("f1"), raw_bytes("v1")));
                assert_eq!(pairs[1], (raw_bytes("f2"), raw_bytes("v2")));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn hash_odd_array_is_an_error() {
        let reply = Reply::Array(vec![bulk("f1")]);
        assert_eq!(
            reply_to_raw("hash", reply),
            Err(ReplyToRawError::UnexpectedShape("hash".into()))
        );
    }

    #[test]
    fn empty_containers_map_to_empty() {
        for (t, reply) in [
            ("hash", Reply::Array(vec![])),
            ("list", Reply::Array(vec![])),
            ("set", Reply::Array(vec![])),
            ("zset", Reply::Array(vec![])),
            ("stream", Reply::Array(vec![])),
        ] {
            let v = reply_to_raw(t, reply).unwrap().expect("value");
            assert_eq!(v.kind().display_name(), t);
        }
    }

    #[test]
    fn list_and_set_map_items() {
        let list = reply_to_raw("list", Reply::Array(vec![bulk("a"), bulk("b")]))
            .unwrap()
            .expect("value");
        match list {
            RawValue::List(items) => {
                assert_eq!(items, vec![raw_bytes("a"), raw_bytes("b")]);
            }
            other => panic!("unexpected {other:?}"),
        }
        let set = reply_to_raw("set", Reply::Array(vec![bulk("x")]))
            .unwrap()
            .expect("value");
        match set {
            RawValue::Set(members) => assert_eq!(members, vec![raw_bytes("x")]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn non_utf8_members_survive_untouched() {
        let binary = vec![0xFF, 0x00, 0xFE];
        let set = reply_to_raw("set", Reply::Array(vec![Reply::Bulk(binary.clone())]))
            .unwrap()
            .expect("value");
        match set {
            RawValue::Set(members) => assert_eq!(members[0].as_bytes(), binary.as_slice()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn zset_parses_scores() {
        let reply = Reply::Array(vec![
            bulk("m1"),
            bulk("1.5"),
            bulk("m2"),
            bulk("-2"),
            bulk("m3"),
            bulk("inf"),
        ]);
        let v = reply_to_raw("zset", reply).unwrap().expect("value");
        match v {
            RawValue::ZSet(entries) => {
                assert_eq!(entries.len(), 3);
                assert_eq!(entries[0].member, raw_bytes("m1"));
                assert_eq!(entries[0].score, 1.5);
                assert_eq!(entries[1].score, -2.0);
                assert!(entries[2].score.is_infinite());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn zset_bad_score_is_an_error() {
        let reply = Reply::Array(vec![bulk("m"), bulk("not-a-number")]);
        assert_eq!(reply_to_raw("zset", reply), Err(ReplyToRawError::BadScore));
    }

    #[test]
    fn stream_entries_map_id_and_pairs() {
        let reply = Reply::Array(vec![
            Reply::Array(vec![
                bulk("1690000000000-1"),
                Reply::Array(vec![bulk("temp"), bulk("21"), bulk("loc"), bulk("eu")]),
            ]),
            Reply::Array(vec![bulk("1690000000001-0"), Reply::Array(vec![])]),
        ]);
        let v = reply_to_raw("stream", reply).unwrap().expect("value");
        match v {
            RawValue::Stream(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].id, raw_bytes("1690000000000-1"));
                assert_eq!(entries[0].fields.len(), 2);
                assert_eq!(entries[0].fields[1], (raw_bytes("loc"), raw_bytes("eu")));
                assert!(entries[1].fields.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn stream_entry_with_odd_fields_is_an_error() {
        let reply = Reply::Array(vec![Reply::Array(vec![
            bulk("id"),
            Reply::Array(vec![bulk("only-field")]),
        ])]);
        assert_eq!(
            reply_to_raw("stream", reply),
            Err(ReplyToRawError::UnexpectedShape("stream".into()))
        );
    }

    #[test]
    fn error_replies_surface_honestly() {
        // redis-io flattens server errors into Reply::Other with a
        // non-sensitive label (e.g. WRONGTYPE after a TYPE race).
        assert_eq!(
            reply_to_raw("string", Reply::Other("server error: WRONGTYPE".into())),
            Err(ReplyToRawError::Server("server error: WRONGTYPE".into()))
        );
        // Wrong shape for a string (e.g. an Int) is not guessed at.
        assert_eq!(
            reply_to_raw("string", Reply::Int(3)),
            Err(ReplyToRawError::UnexpectedShape("string".into()))
        );
    }

    #[test]
    fn unmodeled_types_refuse_conversion() {
        assert_eq!(
            reply_to_raw("graph", Reply::Bulk(b"x".to_vec())),
            Err(ReplyToRawError::UnmodeledType("graph".into()))
        );
    }

    #[test]
    fn full_read_commands_match_the_type() {
        let key = b"k".to_vec();
        let get = full_read_command("string", &key).unwrap();
        assert_eq!(get.name, "GET");
        assert_eq!(get.args, vec![key.clone()]);

        let zrange = full_read_command("zset", &key).unwrap();
        assert_eq!(zrange.name, "ZRANGE");
        assert_eq!(
            zrange.args,
            vec![
                key.clone(),
                b"0".to_vec(),
                b"-1".to_vec(),
                b"WITHSCORES".to_vec()
            ]
        );

        let xrange = full_read_command("stream", &key).unwrap();
        assert_eq!(xrange.args, vec![key, b"-".to_vec(), b"+".to_vec()]);

        // Module types and `none` have no read command.
        assert!(full_read_command("ReJSON-RL", b"k").is_none());
        assert!(full_read_command("none", b"k").is_none());
    }
}
