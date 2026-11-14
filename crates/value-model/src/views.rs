//! Read-only view descriptors: renderers *over* raw bytes, never
//! conversions of them.
//!
//! A view is a way to display a [`RawValue`](crate::raw::RawValue); the bytes
//! underneath are the only source of truth. Everything a view produces
//! ([`RenderedView`]) is owned display data, derived from the bytes on
//! demand — it is never stored in place of the bytes and never fed back into
//! the save path.
//!
//! The JSON view is offered only when the value parses as JSON **and**
//! parse + compact re-serialize is byte-identical to the original bytes —
//! i.e. re-serializing the parsed value does not lose information (key
//! order does round-trip via the pinned `preserve_order` feature; number
//! formatting, whitespace, and duplicate keys must also round-trip). If it
//! would lose anything, the JSON view declines and the value falls back to
//! the text view. That gate is the "no silent conversion" guarantee
//! (PLAN.md section 4).

use crate::raw::RawValue;

#[cfg(test)]
use crate::raw::RawBytes;
use serde_json::Value as JsonValue;

/// The kinds of view the UI can offer for a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    /// Plain text. Only valid when the relevant bytes are valid UTF-8.
    Text,
    /// Pretty-printed JSON. Only offered when the bytes parse as JSON and
    /// the parse is information-preserving (see module docs).
    Json,
    /// Hex dump. Always available; read-only by design.
    Hex,
}

/// One rendered line of a hex dump: `offset  b1 b2 .. b8  |ascii|`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HexLine {
    /// Byte offset of the first byte on this line.
    pub offset: usize,
    /// The raw bytes shown on this line (always `WIDTH` or fewer).
    pub bytes: Vec<u8>,
}

/// Owned display data for one view, derived from (and only from) the
/// original bytes. This is what the UI renders. It is **never** a source of
/// truth: saving always re-encodes from the editor's validated edits, never
/// from this.
#[derive(Debug, Clone, PartialEq)]
pub enum RenderedView {
    /// The text view: one string per value part. For scalars a single
    /// string; for containers, one entry per element/field so the UI can
    /// lay them out (key column, value column).
    Text(Vec<TextPart>),
    /// The JSON view: the pretty-printed document.
    Json(String),
    /// The hex view: one line per 16 bytes.
    Hex(Vec<HexLine>),
}

/// One text-view part: an address (row label, e.g. a hash field) and the
/// value text shown next to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPart {
    /// Row label: a hash field, list index, stream ID ... or `None` for the
    /// string value itself.
    pub label: Option<String>,
    /// The value text (validated UTF-8 when it came from the text view).
    pub text: String,
}

/// Which views can be offered for `value`, in preference order
/// (Text/Json first when valid, Hex always last).
///
/// `available_views` never renders anything; the UI picks a kind and calls
/// [`render_preview`].
pub fn available_views(value: &RawValue) -> Vec<ViewKind> {
    let mut views = Vec::new();
    if value.is_valid_utf8() {
        views.push(ViewKind::Text);
        if json_view_is_lossless(value) {
            views.push(ViewKind::Json);
        }
    }
    // Hex is always available, read-only by design.
    views.push(ViewKind::Hex);
    views
}

/// Renders `kind` over `value`. Panics only on programmer error (a Text or
/// Json view requested for bytes that decline it); the UI chooses from
/// [`available_views`], so a panic here means a UI bug, not bad data.
pub fn render_preview(kind: ViewKind, value: &RawValue) -> RenderedView {
    match kind {
        ViewKind::Text => RenderedView::Text(render_text(value)),
        ViewKind::Json => {
            let doc = json_document(value).expect("Json view requested but not available");
            RenderedView::Json(doc)
        }
        ViewKind::Hex => RenderedView::Hex(render_hex(value)),
    }
}

/// Renders the text view. The caller must have checked
/// [`RawValue::is_valid_utf8`] (via [`available_views`]); invalid leaves
/// are shown lossily here **for display only** — this renderer never feeds
/// the save path.
fn render_text(value: &RawValue) -> Vec<TextPart> {
    let mut parts = Vec::new();
    match value {
        RawValue::String(bytes) | RawValue::Json(bytes) => parts.push(TextPart {
            label: None,
            text: bytes.to_lossy_display().into_owned(),
        }),
        RawValue::Hash(pairs) => {
            for (field, val) in pairs {
                parts.push(TextPart {
                    label: Some(field.to_lossy_display().into_owned()),
                    text: val.to_lossy_display().into_owned(),
                });
            }
        }
        RawValue::List(items) => {
            for (i, item) in items.iter().enumerate() {
                parts.push(TextPart {
                    label: Some(i.to_string()),
                    text: item.to_lossy_display().into_owned(),
                });
            }
        }
        RawValue::Set(members) => {
            for member in members {
                parts.push(TextPart {
                    label: None,
                    text: member.to_lossy_display().into_owned(),
                });
            }
        }
        RawValue::ZSet(entries) => {
            for entry in entries {
                parts.push(TextPart {
                    label: Some(format!("{:?}", entry.score)),
                    text: entry.member.to_lossy_display().into_owned(),
                });
            }
        }
        RawValue::Stream(entries) => {
            for entry in entries {
                for (field, val) in &entry.fields {
                    parts.push(TextPart {
                        label: Some(format!(
                            "{} {}",
                            entry.id.to_lossy_display(),
                            field.to_lossy_display()
                        )),
                        text: val.to_lossy_display().into_owned(),
                    });
                }
            }
        }
    }
    parts
}

/// Hex-dumps every byte leaf of the value, concatenated in traversal order
/// (see [`RawValue::leaf_bytes`]). Offsets are over the whole value.
pub fn render_hex(value: &RawValue) -> Vec<HexLine> {
    let all: Vec<u8> = value
        .leaf_bytes()
        .iter()
        .flat_map(|b| b.as_bytes().iter().copied())
        .collect();
    const WIDTH: usize = 16;
    all.chunks(WIDTH)
        .enumerate()
        .map(|(i, chunk)| HexLine {
            offset: i * WIDTH,
            bytes: chunk.to_vec(),
        })
        .collect()
}

/// The JSON view's losslessness gate.
///
/// Contract: the JSON view is offered iff the (single) scalar value's
/// bytes parse as JSON and parse + compact re-serialize is **byte-identical
/// to the original**; with `preserve_order` (pinned in Cargo.toml because
/// gpui -> schemars enables it workspace-wide) this means the original
/// must already be in compact canonical form. Anything else — re-formatted
/// numbers (`1.50`), whitespace in the original, dropped duplicate keys,
/// float precision loss — does not round-trip and the view declines.
/// (Key order *does* round-trip under `preserve_order`: insertion order is
/// preserved, so `{"b":1,"a":2}` is lossless and correctly offered.)
fn json_view_is_lossless(value: &RawValue) -> bool {
    let bytes = match scalar_json_bytes(value) {
        Some(b) => b,
        None => return false,
    };
    // JSON must be UTF-8, so a strict parse here is safe.
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<JsonValue>(text) else {
        return false;
    };
    // Re-serialize the parsed value compactly; byte-identical to the
    // original means parsing lost nothing (this catches re-formatted
    // numbers, whitespace, dropped duplicate keys, and float precision
    // loss; key order is preserved by the pinned preserve_order feature).
    let Ok(reser) = serde_json::to_string(&parsed) else {
        return false;
    };
    reser == text
}

/// The pretty-printed document for the JSON view (caller checked
/// availability via [`available_views`]).
fn json_document(value: &RawValue) -> Option<String> {
    let bytes = scalar_json_bytes(value)?;
    let parsed: JsonValue = serde_json::from_slice(bytes).ok()?;
    let pretty = serde_json::to_string_pretty(&parsed).ok()?;
    Some(pretty)
}

fn scalar_json_bytes(value: &RawValue) -> Option<&[u8]> {
    match value {
        RawValue::String(bytes) | RawValue::Json(bytes) => Some(bytes.as_bytes()),
        // Containers are not offered a JSON view; only scalar bytes can be
        // JSON documents (RedisJSON values arrive as serialized JSON.GET
        // output).
        _ => None,
    }
}

/// Formats one hex line as `00000010  61 62 ..  ASCII`, matching what the
/// UI renders. Provided here so the data model owns the format once.
pub fn format_hex_line(line: &HexLine) -> String {
    const WIDTH: usize = 16;
    let mut out = format!("{:08x}  ", line.offset);
    for i in 0..WIDTH {
        match line.bytes.get(i) {
            Some(b) => out.push_str(&format!("{b:02x} ")),
            None => out.push_str("   "),
        }
    }
    out.push_str(" |");
    for &b in &line.bytes {
        if (0x20..0x7f).contains(&b) {
            out.push(b as char);
        } else {
            out.push('.');
        }
    }
    out.push('|');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_view_offered_for_valid_utf8() {
        let v = RawValue::String(RawBytes::from("hello"));
        let views = available_views(&v);
        assert!(views.contains(&ViewKind::Text));
        assert!(views.contains(&ViewKind::Hex));
    }

    #[test]
    fn text_view_declines_for_invalid_utf8_hex_remains() {
        let v = RawValue::String(RawBytes::from(vec![0xC3, 0x28]));
        let views = available_views(&v);
        assert!(!views.contains(&ViewKind::Text));
        assert!(views.contains(&ViewKind::Hex));
        assert_eq!(views, vec![ViewKind::Hex]);
    }

    #[test]
    fn json_view_offered_for_roundtripping_json() {
        let v = RawValue::String(RawBytes::from(r#"{"a":1,"b":[true,null]}"#));
        let views = available_views(&v);
        assert!(views.contains(&ViewKind::Json));
        match render_preview(ViewKind::Json, &v) {
            RenderedView::Json(text) => assert!(text.contains("\"a\": 1")),
            other => panic!("expected json render, got {other:?}"),
        }
    }

    #[test]
    fn json_view_declines_when_reserialization_differs() {
        // Whitespace: the original has trailing whitespace after the final
        // brace; the compact re-serialization drops it -> not byte-identical
        // -> the view must decline (no silent re-formatting presented as
        // "the value").
        let v = RawValue::String(RawBytes::from(r#"{"a":1,"b":2} "#));
        assert!(!available_views(&v).contains(&ViewKind::Json));
    }

    #[test]
    fn json_view_declines_on_number_formatting_drift() {
        // 1.50 re-serializes as 1.5 -> information (as presented) changes.
        let v = RawValue::String(RawBytes::from(r#"{"price":1.50}"#));
        assert!(!available_views(&v).contains(&ViewKind::Json));
    }

    #[test]
    fn json_view_declines_on_duplicate_keys() {
        // serde_json drops duplicate keys on parse; the round-trip gate
        // catches it.
        let v = RawValue::String(RawBytes::from(r#"{"a":1,"a":2}"#));
        assert!(!available_views(&v).contains(&ViewKind::Json));
    }

    #[test]
    fn json_view_declines_on_invalid_json() {
        let v = RawValue::String(RawBytes::from("not json"));
        assert!(!available_views(&v).contains(&ViewKind::Json));
    }

    #[test]
    fn json_module_type_uses_same_gate() {
        let ok = RawValue::Json(RawBytes::from(r#"{"k":"v"}"#));
        assert!(available_views(&ok).contains(&ViewKind::Json));
        let drifted = RawValue::Json(RawBytes::from(r#"{"b":1.50,"a":2}"#));
        assert!(!available_views(&drifted).contains(&ViewKind::Json));
    }

    #[test]
    fn invalid_utf8_bytes_survive_hex_render_byte_identical() {
        let original = vec![0xC3, 0x28, 0xF5, 0x00, 0xEF, 0xBB, 0xBF, 0xC3, 0x28];
        let v = RawValue::String(RawBytes::new(original.clone()));
        match render_preview(ViewKind::Hex, &v) {
            RenderedView::Hex(lines) => {
                let dumped: Vec<u8> = lines.iter().flat_map(|l| l.bytes.clone()).collect();
                assert_eq!(dumped, original);
            }
            other => panic!("expected hex render, got {other:?}"),
        }
    }

    #[test]
    fn hex_offsets_are_sequential() {
        let v = RawValue::String(RawBytes::new(vec![0u8; 40]));
        match render_preview(ViewKind::Hex, &v) {
            RenderedView::Hex(lines) => {
                assert_eq!(lines.len(), 3);
                assert_eq!(lines[0].offset, 0);
                assert_eq!(lines[1].offset, 16);
                assert_eq!(lines[2].offset, 32);
                assert_eq!(lines[2].bytes.len(), 8);
            }
            other => panic!("expected hex render, got {other:?}"),
        }
    }

    #[test]
    fn bom_crlf_emoji_render_as_text_and_stay_in_hex() {
        let bytes: Vec<u8> = [0xEF, 0xBB, 0xBF]
            .iter()
            .copied()
            .chain("line1\r\nline2 🦀".bytes())
            .collect();
        let v = RawValue::String(RawBytes::new(bytes.clone()));
        assert!(available_views(&v).contains(&ViewKind::Text));
        match render_preview(ViewKind::Hex, &v) {
            RenderedView::Hex(lines) => {
                let dumped: Vec<u8> = lines.iter().flat_map(|l| l.bytes.clone()).collect();
                assert_eq!(dumped, bytes);
            }
            other => panic!("expected hex render, got {other:?}"),
        }
    }

    #[test]
    fn container_views_partition_fields() {
        let v = RawValue::Hash(vec![
            (RawBytes::from("f1"), RawBytes::from("v1")),
            (RawBytes::from("f2"), RawBytes::from("v2")),
        ]);
        match render_preview(ViewKind::Text, &v) {
            RenderedView::Text(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0].label.as_deref(), Some("f1"));
                assert_eq!(parts[0].text, "v1");
            }
            other => panic!("expected text render, got {other:?}"),
        }
    }

    #[test]
    fn hex_line_format_is_stable() {
        let line = HexLine {
            offset: 16,
            bytes: vec![b'a', 0x00, b'z'],
        };
        let s = format_hex_line(&line);
        assert!(s.starts_with("00000010  61 00 7a "), "got: {s}");
        assert!(s.ends_with("|a.z|"), "got: {s}");
    }
}
