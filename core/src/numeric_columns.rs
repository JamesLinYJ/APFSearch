//! Compact numerical columns with block range bounds. A query reads one dense
//! numeric column, rather than following millions of separately allocated files.
use crate::entry_table::FileEntry;
use crate::{index_store::EntryTable, query::Term};
use roaring::RoaringBitmap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const BLOCK_ENTRIES: usize = 4096;

#[derive(Clone)]
struct Block {
    length: usize,
    minimum: f64,
    maximum: f64,
}
impl Block {
    fn empty() -> Self {
        Self {
            length: 0,
            minimum: f64::INFINITY,
            maximum: f64::NEG_INFINITY,
        }
    }
    fn include(&mut self, value: f64) {
        if !value.is_nan() {
            self.minimum = self.minimum.min(value);
            self.maximum = self.maximum.max(value);
        }
    }
}

#[derive(Clone, Default)]
struct Column {
    blocks: Vec<Arc<Block>>,
    known: Arc<RoaringBitmap>,
    length: usize,
    dirty_blocks: RoaringBitmap,
}
impl Column {
    fn push(&mut self, value: f64) {
        if self.length.is_multiple_of(BLOCK_ENTRIES) {
            self.blocks.push(Arc::new(Block::empty()));
        }
        let block = Arc::make_mut(self.blocks.last_mut().unwrap());
        block.length += 1;
        block.include(value);
        if !value.is_nan() {
            // Roaring's checked append searches the current maximum in dense
            // containers on every value. Direct insertion addresses its bit.
            Arc::make_mut(&mut self.known).insert(self.length as u32);
        }
        self.length += 1;
    }
    fn set(&mut self, slot: u32, value: f64) {
        if slot as usize == self.length {
            self.push(value);
            return;
        }
        if value.is_nan() {
            Arc::make_mut(&mut self.known).remove(slot);
        } else {
            Arc::make_mut(&mut self.known).insert(slot);
        }
        self.dirty_blocks.insert(slot / BLOCK_ENTRIES as u32);
    }
    fn finish_update(&mut self, entries: &EntryTable, field: NumericField) {
        for index in &self.dirty_blocks {
            let block = Arc::make_mut(&mut self.blocks[index as usize]);
            block.minimum = f64::INFINITY;
            block.maximum = f64::NEG_INFINITY;
            let source = &entries.chunks[index as usize];
            for slot in 0..source.len() {
                block.include(field.value(source, slot));
            }
        }
        self.dirty_blocks.clear();
    }
    fn range(
        &self,
        bounds: Bounds,
        entries: &EntryTable,
        field: NumericField,
        live: &RoaringBitmap,
        cancelled: &AtomicBool,
    ) -> Result<RoaringBitmap, String> {
        let ranks = match field {
            NumericField::Size => bounds.rank_range(|value: u64| value as f64),
            NumericField::Modified | NumericField::Created => {
                bounds.rank_range(|value: i64| value as f64)
            }
        };
        crate::query_scratch::with_bytes(self.length.div_ceil(8), |bytes| {
            bytes.resize(self.length.div_ceil(8), 0);
            for (index, block) in self.blocks.iter().enumerate() {
                if cancelled.load(Ordering::Relaxed) {
                    return Err("Query cancelled".into());
                }
                if block.minimum > block.maximum
                    || !bounds.above_lower(block.maximum)
                    || !bounds.below_upper(block.minimum)
                {
                    continue;
                }
                let offset = index * BLOCK_ENTRIES / 8;
                let output = &mut bytes[offset..offset + block.length.div_ceil(8)];
                if bounds.above_lower(block.minimum) && bounds.below_upper(block.maximum) {
                    output.fill(u8::MAX);
                } else if let Some((lower, upper)) = ranks {
                    let source = &entries.chunks[index];
                    match field {
                        NumericField::Size => source.size.fill_rank_range(output, lower, upper),
                        NumericField::Modified => {
                            source.modified.fill_rank_range(output, lower, upper)
                        }
                        NumericField::Created => {
                            source.created.fill_rank_range(output, lower, upper)
                        }
                    }
                }
            }
            if cancelled.load(Ordering::Relaxed) {
                return Err("Query cancelled".into());
            }
            let inside = RoaringBitmap::from_lsb0_bytes(0, bytes);
            let mut result = if bounds.negate {
                self.known.as_ref() - &inside
            } else {
                inside & self.known.as_ref()
            };
            result &= live;
            Ok(result)
        })
    }
}
#[derive(Clone, Copy)]
enum NumericField {
    Size,
    Modified,
    Created,
}
impl NumericField {
    fn value(self, chunk: &crate::entry_table::EntryChunk, slot: usize) -> f64 {
        match self {
            Self::Size if chunk.states.values()[slot] & 1 != 0 => f64::NAN,
            Self::Size => chunk.size.get(slot) as f64,
            Self::Modified => chunk.modified.get(slot) as f64,
            Self::Created => chunk.created.get(slot) as f64,
        }
    }
}
#[derive(Clone, Copy)]
struct Bounds {
    low: f64,
    high: f64,
    include_low: bool,
    include_high: bool,
    negate: bool,
}
#[cfg(test)]
mod rank_bounds_tests {
    use super::*;
    use crate::integer_column::Scalar;
    #[test]
    fn rank_boundaries_preserve_float_ties_infinities_and_signed_limits() {
        fn check<T: Scalar>(values: &[T], convert: impl Fn(T) -> f64 + Copy) {
            let bounds: Vec<_> = [f64::NEG_INFINITY, -0.5, 0.0, 0.5, f64::INFINITY]
                .into_iter()
                .chain(values.iter().copied().map(convert))
                .collect();
            for &low in &bounds {
                for &high in &bounds {
                    if low > high {
                        continue;
                    }
                    for include_low in [false, true] {
                        for include_high in [false, true] {
                            let interval = Bounds {
                                low,
                                high,
                                include_low,
                                include_high,
                                negate: false,
                            };
                            let ranks = interval.rank_range(convert);
                            for &value in values {
                                assert_eq!(
                                    ranks.is_some_and(
                                        |(start, end)| (start..=end).contains(&value.rank())
                                    ),
                                    interval.above_lower(convert(value))
                                        && interval.below_upper(convert(value))
                                );
                            }
                        }
                    }
                }
            }
        }
        check(
            &[
                0u64,
                1,
                (1 << 53) - 1,
                1 << 53,
                (1 << 53) + 1,
                (1 << 53) + 2,
                u64::MAX - 1,
                u64::MAX,
            ],
            |value| value as f64,
        );
        check(
            &[
                i64::MIN,
                i64::MIN + 1,
                -(1 << 53) - 1,
                -1,
                0,
                1,
                (1 << 53) + 1,
                i64::MAX - 1,
                i64::MAX,
            ],
            |value| value as f64,
        );
    }
}
impl Bounds {
    fn rank_range<T: crate::integer_column::Scalar>(
        self,
        convert: impl Fn(T) -> f64,
    ) -> Option<(u64, u64)> {
        // Float conversion is monotone, including ties above 2^53. Locate exact
        // rank boundaries once, preserving the query language's current float
        // semantics without converting every stored integer during the scan.
        let first = |predicate: &dyn Fn(f64) -> bool| {
            let (mut low, mut high) = (0u128, 1u128 << 64);
            while low < high {
                let middle = (low + high) / 2;
                let value = convert(T::from_rank(middle as u64).unwrap());
                if predicate(value) {
                    high = middle;
                } else {
                    low = middle + 1;
                }
            }
            low
        };
        let lower = first(&|value| self.above_lower(value));
        let end = first(&|value| !self.below_upper(value));
        (lower < end).then(|| (lower as u64, (end - 1) as u64))
    }

    fn above_lower(self, value: f64) -> bool {
        if self.include_low {
            value >= self.low
        } else {
            value > self.low
        }
    }
    fn below_upper(self, value: f64) -> bool {
        if self.include_high {
            value <= self.high
        } else {
            value < self.high
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct NumericColumns {
    size: Column,
    modified: Column,
    created: Column,
}
impl NumericColumns {
    pub(crate) fn supports(term: &Term) -> bool {
        matches!(term, Term::Number { field, .. } | Term::Unknown { field, .. } if matches!(field.as_str(), "size" | "modified" | "created"))
    }
    pub(crate) fn build(entries: &crate::index_store::EntryTable) -> Self {
        let mut columns = Self::default();
        for file in entries.iter() {
            columns.size.push(size_value(&file));
            columns.modified.push(file.modified() as f64);
            columns.created.push(file.created() as f64);
        }
        columns
    }
    pub(crate) fn set(&mut self, slot: u32, file: &impl FileEntry) {
        self.size.set(slot, size_value(file));
        self.modified.set(slot, file.modified() as f64);
        self.created.set(slot, file.created() as f64);
    }
    pub(crate) fn finish_update(&mut self, entries: &EntryTable) {
        self.size.finish_update(entries, NumericField::Size);
        self.modified.finish_update(entries, NumericField::Modified);
        self.created.finish_update(entries, NumericField::Created);
    }
    pub(crate) fn exact(
        &self,
        term: &Term,
        entries: &EntryTable,
        live: &RoaringBitmap,
        cancelled: &AtomicBool,
    ) -> Result<Option<RoaringBitmap>, String> {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Query cancelled".into());
        }
        let result = match term {
            Term::Number {
                field,
                low,
                high,
                include_low,
                include_high,
                negate,
            } => {
                let column = match field.as_str() {
                    "size" => &self.size,
                    "modified" => &self.modified,
                    "created" => &self.created,
                    _ => return Ok(None),
                };
                let source = match field.as_str() {
                    "size" => NumericField::Size,
                    "modified" => NumericField::Modified,
                    _ => NumericField::Created,
                };
                column.range(
                    Bounds {
                        low: *low,
                        high: *high,
                        include_low: *include_low,
                        include_high: *include_high,
                        negate: *negate,
                    },
                    entries,
                    source,
                    live,
                    cancelled,
                )?
            }
            Term::Unknown { field, negate } => {
                let known = match field.as_str() {
                    "size" => &self.size.known,
                    "modified" => &self.modified.known,
                    "created" => &self.created.known,
                    _ => return Ok(None),
                };
                if *negate {
                    known.as_ref() & live
                } else {
                    live - known.as_ref()
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(result))
    }
}
fn size_value(file: &impl FileEntry) -> f64 {
    // An uncomputed directory size is unknown, including inside a negated
    // numeric term. The outer Boolean NOT still complements the live universe.
    if file.is_dir() {
        f64::NAN
    } else {
        file.size() as f64
    }
}
