//! Exact permutation validation for persisted orders. Consume each live slot
//! once; matching the live cardinality then also proves there are no omissions.
use roaring::RoaringBitmap;

pub(super) enum RemainingSlots {
    Dense(Vec<u64>),
    Sparse(RoaringBitmap),
}

impl RemainingSlots {
    pub(super) fn new(live: &RoaringBitmap) -> Self {
        let length = live.max().map_or(0, |last| last as usize + 1).div_ceil(64);
        // A direct-address mask is worthwhile only when it takes no more than
        // the u16 payload of a sparse Roaring set. Far-apart slots must not
        // allocate a mask proportional to the highest possible slot.
        if length as u64 * size_of::<u64>() as u64 > live.len() * size_of::<u16>() as u64 {
            return Self::Sparse(live.clone());
        }
        let mut words = vec![0u64; length];
        let mut ranges = live.iter();
        while let Some(range) = ranges.next_range() {
            let first = *range.start() as usize;
            let last = *range.end() as usize;
            let first_word = first / 64;
            let last_word = last / 64;
            let leading = u64::MAX << (first % 64);
            let trailing = u64::MAX >> (63 - last % 64);
            if first_word == last_word {
                words[first_word] |= leading & trailing;
            } else {
                words[first_word] |= leading;
                words[first_word + 1..last_word].fill(u64::MAX);
                words[last_word] |= trailing;
            }
        }
        Self::Dense(words)
    }

    /// Rejects non-live slots and repeated slots with the same exact check.
    pub(super) fn consume(&mut self, slot: u32) -> bool {
        match self {
            Self::Dense(words) => {
                let Some(word) = words.get_mut(slot as usize / 64) else {
                    return false;
                };
                let mask = 1u64 << (slot % 64);
                let present = *word & mask != 0;
                *word &= !mask;
                present
            }
            Self::Sparse(remaining) => remaining.remove(slot),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compare(live: &RoaringBitmap, order: impl IntoIterator<Item = u32>) {
        let mut actual = RemainingSlots::new(live);
        let mut expected: std::collections::HashSet<_> = live.iter().collect();
        for slot in order {
            assert_eq!(actual.consume(slot), expected.remove(&slot), "slot {slot}");
        }
        for slot in live {
            assert_eq!(actual.consume(slot), expected.remove(&slot));
            assert!(!actual.consume(slot));
        }
    }

    #[test]
    fn matches_independent_set_for_all_small_subsets_and_repetitions() {
        for mask in 0..1u32 << 12 {
            let live: RoaringBitmap = (0..12).filter(|bit| mask & (1 << bit) != 0).collect();
            compare(&live, (0..14).rev().chain(0..14).chain([u32::MAX]));
        }
    }

    #[test]
    fn validates_runs_crossing_word_and_container_boundaries() {
        let mut live: RoaringBitmap = (0..70_000).collect();
        for slot in [0, 1, 62, 63, 64, 65, 127, 128, 4095, 65535, 65536, 69_999] {
            live.remove(slot);
        }
        assert!(matches!(
            RemainingSlots::new(&live),
            RemainingSlots::Dense(_)
        ));
        // Coprime stride visits every slot in a non-monotonic order.
        compare(&live, (0..70_000).map(|index| index * 13 % 70_000));
        compare(&live, (0..70_001).rev());
    }

    #[test]
    fn sparse_extreme_slots_do_not_allocate_a_dense_mask() {
        let live = RoaringBitmap::from([0, 63, 64, 65536, u32::MAX - 1, u32::MAX]);
        assert!(matches!(
            RemainingSlots::new(&live),
            RemainingSlots::Sparse(_)
        ));
        compare(&live, [u32::MAX, 0, u32::MAX, 65537, 64, 63, u32::MAX - 1]);
        compare(&RoaringBitmap::new(), [0, u32::MAX]);
    }
}
