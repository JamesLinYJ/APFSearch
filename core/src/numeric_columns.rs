//! Compact numerical columns with block range bounds. A query reads one dense
//! numeric column, rather than following millions of separately allocated files.
use crate::{index_store::IndexedFile, query::Term};
use roaring::RoaringBitmap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const BLOCK_ENTRIES: usize = 4096;

#[derive(Clone)]
struct Block {
    values: Vec<f64>,
    minimum: f64,
    maximum: f64,
}
impl Block {
    fn empty() -> Self {
        Self {
            values: Vec::with_capacity(BLOCK_ENTRIES),
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
    fn recompute_bounds(&mut self) {
        self.minimum = f64::INFINITY;
        self.maximum = f64::NEG_INFINITY;
        for &value in &self.values {
            if !value.is_nan() {
                self.minimum = self.minimum.min(value);
                self.maximum = self.maximum.max(value);
            }
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
        block.values.push(value);
        block.include(value);
        if !value.is_nan() {
            Arc::make_mut(&mut self.known)
                .try_push(self.length as u32)
                .expect("Numeric column slots append in increasing order");
        }
        self.length += 1;
    }
    fn set(&mut self, slot: u32, value: f64) {
        if slot as usize == self.length {
            self.push(value);
            return;
        }
        let block_index = slot as usize / BLOCK_ENTRIES;
        let offset = slot as usize % BLOCK_ENTRIES;
        let old = self.blocks[block_index].values[offset];
        if old.to_bits() == value.to_bits() {
            return;
        }
        Arc::make_mut(&mut self.blocks[block_index]).values[offset] = value;
        if old.is_nan() != value.is_nan() {
            if value.is_nan() {
                Arc::make_mut(&mut self.known).remove(slot);
            } else {
                Arc::make_mut(&mut self.known).insert(slot);
            }
        }
        self.dirty_blocks.insert(block_index as u32);
    }
    fn finish_update(&mut self) {
        for index in &self.dirty_blocks {
            Arc::make_mut(&mut self.blocks[index as usize]).recompute_bounds();
        }
        self.dirty_blocks.clear();
    }
    fn range(
        &self,
        bounds: Bounds,
        live: &RoaringBitmap,
        cancelled: &AtomicBool,
    ) -> Result<RoaringBitmap, String> {
        let mut bytes = vec![0; self.length.div_ceil(8)];
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
            let output = &mut bytes[offset..offset + block.values.len().div_ceil(8)];
            if bounds.above_lower(block.minimum) && bounds.below_upper(block.maximum) {
                output.fill(u8::MAX);
            } else {
                for (values, byte) in block.values.chunks(8).zip(output) {
                    let mut bits = 0;
                    for (bit, &value) in values.iter().enumerate() {
                        bits |=
                            ((bounds.above_lower(value) && bounds.below_upper(value)) as u8) << bit;
                    }
                    *byte = bits;
                }
            }
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err("Query cancelled".into());
        }
        let inside = RoaringBitmap::from_lsb0_bytes(0, &bytes);
        let mut result = if bounds.negate {
            self.known.as_ref() - &inside
        } else {
            inside & self.known.as_ref()
        };
        result &= live;
        Ok(result)
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
impl Bounds {
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
    pub(crate) fn build(entries: &[Arc<IndexedFile>]) -> Self {
        let mut columns = Self::default();
        for file in entries {
            columns.size.push(size_value(file));
            columns.modified.push(file.modified as f64);
            columns.created.push(file.created as f64);
        }
        columns
    }
    pub(crate) fn set(&mut self, slot: u32, file: &IndexedFile) {
        self.size.set(slot, size_value(file));
        self.modified.set(slot, file.modified as f64);
        self.created.set(slot, file.created as f64);
    }
    pub(crate) fn finish_update(&mut self) {
        self.size.finish_update();
        self.modified.finish_update();
        self.created.finish_update();
    }
    pub(crate) fn exact(
        &self,
        term: &Term,
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
                column.range(
                    Bounds {
                        low: *low,
                        high: *high,
                        include_low: *include_low,
                        include_high: *include_high,
                        negate: *negate,
                    },
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
fn size_value(file: &IndexedFile) -> f64 {
    // An uncomputed directory size is unknown, including inside a negated
    // numeric term. The outer Boolean NOT still complements the live universe.
    if file.is_dir {
        f64::NAN
    } else {
        file.size as f64
    }
}
