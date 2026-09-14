//! Test-only experiment: adapt membership checks while preserving an order.
//! Not enabled in production; signed-window gains were not consistent.
use roaring::RoaringBitmap;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) fn select(
    order: &[u32],
    slot_count: usize,
    matched: &RoaringBitmap,
    offset: usize,
    limit: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<u32>, String> {
    check_cancelled(cancelled)?;
    if limit == 0 || offset as u64 >= matched.len() {
        return Ok(Vec::new());
    }
    let mut page = Page {
        offset,
        limit,
        rank: 0,
        slots: Vec::new(),
    };
    // Conversion clears one word per 64 slots and visits each matching slot.
    // Pay that cost only after the ordered scan has already done comparable work.
    let words = slot_count.div_ceil(64);
    let prefix = words
        .saturating_add(matched.len() as usize)
        .min(order.len());
    if page.collect(&order[..prefix], |slot| matched.contains(slot), cancelled)?
        || prefix == order.len()
    {
        return Ok(page.slots);
    }
    let mut bits = Vec::<u64>::new();
    if bits.try_reserve_exact(words).is_err() {
        page.collect(&order[prefix..], |slot| matched.contains(slot), cancelled)?;
        return Ok(page.slots);
    }
    bits.resize(words, 0);
    for (position, slot) in matched.iter().enumerate() {
        if position % 1024 == 0 {
            check_cancelled(cancelled)?;
        }
        bits[slot as usize / 64] |= 1u64 << (slot % 64);
    }
    page.collect(
        &order[prefix..],
        |slot| bits[slot as usize / 64] & (1u64 << (slot % 64)) != 0,
        cancelled,
    )?;
    Ok(page.slots)
}

struct Page {
    offset: usize,
    limit: usize,
    rank: usize,
    slots: Vec<u32>,
}
impl Page {
    fn collect(
        &mut self,
        order: &[u32],
        contains: impl Fn(u32) -> bool,
        cancelled: &AtomicBool,
    ) -> Result<bool, String> {
        for (position, &slot) in order.iter().enumerate() {
            if position % 1024 == 0 {
                check_cancelled(cancelled)?;
            }
            if contains(slot) {
                if self.rank >= self.offset {
                    self.slots.push(slot);
                }
                self.rank += 1;
                if self.slots.len() == self.limit {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}
fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("Query cancelled".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adaptive_membership_preserves_order_offsets_and_visibility() {
        let order: Vec<u32> = (0..4099).map(|i| i * 37 % 4099).collect();
        let cancelled = AtomicBool::new(false);
        for matched in [
            RoaringBitmap::new(),
            [4080].into_iter().collect(),
            (0..4099).step_by(7).collect(),
            (0..4099).collect(),
        ] {
            for offset in [0, 1, 500, 4000, usize::MAX] {
                for limit in [0, 1, 200] {
                    let expected: Vec<_> = order
                        .iter()
                        .copied()
                        .filter(|slot| matched.contains(*slot))
                        .skip(offset)
                        .take(limit)
                        .collect();
                    assert_eq!(
                        select(&order, 4100, &matched, offset, limit, &cancelled).unwrap(),
                        expected
                    );
                }
            }
        }
        cancelled.store(true, Ordering::Relaxed);
        assert!(select(&order, 4100, &(0..4099).collect(), 0, 200, &cancelled).is_err());
    }
}
