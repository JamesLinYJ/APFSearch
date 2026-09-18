//! Immutable sorted leaf blocks. Ranks index a compact prefix-sum directory;
//! scans read whole slices, and updates copy only affected leaves.
use crate::entry_table::Column;
use std::{
    cmp::Ordering,
    ops::Index,
    sync::{Arc, OnceLock},
};
pub(crate) const LEAF_LENGTH: usize = 4096;
#[derive(Clone, Default)]
pub(crate) struct OrderBlock {
    pub(crate) slots: Column<u32>,
    pub(crate) section: OnceLock<Arc<crate::snapshot_cache::Section>>,
}
impl OrderBlock {
    fn owned(slots: Vec<u32>) -> Self {
        Self {
            slots: Column::owned(slots),
            section: OnceLock::new(),
        }
    }
    fn edit(&mut self) -> &mut Vec<u32> {
        self.section.take();
        self.slots.make_mut()
    }
}
#[derive(Clone, Default)]
pub(crate) struct SlotOrder {
    pub(crate) blocks: Vec<Arc<OrderBlock>>,
    ends: Vec<usize>,
}
impl SlotOrder {
    pub(crate) fn from_blocks(blocks: Vec<Arc<OrderBlock>>) -> Self {
        debug_assert!(
            blocks.iter().all(|block| !block.slots.values().is_empty()
                && block.slots.values().len() <= LEAF_LENGTH)
        );
        let mut total = 0;
        let ends = blocks
            .iter()
            .map(|block| {
                total += block.slots.values().len();
                total
            })
            .collect();
        Self { blocks, ends }
    }
    pub(crate) fn len(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }
    pub(crate) fn get(&self, rank: usize) -> Option<&u32> {
        let block = self.ends.partition_point(|end| *end <= rank);
        self.blocks
            .get(block)
            .map(|leaf| &leaf.slots.values()[rank - self.start(block)])
    }
    fn start(&self, block: usize) -> usize {
        if block == 0 { 0 } else { self.ends[block - 1] }
    }
    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &u32> {
        self.blocks.iter().flat_map(|block| block.slots.values())
    }
    pub(crate) fn to_vec(&self) -> Vec<u32> {
        self.iter().copied().collect()
    }
    pub(crate) fn extend_range(&self, output: &mut Vec<u32>, start: usize, end: usize) {
        assert!(start <= end && end <= self.len());
        if start == end {
            return;
        }
        let first = self.ends.partition_point(|rank| *rank <= start);
        for block in first..self.blocks.len() {
            let base = self.start(block);
            if base >= end {
                break;
            }
            let values = self.blocks[block].slots.values();
            output.extend_from_slice(
                &values[start.saturating_sub(base)..values.len().min(end - base)],
            );
        }
    }
    pub(crate) fn binary_search_by(
        &self,
        mut compare: impl FnMut(&u32) -> Ordering,
    ) -> Result<usize, usize> {
        let block = self
            .blocks
            .partition_point(|leaf| compare(leaf.slots.values().last().unwrap()).is_lt());
        if block == self.blocks.len() {
            return Err(self.len());
        }
        let base = self.start(block);
        self.blocks[block]
            .slots
            .values()
            .binary_search_by(compare)
            .map(|rank| base + rank)
            .map_err(|rank| base + rank)
    }
    #[cfg(test)]
    pub(crate) fn windows(&self, length: usize) -> impl Iterator<Item = [u32; 2]> {
        assert_eq!(length, 2);
        self.iter().zip(self.iter().skip(1)).map(|(&a, &b)| [a, b])
    }
    pub(crate) fn updated(
        &self,
        removed_ranks: &mut Vec<usize>,
        replacements: &[u32],
        compare: impl Fn(u32, u32) -> Ordering,
    ) -> Self {
        removed_ranks.sort_unstable();
        removed_ranks.dedup();
        let mut blocks = self.blocks.clone();
        let mut first = 0;
        while first < removed_ranks.len() {
            let block = self
                .ends
                .partition_point(|end| *end <= removed_ranks[first]);
            assert!(block < blocks.len());
            let end = removed_ranks.partition_point(|rank| *rank < self.ends[block]);
            let base = self.start(block);
            let mut next = first;
            let mut offset = 0;
            Arc::make_mut(&mut blocks[block]).edit().retain(|_| {
                let keep = next == end || removed_ranks[next] != base + offset;
                if !keep {
                    next += 1;
                }
                offset += 1;
                keep
            });
            first = end;
        }
        blocks.retain(|block| !block.slots.values().is_empty());
        let mut first = 0;
        while first < replacements.len() && !blocks.is_empty() {
            let index = blocks
                .partition_point(|block| {
                    compare(*block.slots.values().last().unwrap(), replacements[first]).is_lt()
                })
                .min(blocks.len() - 1);
            let last = *blocks[index].slots.values().last().unwrap();
            let end = if index + 1 == blocks.len() {
                replacements.len()
            } else {
                first + replacements[first..].partition_point(|slot| !compare(*slot, last).is_gt())
            };
            let slots = Arc::make_mut(&mut blocks[index]).edit();
            crate::index_store::ordered_merge::insert_sorted(
                slots,
                &replacements[first..end],
                &compare,
            );
            first = end;
        }
        if blocks.is_empty() && !replacements.is_empty() {
            return replacements.to_vec().into();
        }
        let mut result: Vec<Arc<OrderBlock>> = Vec::with_capacity(blocks.len());
        for block in blocks {
            let values = block.slots.values();
            if values.len() > LEAF_LENGTH {
                let count = values.len().div_ceil(LEAF_LENGTH);
                let length = values.len().div_ceil(count);
                result.extend(
                    values
                        .chunks(length)
                        .map(|part| Arc::new(OrderBlock::owned(part.to_vec()))),
                );
            } else if result
                .last()
                .is_some_and(|previous| previous.slots.values().len() + values.len() <= LEAF_LENGTH)
            {
                Arc::make_mut(result.last_mut().unwrap())
                    .edit()
                    .extend_from_slice(values);
            } else {
                result.push(block);
            }
        }
        Self::from_blocks(result)
    }
    pub(crate) fn inventory(&self, inventory: &mut crate::memory_inventory::Inventory) {
        for block in &self.blocks {
            block.slots.inventory(inventory);
        }
        inventory.record(
            "order_directory_capacity_bytes",
            self as *const _ as usize,
            self.blocks.capacity() * std::mem::size_of::<Arc<OrderBlock>>()
                + self.ends.capacity() * std::mem::size_of::<usize>(),
        );
    }
    pub(crate) fn allocations(&self) -> Vec<((u8, usize), usize)> {
        let mut allocations = vec![(
            (1, self as *const _ as usize),
            std::mem::size_of::<Self>()
                + self.blocks.capacity() * std::mem::size_of::<Arc<OrderBlock>>()
                + self.ends.capacity() * std::mem::size_of::<usize>(),
        )];
        for block in &self.blocks {
            allocations.push((
                (4, Arc::as_ptr(block) as usize),
                std::mem::size_of::<OrderBlock>(),
            ));
            if let Some((identity, bytes)) = block.slots.owned_allocation() {
                allocations.push(((5, identity), bytes));
            }
        }
        allocations
    }
}
impl From<Vec<u32>> for SlotOrder {
    fn from(values: Vec<u32>) -> Self {
        Self::from_blocks(
            values
                .chunks(LEAF_LENGTH)
                .map(|part| Arc::new(OrderBlock::owned(part.to_vec())))
                .collect(),
        )
    }
}
impl Index<usize> for SlotOrder {
    type Output = u32;
    fn index(&self, index: usize) -> &u32 {
        self.get(index).expect("Order rank out of range")
    }
}
impl std::fmt::Debug for SlotOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}
impl PartialEq for SlotOrder {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}
impl PartialEq<Vec<u32>> for SlotOrder {
    fn eq(&self, other: &Vec<u32>) -> bool {
        self.iter().eq(other.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_leaf_splits_merges_and_empty_replacement_match_flat_order() {
        let mut reference: Vec<u32> = (0..18000).map(|n| n * 32).collect();
        let original: SlotOrder = reference.clone().into();
        let mut order = original.clone();
        let mut seed = 37u64;
        for round in 0..32u32 {
            let mut removed = Vec::new();
            for _ in 0..(round as usize * 91 + 1).min(reference.len()) {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                removed.push(seed as usize % reference.len());
            }
            removed.sort_unstable();
            removed.dedup();
            let mut replacements: Vec<u32> = (0..1200)
                .map(|n| n * 64 + round + 1)
                .filter(|value| reference.binary_search(value).is_err())
                .collect();
            replacements.sort_unstable();
            let next = order.updated(&mut removed, &replacements, |a, b| a.cmp(&b));
            reference = reference
                .into_iter()
                .enumerate()
                .filter(|(rank, _)| removed.binary_search(rank).is_err())
                .map(|(_, value)| value)
                .chain(replacements)
                .collect();
            reference.sort_unstable();
            assert_eq!(next, reference);
            assert!(
                next.blocks
                    .iter()
                    .all(|block| block.slots.values().len() <= LEAF_LENGTH)
            );
            order = next;
        }
        assert_eq!(original.len(), 18000);
        let empty = order.updated(&mut (0..order.len()).collect(), &[], |a, b| a.cmp(&b));
        assert_eq!(empty.len(), 0);
        assert_eq!(
            empty.updated(&mut vec![], &[0, 42, u32::MAX], |a, b| a.cmp(&b)),
            vec![0, 42, u32::MAX]
        );
    }
    #[test]
    fn block_updates_share_unchanged_leaves_and_keep_rank_lookup_exact() {
        let original: SlotOrder = (0..20000u32).map(|n| n * 2).collect::<Vec<_>>().into();
        let updated = original.updated(&mut vec![4100, 16000], &[8201, 32001, 40001], |a, b| {
            a.cmp(&b)
        });
        let expected: Vec<_> = (0..20000u32)
            .map(|n| n * 2)
            .filter(|v| *v != 8200 && *v != 32000)
            .chain([8201, 32001, 40001])
            .collect();
        let mut expected = expected;
        expected.sort_unstable();
        assert_eq!(updated, expected);
        assert!(Arc::ptr_eq(&updated.blocks[0], &original.blocks[0]));
        for (rank, &value) in expected.iter().enumerate() {
            assert_eq!(updated.get(rank), Some(&value));
            assert_eq!(updated.binary_search_by(|v| v.cmp(&value)), Ok(rank));
        }
        assert_eq!(original.len(), 20000);
        let mut page = Vec::new();
        updated.extend_range(&mut page, 4090, 8200);
        assert_eq!(page, expected[4090..8200]);
    }
}
