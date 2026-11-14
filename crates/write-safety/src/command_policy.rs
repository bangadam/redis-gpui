//! The const command policy table — the shared brain for the console and the
//! edit/save paths (PLAN.md section 4).
//!
//! Every recognized Redis command maps to a [`CommandKind`]. Matching is
//! case-insensitive over the full command name, including a subcommand word
//! where one exists (e.g. `CONFIG GET`, `SCRIPT LOAD`, `MEMORY USAGE`).
//! Commands not in any table are [`CommandKind::Unknown`], which the
//! read-only gate blocks with a clear message.
//!
//! Open decisions from PLAN.md section 9 that are baked in here:
//! - **D1**: `EVAL_RO` / `EVALSHA_RO` are [`CommandKind::ScriptServerSide`]
//!   (blocked in read-only — consistency beats cleverness).
//! - **D2**: `MULTI`/`EXEC` classify as [`CommandKind::Write`]. The console
//!   sends the individual commands later; those are classified on their own.
//!
//! Classification honesty notes:
//! - `GETDEL`, `GETEX`, `TOUCH`, `XREADGROUP`, `XCLAIM`, `ZPOPMIN`/`ZPOPMAX`,
//!   and the `*STORE` set operations all mutate data (or key metadata), so
//!   they are [`CommandKind::Write`], not read-only.
//! - `DEL`/`UNLINK` are [`CommandKind::Write`] for a single key; the
//!   classifier escalates multi-key deletes to destructive per decision D3.

/// Coarse safety classification of a single Redis command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandKind {
    /// Safe to run even when the connection is read-only.
    ReadOnly,
    /// Mutates data; allowed in write mode without extra confirmation.
    Write,
    /// Mutates data at scale; requires a typed confirmation naming what is
    /// lost (FLUSHALL, FLUSHDB, multi-key DEL, ...).
    Destructive,
    /// Server-side scripts (`EVAL` family, `SCRIPT LOAD`, `FCALL`). Blocked
    /// in read-only mode even when read-only in principle (PRD story 37).
    ScriptServerSide,
    /// Not in the policy table. Blocked in read-only with a clear message;
    /// runs in write mode (the user's own server — guardrail, not a wall).
    Unknown,
}

/// Read-only commands — the read-mode allowlist.
pub const READ_ONLY: &[&str] = &[
    // String
    "GET",
    "GETRANGE",
    "MGET",
    "STRLEN",
    "SUBSTR",
    // Keyspace
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "SCAN",
    "KEYS",
    "RANDOMKEY",
    "OBJECT",
    "MEMORY USAGE",
    // Hash
    "HGET",
    "HGETALL",
    "HMGET",
    "HKEYS",
    "HLEN",
    "HVALS",
    "HSTRLEN",
    "HSCAN",
    "HRANDFIELD",
    "HEXISTS",
    // List
    "LRANGE",
    "LLEN",
    "LINDEX",
    "LPOS",
    "LSCAN",
    // Set
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SCARD",
    "SSCAN",
    "SRANDMEMBER",
    "SINTER",
    "SUNION",
    "SDIFF",
    "SINTERCARD",
    // Sorted set
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZREVRANGEBYLEX",
    "ZSCORE",
    "ZMSCORE",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZRANK",
    "ZREVRANK",
    "ZSCAN",
    "ZRANDMEMBER",
    // Stream (XPENDING only lists; the group-mutating ones are Write)
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "XREAD",
    "XINFO",
    "XPENDING",
    // Server
    "INFO",
    "DBSIZE",
    "PING",
    "ECHO",
    "TIME",
    "CONFIG GET",
    "CLIENT LIST",
    "CLIENT INFO",
    "CLIENT GETNAME",
    "COMMAND COUNT",
    "COMMAND INFO",
    "COMMAND DOCS",
    // HyperLogLog (read)
    "PFCOUNT",
];

/// Write commands — mutate data, no extra confirmation in write mode.
pub const WRITE: &[&str] = &[
    // String
    "SET",
    "SETNX",
    "SETEX",
    "PSETEX",
    "MSET",
    "MSETNX",
    "APPEND",
    "SETRANGE",
    "GETSET",
    "GETDEL",
    "GETEX",
    "INCR",
    "DECR",
    "INCRBY",
    "DECRBY",
    "INCRBYFLOAT",
    "SETBIT",
    "GETBIT",
    // Keyspace
    "DEL",
    "UNLINK",
    "RENAME",
    "RENAMENX",
    "EXPIRE",
    "PEXPIRE",
    "EXPIREAT",
    "PEXPIREAT",
    "PERSIST",
    "COPY",
    "MOVE",
    "RESTORE",
    "TOUCH",
    // Hash
    "HSET",
    "HMSET",
    "HSETNX",
    "HDEL",
    "HINCRBY",
    "HINCRBYFLOAT",
    // List
    "LPUSH",
    "RPUSH",
    "LPUSHX",
    "RPUSHX",
    "LPOP",
    "RPOP",
    "LREM",
    "LSET",
    "LTRIM",
    "LINSERT",
    "LMOVE",
    "LMPOP",
    "BLPOP",
    "BRPOP",
    "BLMOVE",
    "BLMPOP",
    "RPOPLPUSH",
    // Set (the STORE variants write destination keys)
    "SADD",
    "SREM",
    "SPOP",
    "SMOVE",
    "SINTERSTORE",
    "SUNIONSTORE",
    "SDIFFSTORE",
    // Sorted set
    "ZADD",
    "ZINCRBY",
    "ZREM",
    "ZREMRANGEBYSCORE",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYLEX",
    "ZPOPMIN",
    "ZPOPMAX",
    "BZPOPMIN",
    "BZPOPMAX",
    "ZMPOP",
    "BZMPOP",
    // Stream (append / delete / trim / consumer-group state)
    "XADD",
    "XDEL",
    "XTRIM",
    "XGROUP",
    "XSETID",
    "XACK",
    "XREADGROUP",
    "XCLAIM",
    "XAUTOCLAIM",
    // Transactions (D2: MULTI is a Write; contents are classified separately)
    "MULTI",
    "EXEC",
    "DISCARD",
    // DB selection
    "SELECT",
    // RedisJSON
    "JSON.SET",
    "JSON.DEL",
    "JSON.ARRAPPEND",
    "JSON.ARRINSERT",
    "JSON.ARRPOP",
    "JSON.ARRTRIM",
    "JSON.ARRINDEX",
    "JSON.ARRLEN",
    // HyperLogLog
    "PFADD",
    "PFMERGE",
];

/// Destructive commands — always require a typed confirmation in write mode.
pub const DESTRUCTIVE: &[&str] = &["FLUSHALL", "FLUSHDB", "SWAPDB", "SCRIPT FLUSH"];

/// Server-side script commands — blocked in read-only mode (PRD story 37).
///
/// Includes the `_RO` variants per D1 (PLAN.md section 9): blocked in
/// read-only for consistency with the PRD's rule on server-side scripts.
pub const SCRIPT_SERVER_SIDE: &[&str] = &[
    "EVAL",
    "EVAL_RO",
    "EVALSHA",
    "EVALSHA_RO",
    "SCRIPT LOAD",
    "FUNCTION LOAD",
    "FUNCTION LIST",
    "FCALL",
    "FCALL_RO",
];

/// Look up the [`CommandKind`] for a command name (case-insensitive,
/// subcommand word included where one exists, e.g. `CONFIG GET`).
///
/// Lookup order matters for subcommands: destructive and script tables are
/// checked before the benign ones, so `SCRIPT FLUSH` resolves to
/// [`CommandKind::Destructive`] rather than the script family, and
/// `SCRIPT LOAD` resolves to [`CommandKind::ScriptServerSide`]. A bare word
/// that only exists with a subcommand (`CONFIG`, `MEMORY`, `SCRIPT`) is
/// [`CommandKind::Unknown`].
pub fn lookup(name: &str) -> CommandKind {
    if contains(name, DESTRUCTIVE) {
        CommandKind::Destructive
    } else if contains(name, SCRIPT_SERVER_SIDE) {
        CommandKind::ScriptServerSide
    } else if contains(name, READ_ONLY) {
        CommandKind::ReadOnly
    } else if contains(name, WRITE) {
        CommandKind::Write
    } else {
        CommandKind::Unknown
    }
}

/// Case-insensitive membership test against a static table.
fn contains(name: &str, table: &[&str]) -> bool {
    table
        .iter()
        .any(|candidate| eq_ignore_ascii_case(name, candidate))
}

/// ASCII-case-insensitive equality (Redis command names are ASCII).
fn eq_ignore_ascii_case(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_lookups() {
        for name in [
            "GET",
            "MGET",
            "EXISTS",
            "TYPE",
            "TTL",
            "PTTL",
            "SCAN",
            "KEYS",
            "HGETALL",
            "HGET",
            "LRANGE",
            "LLEN",
            "SMEMBERS",
            "SISMEMBER",
            "ZRANGE",
            "ZSCORE",
            "ZCARD",
            "XRANGE",
            "XLEN",
            "XINFO",
            "INFO",
            "DBSIZE",
            "PING",
            "ECHO",
            "CONFIG GET",
            "CLIENT LIST",
            "TIME",
            "STRLEN",
            "OBJECT",
            "MEMORY USAGE",
        ] {
            assert_eq!(lookup(name), CommandKind::ReadOnly, "{name}");
        }
    }

    #[test]
    fn write_lookups() {
        for name in [
            "SET",
            "SETEX",
            "MSET",
            "DEL",
            "UNLINK",
            "HSET",
            "HDEL",
            "HMSET",
            "LPUSH",
            "RPUSH",
            "LPOP",
            "RPOP",
            "LREM",
            "LSET",
            "LTRIM",
            "SADD",
            "SREM",
            "SPOP",
            "ZADD",
            "ZINCRBY",
            "ZREM",
            "EXPIRE",
            "PEXPIRE",
            "PERSIST",
            "RENAME",
            "RENAMENX",
            "APPEND",
            "SETRANGE",
            "INCR",
            "DECR",
            "INCRBY",
            "XADD",
            "XDEL",
            "XTRIM",
            "SELECT",
            "JSON.SET",
            "JSON.DEL",
            "JSON.ARRAPPEND",
            "MULTI",
            "EXEC",
            "GETEX",
        ] {
            assert_eq!(lookup(name), CommandKind::Write, "{name}");
        }
    }

    #[test]
    fn destructive_lookups() {
        for name in ["FLUSHALL", "FLUSHDB", "SWAPDB", "SCRIPT FLUSH"] {
            assert_eq!(lookup(name), CommandKind::Destructive, "{name}");
        }
    }

    #[test]
    fn script_server_side_lookups() {
        for name in [
            "EVAL",
            "EVALSHA",
            "SCRIPT LOAD",
            "EVAL_RO",
            "EVALSHA_RO",
            "FCALL",
            "FCALL_RO",
            "FUNCTION LOAD",
        ] {
            assert_eq!(lookup(name), CommandKind::ScriptServerSide, "{name}");
        }
    }

    #[test]
    fn unknown_lookups() {
        for name in ["NOSUCHCMD", "CONFIG", "DEBUG", "SHUTDOWN", "ACL", ""] {
            assert_eq!(lookup(name), CommandKind::Unknown, "{name}");
        }
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(lookup("flushdb"), CommandKind::Destructive);
        assert_eq!(lookup("FlushAll"), CommandKind::Destructive);
        assert_eq!(lookup("flushDB"), CommandKind::Destructive);
        assert_eq!(lookup("get"), CommandKind::ReadOnly);
        assert_eq!(lookup("GeT"), CommandKind::ReadOnly);
        assert_eq!(lookup("set"), CommandKind::Write);
        assert_eq!(lookup("eval"), CommandKind::ScriptServerSide);
        assert_eq!(lookup("eval_ro"), CommandKind::ScriptServerSide);
        assert_eq!(lookup("evalsha_ro"), CommandKind::ScriptServerSide);
        assert_eq!(lookup("script flush"), CommandKind::Destructive);
        assert_eq!(lookup("config get"), CommandKind::ReadOnly);
        assert_eq!(lookup("json.set"), CommandKind::Write);
    }

    #[test]
    fn subcommand_specificity() {
        // SCRIPT FLUSH is Destructive; SCRIPT LOAD is ScriptServerSide; the
        // bare word SCRIPT is not in any table → Unknown.
        assert_eq!(lookup("SCRIPT FLUSH"), CommandKind::Destructive);
        assert_eq!(lookup("SCRIPT LOAD"), CommandKind::ScriptServerSide);
        assert_eq!(lookup("SCRIPT"), CommandKind::Unknown);
        assert_eq!(lookup("MEMORY USAGE"), CommandKind::ReadOnly);
        assert_eq!(lookup("MEMORY"), CommandKind::Unknown);
        assert_eq!(lookup("CONFIG GET"), CommandKind::ReadOnly);
        assert_eq!(lookup("CONFIG SET"), CommandKind::Unknown);
    }

    #[test]
    fn mutating_commands_are_not_read_only() {
        // Honesty per PRD story 37: the read-only allowlist contains only
        // commands that cannot change data or key metadata.
        for name in [
            "GETDEL",
            "GETEX",
            "TOUCH",
            "XREADGROUP",
            "XCLAIM",
            "XAUTOCLAIM",
            "ZPOPMIN",
            "ZPOPMAX",
            "SINTERSTORE",
            "SUNIONSTORE",
            "SDIFFSTORE",
            "ZINCRBY",
            "PFADD",
        ] {
            assert_eq!(
                lookup(name),
                CommandKind::Write,
                "{name} mutates data and must not be read-only"
            );
        }
    }

    #[test]
    fn tables_do_not_overlap() {
        // A command in two tables would make the lookup order load-bearing in
        // a way nobody can reason about. Keep the tables disjoint.
        for (name, _) in all_entries() {
            let hits = [READ_ONLY, WRITE, DESTRUCTIVE, SCRIPT_SERVER_SIDE]
                .iter()
                .filter(|table| contains(name, table))
                .count();
            assert_eq!(hits, 1, "{name} appears in more than one table");
        }
    }

    /// Flattens every table entry with the table it came from (case preserved).
    fn all_entries() -> Vec<(&'static str, &'static [&'static str])> {
        let mut all = Vec::new();
        for table in [READ_ONLY, WRITE, DESTRUCTIVE, SCRIPT_SERVER_SIDE] {
            for name in table {
                all.push((*name, table));
            }
        }
        all
    }

    #[test]
    fn no_duplicate_entries_within_a_table() {
        for table in [READ_ONLY, WRITE, DESTRUCTIVE, SCRIPT_SERVER_SIDE] {
            for (i, name) in table.iter().enumerate() {
                for other in &table[..i] {
                    assert_ne!(name, other, "{name} is listed twice in the same table");
                }
            }
        }
    }
}
