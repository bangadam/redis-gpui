//! Tokenization: console input line → [`ParsedCommand`].
//!
//! Supported syntax (deliberately redis-cli-simple, documented exactly):
//!
//! - Tokens are separated by spaces and tabs (a console line never contains
//!   `\n`/`\r`).
//! - **Double quotes** (`"a b"`) preserve inner whitespace. Inside double
//!   quotes a backslash escapes the **next character verbatim**: `\"` → `"`,
//!   `\\` → `\`, `\x` → `x` for any other `x` (control escapes like `\n`
//!   are intentionally NOT supported — you cannot type them in a one-line
//!   console anyway).
//! - **Single quotes** (`'a b'`) are verbatim: every character is literal,
//!   including backslash, with the single exception `\'` which embeds a
//!   single quote (redis-cli behavior).
//! - Outside quotes a backslash escapes the next character verbatim, so
//!   `hello\ world` is one token and `\"` starts no quote.
//! - Adjacent quoted/unquoted runs join into one token (`"a"'b'c` → `abc`),
//!   like redis-cli.
//!
//! The command name is the first token, **UPPERCASED** (Redis command names
//! are ASCII; a non-ASCII first token is passed through and will classify as
//! unknown). Remaining tokens become the raw-byte arguments, so they match
//! [`ParsedCommand`]'s `Vec<Vec<u8>>` shape. Because the input is a `&str`,
//! every token is valid UTF-8; byte-level construction (e.g. `\xHH` hex
//! pairs) is intentionally out of scope for the typed console.
//!
//! **Subcommand merge**: when the bare name is `CommandKind::Unknown` to
//! [`write_safety`] and the first arg is a single word, the tokenizer tries
//! `name + " " + arg0` ("SCRIPT FLUSH", "CONFIG GET", "MEMORY USAGE") and,
//! if that classifies, emits it as the full name and drops the subcommand
//! word from the args. `write_safety::classify` would retry the merge
//! anyway; doing it here means the [`ParsedCommand`] the app later issues to
//! the server carries the complete, canonical name.
//!
//! Malformed input (unterminated quote, trailing backslash) is an honest
//! [`TokenizeError`] — no partial guesses.

use write_safety::command_policy::lookup;
use write_safety::{CommandKind, ParsedCommand};

/// Why a line could not be tokenized. Reported to the user verbatim; no
/// partial command is ever produced from a malformed line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenizeError {
    /// A quote opened but never closed.
    UnterminatedQuote,
    /// A backslash with nothing left to escape (trailing `\`).
    DanglingEscape,
}

impl std::fmt::Display for TokenizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnterminatedQuote => {
                write!(
                    f,
                    "unterminated quote — every opening quote needs a closing one"
                )
            }
            Self::DanglingEscape => {
                write!(
                    f,
                    "dangling escape — a trailing backslash must escape something"
                )
            }
        }
    }
}

impl std::error::Error for TokenizeError {}

/// Tokenizes one console input line into a [`ParsedCommand`].
///
/// - Empty or whitespace-only input → `Ok(None)` (the UI treats this as a
///   no-op, not an error).
/// - Malformed quoting/escaping → `Err(TokenizeError)`.
///
/// Pure: no I/O, no state.
pub fn tokenize(input: &str) -> Result<Option<ParsedCommand>, TokenizeError> {
    // Byte iteration is safe: the separators and quote/escape characters are
    // all ASCII, and ASCII bytes never appear inside a UTF-8 multi-byte
    // sequence, so multi-byte characters pass through untouched.
    let bytes = input.as_bytes();
    let mut tokens: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut in_token = false;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
                i += 1;
            }
            b'\\' => {
                // Outside quotes (and in double quotes): escape the next
                // character verbatim.
                let Some(&next) = bytes.get(i + 1) else {
                    return Err(TokenizeError::DanglingEscape);
                };
                current.push(next);
                in_token = true;
                i += 2;
            }
            b'\'' => {
                in_token = true;
                i += 1;
                let mut closed = false;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'\'') {
                        current.push(b'\'');
                        i += 2;
                    } else if bytes[i] == b'\'' {
                        i += 1;
                        closed = true;
                        break;
                    } else {
                        current.push(bytes[i]);
                        i += 1;
                    }
                }
                if !closed {
                    return Err(TokenizeError::UnterminatedQuote);
                }
            }
            b'"' => {
                in_token = true;
                i += 1;
                let mut closed = false;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        let Some(&next) = bytes.get(i + 1) else {
                            return Err(TokenizeError::DanglingEscape);
                        };
                        current.push(next);
                        i += 2;
                    } else if bytes[i] == b'"' {
                        i += 1;
                        closed = true;
                        break;
                    } else {
                        current.push(bytes[i]);
                        i += 1;
                    }
                }
                if !closed {
                    return Err(TokenizeError::UnterminatedQuote);
                }
            }
            _ => {
                current.push(bytes[i]);
                in_token = true;
                i += 1;
            }
        }
    }
    if in_token {
        tokens.push(current);
    }

    let Some(first) = tokens.first() else {
        return Ok(None);
    };

    let mut name = String::from_utf8_lossy(first).to_ascii_uppercase();
    let mut args: Vec<Vec<u8>> = tokens.into_iter().skip(1).collect();

    // Subcommand merge: only when the bare name is Unknown and `name + " " +
    // arg0` classifies (see module docs).
    if lookup(&name) == CommandKind::Unknown {
        if let Some(arg0) = args.first() {
            if let Ok(sub) = std::str::from_utf8(arg0) {
                let sub = sub.trim().to_ascii_uppercase();
                if !sub.is_empty() && !sub.contains(' ') && !sub.contains('\t') {
                    let joined = format!("{name} {sub}");
                    if lookup(&joined) != CommandKind::Unknown {
                        name = joined;
                        args.remove(0);
                    }
                }
            }
        }
    }

    Ok(Some(ParsedCommand { name, args }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(line: &str) -> Vec<Vec<u8>> {
        tokenize(line).unwrap().unwrap().args
    }

    #[test]
    fn empty_and_whitespace_only_is_none() {
        assert_eq!(tokenize("").unwrap(), None);
        assert_eq!(tokenize("   ").unwrap(), None);
        assert_eq!(tokenize("\t \t").unwrap(), None);
    }

    #[test]
    fn simple_tokens_split_on_whitespace() {
        let cmd = tokenize("GET foo").unwrap().unwrap();
        assert_eq!(cmd.name, "GET");
        assert_eq!(cmd.args, vec![b"foo".to_vec()]);
    }

    #[test]
    fn command_name_is_uppercased() {
        assert_eq!(tokenize("get key").unwrap().unwrap().name, "GET");
        assert_eq!(tokenize("GeT key").unwrap().unwrap().name, "GET");
        assert_eq!(
            tokenize("config get maxmemory").unwrap().unwrap().name,
            "CONFIG GET"
        );
    }

    #[test]
    fn double_quotes_preserve_inner_spaces() {
        assert_eq!(
            args_of(r#"SET key "hello world""#),
            vec![b"key".to_vec(), b"hello world".to_vec()]
        );
    }

    #[test]
    fn single_quotes_preserve_inner_spaces() {
        assert_eq!(
            args_of("SET key 'hello world'"),
            vec![b"key".to_vec(), b"hello world".to_vec()]
        );
    }

    #[test]
    fn double_quote_escapes_escaped_quote() {
        assert_eq!(
            args_of(r#"SET key "say \"hi\"""#),
            vec![b"key".to_vec(), br#"say "hi""#.to_vec()]
        );
    }

    #[test]
    fn backslash_escape_yields_verbatim_char() {
        // `a\b` → `ab`? No: `\b` escapes to the literal character `b`.
        // The point of the test: one token, backslash removed.
        assert_eq!(
            args_of(r"SET key a\\b"),
            vec![b"key".to_vec(), b"a\\b".to_vec()]
        );
        assert_eq!(
            args_of(r"SET key a\bb"),
            vec![b"key".to_vec(), b"abb".to_vec()]
        );
    }

    #[test]
    fn escaped_space_outside_quotes_joins_one_token() {
        assert_eq!(
            args_of(r"SET key hello\ world"),
            vec![b"key".to_vec(), b"hello world".to_vec()]
        );
    }

    #[test]
    fn single_quote_embedded_quote_via_backslash() {
        assert_eq!(
            args_of(r"SET key 'it\'s'"),
            vec![b"key".to_vec(), b"it's".to_vec()]
        );
    }

    #[test]
    fn single_quotes_keep_backslash_literal() {
        // Inside single quotes a backslash is literal (except `\'`).
        assert_eq!(
            args_of(r"SET key 'a\b'"),
            vec![b"key".to_vec(), b"a\\b".to_vec()]
        );
    }

    #[test]
    fn double_quotes_escape_backslash() {
        assert_eq!(
            args_of(r#"SET key "a\\b""#),
            vec![b"key".to_vec(), b"a\\b".to_vec()]
        );
    }

    #[test]
    fn adjacent_quoted_runs_join_into_one_token() {
        assert_eq!(args_of(r#"SET "a"'b'c"#), vec![b"abc".to_vec()]);
    }

    #[test]
    fn empty_quoted_arg_is_an_empty_token() {
        assert_eq!(args_of(r#"SET key """#), vec![b"key".to_vec(), Vec::new()]);
        assert_eq!(args_of("SET key ''"), vec![b"key".to_vec(), Vec::new()]);
    }

    #[test]
    fn unterminated_double_quote_is_an_error() {
        assert_eq!(
            tokenize(r#"SET key "oops"#),
            Err(TokenizeError::UnterminatedQuote)
        );
    }

    #[test]
    fn unterminated_single_quote_is_an_error() {
        assert_eq!(
            tokenize("SET key 'oops"),
            Err(TokenizeError::UnterminatedQuote)
        );
    }

    #[test]
    fn trailing_backslash_is_an_error() {
        assert_eq!(tokenize(r"SET key a\"), Err(TokenizeError::DanglingEscape));
        assert_eq!(
            tokenize(r#"SET key "a\"#),
            Err(TokenizeError::DanglingEscape)
        );
    }

    #[test]
    fn script_flush_merges_subcommand() {
        let cmd = tokenize("SCRIPT FLUSH").unwrap().unwrap();
        assert_eq!(cmd.name, "SCRIPT FLUSH");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn config_get_merges_and_keeps_remaining_args() {
        let cmd = tokenize("CONFIG GET maxmemory").unwrap().unwrap();
        assert_eq!(cmd.name, "CONFIG GET");
        assert_eq!(cmd.args, vec![b"maxmemory".to_vec()]);
    }

    #[test]
    fn memory_usage_merges_subcommand() {
        let cmd = tokenize("MEMORY USAGE mykey").unwrap().unwrap();
        assert_eq!(cmd.name, "MEMORY USAGE");
        assert_eq!(cmd.args, vec![b"mykey".to_vec()]);
    }

    #[test]
    fn no_merge_when_bare_name_is_recognized() {
        // XINFO (bare) is a known read-only command, so no merge happens;
        // STREAM stays an ordinary arg.
        let cmd = tokenize("XINFO STREAM mystream").unwrap().unwrap();
        assert_eq!(cmd.name, "XINFO");
        assert_eq!(cmd.args, vec![b"STREAM".to_vec(), b"mystream".to_vec()]);
        let cmd = tokenize("GET foo").unwrap().unwrap();
        assert_eq!(cmd.name, "GET");
        assert_eq!(cmd.args, vec![b"foo".to_vec()]);
    }

    #[test]
    fn no_merge_when_joined_name_is_unknown() {
        let cmd = tokenize("WIBBLE x").unwrap().unwrap();
        assert_eq!(cmd.name, "WIBBLE");
        assert_eq!(cmd.args, vec![b"x".to_vec()]);
    }

    #[test]
    fn merged_subcommand_word_is_uppercased() {
        let cmd = tokenize("script flush").unwrap().unwrap();
        assert_eq!(cmd.name, "SCRIPT FLUSH");
    }

    #[test]
    fn quoted_subcommand_word_still_merges() {
        let cmd = tokenize(r#"SCRIPT "FLUSH""#).unwrap().unwrap();
        assert_eq!(cmd.name, "SCRIPT FLUSH");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn error_messages_are_renderable() {
        let e = TokenizeError::UnterminatedQuote;
        assert!(e.to_string().contains("quote"));
        let e = TokenizeError::DanglingEscape;
        assert!(e.to_string().contains("backslash"));
    }
}
