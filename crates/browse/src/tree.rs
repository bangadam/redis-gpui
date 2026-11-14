//! Colon-separator tree over accumulated scan keys (PRD story 11).
//!
//! # Design choices (documented, as required)
//!
//! - **Same accumulation seam as [`crate::flat`]:** the tree is fed by
//!   [`KeyTree::apply_page`] with the identical stale-generation guard, so
//!   it is safe to wire directly to the page event channel next to
//!   `FlatKeyList` and stays consistent with it (both drop pages from any
//!   generation other than the current one).
//! - **Ordering: lexical byte order.** Children are stored in a
//!   `BTreeMap<Vec<u8>, _>` keyed by raw segment bytes, so every level of
//!   the rendered tree comes out sorted in Redis's own lexical order —
//!   the same order [`crate::flat::FlatKeyList::sorted`] uses. Binary
//!   (non-UTF-8) segments sort by raw bytes, never by a lossy string
//!   projection.
//! - **A node can be both leaf and parent.** `user` and `user:1` are both
//!   keys; the node `user` carries its own full key as a leaf *and*
//!   children. A leaf is therefore "this exact path is a key", not
//!   "this node has no children".
//! - **Keys are stored, not rows.** Memory is O(bytes of kept keys),
//!   same bound as the flat list. Rows ([`TreeRow`]) are materialized on
//!   demand by [`KeyTree::rows`], like `sorted()` in the flat module.
//! - **Lazy expansion is a two-piece protocol** (the engine half lives in
//!   [`KeyTree::scan_for_prefix`]):
//!   1. Every interior node tracks `children_loaded: bool`. It starts
//!      `false` and is only ever set by [`KeyTree::mark_subtree_loaded`],
//!      which the app calls when a scan *covering* that subtree reports
//!      [`crate::scan::PageOutcome::Exhausted`]. Applying pages never
//!      sets it; if a page introduces a child under a node already
//!      marked loaded, the flag defensively resets to `false` (complete
//!      knowledge was claimed too early).
//!   2. When the user expands an unloaded node, the app issues a *scoped*
//!      scan whose MATCH is the node's path + separator + `*`
//!      ([`KeyTree::scan_for_prefix`] builds it, glob-escaping the path).
//!      Pages from that scan flow back through `apply_page` (the scoped
//!      pattern only matches keys under the subtree); when its cursor
//!      exhausts, the app calls `mark_subtree_loaded(path)`.
//! - **Depth guard (pathological keys):** a key with 20+ colon segments
//!   or one huge segment cannot blow up the rendered model. Rendering is
//!   capped at [`MAX_RENDERED_DEPTH`] (6) levels: every key still appears
//!   exactly once, but beyond the cap its remaining segments are shown
//!   as a single combined label (segments joined by the separator) at
//!   the deepest level. Storage is unaffected (it is bounded by the key
//!   bytes themselves); only row count per key is bounded (≤ 6).
//! - **Per-row TYPE tags are app-side:** the engine cannot know types
//!   without one `TYPE` round trip per key (forbidden at this scale —
//!   see [`crate::scan`], whose only seam is `scan_batch`). The app
//!   probes `TYPE` on key selection (as `session.rs` already does) and
//!   filters with [`crate::type_filter`]. Server-side `SCAN ... TYPE`
//!   passthrough is carried by [`crate::scan::KeyScan::with_type`].
//! - **SELECT / DB switch:** switching logical DBs restarts browsing
//!   (new generation, reset lists) — that is app policy; this tree only
//!   reacts via the generation guard and [`KeyTree::reset`].

use std::collections::BTreeMap;

use crate::cancellation::SearchGeneration;
use crate::scan::{KeyScan, ScanPage};
use crate::search::escape_glob;

/// Default separator: `:` (PRD story 11 — "session:42:cart" groups
/// naturally by colon).
pub const DEFAULT_SEPARATOR: &[u8] = b":";

/// Maximum number of levels the rendered model shows (see the depth
/// guard in the module docs). Levels are 0-based, so rows have
/// `depth < MAX_RENDERED_DEPTH`; anything deeper is folded into one
/// combined label at `depth == MAX_RENDERED_DEPTH - 1`.
pub const MAX_RENDERED_DEPTH: usize = 6;

/// One rendered row of the tree (the model the UI renders).
///
/// Produced by [`KeyTree::rows`] in depth-first order with children in
/// lexical byte order. Every kept key yields exactly one row with
/// `is_leaf == true` and `key == Some(full key)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    /// Display label: the node's segment, or — at the depth cap — the
    /// combined remaining segments joined by the separator.
    pub label: Vec<u8>,
    /// Full path from the root to this node, segments joined by the tree
    /// separator. Pass to [`KeyTree::scan_for_prefix`] or
    /// [`KeyTree::mark_subtree_loaded`]. Empty only for a degenerate
    /// empty-key root child.
    pub path: Vec<u8>,
    /// Indent level; always `< MAX_RENDERED_DEPTH`.
    pub depth: usize,
    /// A full key terminates exactly at this row.
    pub is_leaf: bool,
    /// The row has children rows under it (the expansion affordance).
    /// Always `false` for combined-label rows at the depth cap.
    pub has_children: bool,
    /// Whether the children are known *complete*: only true after the
    /// app marked a covering scan's subtree loaded (see module docs).
    pub children_loaded: bool,
    /// The full raw key when `is_leaf` (the selection target; may be
    /// non-UTF-8).
    pub key: Option<Vec<u8>>,
}

/// The tree over accumulated keys, split by a per-tree separator.
#[derive(Debug)]
pub struct KeyTree {
    separator: Vec<u8>,
    root: TreeNode,
    /// Generation of the last applied page; pages from other
    /// generations are rejected (acceptance 2), exactly like
    /// `FlatKeyList`.
    generation: Option<u64>,
    /// Distinct keys kept (leaf count), maintained on insert.
    key_count: usize,
}

/// A node in the tree. Opaque to the app — the rendering model is
/// [`TreeRow`] and all operations take paths.
#[derive(Debug, Default)]
struct TreeNode {
    /// Children keyed by raw segment bytes; `BTreeMap` gives the
    /// documented lexical byte ordering for free.
    children: BTreeMap<Vec<u8>, TreeNode>,
    /// The full key when this exact path is itself a key.
    leaf_key: Option<Vec<u8>>,
    /// Whether the children are known complete (module docs: only
    /// [`KeyTree::mark_subtree_loaded`] sets this).
    children_loaded: bool,
}

impl Default for KeyTree {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyTree {
    /// A tree with the default `:` separator.
    pub fn new() -> Self {
        Self::with_separator(DEFAULT_SEPARATOR.to_vec())
    }

    /// A tree with a custom separator (e.g. `.` or `::`). The separator
    /// is a byte sequence; multi-byte separators split on the whole
    /// sequence.
    pub fn with_separator(separator: Vec<u8>) -> Self {
        Self {
            separator,
            root: TreeNode::default(),
            generation: None,
            key_count: 0,
        }
    }

    /// The separator this tree splits keys on.
    pub fn separator(&self) -> &[u8] {
        &self.separator
    }

    /// Distinct keys currently kept (leaf count).
    pub fn len(&self) -> usize {
        self.key_count
    }

    /// Whether no keys are kept.
    pub fn is_empty(&self) -> bool {
        self.key_count == 0
    }

    /// The generation of the last applied page (None = fresh tree).
    pub fn generation(&self) -> Option<u64> {
        self.generation
    }

    /// Whether the exact key is kept as a leaf path.
    pub fn contains_key(&self, key: &[u8]) -> bool {
        let mut node = &self.root;
        for seg in split_segments(key, &self.separator) {
            let Some(child) = node.children.get(seg) else {
                return false;
            };
            node = child;
        }
        node.leaf_key.is_some()
    }

    /// Apply one scan page, if it is not stale (the same seam and guard
    /// as `FlatKeyList::apply_page`).
    ///
    /// Returns `true` if the page was applied. Duplicate keys (SCAN
    /// revisits, double delivery) count once.
    pub fn apply_page(&mut self, page: &ScanPage, gen: &SearchGeneration) -> bool {
        if !gen.is_current(page.generation) {
            return false; // stale batch: dropped, not rendered
        }
        for key in &page.new_keys {
            self.insert(key);
        }
        self.generation = Some(page.generation);
        true
    }

    /// Force a full reset (new scan run / DB switch / new search):
    /// drops all keys and loaded flags so a fresh generation starts
    /// from an empty tree.
    pub fn reset(&mut self) {
        self.root = TreeNode::default();
        self.generation = None;
        self.key_count = 0;
    }

    /// Insert one key by segments, creating nodes as needed. Duplicate
    /// leaf keys are no-ops. Defensively demotes `children_loaded` on a
    /// node that gains a *new* child while claiming completeness.
    fn insert(&mut self, key: &[u8]) {
        let mut node = &mut self.root;
        for seg in split_segments(key, &self.separator) {
            if node.children_loaded && !node.children.contains_key(seg) {
                // A covering scan finished, yet a new child appeared:
                // the "complete" claim was wrong (or a wider scan is
                // feeding us). Reset it — the flag only tells the truth.
                node.children_loaded = false;
            }
            node = node.children.entry(seg.to_vec()).or_default();
        }
        if node.leaf_key.is_none() {
            node.leaf_key = Some(key.to_vec());
            self.key_count += 1;
        }
    }

    /// Mark the subtree rooted at `prefix` (a joined node path, as found
    /// in [`TreeRow::path`]; empty = the whole tree) as having complete
    /// children.
    ///
    /// The app calls this when the scan covering that subtree — the
    /// global scan for the root, a `prefix*` scoped scan for a node —
    /// reports cursor exhaustion. Propagates to all descendants: a
    /// completed covering scan knows the whole subtree. Returns `false`
    /// (and changes nothing) when the path names no node **or names a
    /// childless leaf** — there are no children to load, so the
    /// "covering scan completed" event is meaningless for it (its
    /// children list — empty — is trivially complete either way; the
    /// UI must not offer expansion on a leaf, and `TreeRow` already
    /// reports `has_children == false` for it).
    pub fn mark_subtree_loaded(&mut self, prefix: &[u8]) -> bool {
        let Some(node) = self.node_at_mut(prefix) else {
            return false;
        };
        if node.children.is_empty() {
            return false; // a leaf: nothing to load
        }
        set_loaded_recursive(node);
        true
    }

    /// Build the scoped `KeyScan` for expanding the node at `prefix`:
    /// MATCH is `prefix<separator>*` with the prefix glob-escaped (a
    /// literal `*` in a key must not widen the scan). An empty prefix
    /// (the root) scans everything (`*`).
    ///
    /// The run's pages flow back through [`KeyTree::apply_page`] — the
    /// scoped pattern only matches keys of this subtree — and its
    /// exhaustion flips the node's `children_loaded` via
    /// [`KeyTree::mark_subtree_loaded`].
    pub fn scan_for_prefix(
        &self,
        prefix: &[u8],
        generation: u64,
        page_size: usize,
        batch_hint: u64,
    ) -> KeyScan {
        KeyScan::with_pattern(
            generation,
            page_size,
            batch_hint,
            Some(self.prefix_pattern(prefix)),
        )
    }

    /// The MATCH glob [`KeyTree::scan_for_prefix`] uses (exposed for
    /// tests and app-side diagnostics).
    pub fn prefix_pattern(&self, prefix: &[u8]) -> Vec<u8> {
        if prefix.is_empty() {
            b"*".to_vec()
        } else {
            [escape_glob(prefix), self.separator.clone(), b"*".to_vec()].concat()
        }
    }

    /// Render the whole tree as rows: depth-first, children in lexical
    /// byte order, depth capped at [`MAX_RENDERED_DEPTH`]. O(kept keys);
    /// call on demand (like `FlatKeyList::sorted`), never per frame.
    ///
    /// Invariant: the number of rows with `is_leaf` equals
    /// [`KeyTree::len`] — every kept key appears exactly once, even past
    /// the depth cap (as a combined-label row).
    pub fn rows(&self) -> Vec<TreeRow> {
        let mut out = Vec::new();
        for (seg, child) in &self.root.children {
            self.render_node(child, seg, &[], 0, &mut out);
        }
        out
    }

    /// Render one node (recursive). `parent_path` is the joined path of
    /// the parent; this node's path is `parent_path + sep + seg`.
    fn render_node(
        &self,
        node: &TreeNode,
        seg: &[u8],
        parent_path: &[u8],
        depth: usize,
        out: &mut Vec<TreeRow>,
    ) {
        let mut path = parent_path.to_vec();
        if !path.is_empty() {
            path.extend_from_slice(&self.separator);
        }
        path.extend_from_slice(seg);

        if depth < MAX_RENDERED_DEPTH - 1 {
            out.push(TreeRow {
                label: seg.to_vec(),
                path: path.clone(),
                depth,
                is_leaf: node.leaf_key.is_some(),
                has_children: !node.children.is_empty(),
                children_loaded: node.children_loaded,
                key: node.leaf_key.clone(),
            });
            for (cseg, child) in &node.children {
                self.render_node(child, cseg, &path, depth + 1, out);
            }
        } else {
            // Depth cap: do not recurse. One combined-label row per key
            // in this subtree, each carrying the full key, so nothing is
            // lost — only the level count is bounded.
            let mut tails: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            Self::collect_tails(node, seg.to_vec(), &self.separator, &mut tails);
            for (label, key) in tails {
                out.push(TreeRow {
                    label,
                    path: path.clone(),
                    depth,
                    is_leaf: true,
                    has_children: false, // nothing expandable past the cap
                    children_loaded: node.children_loaded,
                    key: Some(key),
                });
            }
        }
    }

    /// Collect `(label, full key)` pairs for every key at or below
    /// `node`, where `label` is the joined segments from `node`'s own
    /// segment down to the key. Lexical order (BTreeMap walk).
    fn collect_tails(
        node: &TreeNode,
        prefix: Vec<u8>,
        sep: &[u8],
        tails: &mut Vec<(Vec<u8>, Vec<u8>)>,
    ) {
        if let Some(k) = &node.leaf_key {
            tails.push((prefix.clone(), k.clone()));
        }
        for (cseg, child) in &node.children {
            let mut next = prefix.clone();
            next.extend_from_slice(sep);
            next.extend_from_slice(cseg);
            Self::collect_tails(child, next, sep, tails);
        }
    }

    /// The node at a joined `path` (empty path = the root).
    fn node_at_mut(&mut self, path: &[u8]) -> Option<&mut TreeNode> {
        if path.is_empty() {
            return Some(&mut self.root);
        }
        let mut node = &mut self.root;
        for seg in split_segments(path, &self.separator) {
            node = node.children.get_mut(seg)?;
        }
        Some(node)
    }
}

/// Split `key` on the whole `sep` byte sequence, keeping empty segments
/// (`a::b` → `a`, ``, `b`; `a:` → `a`, ``). A degenerate empty
/// separator never splits: the whole key is one segment.
fn split_segments<'a>(key: &'a [u8], sep: &[u8]) -> Vec<&'a [u8]> {
    if sep.is_empty() {
        return vec![key];
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + sep.len() <= key.len() {
        if &key[i..i + sep.len()] == sep {
            out.push(&key[start..i]);
            i += sep.len();
            start = i;
        } else {
            i += 1;
        }
    }
    out.push(&key[start..]);
    out
}

/// Set `children_loaded` on a node and all descendants.
fn set_loaded_recursive(node: &mut TreeNode) {
    node.children_loaded = true;
    for child in node.children.values_mut() {
        set_loaded_recursive(child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(gen: u64, keys: &[&[u8]]) -> ScanPage {
        ScanPage {
            new_keys: keys.iter().map(|k| k.to_vec()).collect(),
            generation: gen,
        }
    }

    fn tree_with(keys: &[&[u8]]) -> (KeyTree, SearchGeneration, u64) {
        let mut tree = KeyTree::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        assert!(tree.apply_page(&page(g, keys), &gen));
        (tree, gen, g)
    }

    #[test]
    fn every_key_is_a_leaf_path() {
        let keys: &[&[u8]] = &[b"user:1", b"user:2:cart", b"user", b"sess:42", b"plain"];
        let (tree, _, _) = tree_with(keys);
        for k in keys {
            assert!(tree.contains_key(k), "missing leaf {k:?}");
        }
        assert_eq!(tree.len(), 5);
        // Rows: exactly one leaf row (is_leaf, key present) per key.
        let rows = tree.rows();
        let leaf_rows = rows.iter().filter(|r| r.is_leaf).count();
        assert_eq!(leaf_rows, 5);
        let with_key = rows.iter().filter(|r| r.key.is_some()).count();
        assert_eq!(with_key, 5);
        assert!(!tree.contains_key(b"user:3"));
        assert!(!tree.contains_key(b"user:2:cart:x"));
    }

    #[test]
    fn a_node_can_be_leaf_and_parent() {
        // `user` is a key AND an interior node.
        let (tree, _, _) = tree_with(&[b"user", b"user:1"]);
        let rows = tree.rows();
        let user_row = rows.iter().find(|r| r.path == b"user").unwrap();
        assert!(user_row.is_leaf);
        assert!(user_row.has_children);
        assert_eq!(user_row.key.as_deref(), Some(b"user".as_slice()));
        let user1 = rows.iter().find(|r| r.path == b"user:1").unwrap();
        assert!(user1.is_leaf);
        assert!(!user1.has_children);
    }

    #[test]
    fn dedup_on_revisit() {
        let mut tree = KeyTree::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        // Same key in two pages (SCAN revisit) + duplicated in-page.
        tree.apply_page(&page(g, &[b"user:1", b"user:1"]), &gen);
        tree.apply_page(&page(g, &[b"user:1"]), &gen);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.rows().len(), 2); // user + user:1 rows only
    }

    #[test]
    fn stale_pages_are_rejected() {
        let mut tree = KeyTree::new();
        let gen = SearchGeneration::new();
        let g1 = gen.next_generation();
        let g2 = gen.next_generation();
        assert!(tree.apply_page(&page(g2, &[b"current"]), &gen));
        assert!(!tree.apply_page(&page(g1, &[b"stale"]), &gen));
        assert_eq!(tree.len(), 1);
        assert!(!tree.contains_key(b"stale"));
        assert_eq!(tree.generation(), Some(g2));
    }

    #[test]
    fn keys_without_separator_are_single_leaves() {
        let (tree, _, _) = tree_with(&[b"plainkey", b"other"]);
        let rows = tree.rows();
        assert_eq!(rows.len(), 2); // no nesting
        assert!(rows[0].is_leaf);
        assert_eq!(rows[0].depth, 0);
        // Lexical byte order at the root.
        assert_eq!(rows[0].label, b"other");
        assert_eq!(rows[1].label, b"plainkey");
    }

    #[test]
    fn children_are_lexical_byte_order() {
        let (tree, _, _) = tree_with(&[
            b"z:1", b"a:2", b"\xff:0", // 0xff sorts after any ASCII
            b"a:1",
        ]);
        let rows = tree.rows();
        let top: Vec<&[u8]> = rows
            .iter()
            .filter(|r| r.depth == 0)
            .map(|r| r.label.as_slice())
            .collect();
        assert_eq!(top, [b"a".as_slice(), b"z", b"\xff"]);
        // Under `a`: 1 before 2.
        let a_children: Vec<&[u8]> = rows
            .iter()
            .filter(|r| r.depth == 1 && r.path.starts_with(b"a:"))
            .map(|r| r.label.as_slice())
            .collect();
        assert_eq!(a_children, [b"1".as_slice(), b"2"]);
    }

    #[test]
    fn empty_segments_are_preserved() {
        // `a::b` → segments a, "", b — the empty middle segment is a
        // real node, not a skip.
        let (tree, _, _) = tree_with(&[b"a::b"]);
        assert!(tree.contains_key(b"a::b"));
        let rows = tree.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].label, b"");
        assert_eq!(rows[1].path, b"a:");
        // Trailing separator: `a:` → segments a, "".
        let (tree2, _, _) = tree_with(&[b"a:"]);
        assert!(tree2.contains_key(b"a:"));
        assert_eq!(tree2.rows().len(), 2);
    }

    #[test]
    fn children_loaded_only_via_mark_and_only_for_the_subtree() {
        let mut tree = KeyTree::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        tree.apply_page(&page(g, &[b"user:1", b"user:2:deep", b"sess:1"]), &gen);

        // Pages alone never mark anything loaded.
        for row in tree.rows() {
            assert!(!row.children_loaded, "page must not set children_loaded");
        }

        // Mark only the `user` subtree (its scoped scan finished).
        assert!(tree.mark_subtree_loaded(b"user"));
        let rows = tree.rows();
        let user = rows.iter().find(|r| r.path == b"user").unwrap();
        assert!(user.children_loaded);
        let user2 = rows.iter().find(|r| r.path == b"user:2").unwrap();
        assert!(user2.children_loaded); // propagated to descendants
        let sess = rows.iter().find(|r| r.path == b"sess").unwrap();
        assert!(!sess.children_loaded); // outside the covering scan

        // A path that names no node: no-op, returns false.
        assert!(!tree.mark_subtree_loaded(b"nope"));
        assert!(!tree.mark_subtree_loaded(b"user:1")); // leaf, no children to mark
    }

    #[test]
    fn marking_the_root_loads_everything() {
        let mut tree = KeyTree::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        tree.apply_page(&page(g, &[b"a:b", b"c:d:e"]), &gen);
        // The global scan exhausted: root covers everything.
        assert!(tree.mark_subtree_loaded(b""));
        for row in tree.rows() {
            if row.has_children {
                assert!(row.children_loaded);
            }
        }
    }

    #[test]
    fn new_child_under_a_loaded_node_demotes_the_flag() {
        let mut tree = KeyTree::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        tree.apply_page(&page(g, &[b"user:1"]), &gen);
        assert!(tree.mark_subtree_loaded(b"user"));

        // A current-generation page arrives with a NEW child under the
        // "loaded" node: the completeness claim is now false.
        assert!(tree.apply_page(&page(g, &[b"user:9"]), &gen));
        let user = tree.rows().into_iter().find(|r| r.path == b"user").unwrap();
        assert!(!user.children_loaded);
    }

    #[test]
    fn reset_clears_everything() {
        let mut tree = KeyTree::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        tree.apply_page(&page(g, &[b"user:1"]), &gen);
        tree.mark_subtree_loaded(b"user");
        tree.reset();
        assert!(tree.is_empty());
        assert_eq!(tree.generation(), None);
        assert!(tree.rows().is_empty());
        // Fresh generation flows in cleanly after reset.
        let g2 = gen.next_generation();
        assert!(tree.apply_page(&page(g2, &[b"fresh:1"]), &gen));
        assert!(tree.contains_key(b"fresh:1"));
    }

    #[test]
    fn depth_cap_bounds_rows_per_key() {
        // 20 segments deep: exactly MAX_RENDERED_DEPTH rows, the last
        // one a combined label of the remaining segments.
        // build "s0:s1:...:s19"
        let segs: Vec<String> = (0..20).map(|i| format!("s{i}")).collect();
        let deep = segs.join(":").into_bytes();

        let (tree, _, _) = tree_with(&[deep.as_slice()]);
        let rows = tree.rows();
        assert_eq!(rows.len(), MAX_RENDERED_DEPTH, "row count must be capped");
        // First five rows are plain segments.
        assert_eq!(rows[0].label, b"s0");
        assert_eq!(rows[4].label, b"s4");
        assert_eq!(rows[4].depth, 4);
        // Sixth row: combined tail s5:...:s19, still one row, leaf.
        let last = &rows[5];
        assert_eq!(last.depth, 5);
        assert!(last.is_leaf);
        assert_eq!(last.key.as_deref(), Some(deep.as_slice()));
        let expected_tail = segs[5..].join(":").into_bytes();
        assert_eq!(last.label, expected_tail);
        assert!(!last.has_children);
    }

    #[test]
    fn deep_sibling_keys_each_get_one_combined_row() {
        // Two keys sharing 5 segments, diverging below the cap: each
        // still appears exactly once.
        let k1 = b"a:b:c:d:e:f:1:2";
        let k2 = b"a:b:c:d:e:g:3";
        let (tree, _, _) = tree_with(&[k1, k2]);
        let rows = tree.rows();
        // 5 shared segment rows (a..e) + 2 combined rows.
        assert_eq!(rows.len(), 7);
        let combined: Vec<&TreeRow> = rows.iter().filter(|r| r.depth == 5).collect();
        assert_eq!(combined.len(), 2);
        assert_eq!(combined[0].label, b"f:1:2");
        assert_eq!(combined[0].key.as_deref(), Some(k1.as_slice()));
        assert_eq!(combined[1].label, b"g:3");
        assert_eq!(combined[1].key.as_deref(), Some(k2.as_slice()));
        // Exactly-at-cap key (6 segments) is NOT combined.
        let (tree2, _, _) = tree_with(&[b"a:b:c:d:e:f"]);
        let rows2 = tree2.rows();
        assert_eq!(rows2.len(), 6);
        assert_eq!(rows2[5].label, b"f");
        assert!(rows2[5].is_leaf);
    }

    #[test]
    fn huge_single_segment_does_not_blow_up() {
        // One 1 MiB segment: a single leaf row; memory is the key bytes
        // themselves, rows are one label.
        let mut big = vec![b'x'; 1024 * 1024];
        big[0] = b'k';
        let (tree, _, _) = tree_with(&[big.as_slice()]);
        let rows = tree.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label.len(), big.len());
        assert!(rows[0].is_leaf);
    }

    #[test]
    fn scan_for_prefix_builds_escaped_glob() {
        let tree = KeyTree::new();
        // Root: everything.
        assert_eq!(tree.prefix_pattern(b""), b"*");
        // Node path: path + separator + star.
        assert_eq!(tree.prefix_pattern(b"user"), b"user:*");
        // Glob metacharacters in the path are escaped so they match
        // literally (a key containing `*` must not widen the scan).
        assert_eq!(tree.prefix_pattern(b"a*b"), b"a\\*b:*");
        assert_eq!(tree.prefix_pattern(b"q?"), b"q\\?:*");
        assert_eq!(tree.prefix_pattern(b"[x]"), b"\\[x\\]:*");
        assert_eq!(tree.prefix_pattern(b"back\\slash"), b"back\\\\slash:*");
        // Custom separator is honored.
        let dot_tree = KeyTree::with_separator(b".".to_vec());
        assert_eq!(dot_tree.prefix_pattern(b"user"), b"user.*");
    }

    #[test]
    fn custom_separator_splits_on_the_whole_sequence() {
        let mut tree = KeyTree::with_separator(b"::".to_vec());
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        tree.apply_page(&page(g, &[b"user::1", b"user:plain"]), &gen);
        // `user::1` nests; `user:plain` (single colon) is one segment
        // under a node literally named "user:plain" at the root.
        assert!(tree.contains_key(b"user::1"));
        assert!(tree.contains_key(b"user:plain"));
        let rows = tree.rows();
        let paths: Vec<&[u8]> = rows.iter().map(|r| r.path.as_slice()).collect();
        assert!(paths.contains(&b"user".as_slice()));
        assert!(paths.contains(&b"user::1".as_slice()));
        assert!(paths.contains(&b"user:plain".as_slice()));
        assert_eq!(tree.separator(), b"::".as_slice());
    }

    #[test]
    fn scan_for_prefix_returns_a_ready_engine() {
        let (tree, gen, _) = tree_with(&[b"user:1"]);
        let g2 = gen.next_generation();
        let scan = tree.scan_for_prefix(b"user", g2, 50, 100);
        assert_eq!(scan.generation(), g2);
        assert_eq!(scan.pattern(), Some(b"user:*".to_vec().as_slice()));
        // The empty prefix (root) scans everything.
        let root_scan = tree.scan_for_prefix(b"", g2, 50, 100);
        assert_eq!(root_scan.pattern(), Some(b"*".to_vec().as_slice()));
    }
}
