//! Flat key-list projection (PRD story 12): the ordered, accumulate-as-
//! pages-arrive list the UI renders for the flat view.
//!
//! # Design choices (documented, as required)
//!
//! - **Arrival order kept, sorting on demand.** The list stores keys in
//!   the order the scan delivered them, because:
//!   (a) SCAN order is not lexical anyway — presorting pages would give
//!   the user a false sense of "sorted" while pages 2+ arrive out of
//!   order; (b) sorting per page is O(n log n) churn on every page for
//!   no honest gain. The UI calls [`FlatKeyList::sorted`] (or
//!   [`FlatKeyList::sorted_range`]) when it wants lexical order; a real
//!   sort happens only when the user toggles it.
//! - **Dedup at the seam.** The engine already dedupes, but a defensive
//!   [`FlatKeyList::apply_page`] dedupe makes the list correct even if a
//!   page is (buggily or by racing) delivered twice. The generation
//!   guard is also applied here: a page from a stale generation is
//!   ignored, so the projection itself is safe to wire directly to the
//!   event channel.
//! - **Type tags are Phase 3** (TYPE probes): keys are raw bytes only,
//!   matching what SCAN returns.
//! - **Binary keys:** keys are `Vec<u8>` (non-UTF-8 allowed). Sorting
//!   uses raw byte order (Redis's own lexical order), not a lossy UTF-8
//!   projection.

use crate::cancellation::SearchGeneration;
use crate::scan::{KeyCount, ScanPage};

/// Ordered flat list of keys accumulated from scan pages.
///
/// Cheap to construct per connection tab; holds only the keys the UI
/// chose to keep (the never-materialize-everything rule).
#[derive(Debug, Default)]
pub struct FlatKeyList {
    keys: Vec<Vec<u8>>,
    /// Generation of the last applied page; pages from other
    /// generations are rejected (acceptance 2: stale results dropped).
    generation: Option<u64>,
}

impl FlatKeyList {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one scan page, if it is not stale.
    ///
    /// Returns `true` if the page was applied. Deduplicates against the
    /// existing list (SCAN revisits and duplicate deliveries count once).
    pub fn apply_page(&mut self, page: &ScanPage, gen: &SearchGeneration) -> bool {
        if !gen.is_current(page.generation) {
            return false; // stale batch: dropped, not rendered
        }
        for key in &page.new_keys {
            if !self.keys.iter().any(|k| k == key) {
                self.keys.push(key.clone());
            }
        }
        self.generation = Some(page.generation);
        true
    }

    /// Force a full reset (new scan run): drops all keys so a fresh
    /// generation starts from an empty view.
    pub fn reset(&mut self) {
        self.keys.clear();
        self.generation = None;
    }

    /// Number of keys currently kept.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no keys are kept.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Keys in scan arrival order.
    pub fn arrival_order(&self) -> &[Vec<u8>] {
        &self.keys
    }

    /// The generation of the last applied page (None = fresh list).
    pub fn generation(&self) -> Option<u64> {
        self.generation
    }

    /// All keys in Redis lexical (raw byte) order. O(n log n) per call —
    /// call when the user asks for sorted, not per frame.
    pub fn sorted(&self) -> Vec<Vec<u8>> {
        let mut v = self.keys.clone();
        v.sort();
        v
    }

    /// A sorted window of the list: the `page_len` keys starting at
    /// `from` (0-based) in lexical order. For UI pagination over the
    /// accumulated view. O(n log n) while `from + page_len` is within
    /// the list; cheaper than sorting then slicing repeatedly only if
    /// cached by the caller — documented, not hidden.
    pub fn sorted_range(&self, from: usize, page_len: usize) -> Vec<Vec<u8>> {
        if from >= self.keys.len() {
            return Vec::new();
        }
        let end = (from + page_len).min(self.keys.len());
        // Partial selection: sort only enough to know the window is the
        // lexically-smallest `end` keys in order. select_nth is unstable
        // API; a full sort of the prefix set is the honest simple bound.
        let mut v = self.keys.clone();
        v.sort();
        v[from..end].to_vec()
    }

    /// Honest count for the UI: `AtLeast(n)` while a scan may still
    /// deliver (caller passes the engine's `KeyCount`; when the engine
    /// says `Exact`, that propagates — the flat list never invents
    /// exactness).
    pub fn key_count(&self, engine_count: KeyCount) -> KeyCount {
        match engine_count {
            KeyCount::Exact(_) => KeyCount::Exact(self.len() as u64),
            KeyCount::AtLeast(_) => KeyCount::AtLeast(self.len() as u64),
            KeyCount::Unknown => {
                if self.keys.is_empty() {
                    KeyCount::Unknown
                } else {
                    KeyCount::AtLeast(self.len() as u64)
                }
            }
        }
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

    #[test]
    fn keeps_arrival_order_and_counts() {
        let mut list = FlatKeyList::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        assert!(list.apply_page(&page(g, &[b"b:1", b"a:1"]), &gen));
        assert!(list.apply_page(&page(g, &[b"c:1"]), &gen));
        assert_eq!(
            list.arrival_order(),
            [b"b:1".to_vec(), b"a:1".to_vec(), b"c:1".to_vec()]
        );
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn sorts_on_demand_lexically() {
        let mut list = FlatKeyList::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        list.apply_page(&page(g, &[b"z", b"m", b"a", b"m"]), &gen);
        assert_eq!(list.sorted(), [b"a".to_vec(), b"m".to_vec(), b"z".to_vec()]);
        // arrival order untouched by sorted()
        assert_eq!(list.arrival_order()[0], b"z".to_vec());
    }

    #[test]
    fn rejects_stale_generation_pages() {
        let mut list = FlatKeyList::new();
        let gen = SearchGeneration::new();
        let g1 = gen.next_generation();
        let g2 = gen.next_generation();
        assert!(list.apply_page(&page(g2, &[b"current"]), &gen));
        assert!(!list.apply_page(&page(g1, &[b"stale"]), &gen)); // dropped
        assert_eq!(list.arrival_order(), [b"current".to_vec()]);
    }

    #[test]
    fn resets_for_a_new_scan_run() {
        let mut list = FlatKeyList::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        list.apply_page(&page(g, &[b"k1"]), &gen);
        list.reset();
        assert!(list.is_empty());
        let g2 = gen.next_generation();
        assert!(list.apply_page(&page(g2, &[b"fresh"]), &gen));
        assert_eq!(list.arrival_order(), [b"fresh".to_vec()]);
    }

    #[test]
    fn sorted_range_windows_lexically() {
        let mut list = FlatKeyList::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        list.apply_page(&page(g, &[b"e", b"d", b"c", b"b", b"a"]), &gen);
        assert_eq!(list.sorted_range(1, 2), [b"b".to_vec(), b"c".to_vec()]);
        assert!(list.sorted_range(9, 5).is_empty());
        assert_eq!(list.sorted_range(3, 99).len(), 2); // clamped
    }

    #[test]
    fn key_count_is_honest_by_propagation() {
        let mut list = FlatKeyList::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        list.apply_page(&page(g, &[b"k", b"k2"]), &gen);
        // engine still scanning → AtLeast over the list's own length
        assert_eq!(list.key_count(KeyCount::AtLeast(2)), KeyCount::AtLeast(2));
        // engine exhausted → Exact over the list's length
        assert_eq!(list.key_count(KeyCount::Exact(99)), KeyCount::Exact(2));
        // empty + unknown → unknown
        let empty = FlatKeyList::new();
        assert_eq!(empty.key_count(KeyCount::Unknown), KeyCount::Unknown);
    }

    #[test]
    fn binary_keys_sort_in_raw_byte_order() {
        let mut list = FlatKeyList::new();
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        list.apply_page(&page(g, &[b"\xff", b"a", b"\x00"]), &gen);
        assert_eq!(list.sorted(), [vec![0x00], b"a".to_vec(), vec![0xff]]);
    }
}
