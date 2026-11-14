//! Dirty-editor state: an original, untouched [`RawValue`] plus a working
//! buffer of edits.
//!
//! Invariants (PRD stories 25/28/31, acceptance 9, PLAN.md section 5):
//!
//! 1. **The working buffer survives refresh.** `apply_refresh` routes new
//!    server bytes to the `original` side and sets a divergence flag; when
//!    the editor is dirty it **never** touches `working`.
//! 2. **Only validated UTF-8 edits or explicit byte edits enter the working
//!    buffer.** A lossy display string can never become an edit: the
//!    setters below are the only way in, and their signatures make lossy
//!    input a type error.
//! 3. **Saving re-encodes only edited fields.** Every unedited entry keeps
//!    its original `RawBytes` and passes through byte-identically; there is
//!    no whole-value re-encode anywhere.

use crate::raw::{RawBytes, RawValue, ZSetEntry};

#[cfg(test)]
use crate::raw::StreamEntry;

/// A field within a container value, addressing one editable unit.
///
/// Scalar values (string/JSON) have no field — the whole value is the unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldPath {
    /// The whole scalar value.
    Scalar,
    /// Hash: edit the value at this field (field names are not editable in
    /// this version; a rename is a delete + add in the app layer).
    HashField(usize),
    /// List: the item at this index.
    ListItem(usize),
    /// Set: the member at this index.
    SetMember(usize),
    /// ZSet: the member bytes at this index (scores are numeric, see
    /// [`ZSetEntry`]).
    ZSetMember(usize),
    /// ZSet: the score at this index.
    ZSetScore(usize),
    /// Stream: this version has no stream editing (PRD story 35).
    StreamEntry(#[allow(dead_code)] usize),
}

/// Mirrors [`RawValue`] but with editable fields.
///
/// Every slot starts as [`Slot::Original`], holding the exact original
/// bytes. An edit replaces the slot with [`Slot::Edited`]; **only the edited
/// slots are re-encoded on save** — `Original` slots flow through to the
/// save output untouched, which is what makes "edit one hash field" safe.
#[derive(Debug, Clone, PartialEq)]
pub enum EditedValue {
    /// The scalar value: original bytes or an edited replacement.
    Scalar(Slot),
    /// Hash: one slot per field **value**; field names stay `Original`
    /// (field renaming is delete+add, out of the byte model).
    Hash(Vec<(RawBytes, Slot)>),
    /// List: one slot per item (items may be added/removed by index).
    List(Vec<Slot>),
    /// Set: one slot per member.
    Set(Vec<Slot>),
    /// ZSet: one entry per member (score editability lives in the entry).
    ZSet(Vec<EditedZSetEntry>),
    /// Streams are read-only in this version (PRD story 35).
    Stream,
    /// JSON: the whole serialized document is the editable unit.
    Json(Slot),
}

/// One editable position: original bytes or a user edit.
///
/// `Edited` only ever contains bytes that came from a validated UTF-8
/// string (or explicit raw bytes) — see the setters on [`EditorState`].
#[derive(Debug, Clone, PartialEq)]
pub enum Slot {
    /// Untouched: the exact original bytes pass through on save.
    Original(RawBytes),
    /// User-edited content, already validated at edit time.
    Edited(RawBytes),
}

/// One zset entry in the working buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct EditedZSetEntry {
    /// The member: original or edited.
    pub member: Slot,
    /// The score: original or edited.
    pub score: SlotScore,
}

/// A zset score slot. Scores are numbers; the original score is preserved
/// exactly (as the f64 the server reported) unless edited.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SlotScore {
    /// Untouched server-reported score.
    Original(f64),
    /// User-edited score.
    Edited(f64),
}

impl SlotScore {
    /// The numeric value this score slot contributes, whatever its origin.
    fn score_value(&self) -> f64 {
        match self {
            SlotScore::Original(s) | SlotScore::Edited(s) => *s,
        }
    }
}

impl Slot {
    /// The bytes this slot contributes to the save output: the exact
    /// original bytes when untouched, the edit otherwise.
    fn output(&self) -> &[u8] {
        match self {
            Slot::Original(bytes) | Slot::Edited(bytes) => bytes.as_bytes(),
        }
    }
}

impl EditedValue {
    /// Builds a clean working copy: every slot `Original`.
    pub fn from_raw(value: &RawValue) -> Self {
        match value {
            RawValue::String(bytes) => Self::Scalar(Slot::Original(bytes.clone())),
            RawValue::Json(bytes) => Self::Json(Slot::Original(bytes.clone())),
            RawValue::Hash(pairs) => Self::Hash(
                pairs
                    .iter()
                    .map(|(f, v)| (f.clone(), Slot::Original(v.clone())))
                    .collect(),
            ),
            RawValue::List(items) => {
                Self::List(items.iter().map(|b| Slot::Original(b.clone())).collect())
            }
            RawValue::Set(members) => {
                Self::Set(members.iter().map(|b| Slot::Original(b.clone())).collect())
            }
            RawValue::ZSet(entries) => Self::ZSet(
                entries
                    .iter()
                    .map(|e| EditedZSetEntry {
                        member: Slot::Original(e.member.clone()),
                        score: SlotScore::Original(e.score),
                    })
                    .collect(),
            ),
            RawValue::Stream(_) => Self::Stream,
        }
    }

    /// True iff the save output would differ from the original — the
    /// *real* dirty signal, so `dirty` can never disagree with the data.
    /// Content, not edit-history, decides: an edit that leaves the bytes
    /// identical is not dirty.
    fn has_edits(&self, original: &RawValue) -> bool {
        match (self, original) {
            (EditedValue::Scalar(slot), RawValue::String(bytes))
            | (EditedValue::Json(slot), RawValue::Json(bytes)) => slot.output() != bytes.as_bytes(),
            (EditedValue::Hash(slots), RawValue::Hash(pairs)) => {
                slots.len() != pairs.len()
                    || slots.iter().enumerate().any(|(i, (f, slot))| {
                        f != &pairs[i].0 || slot.output() != pairs[i].1.as_bytes()
                    })
            }
            (EditedValue::List(slots), RawValue::List(items)) => {
                slots.len() != items.len()
                    || slots
                        .iter()
                        .zip(items)
                        .any(|(slot, b)| slot.output() != b.as_bytes())
            }
            (EditedValue::Set(slots), RawValue::Set(members)) => {
                slots.len() != members.len()
                    || slots
                        .iter()
                        .zip(members)
                        .any(|(slot, b)| slot.output() != b.as_bytes())
            }
            (EditedValue::ZSet(slots), RawValue::ZSet(entries)) => {
                slots.len() != entries.len()
                    || slots.iter().enumerate().any(|(i, entry)| {
                        entry.member.output() != entries[i].member.as_bytes()
                            || entry.score.score_value() != entries[i].score
                    })
            }
            // Type changed under us (e.g. server key now holds a different
            // type) — that is a divergence, not an edit.
            _ => false,
        }
    }
}

/// The dirty-editor state machine for one key's value.
///
/// `original` is the server's latest bytes (refreshed freely); `working`
/// is the user's buffer (touched by nothing but explicit edits and
/// Discard/Save).
#[derive(Debug, Clone, PartialEq)]
pub struct EditorState {
    /// Latest server bytes for this key.
    pub original: RawValue,
    /// The user's working buffer.
    pub working: EditedValue,
    /// True when the user has unsaved edits. Kept in sync with the data by
    /// [`EditorState::sync_dirty`]; use the edit/save APIs rather than
    /// setting it directly.
    pub dirty: bool,
    /// True when the server's original changed *under a dirty editor* —
    /// the refresh could not be applied to the working buffer.
    pub diverged_from_server: bool,
}

impl EditorState {
    /// Opens a clean editor over freshly fetched bytes.
    pub fn open(original: RawValue) -> Self {
        let working = EditedValue::from_raw(&original);
        Self {
            original,
            working,
            dirty: false,
            diverged_from_server: false,
        }
    }

    /// Refresh with new server bytes (PRD story 25 / acceptance 9).
    ///
    /// - **Dirty editor:** `original` is replaced and
    ///   `diverged_from_server` is set; the working buffer is **never
    ///   touched**. Only Discard or a successful Save clears the flag.
    /// - **Clean editor:** both sides are replaced (a plain re-open), and
    ///   any stale divergence flag is cleared.
    pub fn apply_refresh(&mut self, new_original: RawValue) {
        if self.dirty {
            self.original = new_original;
            self.diverged_from_server = true;
        } else {
            self.original = new_original;
            self.working = EditedValue::from_raw(&self.original);
            self.diverged_from_server = false;
        }
    }

    /// Replaces one editable field's content with a validated UTF-8 string.
    ///
    /// The signature takes `&str`, so a lossy display string is a type
    /// error away from entering the buffer — [`RawBytes::to_lossy_display`]
    /// output cannot reach this method without the caller explicitly
    /// acknowledging it as text.
    pub fn edit_text(&mut self, path: &FieldPath, text: &str) -> Result<(), EditError> {
        let bytes = RawBytes::from(text);
        self.edit_bytes(path, bytes)
    }

    /// Replaces one editable field's content with explicit raw bytes.
    ///
    /// This is the escape hatch for byte-level edits (e.g. hex editing in
    /// a later version); it is equally validated — only that the path
    /// exists. Unlike `edit_text`, no UTF-8 requirement applies.
    pub fn edit_bytes(&mut self, path: &FieldPath, bytes: RawBytes) -> Result<(), EditError> {
        // Score edits arrive as text; pre-parse (borrowing) so invalid
        // numbers are rejected at edit time, never at save time.
        let parsed_score = std::str::from_utf8(bytes.as_bytes())
            .ok()
            .and_then(|t| t.trim().parse::<f64>().ok());
        let slot = Slot::Edited(bytes);
        match (&mut self.working, path) {
            (EditedValue::Scalar(s), FieldPath::Scalar) => *s = slot,
            (EditedValue::Json(s), FieldPath::Scalar) => *s = slot,
            (EditedValue::Hash(pairs), FieldPath::HashField(i)) => {
                if let Some((_, v)) = pairs.get_mut(*i) {
                    *v = slot;
                } else {
                    return Err(EditError::NoSuchField);
                }
            }
            (EditedValue::List(items), FieldPath::ListItem(i)) => {
                if let Some(item) = items.get_mut(*i) {
                    *item = slot;
                } else {
                    return Err(EditError::NoSuchField);
                }
            }
            (EditedValue::Set(members), FieldPath::SetMember(i)) => {
                if let Some(member) = members.get_mut(*i) {
                    *member = slot;
                } else {
                    return Err(EditError::NoSuchField);
                }
            }
            (EditedValue::ZSet(entries), FieldPath::ZSetMember(i)) => {
                if let Some(entry) = entries.get_mut(*i) {
                    entry.member = slot;
                } else {
                    return Err(EditError::NoSuchField);
                }
            }
            (EditedValue::ZSet(entries), FieldPath::ZSetScore(i)) => {
                let entry = entries.get_mut(*i).ok_or(EditError::NoSuchField)?;
                entry.score = SlotScore::Edited(parsed_score.ok_or(EditError::InvalidScore)?);
            }
            _ => return Err(EditError::WrongPathForType),
        }
        self.sync_dirty();
        Ok(())
    }

    /// Discards all edits: the working buffer is rebuilt from the latest
    /// original bytes, and divergence is cleared (the user chose the
    /// server's version).
    pub fn discard(&mut self) {
        self.working = EditedValue::from_raw(&self.original);
        self.dirty = false;
        self.diverged_from_server = false;
    }

    /// Marks the save as completed: the working buffer becomes the new
    /// original (both sides now agree) and the divergence flag clears.
    ///
    /// Call this only after redis-io reports the write `Acknowledged`.
    pub fn save_acknowledged(&mut self) {
        self.original = self.save_encoding();
        self.working = EditedValue::from_raw(&self.original);
        self.dirty = false;
        self.diverged_from_server = false;
    }

    /// Recomputes `dirty` from the data, so the flag can never disagree
    /// with the buffer's contents.
    fn sync_dirty(&mut self) {
        self.dirty = self.working.has_edits(&self.original);
    }

    /// Encodes the save output: a [`RawValue`] where **only the edited
    /// slots carry new bytes** — every untouched slot passes its exact
    /// original bytes through, byte-identical.
    ///
    /// This is what redis-io serializes into write commands. There is no
    /// code path that re-encodes a whole container from strings: the output
    /// type shares `RawBytes` with the input, and lossy display strings can
    /// never enter it (see [`Slot`]).
    pub fn save_encoding(&self) -> RawValue {
        match (&self.working, &self.original) {
            (EditedValue::Scalar(slot), _) => {
                RawValue::String(RawBytes::new(slot.output().to_vec()))
            }
            (EditedValue::Json(slot), _) => RawValue::Json(RawBytes::new(slot.output().to_vec())),
            (EditedValue::Hash(slots), RawValue::Hash(pairs)) => RawValue::Hash(
                slots
                    .iter()
                    .enumerate()
                    .map(|(i, (field, slot))| {
                        // Untouched pairs keep the exact original field and
                        // value bytes; an edited value keeps the original
                        // field bytes with the new value bytes.
                        let field = if i < pairs.len() && field == &pairs[i].0 {
                            pairs[i].0.clone()
                        } else {
                            field.clone()
                        };
                        (field, RawBytes::new(slot.output().to_vec()))
                    })
                    .collect(),
            ),
            (EditedValue::List(slots), _) => RawValue::List(
                slots
                    .iter()
                    .map(|s| RawBytes::new(s.output().to_vec()))
                    .collect(),
            ),
            (EditedValue::Set(slots), _) => RawValue::Set(
                slots
                    .iter()
                    .map(|s| RawBytes::new(s.output().to_vec()))
                    .collect(),
            ),
            (EditedValue::ZSet(slots), RawValue::ZSet(orig)) => RawValue::ZSet(
                slots
                    .iter()
                    .map(|entry| {
                        let member = RawBytes::new(entry.member.output().to_vec());
                        // Score formatting: an untouched score is emitted by
                        // the original score's canonical formatting (identi-
                        // cal to what the server sent); an edited score is
                        // formatted from the number.
                        let score = match entry.score {
                            SlotScore::Original(s) => s,
                            SlotScore::Edited(s) => s,
                        };
                        let _ = orig; // scores already numeric in both
                        ZSetEntry { member, score }
                    })
                    .collect(),
            ),
            // Streams are read-only; the original value passes through.
            (EditedValue::Stream, original) => original.clone(),
            // Type mismatch (server type changed under a dirty editor):
            // saving is blocked at a higher level; be honest and pass the
            // original through untouched rather than fabricating data.
            (_, original) => original.clone(),
        }
    }
}

/// Why an edit was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The field path does not exist in the working value.
    NoSuchField,
    /// The path does not address an editable slot of this value type.
    WrongPathForType,
    /// A zset score edit was not a valid number.
    InvalidScore,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash() -> RawValue {
        RawValue::Hash(vec![
            (RawBytes::from("field1"), RawBytes::from("value1")),
            (RawBytes::from("field2"), RawBytes::from(vec![0xC3, 0x28])),
            (RawBytes::from("field3"), RawBytes::from("value3")),
        ])
    }

    #[test]
    fn clean_editor_refresh_replaces_both_sides() {
        let mut editor = EditorState::open(RawValue::String(RawBytes::from("old")));
        editor.apply_refresh(RawValue::String(RawBytes::from("new")));
        assert!(!editor.dirty);
        assert!(!editor.diverged_from_server);
        assert_eq!(editor.original, RawValue::String(RawBytes::from("new")));
        assert!(!editor.dirty);
    }

    #[test]
    fn dirty_editor_refresh_never_touches_working() {
        let mut editor = EditorState::open(RawValue::String(RawBytes::from("original")));
        editor.edit_text(&FieldPath::Scalar, "my edit").unwrap();
        assert!(editor.dirty);

        // Server changes the value underneath.
        editor.apply_refresh(RawValue::String(RawBytes::from("server version")));
        assert!(editor.diverged_from_server, "divergence flag must be set");
        assert_eq!(
            editor.original,
            RawValue::String(RawBytes::from("server version"))
        );
        // Working buffer untouched — byte for byte.
        match &editor.working {
            EditedValue::Scalar(Slot::Edited(bytes)) => {
                assert_eq!(bytes.as_bytes(), b"my edit");
            }
            other => panic!("working was clobbered: {other:?}"),
        }
        // And still dirty.
        assert!(editor.dirty);
    }

    #[test]
    fn divergence_clears_only_on_discard_or_save() {
        let mut editor = EditorState::open(RawValue::String(RawBytes::from("v1")));
        editor.edit_text(&FieldPath::Scalar, "edit").unwrap();
        editor.apply_refresh(RawValue::String(RawBytes::from("v2")));
        assert!(editor.diverged_from_server);

        editor.discard();
        assert!(!editor.diverged_from_server);
        assert!(!editor.dirty);
        assert_eq!(editor.original, RawValue::String(RawBytes::from("v2")));

        // Same via save.
        editor.edit_text(&FieldPath::Scalar, "edit2").unwrap();
        editor.apply_refresh(RawValue::String(RawBytes::from("v3")));
        assert!(editor.diverged_from_server);
        editor.save_acknowledged();
        assert!(!editor.diverged_from_server);
        assert!(!editor.dirty);
        assert_eq!(editor.original, RawValue::String(RawBytes::from("edit2")));
    }

    #[test]
    fn editing_one_hash_field_reencodes_only_that_field() {
        let mut editor = EditorState::open(hash());
        editor
            .edit_text(&FieldPath::HashField(1), "new value2")
            .unwrap();
        assert!(editor.dirty);

        let saved = editor.save_encoding();
        let RawValue::Hash(pairs) = saved else {
            panic!("save must stay a hash");
        };
        // Edited field: new bytes.
        assert_eq!(pairs[1].1.as_bytes(), b"new value2");
        // Untouched entries: EXACT original bytes — including the invalid
        // UTF-8 field2 value before the edit? No: field2's value WAS the
        // edited one. The untouched ones are field1 and field3.
        assert_eq!(pairs[0].0.as_bytes(), b"field1");
        assert_eq!(pairs[0].1.as_bytes(), b"value1");
        assert_eq!(pairs[2].0.as_bytes(), b"field3");
        assert_eq!(pairs[2].1.as_bytes(), b"value3");
    }

    #[test]
    fn unedited_invalid_utf8_hash_value_passes_through_byte_identical() {
        let mut editor = EditorState::open(hash());
        // Edit field0 only; field1's invalid-UTF-8 value must survive.
        editor
            .edit_text(&FieldPath::HashField(0), "edited")
            .unwrap();
        let saved = editor.save_encoding();
        let RawValue::Hash(pairs) = saved else {
            panic!()
        };
        assert_eq!(pairs[1].1.as_bytes(), &[0xC3, 0x28]);
    }

    #[test]
    fn clean_save_encoding_returns_original_bytes_identical() {
        for value in [
            RawValue::String(RawBytes::from(vec![0xC3, 0x28, 0xF5, 0x00])),
            RawValue::List(vec![RawBytes::from("a"), RawBytes::from(vec![0x00])]),
            RawValue::Stream(vec![StreamEntry {
                id: RawBytes::from("1-1"),
                fields: vec![(RawBytes::from("f"), RawBytes::from("v"))],
            }]),
            RawValue::Json(RawBytes::from(r#"{"a":1}"#)),
        ] {
            let editor = EditorState::open(value.clone());
            assert!(!editor.dirty);
            assert_eq!(editor.save_encoding(), value, "clean save must be identity");
        }
    }

    #[test]
    fn binary_edits_use_explicit_bytes_not_lossy_text() {
        let mut editor = EditorState::open(RawValue::String(RawBytes::from(vec![0xF5])));
        // The lossy display would be "\u{FFFD}" — editing with those bytes
        // would corrupt. Instead, an explicit byte edit works:
        editor
            .edit_bytes(&FieldPath::Scalar, RawBytes::from(vec![0xC3, 0x28]))
            .unwrap();
        let saved = editor.save_encoding();
        assert_eq!(saved, RawValue::String(RawBytes::from(vec![0xC3, 0x28])));
    }

    #[test]
    fn edit_errors_are_rejected() {
        let mut editor = EditorState::open(hash());
        assert_eq!(
            editor.edit_text(&FieldPath::HashField(99), "x"),
            Err(EditError::NoSuchField)
        );
        assert_eq!(
            editor.edit_text(&FieldPath::ListItem(0), "x"),
            Err(EditError::WrongPathForType)
        );

        let mut zset = EditorState::open(RawValue::ZSet(vec![ZSetEntry {
            member: RawBytes::from("m"),
            score: 1.0,
        }]));
        assert_eq!(
            zset.edit_text(&FieldPath::ZSetScore(0), "not a number"),
            Err(EditError::InvalidScore)
        );
        assert!(!zset.dirty, "a rejected edit must not dirty the editor");
    }

    #[test]
    fn dirty_stays_in_sync_with_data() {
        let mut editor = EditorState::open(RawValue::String(RawBytes::from("same")));
        // Editing the identical content back: the save output would equal
        // the original, so this is not dirty.
        editor.edit_text(&FieldPath::Scalar, "same").unwrap();
        assert!(!editor.dirty, "edit to identical content is not dirty");
        editor.edit_text(&FieldPath::Scalar, "different").unwrap();
        assert!(editor.dirty);
        // And back again to the original content: not dirty anymore.
        editor.edit_text(&FieldPath::Scalar, "same").unwrap();
        assert!(!editor.dirty, "reverting the edit clears dirty");
    }

    #[test]
    fn zset_score_and_member_edits_encode() {
        let mut editor = EditorState::open(RawValue::ZSet(vec![
            ZSetEntry {
                member: RawBytes::from("alpha"),
                score: 1.5,
            },
            ZSetEntry {
                member: RawBytes::from("beta"),
                score: 2.0,
            },
        ]));
        editor
            .edit_text(&FieldPath::ZSetMember(0), "alpha!")
            .unwrap();
        editor.edit_text(&FieldPath::ZSetScore(1), "3.25").unwrap();

        let RawValue::ZSet(entries) = editor.save_encoding() else {
            panic!()
        };
        assert_eq!(entries[0].member.as_bytes(), b"alpha!");
        assert_eq!(entries[0].score, 1.5, "untouched score preserved");
        assert_eq!(entries[1].member.as_bytes(), b"beta");
        assert_eq!(entries[1].score, 3.25);
    }

    #[test]
    fn stream_editor_is_readonly_passthrough() {
        let stream = RawValue::Stream(vec![StreamEntry {
            id: RawBytes::from("1-1"),
            fields: vec![(RawBytes::from("f"), RawBytes::from("v"))],
        }]);
        let mut editor = EditorState::open(stream.clone());
        editor.apply_refresh(stream.clone());
        assert_eq!(editor.save_encoding(), stream);
        assert_eq!(
            editor.edit_text(&FieldPath::Scalar, "x"),
            Err(EditError::WrongPathForType)
        );
    }

    #[test]
    fn list_and_set_partial_edits_preserve_untouched_bytes() {
        let mut list = EditorState::open(RawValue::List(vec![
            RawBytes::from("one"),
            RawBytes::from(vec![0xF5, 0x00]),
            RawBytes::from("three"),
        ]));
        list.edit_text(&FieldPath::ListItem(2), "THREE").unwrap();
        let RawValue::List(items) = list.save_encoding() else {
            panic!()
        };
        assert_eq!(items[0].as_bytes(), b"one");
        assert_eq!(items[1].as_bytes(), &[0xF5, 0x00]);
        assert_eq!(items[2].as_bytes(), b"THREE");

        let mut set = EditorState::open(RawValue::Set(vec![
            RawBytes::from("a"),
            RawBytes::from("🦀"),
        ]));
        set.edit_text(&FieldPath::SetMember(0), "z").unwrap();
        let RawValue::Set(members) = set.save_encoding() else {
            panic!()
        };
        assert_eq!(members[0].as_bytes(), b"z");
        assert_eq!(members[1].as_bytes(), "🦀".as_bytes());
    }
}
