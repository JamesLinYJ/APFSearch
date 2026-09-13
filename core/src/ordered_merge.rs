//! Insert a sorted delta without repeatedly shifting an entire slot vector.
use std::cmp::Ordering;

/// Both inputs must already be sorted by `compare`. Existing items precede new
/// items with equal keys. Galloping searches visit only logarithmically many
/// keys per intervening run; reverse bulk copies move each old slot at most
/// once. Additional workspace is O(k) insertion offsets, not another O(n) array.
pub(super) fn insert_sorted(
    slots: &mut Vec<u32>,
    replacements: &[u32],
    mut compare: impl FnMut(u32, u32) -> Ordering,
) {
    if replacements.is_empty() {
        return;
    }
    let mut positions = Vec::with_capacity(replacements.len());
    let mut cursor = 0;
    for &replacement in replacements {
        let remaining = &slots[cursor..];
        let mut low = 0;
        let mut high = 0;
        // Probe 0, 1, 3, 7, ... instead of re-reading every intervening path.
        while high < remaining.len() && !compare(remaining[high], replacement).is_gt() {
            low = high + 1;
            high = high.saturating_mul(2).saturating_add(1);
        }
        let end = high.min(remaining.len());
        cursor += low
            + remaining[low..end]
                .partition_point(|slot| !compare(*slot, replacement).is_gt());
        positions.push(cursor);
    }
    let mut end = slots.len();
    slots.resize(end + replacements.len(), 0);
    for index in (0..replacements.len()).rev() {
        let start = positions[index];
        slots.copy_within(start..end, start + index + 1);
        slots[start + index] = replacements[index];
        end = start;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhaustive_small_merges_match_a_sorted_reference() {
        for old_mask in 0u32..128 {
            for new_mask in 0u32..128 {
                let mut old: Vec<_> = (0..7).filter(|bit| old_mask & (1 << bit) != 0).collect();
                let new: Vec<_> = (0..7).filter(|bit| new_mask & (1 << bit) != 0).collect();
                let mut expected = old.clone();
                expected.extend_from_slice(&new);
                expected.sort_unstable();
                insert_sorted(&mut old, &new, |a, b| a.cmp(&b));
                assert_eq!(old, expected, "{old_mask}, {new_mask}");
            }
        }
    }

    #[test]
    fn descending_and_equal_keys_keep_their_order() {
        let mut slots = vec![9, 7, 5, 3, 1];
        insert_sorted(&mut slots, &[10, 8, 6, 4, 2, 0], |a, b| b.cmp(&a));
        assert_eq!(slots, (0..=10).rev().collect::<Vec<_>>());
        let mut slots = vec![10, 11, 20, 30];
        insert_sorted(&mut slots, &[12, 13, 21, 31], |a, b| (a / 10).cmp(&(b / 10)));
        assert_eq!(slots, vec![10, 11, 12, 13, 20, 21, 30, 31]);
    }

    #[test]
    fn sparse_delta_does_not_compare_every_key() {
        let mut slots: Vec<_> = (0..100_000).map(|slot| slot * 2).collect();
        let replacements: Vec<_> = (0..33).map(|slot| slot * 6_000 + 1).collect();
        let mut comparisons = 0;
        insert_sorted(&mut slots, &replacements, |a, b| {
            comparisons += 1;
            a.cmp(&b)
        });
        assert!(comparisons < 2_000, "{comparisons} comparisons for 33 updates");
        assert!(slots.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(slots.len(), 100_033);
    }
}
