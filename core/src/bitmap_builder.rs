//! Stream consecutive matches into ranges without buffering the result slots.
use roaring::RoaringBitmap;

#[derive(Default)]
pub(crate) struct RunBitmapBuilder {
    bitmap: RoaringBitmap,
    pending: Option<(u32, u32)>,
}

impl RunBitmapBuilder {
    pub(crate) fn insert(&mut self, slot: u32) {
        if let Some((_, end)) = &mut self.pending
            && end.checked_add(1) == Some(slot)
        {
            *end = slot;
            return;
        }
        self.flush();
        self.pending = Some((slot, slot));
    }

    fn flush(&mut self) {
        if let Some((start, end)) = self.pending.take() {
            if start == end {
                self.bitmap.insert(start);
            } else {
                self.bitmap.insert_range(start..=end);
            }
        }
    }

    pub(crate) fn finish(mut self) -> RoaringBitmap {
        self.flush();
        self.bitmap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verify(slots: impl IntoIterator<Item = u32>) {
        let mut builder = RunBitmapBuilder::default();
        let mut expected = RoaringBitmap::new();
        for slot in slots {
            builder.insert(slot);
            expected.insert(slot);
        }
        assert_eq!(builder.finish(), expected);
    }

    #[test]
    fn all_small_subsets_preserve_gaps_and_singletons() {
        for mask in 0..4096u32 {
            verify((0..12).filter(|bit| mask & (1 << bit) != 0));
        }
    }

    #[test]
    fn container_boundaries_duplicates_and_maximum_slot_are_exact() {
        verify(65530..65545);
        verify([0, 1, 1, 0, 5, 3, 4, 2]);
        verify([u32::MAX - 1, u32::MAX, 0, u32::MAX]);
        verify((0..1_000_000).step_by(97));
    }
}
