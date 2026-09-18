//! Reusable term-set differences. Unchanged membership must not make a shared
//! posting mutable: doing so clones a corpus-wide bitmap for no logical change.
use crate::posting_map::{PostingKey, PostingMap};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct GramChanges<const N: usize> {
    previous: Vec<[u8; N]>,
    next: Vec<[u8; N]>,
}

impl<const N: usize> GramChanges<N>
where
    [u8; N]: PostingKey,
{
    pub(crate) fn apply(
        &mut self,
        postings: &mut Arc<PostingMap<[u8; N]>>,
        slot: u32,
        previous: &str,
        next: &str,
    ) {
        if previous == next {
            return;
        }
        for (terms, text) in [(&mut self.previous, previous), (&mut self.next, next)] {
            terms.clear();
            terms.extend(
                text.as_bytes()
                    .windows(N)
                    .map(|bytes| <[u8; N]>::try_from(bytes).unwrap()),
            );
            terms.sort_unstable();
            terms.dedup();
        }
        let (mut before, mut after) = (0, 0);
        while before < self.previous.len() || after < self.next.len() {
            match (self.previous.get(before), self.next.get(after)) {
                (Some(old), Some(new)) if old == new => {
                    before += 1;
                    after += 1;
                }
                (Some(old), new) if new.is_none_or(|new| old < new) => {
                    let postings = Arc::make_mut(postings);
                    if let Some(bitmap) = postings.get_mut(old) {
                        let bitmap = Arc::make_mut(bitmap);
                        bitmap.remove(slot);
                        if bitmap.is_empty() {
                            postings.remove(old);
                        }
                    }
                    before += 1;
                }
                (_, Some(new)) => {
                    Arc::make_mut(Arc::make_mut(postings).entry(*new).or_default()).insert(slot);
                    after += 1;
                }
                _ => unreachable!("Both term sets are exhausted"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roaring::RoaringBitmap;

    #[test]
    fn shared_terms_keep_their_bitmap_and_changed_terms_preserve_other_slots() {
        let mut postings = Arc::new(PostingMap::<[u8; 3]>::default());
        let original = "report-old.txt";
        for term in original.as_bytes().windows(3) {
            Arc::make_mut(&mut postings).insert(
                term.try_into().unwrap(),
                Arc::new(RoaringBitmap::from([7, 9])),
            );
        }
        let retained = postings.clone();
        GramChanges::default().apply(&mut postings, 7, original, "report-new.txt");
        assert!(Arc::ptr_eq(
            postings.get(b"rep").unwrap(),
            retained.get(b"rep").unwrap()
        ));
        assert_eq!(
            postings.get(b"old").unwrap().as_ref(),
            &RoaringBitmap::from([9])
        );
        assert_eq!(
            postings.get(b"new").unwrap().as_ref(),
            &RoaringBitmap::from([7])
        );
        assert_eq!(
            retained.get(b"old").unwrap().as_ref(),
            &RoaringBitmap::from([7, 9])
        );
    }

    #[test]
    fn term_sets_match_an_independent_reference_across_repeated_updates() {
        let mut postings = Arc::new(PostingMap::<[u8; 3]>::default());
        let mut changes = GramChanges::default();
        let mut previous = "";
        for next in [
            "a",
            "aa",
            "aaaaa",
            "baaaaab",
            "报告报告.txt",
            "straße.txt",
            "café.txt",
            "",
            "abc",
            "abc",
        ] {
            let retained = postings.clone();
            changes.apply(&mut postings, 0, previous, next);
            let expected: std::collections::HashSet<[u8; 3]> = next
                .as_bytes()
                .windows(3)
                .map(|term| term.try_into().unwrap())
                .collect();
            for shard in 0..crate::posting_map::SHARDS {
                for (term, bitmap) in postings.partition_entries(shard) {
                    assert!(expected.contains(term));
                    assert_eq!(bitmap.as_ref(), &RoaringBitmap::from([0]));
                }
            }
            for term in &expected {
                assert!(postings.get(term).unwrap().contains(0));
            }
            if previous == next {
                assert!(Arc::ptr_eq(&retained, &postings));
            }
            previous = next;
        }
    }
}
